//! The terminal's character-cell font.
//!
//! A terminal renders text in exactly one font: its cell grid. Every
//! narrow grapheme advances one cell, a wide (East Asian Wide/Fullwidth,
//! emoji) grapheme two, and every line of text occupies one row, whatever
//! `font-family`, `font-size` or `line-height` the author asked for. When a
//! thread lays a page out for the terminal frontend it measures text with that
//! font. CSS Fonts 4 #font-matching-algorithm has the UA lay out with the font
//! it actually renders; proportional advances painted one glyph per cell
//! overflowed every box they were measured for. Canonical CSS-pixel geometry,
//! the CSSOM View geometry page script reads, and the painted cells now
//! agree.
//!
//! Line breaking keeps CSS Text 3 §5: UAX #14 soft wrap opportunities from the
//! same ICU4X segmenter the inline layout already consults, `word-break`
//! tailoring, and `overflow-wrap` emergency breaks at grapheme boundaries
//! (CSS Text 3 #overflow-wrap-property, whose `break-word` opportunities do
//! not count toward min-content sizes).
//!
//! CSS Inline 3 derives `line-height: normal` from the first available font;
//! here that font's ascent plus descent is one row. The terminal cannot place
//! a glyph at a fractional row, so author line heights are not honored: a
//! text line box is one row tall. This is the deviation the frontend makes
//! instead of quantizing proportional line pitches into irregular blank rows.
//!
//! Graphical frontends never enable this mode; their layout and shaping are
//! untouched. Canvas text keeps proportional shaping on every frontend
//! because it is rasterized, not drawn in cells.

use icu_segmenter::LineSegmenter;
use icu_segmenter::options::{LineBreakOptions, LineBreakWordOption};
use unicode_segmentation::UnicodeSegmentation as _;
use unicode_width::UnicodeWidthStr as _;

use super::{
    Cluster, ShapedText, TextBreakStyle, TextOverflowWrap, TextStyle, TextWordBreak,
    zero_size_shape,
};

/// The size of one terminal cell, in CSS pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CellMetrics {
    pub width: f32,
    pub height: f32,
}

impl CellMetrics {
    /// Cell metrics from a terminal's font size in pixels, or `None` for a
    /// degenerate size, which keeps proportional shaping.
    pub fn new(width: f32, height: f32) -> Option<Self> {
        (width.is_finite() && height.is_finite() && width > 0.0 && height > 0.0)
            .then_some(Self { width, height })
    }

    /// The cell font's ascent: the baseline sits this far below the top of
    /// its row, leaving a descender band below it.
    fn ascent(self) -> f32 {
        self.height * 0.8
    }
}

/// The display width of `text` in cells, as the terminal paints it.
pub(super) fn columns(text: &str) -> usize {
    text.width()
}

fn advance(text: &str, cells: CellMetrics) -> f32 {
    columns(text) as f32 * cells.width
}

/// Collapsible or preserved spaces at a line's end hang (CSS Text 3 §4.1.3)
/// and do not decide whether the line fits.
fn hanging_advance(text: &str, cells: CellMetrics) -> f32 {
    advance(text.trim_end(), cells)
}

pub(super) fn shape(text: &str, style: &TextStyle, cells: CellMetrics) -> ShapedText {
    if text.is_empty() || style.size <= 0.0 {
        return zero_size_shape(text, style);
    }
    let mut clusters = Vec::new();
    let mut x = 0.0;
    for (start, grapheme) in text.grapheme_indices(true) {
        let advance = advance(grapheme, cells);
        clusters.push(Cluster {
            text_range: start..start + grapheme.len(),
            x,
            advance,
            rtl: false,
        });
        x += advance;
    }
    let ascent = cells.ascent();
    ShapedText {
        text: text.to_string(),
        advance: x,
        ascent,
        descent: cells.height - ascent,
        leading: 0.0,
        line_height: cells.height,
        baseline: ascent,
        underline: style.underline,
        strikethrough: style.strikethrough,
        runs: Vec::new(),
        clusters,
    }
}

