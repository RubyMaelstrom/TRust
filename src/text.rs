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

use parley::{
    FontContext, FontFamily, FontStyle, FontWeight, Language, Layout, LayoutContext, LineHeight,
    OverflowWrap, PositionedLayoutItem, StyleProperty, TextWrapMode, WordBreak,
    editing::PlainEditor,
};
use unicode_segmentation::UnicodeSegmentation as _;

use crate::core::{ImeAction, Key, KeyInput, KeyState};

mod layout_cache;
mod outline;
pub(crate) use outline::{append_text_path, x_height};

/// CSS-facing text style. No Parley, Glifo, or renderer type escapes this
/// boundary.
#[derive(Clone, Debug, PartialEq)]
pub struct TextStyle {
    pub family: String,
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
}

impl Default for TextStyle {
    fn default() -> Self {
        Self {
            family: String::from("sans-serif"),
            language: None,
            size: 16.0,
            weight: 400.0,
            italic: false,
            letter_spacing: 0.0,
            word_spacing: 0.0,
            line_height: CssLineHeight::Normal,
            underline: false,
            strikethrough: false,
        }
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

/// A TRust-owned Unicode text editor backed by Parley's cluster, bidi and
/// line-layout machinery. Native frontends never manipulate byte indices or
/// guess grapheme boundaries themselves. This is used by browser chrome and
/// HTML text controls; password masking is a paint concern, while the logical
/// value remains available for form submission.
#[derive(Clone)]
pub struct TextEditor {
    editor: PlainEditor<()>,
    multiline: bool,
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
        styles.insert(StyleProperty::FontStyle(if style.italic {
            FontStyle::Italic
        } else {
            FontStyle::Normal
        }));
        styles.insert(StyleProperty::LetterSpacing(style.letter_spacing));
        styles.insert(StyleProperty::WordSpacing(style.word_spacing));
        styles.insert(StyleProperty::LineHeight(match style.line_height {
            CssLineHeight::Normal => LineHeight::default(),
            CssLineHeight::Number(number) => LineHeight::FontSizeRelative(number.max(0.0)),
            CssLineHeight::Length(px) => LineHeight::Absolute(px.max(0.0)),
        }));
        let mut this = Self { editor, multiline };
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
        self.editor
            .set_width(self.multiline.then_some(width.max(1.0)));
    }

    pub fn is_composing(&self) -> bool {
        self.editor.is_composing()
    }

    pub fn selection(&self) -> std::ops::Range<usize> {
        self.editor.raw_selection().text_range()
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
                    self.drive(|driver| driver.finish_compose());
                } else {
                    self.replace_selection(text);
                }
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
            system.refresh_page_fonts();
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
            system.refresh_page_fonts();
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
    #[cfg(test)]
    shaped_input_bytes: usize,
}

// A gallery-sized DOM can contain several thousand distinct words/runs. Keep
// that normal working set resident so a JS geometry pass followed by a native
// relayout does not turn FIFO eviction into a full sequential cache miss. The
// independent byte ceiling remains the authoritative memory bound.
const MAX_SHAPE_CACHE_ENTRIES: usize = 8_192;
const MAX_SHAPE_CACHE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct ShapeKey {
    text: String,
    style: TextStyleKey,
    quantize: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct TextStyleKey {
    family: String,
    language: Option<String>,
    size: u32,
    weight: u32,
    italic: bool,
    letter_spacing: u32,
    word_spacing: u32,
    line_height: (u8, u32),
    underline: bool,
    strikethrough: bool,
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
            language: style.language.clone(),
            size: style.size.to_bits(),
            weight: style.weight.to_bits(),
            italic: style.italic,
            letter_spacing: style.letter_spacing.to_bits(),
            word_spacing: style.word_spacing.to_bits(),
            line_height,
            underline: style.underline,
            strikethrough: style.strikethrough,
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
            shape_cache: HashMap::new(),
            shape_order: VecDeque::new(),
            shape_cache_bytes: 0,
            layout_cache: Default::default(),
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
        self.refresh_page_fonts();
        let key = ShapeKey {
            text: text.to_string(),
            style: style.into(),
            quantize,
        };
        if let Some(hit) = self.shape_cache.get(&key) {
            return hit.shaped.clone();
        }
        let shaped = self.shape_uncached(text, style, quantize);
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

    fn shape_uncached(&mut self, text: &str, style: &TextStyle, quantize: bool) -> ShapedText {
        if text.is_empty() || style.size <= 0.0 {
            return ShapedText {
                text: text.to_string(),
                ..ShapedText::default()
            };
        }
        let mut layout = self.styled_layout(text, style, quantize, None);
        layout.break_all_lines(None);
        let mut retained = retain_first_line(text, &layout);
        retained.underline = style.underline;
        retained.strikethrough = style.strikethrough;
        retained
    }

    fn styled_layout(
        &mut self,
        text: &str,
        style: &TextStyle,
        quantize: bool,
        breaks: Option<TextBreakStyle>,
    ) -> Layout<()> {
        #[cfg(test)]
        {
            self.shaped_input_bytes += text.len();
        }
        let mut builder = self
            .layouts
            .ranged_builder(&mut self.fonts, text, 1.0, quantize);
        configure_layout(&mut builder, style, breaks);
        builder.build(text)
    }

    fn first_line_end_uncached(
        &mut self,
        text: &str,
        style: &TextStyle,
        width: f32,
        breaks: TextBreakStyle,
    ) -> usize {
        self.refresh_page_fonts();
        if text.is_empty() || width <= 0.0 || style.size <= 0.0 {
            return 0;
        }
        let mut layout = self.styled_layout(text, style, true, Some(breaks));
        let mut breaker = layout.break_lines();
        breaker.state_mut().set_layout_max_advance(width.max(0.01));
        breaker.state_mut().set_line_max_advance(width.max(0.01));
        breaker.break_next();
        breaker.finish();
        layout
            .lines()
            .next()
            .map(|line| line.text_range().end.min(text.len()))
            .unwrap_or(0)
    }

    fn wrapped_lines_uncached(
        &mut self,
        text: &str,
        style: &TextStyle,
        first_width: f32,
        width: f32,
        breaks: TextBreakStyle,
    ) -> Vec<ShapedText> {
        self.refresh_page_fonts();
        if text.is_empty() || style.size <= 0.0 {
            return Vec::new();
        }
        // Shape one paragraph, then advance a single greedy breaker. Rebuilding
        // the shrinking tail on every line makes long preserved text quadratic.
        // CSS Text 3 #word-break-shaping also requires joining forms to survive
        // emergency line breaks; retain glyphs from the whole shaped paragraph.
        let mut layout = self.styled_layout(text, style, true, Some(breaks));
        let mut breaker = layout.break_lines();
        breaker.state_mut().set_layout_max_advance(f32::INFINITY);
        breaker
            .state_mut()
            .set_line_max_advance(first_width.max(0.01));
        while breaker.break_next().is_some() {
            breaker.state_mut().set_line_max_advance(width.max(0.01));
        }
        breaker.finish();
        layout
            .lines()
            .map(|line| {
                let mut shaped = retain_line(text, &line);
                shaped.underline = style.underline;
                shaped.strikethrough = style.strikethrough;
                shaped
            })
            .collect()
    }

    fn content_widths_uncached(
        &mut self,
        text: &str,
        style: &TextStyle,
        breaks: TextBreakStyle,
    ) -> (f32, f32) {
        self.refresh_page_fonts();
        if text.is_empty() || style.size <= 0.0 {
            return (0.0, 0.0);
        }
        let mut builder = self
            .layouts
            .ranged_builder(&mut self.fonts, text, 1.0, true);
        let family = font_family_source(&style.family);
        builder.push_default(StyleProperty::FontFamily(FontFamily::Source(family)));
        builder.push_default(StyleProperty::Locale(text_language(style)));
        builder.push_default(StyleProperty::FontSize(style.size.max(0.01)));
        builder.push_default(StyleProperty::FontWeight(FontWeight::new(
            style.weight.clamp(1.0, 1000.0),
        )));
        builder.push_default(StyleProperty::FontStyle(if style.italic {
            FontStyle::Italic
        } else {
            FontStyle::Normal
        }));
        builder.push_default(StyleProperty::LetterSpacing(style.letter_spacing));
        builder.push_default(StyleProperty::WordSpacing(style.word_spacing));
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
        let layout: Layout<()> = builder.build(text);
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
        self.page_font_epoch = epoch;
    }
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
    builder.push_default(StyleProperty::FontStyle(if style.italic {
        FontStyle::Italic
    } else {
        FontStyle::Normal
    }));
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

thread_local! {
    // LayoutContext is reusable scratch storage. A thread-local owner avoids a
    // global lock across the terminal owner, desktop event loop, and live-page
    // actor while still reusing font discovery and allocations on each thread.
    static TEXT: RefCell<TextSystem> = RefCell::new(TextSystem::new());
}

/// Shape one unbroken text piece at CSS-pixel scale.
pub fn shape(text: &str, style: &TextStyle) -> ShapedText {
    TEXT.with_borrow_mut(|system| system.shape(text, style))
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
        system.refresh_page_fonts();
        let mut builder = system
            .color_layouts
            .ranged_builder(&mut system.fonts, text, 1.0, true);
        configure_layout(
            &mut builder,
            style,
            Some(TextBreakStyle {
                wrap: width.is_some(),
                overflow_wrap: TextOverflowWrap::Anywhere,
                ..TextBreakStyle::default()
            }),
        );
        for (range, color) in colors {
            builder.push(StyleProperty::Brush(Some(color)), range);
        }
        let mut layout = builder.build(text);
        layout.break_all_lines(width.map(|w| w.max(0.01)));
        layout
            .lines()
            .take(if width.is_some() { usize::MAX } else { 1 })
            .map(|line| {
                let mut shaped = retain_line(text, &line);
                if width.is_none() {
                    // Match shape()/retain_first_line for unwrapped text.
                    shaped.text = text.to_string();
                }
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

/// CSS `ch` basis: advance measure of U+0030 ZERO in the element's font.
pub fn zero_advance(style: &TextStyle) -> f32 {
    shape("0", style).advance.max(style.size * 0.25)
}

/// Return the byte end of the first Unicode/CSS line that fits `width`.
/// Parley supplies UAX #14 opportunities, bidi-aware cluster boundaries, and
/// emergency wrapping. The caller remains responsible for CSS whitespace
/// processing and for composing differently styled inline boxes.
pub fn first_line_end(text: &str, style: &TextStyle, width: f32, breaks: TextBreakStyle) -> usize {
    TEXT.with_borrow_mut(|system| system.first_line_end(text, style, width, breaks))
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
    TEXT.with_borrow_mut(|system| system.wrapped_lines(text, style, first_width, width, breaks))
}

/// CSS min-/max-content bounds for a single styled text run.
pub fn content_widths(text: &str, style: &TextStyle, breaks: TextBreakStyle) -> (f32, f32) {
    TEXT.with_borrow_mut(|system| system.content_widths(text, style, breaks))
}

fn retain_first_line(text: &str, layout: &Layout<()>) -> ShapedText {
    let Some(line) = layout.lines().next() else {
        return ShapedText {
            text: text.to_string(),
            ..ShapedText::default()
        };
    };
    let mut shaped = retain_line(text, &line);
    shaped.text = text.to_string();
    shaped
}

fn retain_line<B: parley::Brush>(text: &str, line: &parley::Line<'_, B>) -> ShapedText {
    let range = line.text_range();
    let line_text = &text[range.clone()];
    let metrics = line.metrics();
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
        let synthesis = run.synthesis();
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
                    y: glyph.y - metrics.block_min_coord,
                    advance: glyph.advance,
                })
                .collect(),
            text_range: run.text_range().start - range.start..run.text_range().end - range.start,
            rtl: run.is_rtl(),
            synth_bold: synthesis.embolden(),
            synth_skew_degrees: synthesis.skew(),
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
        baseline: metrics.baseline - metrics.block_min_coord,
        underline: false,
        strikethrough: false,
        runs,
        clusters,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