/// CSS `ch`: the advance of U+0030 in the cell font is one cell.
pub(super) fn zero_advance(style: &TextStyle, cells: CellMetrics) -> f32 {
    if style.size <= 0.0 { 0.0 } else { cells.width }
}

fn segmenter(word_break: TextWordBreak) -> icu_segmenter::LineSegmenterBorrowed<'static> {
    const fn options(word_option: LineBreakWordOption) -> LineBreakOptions<'static> {
        let mut options = LineBreakOptions::default();
        options.word_option = Some(word_option);
        options
    }
    match word_break {
        TextWordBreak::Normal => {
            const { LineSegmenter::new_for_non_complex_scripts(options(LineBreakWordOption::Normal)) }
        }
        TextWordBreak::BreakAll => {
            const { LineSegmenter::new_for_non_complex_scripts(options(LineBreakWordOption::BreakAll)) }
        }
        TextWordBreak::KeepAll => {
            const { LineSegmenter::new_for_non_complex_scripts(options(LineBreakWordOption::KeepAll)) }
        }
    }
}

/// UAX #14 soft wrap opportunities after the start of `text`, in order. The
/// end of the text is always the last.
fn opportunities(text: &str, word_break: TextWordBreak) -> impl Iterator<Item = usize> + '_ {
    segmenter(word_break)
        .segment_str(text)
        .filter(|&index| index > 0 && index <= text.len())
}

/// UAX #14 class BK/CR/LF/NL: the line must end after this character.
fn ends_with_mandatory_break(text: &str) -> bool {
    text.ends_with([
        '\n', '\r', '\u{0B}', '\u{0C}', '\u{85}', '\u{2028}', '\u{2029}',
    ])
}

/// The byte end of the first line of `text` that fits `width` CSS pixels:
/// the last soft wrap opportunity that fits, else an emergency break
/// (`overflow-wrap: anywhere | break-word`) after as many graphemes as fit
/// (at least one), else the first opportunity, overflowing the line.
pub(super) fn first_line_end(
    text: &str,
    style: &TextStyle,
    width: f32,
    breaks: TextBreakStyle,
    cells: CellMetrics,
) -> usize {
    if text.is_empty() || width <= 0.0 || style.size <= 0.0 {
        return 0;
    }
    if !breaks.wrap {
        return text.len();
    }
    let fits = |end: usize| hanging_advance(&text[..end], cells) <= width + 0.01;
    let mut first = None;
    let mut last_fitting = None;
    for index in opportunities(text, breaks.word_break) {
        first.get_or_insert(index);
        if !fits(index) {
            break;
        }
        last_fitting = Some(index);
        if ends_with_mandatory_break(&text[..index]) {
            break;
        }
    }
    if let Some(end) = last_fitting {
        return end;
    }
    if breaks.overflow_wrap != TextOverflowWrap::Normal {
        let mut end = 0;
        for (start, grapheme) in text.grapheme_indices(true) {
            let next = start + grapheme.len();
            if end > 0 && !fits(next) {
                break;
            }
            end = next;
        }
        return end;
    }
    first.unwrap_or(text.len())
}

/// Break a preserved-whitespace run into lines: `first_width` for the first
/// line and `width` for the rest.
pub(super) fn wrapped_lines(
    text: &str,
    style: &TextStyle,
    first_width: f32,
    width: f32,
    breaks: TextBreakStyle,
    cells: CellMetrics,
) -> Vec<ShapedText> {
    if text.is_empty() || style.size <= 0.0 {
        return Vec::new();
    }
    let mut lines = Vec::new();
    let mut start = 0;
    let mut line_width = first_width;
    while start < text.len() {
        let rest = &text[start..];
        let mut end = first_line_end(rest, style, line_width.max(0.01), breaks, cells);
        if end == 0 {
            // Every line holds at least one grapheme, so breaking terminates.
            end = rest.graphemes(true).next().map_or(rest.len(), str::len);
        }
        lines.push(shape(&rest[..end], style, cells));
        start += end;
        line_width = width;
    }
    lines
}

/// CSS Sizing 3 min-/max-content contributions of one text run (CSS Text 3
/// §5.5): the widest unbreakable segment and the widest forced line.
pub(super) fn content_widths(
    text: &str,
    style: &TextStyle,
    breaks: TextBreakStyle,
    cells: CellMetrics,
) -> (f32, f32) {
    if text.is_empty() || style.size <= 0.0 {
        return (0.0, 0.0);
    }
    let mut max_content = 0.0f32;
    let mut min_content = 0.0f32;
    let mut line_start = 0;
    let mut segment_start = 0;
    for index in opportunities(text, breaks.word_break) {
        min_content = min_content.max(hanging_advance(&text[segment_start..index], cells));
        segment_start = index;
        if ends_with_mandatory_break(&text[..index]) || index == text.len() {
            max_content = max_content.max(hanging_advance(&text[line_start..index], cells));
            line_start = index;
        }
    }
    if breaks.overflow_wrap == TextOverflowWrap::Anywhere {
        min_content = text
            .graphemes(true)
            .map(|grapheme| advance(grapheme, cells))
            .fold(0.0, f32::max);
    }
    (min_content, max_content)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CELLS: CellMetrics = CellMetrics {
        width: 8.0,
        height: 16.0,
    };

    fn style() -> TextStyle {
        TextStyle {
            size: 15.0,
            line_height: super::super::CssLineHeight::Number(1.6),
            ..TextStyle::default()
        }
    }

    fn wrapping(overflow_wrap: TextOverflowWrap) -> TextBreakStyle {
        TextBreakStyle {
            wrap: true,
            overflow_wrap,
            ..TextBreakStyle::default()
        }
    }

    #[test]
    fn graphemes_advance_by_their_terminal_cell_width() {
        // Narrow glyphs take one cell whatever their proportional width; East
        // Asian Wide characters and emoji take two; a combining mark joins its
        // base's cell (UAX #29 grapheme clusters, UAX #11 widths).
        let shaped = shape("iM水e\u{301}", &style(), CELLS);
        assert_eq!(shaped.advance, 5.0 * 8.0);
        let clusters: Vec<_> = shaped
            .clusters
            .iter()
            .map(|cluster| (cluster.text_range.clone(), cluster.x, cluster.advance))
            .collect();
        assert_eq!(
            clusters,
            [
                (0..1, 0.0, 8.0),
                (1..2, 8.0, 8.0),
                (2..5, 16.0, 16.0),
                (5..8, 32.0, 8.0)
            ]
        );
        assert!(shaped.runs.is_empty(), "no glyphs: the terminal draws text");
    }

    #[test]
    fn a_line_of_text_is_one_row_whatever_its_font_size_and_line_height() {
        for size in [9.0, 15.0, 48.0] {
            let shaped = shape(
                "Heading",
                &TextStyle {
                    size,
                    line_height: super::super::CssLineHeight::Length(80.0),
                    ..TextStyle::default()
                },
                CELLS,
            );
            assert_eq!(shaped.line_height, 16.0);
            assert_eq!(shaped.ascent + shaped.descent, 16.0);
            assert_eq!(shaped.baseline, shaped.ascent);
        }
        // A zero-size font still contributes nothing but its leading.
        let hidden = shape(
            " ",
            &TextStyle {
                size: 0.0,
                ..TextStyle::default()
            },
            CELLS,
        );
        assert_eq!((hidden.advance, hidden.line_height), (0.0, 0.0));
        assert_eq!(zero_advance(&style(), CELLS), 8.0, "CSS `ch` is one cell");
    }

    #[test]
    fn lines_break_at_the_last_fitting_uax14_opportunity() {
        let normal = wrapping(TextOverflowWrap::Normal);
        // 10 cells hold "one two " (the trailing space hangs, CSS Text 3
        // §4.1.3) but not "one two three".
        assert_eq!(
            first_line_end("one two three", &style(), 80.0, normal, CELLS),
            8
        );
        // A hyphen is an opportunity after itself.
        assert_eq!(
            first_line_end("well-known words", &style(), 48.0, normal, CELLS),
            5
        );
        // A mandatory break ends the line even when more would fit.
        assert_eq!(first_line_end("ab\ncd", &style(), 800.0, normal, CELLS), 3);
        // Without wrapping, the whole run is one line.
        assert_eq!(
            first_line_end(
                "one two three",
                &style(),
                8.0,
                TextBreakStyle::default(),
                CELLS
            ),
            13
        );
    }

    #[test]
    fn overflow_wrap_breaks_an_unbreakable_word_only_when_it_overflows() {
        // CSS Text 3 #overflow-wrap-property: `normal` lets the word overflow;
        // `anywhere`/`break-word` break it at the last grapheme that fits.
        let word = "Supercalifragilistic";
        let normal = first_line_end(
            word,
            &style(),
            40.0,
            wrapping(TextOverflowWrap::Normal),
            CELLS,
        );
        assert_eq!(normal, word.len());
        for wrap in [TextOverflowWrap::Anywhere, TextOverflowWrap::BreakWord] {
            assert_eq!(
                first_line_end(word, &style(), 40.0, wrapping(wrap), CELLS),
                5
            );
        }
        // Even a line narrower than one cell keeps one grapheme.
        assert_eq!(
            first_line_end(
                word,
                &style(),
                1.0,
                wrapping(TextOverflowWrap::Anywhere),
                CELLS
            ),
            1
        );
        let lines = wrapped_lines(
            "abcdefgh",
            &style(),
            24.0,
            32.0,
            wrapping(TextOverflowWrap::Anywhere),
            CELLS,
        );
        let texts: Vec<_> = lines.iter().map(|line| line.text.as_str()).collect();
        assert_eq!(texts, ["abc", "defg", "h"]);
    }

    #[test]
    fn min_content_counts_break_word_opportunities_only_for_anywhere() {
        // CSS Text 3 #overflow-wrap-property: soft wrap opportunities from
        // `break-word` do not count toward min-content; `anywhere`'s do.
        let text = "tiny enormousword\nend";
        let widths = |wrap| {
            content_widths(
                text,
                &style(),
                TextBreakStyle {
                    overflow_wrap: wrap,
                    ..TextBreakStyle::default()
                },
                CELLS,
            )
        };
        assert_eq!(widths(TextOverflowWrap::Normal), (12.0 * 8.0, 17.0 * 8.0));
        assert_eq!(
            widths(TextOverflowWrap::BreakWord),
            (12.0 * 8.0, 17.0 * 8.0)
        );
        assert_eq!(widths(TextOverflowWrap::Anywhere), (8.0, 17.0 * 8.0));
    }

    #[test]
    fn the_cell_font_is_scoped_to_its_thread_and_restored() {
        assert_eq!(super::super::cell_metrics(), None);
        let proportional = super::super::shape("iiii", &style()).advance;
        {
            let _cells = super::super::cell_metrics_scope(Some(CELLS));
            assert_eq!(super::super::shape("iiii", &style()).advance, 32.0);
            assert_eq!(super::super::zero_advance(&style()), 8.0);
            // Canvas text is rasterized, never drawn in cells.
            assert_ne!(super::super::shape_canvas("iiii", &style()).advance, 32.0);
        }
        assert_eq!(super::super::cell_metrics(), None);
        assert_eq!(super::super::shape("iiii", &style()).advance, proportional);
    }
}
