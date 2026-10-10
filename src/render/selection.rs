//! Selection API boundary points over the display list, and the painting of
//! a selection's highlight (CSS Pseudo 4 #highlight-painting).
//!
//! Layout attributes the text of every glyph run to the DOM Text data it
//! lays out ([`TextSourceSpan`], Selection API §2 with the DOM's boundary
//! points). Frontends anchor a selection to those points, never to
//! display-list indices, so the selection survives relayout, scrolling and
//! the commands its own highlight inserts. Generated content, form control
//! labels and text whose `user-select` is `none` (CSS UI 4
//! #content-selection) carry no sources: they can neither start nor show a
//! selection, and copying leaves them out.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::Arc;

use crate::core::{CssPoint, TextPoint};
use crate::text::ShapedText;

use super::{Affine2d, CssRect, DisplayCommand, PagePaint, PaintColor, PaintShape, Scene};

/// A byte range of a glyph run's text and the range of the DOM Text node's
/// data it lays out, in UTF-16 code units. Collapsed white space between
/// spans (a materialized U+0020 or a run's preserved spaces) belongs to no
/// span; a position there stands for the end of the span before it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TextSourceSpan {
    pub start: u32,
    pub end: u32,
    pub node: usize,
    pub offset: u32,
    pub offset_end: u32,
}

pub(crate) fn utf16_len(text: &str) -> u32 {
    text.chars().map(|c| c.len_utf16() as u32).sum()
}

impl TextSourceSpan {
    fn bytes(&self) -> Range<usize> {
        self.start as usize..self.end as usize
    }

    /// Whether the laid-out text is the data itself, character for
    /// character. A `text-transform` that changes the number of characters
    /// (CSS Text 3 #text-transform-property) maps only proportionally.
    fn exact(&self, text: &str) -> bool {
        text.get(self.bytes())
            .is_some_and(|part| utf16_len(part) == self.offset_end - self.offset)
    }

    /// The data offset laid out at `byte` of the run's text.
    pub fn offset_at(&self, text: &str, byte: usize) -> u32 {
        let byte = byte.clamp(self.start as usize, self.end as usize);
        if byte == self.start as usize {
            return self.offset;
        }
        if byte == self.end as usize {
            return self.offset_end;
        }
        if self.exact(text)
            && let Some(prefix) = text.get(self.start as usize..byte)
        {
            return self.offset + utf16_len(prefix);
        }
        let fraction = (byte - self.start as usize) as f64 / f64::from(self.end - self.start);
        self.offset + (f64::from(self.offset_end - self.offset) * fraction).round() as u32
    }

    /// The byte of the run's text that lays out data offset `offset`.
    pub fn byte_at(&self, text: &str, offset: u32) -> usize {
        let offset = offset.clamp(self.offset, self.offset_end);
        if offset == self.offset {
            return self.start as usize;
        }
        if offset == self.offset_end {
            return self.end as usize;
        }
        let Some(part) = text.get(self.bytes()) else {
            return self.start as usize;
        };
        if self.exact(text) {
            let mut units = self.offset;
            for (index, c) in part.char_indices() {
                if units >= offset {
                    return self.start as usize + index;
                }
                units += c.len_utf16() as u32;
            }
            return self.end as usize;
        }
        let fraction =
            f64::from(offset - self.offset) / f64::from((self.offset_end - self.offset).max(1));
        let mut byte = self.start as usize + (part.len() as f64 * fraction).round() as usize;
        while !text.is_char_boundary(byte) {
            byte -= 1;
        }
        byte
    }

    /// Whether this span lays out boundary point `point`.
    fn holds(&self, point: TextPoint) -> bool {
        self.node == point.node && self.offset <= point.offset && point.offset <= self.offset_end
    }
}

/// The spans of `text[range]`, rebased to that range's start: the sources
/// of a piece of a run (a bidi level run, a line's ellipsis prefix, a
/// highlighted slice).
pub fn slice_sources(
    text: &str,
    spans: &[TextSourceSpan],
    range: Range<usize>,
) -> Vec<TextSourceSpan> {
    spans
        .iter()
        .filter_map(|span| {
            let start = (span.start as usize).max(range.start);
            let end = (span.end as usize).min(range.end);
            (start < end || (start == end && span.start == span.end && range.contains(&start)))
                .then(|| TextSourceSpan {
                    start: (start - range.start) as u32,
                    end: (end - range.start) as u32,
                    node: span.node,
                    offset: span.offset_at(text, start),
                    offset_end: span.offset_at(text, end),
                })
        })
        .collect()
}

/// The boundary point that position `byte` of a run's text stands for.
pub fn point_in_run(text: &str, spans: &[TextSourceSpan], byte: usize) -> Option<TextPoint> {
    let first = spans.first()?;
    let Some(span) = spans.iter().rev().find(|span| span.start as usize <= byte) else {
        return Some(TextPoint {
            node: first.node,
            offset: first.offset,
        });
    };
    Some(TextPoint {
        node: span.node,
        offset: if byte <= span.end as usize {
            span.offset_at(text, byte)
        } else {
            span.offset_end
        },
    })
}

/// The position of a run's text that lays out `point`, if this run lays it
/// out. A point inside white space collapsed between two spans of its node
/// is at the gap's start.
pub fn byte_in_run(text: &str, spans: &[TextSourceSpan], point: TextPoint) -> Option<usize> {
    if let Some(span) = spans.iter().find(|span| span.holds(point)) {
        return Some(span.byte_at(text, point.offset));
    }
    spans.windows(2).find_map(|pair| {
        (pair[0].node == point.node
            && pair[1].node == point.node
            && pair[0].offset_end < point.offset
            && point.offset < pair[1].offset)
            .then_some(pair[0].end as usize)
    })
}

/// How a highlight paints (CSS Pseudo 4 #highlight-styling): the colors of
/// the run's originating element's `::selection`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum HighlightPaint {
    /// No author colors (#paired-defaults): the UA's `Highlight` background,
    /// made translucent where the scheme's is opaque, with the text keeping
    /// its own colors, as #highlight-styling permits without author colors.
    /// Which scheme's `Highlight` is the UA's processing too: the one that
    /// contrasts with the text (light text, as a dark scheme paints it,
    /// takes the dark scheme's), resolved when the run paints.
    #[default]
    Ua,
    /// Author colors, respected exactly. `color: None` is `currentColor`,
    /// which leaves the text's own color.
    Author {
        color: Option<PaintColor>,
        background: PaintColor,
    },
}

/// The UA highlight of the light color scheme: `Highlight` (#0078d7) at
/// the translucency it gets over text that keeps its colors.
pub const UA_LIGHT_HIGHLIGHT: PaintColor = PaintColor::Rgba(0, 120, 215, 92);
/// The dark color scheme's `Highlight`, already translucent.
pub const UA_DARK_HIGHLIGHT: PaintColor = PaintColor::Rgba(0, 221, 255, 78);

/// The UA highlight behind text of color `text`.
fn ua_highlight(text: PaintColor) -> PaintColor {
    match text {
        PaintColor::Rgba(r, g, b, _)
            if 0.2126 * f32::from(r) + 0.7152 * f32::from(g) + 0.0722 * f32::from(b) > 127.5 =>
        {
            UA_DARK_HIGHLIGHT
        }
        _ => UA_LIGHT_HIGHLIGHT,
    }
}

impl HighlightPaint {
    /// The background painted below selected text of color `text`.
    pub fn background(self, text: PaintColor) -> PaintColor {
        match self {
            Self::Ua => ua_highlight(text),
            Self::Author { background, .. } => background,
        }
    }

    /// The color selected text is redrawn in, if not its own.
    pub fn text(self) -> Option<PaintColor> {
        match self {
            Self::Ua => None,
            Self::Author { color, .. } => color,
        }
    }

    /// CSS Pseudo 4 #highlight-replaced: the semi-transparent wash over
    /// selected replaced content, of the background unless transparent,
    /// else of the color.
    pub fn wash(self) -> PaintColor {
        let base = match self {
            Self::Ua => UA_LIGHT_HIGHLIGHT,
            Self::Author { background, color } => {
                if alpha(background) > 0 {
                    background
                } else {
                    color.unwrap_or(UA_LIGHT_HIGHLIGHT)
                }
            }
        };
        match base {
            PaintColor::Rgba(r, g, b, a) => PaintColor::Rgba(r, g, b, a.clamp(64, 128)),
            other => other,
        }
    }
}

fn alpha(color: PaintColor) -> u8 {
    match color {
        PaintColor::Rgba(_, _, _, a) => a,
        _ => 255,
    }
}

/// How the line box a run begins started, for turning a selection into
/// text the way HTML's rendered text collection steps do: a soft wrap is
/// a space, a forced break (`<br>`, a preserved newline) a line break, and
/// a new block a line break too.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LineStart {
    /// The run continues its line (it does not begin it).
    #[default]
    Within,
    Soft,
    Forced,
    Block,
}

/// What layout knows of a glyph run for selection: where its text came
/// from, how its originating element's `::selection` paints, and how the
/// line it begins started.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RunSelection {
    pub sources: Option<Arc<[TextSourceSpan]>>,
    pub highlight: HighlightPaint,
    pub line_start: LineStart,
    /// The run's element has `user-select: none` (CSS UI 4): pressing on
    /// it neither starts a selection nor affects the one there is.
    pub unselectable: bool,
}

impl RunSelection {
    pub fn spans(&self) -> &[TextSourceSpan] {
        self.sources.as_deref().unwrap_or(&[])
    }

    pub fn selectable(&self) -> bool {
        !self.spans().is_empty()
    }
}

/// A page's Selection as the frontend paints it.
#[derive(Clone, Debug, PartialEq)]
pub enum PageSelection {
    /// The text from one boundary point to another in the order the page's
    /// runs paint. The frontend knows no document order without the page's
    /// DOM: static and line-model pages, and a live page's selection before
    /// the page has reported its own.
    Painted { anchor: TextPoint, focus: TextPoint },
    /// The page's own selection in document order (Selection API §2).
    Document(Arc<SelectionSegments>),
}

impl PageSelection {
    pub fn is_collapsed(&self) -> bool {
        match self {
            Self::Painted { anchor, focus } => anchor == focus,
            Self::Document(segments) => segments.is_empty(),
        }
    }
}

/// The selected parts of a document: each Text node's selected data, in
/// UTF-16 code units, and the replaced elements a selection contains.
/// Content whose `user-select` is `none` is already left out (CSS UI 4:
/// "must exclude this element from such a selection").
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SelectionSegments {
    pub text: HashMap<usize, Vec<(u32, u32)>>,
    pub replaced: HashSet<usize>,
}

impl SelectionSegments {
    pub fn is_empty(&self) -> bool {
        self.replaced.is_empty()
            && self
                .text
                .values()
                .all(|ranges| ranges.iter().all(|(start, end)| start >= end))
    }

    fn covers(&self, node: usize, from: u32, to: u32) -> bool {
        self.text.get(&node).is_some_and(|ranges| {
            ranges
                .iter()
                .any(|&(start, end)| start <= from && to <= end && start < end)
        })
    }

    /// The byte ranges of a run's text the selection contains. The white
    /// space collapsed between two spans is selected when the selection
    /// reaches into it from either side.
    pub fn selected_bytes(&self, text: &str, spans: &[TextSourceSpan]) -> Vec<Range<usize>> {
        let mut ranges: Vec<Range<usize>> = Vec::new();
        for (index, span) in spans.iter().enumerate() {
            if let Some(selected) = self.text.get(&span.node) {
                for &(start, end) in selected {
                    let (from, to) = (start.max(span.offset), end.min(span.offset_end));
                    if from < to {
                        ranges.push(span.byte_at(text, from)..span.byte_at(text, to));
                    }
                }
            }
            if let Some(next) = spans.get(index + 1)
                && next.start > span.end
            {
                let after = self.covers(span.node, span.offset_end, span.offset_end + 1);
                let before =
                    next.offset > 0 && self.covers(next.node, next.offset - 1, next.offset);
                if after || before {
                    ranges.push(span.end as usize..next.start as usize);
                }
            }
        }
        ranges.sort_by_key(|range| range.start);
        let mut merged: Vec<Range<usize>> = Vec::with_capacity(ranges.len());
        for range in ranges.into_iter().filter(|range| !range.is_empty()) {
            match merged.last_mut() {
                Some(last) if range.start <= last.end => last.end = last.end.max(range.end),
                _ => merged.push(range),
            }
        }
        merged
    }
}

/// A position among the selectable content of a page in paint order: the
/// ordinal of a selectable glyph run or replaced image, and a byte of a
/// run's text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct PaintedPosition {
    pub ordinal: usize,
    pub byte: usize,
}

/// A [`PageSelection::Painted`] resolved against the page's paint order.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PaintedRange {
    pub start: PaintedPosition,
    pub end: PaintedPosition,
}

impl PaintedRange {
    /// The bytes selected of the run at `ordinal` whose text is `len` long.
    pub fn run_bytes(&self, ordinal: usize, len: usize) -> Option<Range<usize>> {
        if ordinal < self.start.ordinal || ordinal > self.end.ordinal {
            return None;
        }
        let from = if ordinal == self.start.ordinal {
            self.start.byte.min(len)
        } else {
            0
        };
        let to = if ordinal == self.end.ordinal {
            self.end.byte.min(len)
        } else {
            len
        };
        (from < to).then_some(from..to)
    }

    /// Whether replaced content at `ordinal` lies wholly inside the range.
    pub fn contains_replaced(&self, ordinal: usize) -> bool {
        self.start.ordinal < ordinal && ordinal < self.end.ordinal
    }
}

enum CursorKind<'a> {
    Painted(PaintedRange),
    Document(&'a SelectionSegments),
}

/// The selection as it applies while composing a page: resolved once per
/// composition, then asked about each selectable run and replaced image in
/// paint order.
pub(crate) struct SelectionCursor<'a> {
    kind: CursorKind<'a>,
    ordinal: usize,
    highlight: HighlightPaint,
    /// The last run highlighted through its end, in the current coordinate
    /// space: its line's top and height and its right edge. The collapsed
    /// space between it and the next selected run on the line is part of
    /// the selection (#highlight-bounds: spacing between two selected
    /// characters may belong to the overlay).
    pub(super) line_tail: Option<(f32, f32, f32)>,
}

impl SelectionCursor<'_> {
    /// The selected bytes of the next selectable run in paint order.
    pub fn next_run(&mut self, text: &str, selection: &RunSelection) -> Vec<Range<usize>> {
        if !selection.selectable() {
            return Vec::new();
        }
        let ordinal = self.ordinal;
        self.ordinal += 1;
        let selected = match &self.kind {
            CursorKind::Painted(range) => {
                range.run_bytes(ordinal, text.len()).into_iter().collect()
            }
            CursorKind::Document(segments) => segments.selected_bytes(text, selection.spans()),
        };
        if !selected.is_empty() {
            self.highlight = selection.highlight;
        }
        selected
    }

    /// Whether the next replaced image in paint order is selected.
    pub fn next_replaced(&mut self, node: usize) -> bool {
        let ordinal = self.ordinal;
        self.ordinal += 1;
        match &self.kind {
            CursorKind::Painted(range) => range.contains_replaced(ordinal),
            CursorKind::Document(segments) => segments.replaced.contains(&node),
        }
    }

    /// The wash over selected replaced content, in the colors of the text
    /// highlighted last.
    pub fn wash(&self) -> PaintColor {
        self.highlight.wash()
    }
}

/// The page's paint lists in the order a scene composes them.
fn composition(page: &PagePaint) -> impl Iterator<Item = &DisplayCommand> {
    let fixed: &[DisplayCommand] = if page.fixed_interleaved {
        &[]
    } else {
        &page.fixed_primitives
    };
    page.fixed_under_primitives
        .iter()
        .chain(page.primitives.iter())
        .chain(fixed.iter())
        .chain(
            page.top_layer
                .iter()
                .flat_map(|entry| entry.primitives.iter()),
        )
}

/// Resolve `selection` for composing `page`; `None` paints nothing.
pub(crate) fn cursor<'a>(
    page: &PagePaint,
    selection: &'a PageSelection,
) -> Option<SelectionCursor<'a>> {
    let kind = match selection {
        PageSelection::Document(segments) => {
            if segments.is_empty() {
                return None;
            }
            CursorKind::Document(segments)
        }
        PageSelection::Painted { anchor, focus } => {
            if anchor == focus {
                return None;
            }
            CursorKind::Painted(painted_range(page, *anchor, *focus)?)
        }
    };
    Some(SelectionCursor {
        kind,
        ordinal: 0,
        highlight: HighlightPaint::default(),
        line_tail: None,
    })
}

/// Where two boundary points lie in the page's paint order.
fn painted_positions(
    page: &PagePaint,
    anchor: TextPoint,
    focus: TextPoint,
) -> Option<(PaintedPosition, PaintedPosition)> {
    let (mut at_anchor, mut at_focus) = (None, None);
    let mut ordinal = 0;
    for command in composition(page) {
        match command {
            DisplayCommand::GlyphRun {
                shaped, selection, ..
            } if selection.selectable() => {
                let spans = selection.spans();
                if at_anchor.is_none() {
                    at_anchor = byte_in_run(&shaped.text, spans, anchor)
                        .map(|byte| PaintedPosition { ordinal, byte });
                }
                if at_focus.is_none() {
                    at_focus = byte_in_run(&shaped.text, spans, focus)
                        .map(|byte| PaintedPosition { ordinal, byte });
                }
                ordinal += 1;
            }
            DisplayCommand::Image { node, .. } if page.replaced_images.contains(node) => {
                ordinal += 1;
            }
            _ => {}
        }
        if at_anchor.is_some() && at_focus.is_some() {
            break;
        }
    }
    Some((at_anchor?, at_focus?))
}

fn painted_range(page: &PagePaint, anchor: TextPoint, focus: TextPoint) -> Option<PaintedRange> {
    let (anchor, focus) = painted_positions(page, anchor, focus)?;
    let (start, end) = if anchor <= focus {
        (anchor, focus)
    } else {
        (focus, anchor)
    };
    Some(PaintedRange { start, end })
}

/// The order of two boundary points in the page's paint order, where the
/// page lays both out.
pub fn paint_order(page: &PagePaint, a: TextPoint, b: TextPoint) -> Option<std::cmp::Ordering> {
    let (a, b) = painted_positions(page, a, b)?;
    Some(a.cmp(&b))
}

/// The first and last boundary points of the page's selectable text in
/// paint order: what selecting everything selects without the page's DOM.
pub fn page_text_bounds(page: &PagePaint) -> Option<(TextPoint, TextPoint)> {
    let mut first = None;
    let mut last = None;
    for command in composition(page) {
        if let DisplayCommand::GlyphRun {
            shaped, selection, ..
        } = command
            && selection.selectable()
        {
            let spans = selection.spans();
            if first.is_none() {
                first = point_in_run(&shaped.text, spans, 0);
            }
            last = point_in_run(&shaped.text, spans, shaped.text.len()).or(last);
        }
    }
    Some((first?, last?))
}

/// The visual extents of the clusters whose text starts inside `selected`.
fn selected_intervals(shaped: &ShapedText, selected: &[Range<usize>]) -> Vec<(f32, f32)> {
    let mut extents: Vec<(f32, f32)> = shaped
        .clusters
        .iter()
        .filter(|cluster| {
            selected
                .iter()
                .any(|range| range.contains(&cluster.text_range.start))
        })
        .map(|cluster| (cluster.x, cluster.x + cluster.advance.max(0.0)))
        .collect();
    extents.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut merged: Vec<(f32, f32)> = Vec::with_capacity(extents.len());
    for (from, to) in extents {
        if let Some(last) = merged.last_mut()
            && from <= last.1 + 0.5
        {
            last.1 = last.1.max(to);
        } else {
            merged.push((from, to));
        }
    }
    merged
}

/// The bytes of a run's text painted by its clusters in `[from, to)`.
fn bytes_painted(shaped: &ShapedText, from: f32, to: f32) -> Option<Range<usize>> {
    let mut bytes: Option<Range<usize>> = None;
    for cluster in &shaped.clusters {
        if cluster.x >= from - 0.01 && cluster.x < to - 0.01 {
            bytes = Some(match bytes {
                Some(range) => {
                    range.start.min(cluster.text_range.start)..range.end.max(cluster.text_range.end)
                }
                None => cluster.text_range.clone(),
            });
        }
    }
    bytes
}

/// The byte of `shaped` nearest to `x` from its origin: a cluster
/// boundary, on the side of the cluster's midpoint `x` falls.
pub(crate) fn byte_at_x(shaped: &ShapedText, x: f32) -> usize {
    let cluster =
        shaped.clusters.iter().min_by(|left, right| {
            super::distance_to_cluster(x, left.x, left.advance)
                .total_cmp(&super::distance_to_cluster(x, right.x, right.advance))
        });
    cluster.map_or(0, |cluster| {
        let after = x >= cluster.x + cluster.advance / 2.0;
        match (cluster.rtl, after) {
            (false, false) | (true, true) => cluster.text_range.start,
            _ => cluster.text_range.end,
        }
    })
}

impl Scene {
    /// Push a glyph run with the selected bytes `selected` highlighted (CSS
    /// Pseudo 4 #highlight-painting): the highlight background below the
    /// selected text, over the line box's height; with author colors, the
    /// selected text redrawn in the highlight's color (#highlight-text),
    /// the rest in its own.
    pub(super) fn push_highlighted_run(
        &mut self,
        command: &DisplayCommand,
        selected: &[Range<usize>],
        line_tail: &mut Option<(f32, f32, f32)>,
    ) {
        let DisplayCommand::GlyphRun {
            origin,
            shaped,
            color,
            decoration,
            shadows,
            clip,
            node,
            link,
            selection,
        } = command
        else {
            self.primitives.push(command.clone());
            return;
        };
        let intervals = selected_intervals(shaped, selected);
        let tail = line_tail.take();
        if intervals.is_empty() {
            self.primitives.push(command.clone());
            return;
        }
        let background = selection.highlight.background(*color);
        let (run_start, run_end) = shaped.clusters.iter().fold(
            (f32::INFINITY, f32::NEG_INFINITY),
            |(start, end), cluster| (start.min(cluster.x), end.max(cluster.x + cluster.advance)),
        );
        let first = intervals[0];
        if let Some((top, height, right)) = tail
            && first.0 <= run_start + 0.5
            && (top - origin.y).abs() < 0.5
            && (height - shaped.line_height).abs() < 0.5
            && origin.x + first.0 > right
            && origin.x + first.0 - right < shaped.line_height * 1.5
        {
            let gap = CssRect::new(
                right,
                origin.y,
                origin.x + first.0 - right,
                shaped.line_height,
            );
            if let Some(gap) = clip.map_or(Some(gap), |clip| super::intersect_css_rect(gap, clip)) {
                self.primitives.push(DisplayCommand::FillRect {
                    rect: gap,
                    color: background,
                });
            }
        }
        if let Some(&(_, to)) = intervals.last()
            && to >= run_end - 0.5
        {
            *line_tail = Some((origin.y, shaped.line_height, origin.x + to));
        }
        for &(from, to) in &intervals {
            let rect = CssRect::new(origin.x + from, origin.y, to - from, shaped.line_height);
            if let Some(rect) =
                clip.map_or(Some(rect), |clip| super::intersect_css_rect(rect, clip))
            {
                self.primitives.push(DisplayCommand::FillRect {
                    rect,
                    color: background,
                });
            }
        }
        let Some(highlight_color) = selection.highlight.text().filter(|text| text != color) else {
            self.primitives.push(command.clone());
            return;
        };
        let slice = |from: f32, to: f32, highlighted: bool| {
            let part = shaped.visual_range(from, to);
            let sources = bytes_painted(shaped, from, to)
                .map(|bytes| slice_sources(&shaped.text, selection.spans(), bytes))
                .filter(|spans| !spans.is_empty())
                .map(Arc::from);
            let color = if highlighted { highlight_color } else { *color };
            DisplayCommand::GlyphRun {
                origin: CssPoint::new(origin.x + from, origin.y),
                shaped: part,
                color,
                decoration: super::TextDecorationPaint {
                    color: if highlighted {
                        highlight_color
                    } else {
                        decoration.color
                    },
                    ..*decoration
                },
                shadows: shadows.clone(),
                clip: *clip,
                node: *node,
                link: link.clone(),
                selection: RunSelection {
                    sources,
                    highlight: selection.highlight,
                    line_start: if from <= 0.0 {
                        selection.line_start
                    } else {
                        LineStart::Within
                    },
                    unselectable: selection.unselectable,
                },
            }
        };
        let mut x = shaped
            .clusters
            .iter()
            .map(|cluster| cluster.x)
            .fold(0.0f32, f32::min);
        for &(from, to) in &intervals {
            if from > x + 0.01 {
                self.primitives.push(slice(x, from, false));
            }
            self.primitives.push(slice(from, to, true));
            x = to;
        }
        let end = shaped
            .clusters
            .iter()
            .map(|cluster| cluster.x + cluster.advance)
            .fold(shaped.advance, f32::max);
        if end > x + 0.01 {
            self.primitives.push(slice(x, end + 1.0, false));
        }
    }

    /// Visit every selectable glyph run of the scene in paint order with
    /// its transform to window coordinates and whether a window point is
    /// inside the clips that apply to it.
    fn visit_selectable_runs(
        &self,
        mut visit: impl FnMut(usize, &DisplayCommand, Affine2d, &dyn Fn(CssPoint) -> bool),
    ) {
        let mut transform = Affine2d::IDENTITY;
        let mut transforms = Vec::new();
        let mut clips: Vec<(PaintShape, Option<Affine2d>)> = Vec::new();
        for (index, primitive) in self.primitives.iter().enumerate() {
            match primitive {
                DisplayCommand::PushTransform(next) => {
                    transforms.push(transform);
                    transform = transform.then(*next);
                }
                DisplayCommand::PopTransform => {
                    transform = transforms.pop().unwrap_or(Affine2d::IDENTITY)
                }
                DisplayCommand::PushClip(shape) => clips.push((shape.clone(), transform.inverse())),
                DisplayCommand::PopClip => {
                    let _ = clips.pop();
                }
                DisplayCommand::GlyphRun {
                    selection, clip, ..
                } if selection.selectable() => {
                    let inverse = transform.inverse();
                    let visible = |point: CssPoint| {
                        clips.iter().all(|(shape, inverse)| {
                            inverse.is_some_and(|inverse| {
                                super::shape_contains(shape, inverse.map_point(point))
                            })
                        }) && clip.is_none_or(|clip| {
                            inverse.is_some_and(|inverse| clip.contains(inverse.map_point(point)))
                        })
                    };
                    visit(index, primitive, transform, &visible);
                }
                _ => {}
            }
        }
    }

    /// The selectable text under a window point: the topmost run whose
    /// glyph box contains it, and the byte nearest to it.
    fn text_hit_at(&self, point: CssPoint) -> Option<(usize, usize)> {
        let mut hit = None;
        self.visit_selectable_runs(|index, command, transform, visible| {
            let DisplayCommand::GlyphRun { origin, shaped, .. } = command else {
                return;
            };
            let Some(local) = transform.inverse().map(|inverse| inverse.map_point(point)) else {
                return;
            };
            let bounds = CssRect::new(
                origin.x,
                origin.y,
                shaped.advance.max(1.0),
                shaped.line_height.max(1.0),
            );
            if bounds.contains(local) && visible(point) {
                hit = Some((index, byte_at_x(shaped, local.x - origin.x)));
            }
        });
        hit
    }

    fn run_point(&self, index: usize, byte: usize) -> Option<TextPoint> {
        let DisplayCommand::GlyphRun {
            shaped, selection, ..
        } = self.primitives.get(index)?
        else {
            return None;
        };
        point_in_run(&shaped.text, selection.spans(), byte)
    }

    /// Whether selectable text is under a window point (the text cursor's
    /// condition).
    pub fn selectable_text_at(&self, point: CssPoint) -> bool {
        self.text_hit_at(point).is_some()
    }

    /// Whether the topmost text under a window point is `user-select:
    /// none` text.
    pub fn unselectable_text_at(&self, point: CssPoint) -> bool {
        let mut unselectable = false;
        let mut transform = Affine2d::IDENTITY;
        let mut transforms = Vec::new();
        for primitive in &self.primitives {
            match primitive {
                DisplayCommand::PushTransform(next) => {
                    transforms.push(transform);
                    transform = transform.then(*next);
                }
                DisplayCommand::PopTransform => {
                    transform = transforms.pop().unwrap_or(Affine2d::IDENTITY)
                }
                DisplayCommand::GlyphRun {
                    origin,
                    shaped,
                    selection,
                    ..
                } => {
                    let Some(local) = transform.inverse().map(|inverse| inverse.map_point(point))
                    else {
                        continue;
                    };
                    if CssRect::new(
                        origin.x,
                        origin.y,
                        shaped.advance.max(1.0),
                        shaped.line_height.max(1.0),
                    )
                    .contains(local)
                    {
                        unselectable = selection.unselectable;
                    }
                }
                _ => {}
            }
        }
        unselectable
    }

    /// The boundary point of the selectable text under a window point.
    pub fn text_point_at(&self, point: CssPoint) -> Option<TextPoint> {
        let (index, byte) = self.text_hit_at(point)?;
        self.run_point(index, byte)
    }

    /// The boundary point nearest to a window point anywhere among the
    /// visible selectable text, as a drag past the end of a line or into a
    /// margin extends a selection: the text on the point's line first,
    /// else the nearest line.
    pub fn nearest_text_point(&self, point: CssPoint) -> Option<TextPoint> {
        if let Some(found) = self.text_point_at(point) {
            return Some(found);
        }
        let mut best: Option<((f32, f32), usize, usize)> = None;
        self.visit_selectable_runs(|index, command, transform, visible| {
            let DisplayCommand::GlyphRun { origin, shaped, .. } = command else {
                return;
            };
            let bounds = super::transformed_bounds(
                CssRect::new(
                    origin.x,
                    origin.y,
                    shaped.advance.max(1.0),
                    shaped.line_height.max(1.0),
                ),
                transform,
            );
            let probe = CssPoint::new(
                point.x.clamp(bounds.x, bounds.x + bounds.width),
                point.y.clamp(bounds.y, bounds.y + bounds.height),
            );
            if !visible(probe) {
                return;
            }
            let score = ((point.y - probe.y).abs(), (point.x - probe.x).abs());
            if best.is_none_or(|(best, ..)| {
                score.0 < best.0 || (score.0 == best.0 && score.1 <= best.1)
            }) {
                let Some(local) = transform.inverse().map(|inverse| inverse.map_point(probe))
                else {
                    return;
                };
                best = Some((score, index, byte_at_x(shaped, local.x - origin.x)));
            }
        });
        let (_, index, byte) = best?;
        self.run_point(index, byte)
    }

    /// The word (UAX #29 word boundaries) under a window point, as a
    /// double click selects it: its two boundary points in text order.
    pub fn word_at(&self, point: CssPoint) -> Option<(TextPoint, TextPoint)> {
        use unicode_segmentation::UnicodeSegmentation as _;
        let (index, byte) = self.text_hit_at(point)?;
        let DisplayCommand::GlyphRun { shaped, .. } = self.primitives.get(index)? else {
            return None;
        };
        let text = shaped.text.as_str();
        let (start, word) = text
            .split_word_bound_indices()
            .find(|(start, word)| byte < start + word.len())
            .or_else(|| text.split_word_bound_indices().next_back())?;
        Some((
            self.run_point(index, start)?,
            self.run_point(index, start + word.len())?,
        ))
    }

    /// The paragraph under a window point, as a triple click selects it:
    /// the runs from the line start that follows a block start or forced
    /// break to the next one.
    pub fn paragraph_at(&self, point: CssPoint) -> Option<(TextPoint, TextPoint)> {
        let (hit, _) = self.text_hit_at(point)?;
        let mut runs = Vec::new();
        self.visit_selectable_runs(|index, command, _, _| {
            if let DisplayCommand::GlyphRun {
                shaped, selection, ..
            } = command
            {
                runs.push((index, shaped.text.len(), selection.line_start));
            }
        });
        let at = runs.iter().position(|(index, ..)| *index == hit)?;
        let hard = |start: LineStart| matches!(start, LineStart::Block | LineStart::Forced);
        let mut first = at;
        while first > 0 && !hard(runs[first].2) {
            first -= 1;
        }
        let mut last = at;
        while last + 1 < runs.len() && !hard(runs[last + 1].2) {
            last += 1;
        }
        Some((
            self.run_point(runs[first].0, 0)?,
            self.run_point(runs[last].0, runs[last].1)?,
        ))
    }
}

/// The text a selection holds, as copying puts it on the clipboard when
/// the frontend has no DOM to run HTML's rendered text collection steps
/// over: the selected runs in paint order, a soft-wrapped line joined to
/// the line before by a space, a forced break or a new block starting a
/// new line, and a blank line where a block's margins leave one.
pub fn selected_page_text(page: &PagePaint, selection: &PageSelection) -> String {
    let Some(mut cursor) = cursor(page, selection) else {
        return String::new();
    };
    let mut out = String::new();
    // The previous piece: its bottom edge, line height, right edge, and
    // its last source.
    let mut previous: Option<(f32, f32, f32, f32, Option<TextSourceSpan>)> = None;
    for command in composition(page) {
        match command {
            DisplayCommand::GlyphRun {
                origin,
                shaped,
                selection: run,
                ..
            } => {
                for range in cursor.next_run(&shaped.text, run) {
                    let Some(text) = shaped.text.get(range.clone()) else {
                        continue;
                    };
                    let line_height = shaped.line_height.max(1.0);
                    let x = origin.x
                        + shaped
                            .clusters
                            .iter()
                            .filter(|cluster| cluster.text_range.start >= range.start)
                            .map(|cluster| cluster.x)
                            .fold(f32::INFINITY, f32::min)
                            .min(shaped.advance);
                    let first_source = run
                        .spans()
                        .iter()
                        .find(|span| span.end as usize > range.start)
                        .copied();
                    if let Some((top, bottom, height, right, last_source)) = previous {
                        let starts_run = range.start == 0;
                        let same_line = (origin.y - top).abs() < height.min(line_height) * 0.5;
                        let separator = if same_line {
                            let spaced = match (last_source, first_source) {
                                (Some(last), Some(first)) if last.node == first.node => {
                                    first.offset > last.offset_end
                                }
                                _ => x - right > line_height * 0.15,
                            };
                            if spaced { " " } else { "" }
                        } else {
                            match if starts_run {
                                run.line_start
                            } else {
                                LineStart::Within
                            } {
                                LineStart::Soft => " ",
                                LineStart::Forced | LineStart::Block
                                    if origin.y - bottom > line_height * 0.5 =>
                                {
                                    "\n\n"
                                }
                                _ => "\n",
                            }
                        };
                        let joined = separator == " "
                            && (out.ends_with(char::is_whitespace)
                                || text.starts_with(char::is_whitespace));
                        if !joined {
                            out.push_str(separator);
                        }
                    }
                    out.push_str(text);
                    let right = origin.x
                        + shaped
                            .clusters
                            .iter()
                            .filter(|cluster| cluster.text_range.start < range.end)
                            .map(|cluster| cluster.x + cluster.advance)
                            .fold(0.0f32, f32::max);
                    let last_source = run
                        .spans()
                        .iter()
                        .rev()
                        .find(|span| (span.start as usize) < range.end)
                        .copied();
                    previous = Some((
                        origin.y,
                        origin.y + line_height,
                        line_height,
                        right,
                        last_source,
                    ));
                }
            }
            DisplayCommand::Image { node, .. } if page.replaced_images.contains(node) => {
                cursor.next_replaced(*node);
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(start: u32, end: u32, node: usize, offset: u32, offset_end: u32) -> TextSourceSpan {
        TextSourceSpan {
            start,
            end,
            node,
            offset,
            offset_end,
        }
    }

    #[test]
    fn collapsed_white_space_maps_to_the_end_of_the_word_before_it() {
        // "  Hello   world" laid out as "Hello world".
        let text = "Hello world";
        let spans = [span(0, 5, 7, 2, 7), span(6, 11, 7, 10, 15)];
        let point = |byte| point_in_run(text, &spans, byte).unwrap();
        assert_eq!(point(0), TextPoint { node: 7, offset: 2 });
        assert_eq!(point(3), TextPoint { node: 7, offset: 5 });
        assert_eq!(point(5), TextPoint { node: 7, offset: 7 });
        assert_eq!(
            point(6),
            TextPoint {
                node: 7,
                offset: 10
            }
        );
        assert_eq!(
            point(11),
            TextPoint {
                node: 7,
                offset: 15
            }
        );
        let byte = |offset| byte_in_run(text, &spans, TextPoint { node: 7, offset });
        assert_eq!(byte(4), Some(2));
        assert_eq!(byte(8), Some(5), "inside the collapsed run of spaces");
        assert_eq!(byte(12), Some(8));
        assert_eq!(byte(0), None, "leading white space is not laid out here");
        assert_eq!(
            byte_in_run(text, &spans, TextPoint { node: 8, offset: 1 }),
            None
        );
    }

    #[test]
    fn utf16_offsets_count_surrogate_pairs() {
        let text = "a😀b";
        let spans = [span(0, text.len() as u32, 3, 0, 4)];
        assert_eq!(point_in_run(text, &spans, 1).unwrap().offset, 1);
        assert_eq!(point_in_run(text, &spans, 5).unwrap().offset, 3);
        assert_eq!(
            byte_in_run(text, &spans, TextPoint { node: 3, offset: 3 }),
            Some(5)
        );
    }

    #[test]
    fn a_transform_that_changes_length_maps_proportionally() {
        // `text-transform: uppercase` of "straße" lays out "STRASSE".
        let text = "STRASSE";
        let spans = [span(0, 7, 1, 0, 6)];
        assert_eq!(point_in_run(text, &spans, 7).unwrap().offset, 6);
        assert_eq!(point_in_run(text, &spans, 0).unwrap().offset, 0);
        let middle = point_in_run(text, &spans, 3).unwrap().offset;
        assert!((2..=3).contains(&middle));
    }

    #[test]
    fn slicing_rebases_and_maps_offsets() {
        let text = "Hello world";
        let spans = [span(0, 5, 7, 2, 7), span(6, 11, 7, 10, 15)];
        let sliced = slice_sources(text, &spans, 3..9);
        assert_eq!(sliced, vec![span(0, 2, 7, 5, 7), span(3, 6, 7, 10, 13)]);
    }

    #[test]
    fn document_segments_select_bytes_and_the_gaps_they_reach_into() {
        let text = "Hello world";
        let spans = [span(0, 5, 7, 2, 7), span(6, 11, 7, 10, 15)];
        let mut segments = SelectionSegments::default();
        segments.text.insert(7, vec![(4, 12)]);
        assert_eq!(segments.selected_bytes(text, &spans), vec![2..8]);
        segments.text.insert(7, vec![(4, 7)]);
        assert_eq!(
            segments.selected_bytes(text, &spans),
            vec![2..5],
            "a selection ending at a word's end does not take the space after it"
        );
        segments.text.insert(7, vec![(7, 9)]);
        assert_eq!(segments.selected_bytes(text, &spans), vec![5..6]);
    }

    fn lay_out(html: &str, width: f32) -> (crate::dom::Dom, PagePaint) {
        let dom = crate::dom::Dom::parse_document(html);
        let layout = crate::layout2::lay_out_graphical(
            &dom,
            &url::Url::parse("https://example.test/").unwrap(),
            crate::layout2::Viewport::new(width, 400.0),
            &[],
            &Default::default(),
            &Default::default(),
        );
        (dom, layout.paint)
    }

    fn text_of(dom: &crate::dom::Dom, id: &str, index: usize) -> usize {
        let element = dom.get_by_id(id).unwrap();
        dom.children(element)
            .into_iter()
            .filter(|&child| dom.tag_name(child).is_none())
            .nth(index)
            .unwrap()
    }

    fn compose(page: &PagePaint, selection: Option<&PageSelection>) -> Scene {
        let physical = crate::core::PhysicalSize::new(640, 400);
        let mut scene = Scene {
            viewport: crate::core::ViewportMetrics::from_physical(physical, Default::default()),
            primitives: Vec::new(),
            controls: Vec::new(),
            content_viewport: CssRect::new(0.0, 0.0, 640.0, 400.0),
            image_store: Default::default(),
            canvas_images: Default::default(),
            page_scroll_containers: Vec::new(),
            page_size: Default::default(),
        };
        scene.append_page_with_selection(
            page,
            CssPoint::default(),
            0.0,
            &HashMap::new(),
            selection,
        );
        scene
    }

    fn point(node: usize, offset: u32) -> TextPoint {
        TextPoint { node, offset }
    }

    /// Glyph runs as (index, text, color) and highlight backgrounds as
    /// (index, rect, color), in scene order.
    fn runs(scene: &Scene) -> Vec<(usize, String, PaintColor)> {
        scene
            .primitives
            .iter()
            .enumerate()
            .filter_map(|(index, command)| match command {
                DisplayCommand::GlyphRun { shaped, color, .. } => {
                    Some((index, shaped.text.clone(), *color))
                }
                _ => None,
            })
            .collect()
    }

    fn fills(scene: &Scene, color: PaintColor) -> Vec<(usize, CssRect)> {
        scene
            .primitives
            .iter()
            .enumerate()
            .filter_map(|(index, command)| match command {
                DisplayCommand::FillRect { rect, color: fill } if *fill == color => {
                    Some((index, *rect))
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_highlight_paints_below_its_text_in_paint_order() {
        // CSS Pseudo 4 #highlight-backgrounds: below the selected text, so
        // content painted after it (a positioned box) covers the highlight.
        let (dom, page) = lay_out(
            "<p id=p style='margin:0;font:16px/20px monospace'>Hello world</p>\
             <div style='position:absolute;top:0;left:0;width:20px;height:20px;\
             background:rgb(1,2,3)'></div>",
            640.0,
        );
        let text = text_of(&dom, "p", 0);
        for selection in [
            PageSelection::Painted {
                anchor: point(text, 0),
                focus: point(text, 5),
            },
            PageSelection::Document(Arc::new(SelectionSegments {
                text: HashMap::from([(text, vec![(0, 5)])]),
                ..Default::default()
            })),
        ] {
            let scene = compose(&page, Some(&selection));
            let highlight = fills(&scene, UA_LIGHT_HIGHLIGHT);
            assert_eq!(highlight.len(), 1, "{selection:?}");
            let (at, rect) = highlight[0];
            let (run, laid, _) = runs(&scene)
                .into_iter()
                .find(|(_, text, _)| text.starts_with("Hello"))
                .unwrap();
            assert_eq!(laid, "Hello world");
            assert_eq!(at + 1, run, "the background directly below its run");
            let hello = crate::text::shape(
                "Hello",
                &crate::text::TextStyle {
                    family: "monospace".into(),
                    size: 16.0,
                    ..Default::default()
                },
            );
            assert!((rect.width - hello.advance).abs() < 0.5, "{rect:?}");
            assert_eq!(rect.height, 20.0);
            let cover = fills(&scene, PaintColor::Rgba(1, 2, 3, 255));
            assert!(cover.iter().all(|(index, _)| *index > at));
        }
        assert!(fills(&compose(&page, None), UA_LIGHT_HIGHLIGHT).is_empty());
    }

    #[test]
    fn a_highlight_spans_the_space_between_inline_elements() {
        // #highlight-bounds: the collapsed space laid out as a gap between
        // two selected runs is highlighted with them, without a seam.
        let (dom, page) = lay_out(
            "<p id=p style='margin:0'>Hello <b id=b>world</b> again</p>",
            640.0,
        );
        let (hello, again) = (text_of(&dom, "p", 0), text_of(&dom, "p", 1));
        let scene = compose(
            &page,
            Some(&PageSelection::Painted {
                anchor: point(hello, 0),
                focus: point(again, 6),
            }),
        );
        let mut rects: Vec<CssRect> = fills(&scene, UA_LIGHT_HIGHLIGHT)
            .into_iter()
            .map(|(_, rect)| rect)
            .collect();
        rects.sort_by(|a, b| a.x.total_cmp(&b.x));
        assert!(rects.len() >= 3, "{rects:?}");
        for pair in rects.windows(2) {
            assert!(
                (pair[1].x - (pair[0].x + pair[0].width)).abs() < 0.6,
                "a seam between {:?} and {:?}",
                pair[0],
                pair[1]
            );
        }
    }

    #[test]
    fn author_highlight_colors_redraw_only_the_selected_text() {
        // #highlight-text: with author colors the selected text is redrawn
        // in the highlight's color; the rest keeps its own, and every piece
        // still names its DOM text for hit testing.
        let (dom, page) = lay_out(
            "<style>::selection{color:rgb(255,255,255);background-color:rgb(0,0,128)}</style>\
             <p id=p style='margin:0;color:rgb(10,20,30)'>Hello world</p>",
            640.0,
        );
        let text = text_of(&dom, "p", 0);
        let selection = PageSelection::Painted {
            anchor: point(text, 0),
            focus: point(text, 5),
        };
        let scene = compose(&page, Some(&selection));
        assert_eq!(fills(&scene, PaintColor::Rgba(0, 0, 128, 255)).len(), 1);
        let pieces: Vec<_> = runs(&scene)
            .into_iter()
            .map(|(_, text, color)| (text, color))
            .collect();
        assert_eq!(
            pieces,
            vec![
                ("Hello".to_string(), PaintColor::Rgba(255, 255, 255, 255)),
                (" world".to_string(), PaintColor::Rgba(10, 20, 30, 255)),
            ]
        );
        let world = scene
            .primitives
            .iter()
            .find_map(|command| match command {
                DisplayCommand::GlyphRun {
                    shaped, selection, ..
                } if shaped.text == " world" => Some(selection.spans().to_vec()),
                _ => None,
            })
            .unwrap();
        assert_eq!(world[0].offset, 6);
    }

    #[test]
    fn highlight_inheritance_and_paired_defaults_resolve_per_element() {
        // CSS Pseudo 4 #highlight-cascade and #paired-defaults.
        let (_, page) = lay_out(
            "<style>p::selection{background-color:rgb(0,128,0)} \
             em::selection{color:rgb(255,0,0)}</style>\
             <p>plain <em>emph</em></p><div>other</div>",
            640.0,
        );
        let highlight = |text: &str| {
            page.primitives
                .iter()
                .find_map(|command| match command {
                    DisplayCommand::GlyphRun {
                        shaped, selection, ..
                    } if shaped.text.trim() == text => Some(selection.highlight),
                    _ => None,
                })
                .unwrap()
        };
        assert_eq!(
            highlight("plain"),
            HighlightPaint::Author {
                color: None,
                background: PaintColor::Rgba(0, 128, 0, 255),
            }
        );
        assert_eq!(
            highlight("emph"),
            HighlightPaint::Author {
                color: Some(PaintColor::Rgba(255, 0, 0, 255)),
                background: PaintColor::Rgba(0, 128, 0, 255),
            },
            "the background inherits from the parent's highlight"
        );
        assert_eq!(highlight("other"), HighlightPaint::Ua);
    }

    #[test]
    fn scene_queries_find_points_words_and_paragraphs() {
        let (dom, page) = lay_out(
            "<body style='margin:0'><p id=p style='margin:0;font:16px/20px monospace;width:200px'>\
             alpha beta gamma delta epsilon zeta</p>\
             <p id=q style='margin:20px 0 0;font:16px/20px monospace'>next</p></body>",
            640.0,
        );
        let (text, next) = (text_of(&dom, "p", 0), text_of(&dom, "q", 0));
        let scene = compose(&page, None);
        let cell = crate::text::shape(
            "a",
            &crate::text::TextStyle {
                family: "monospace".into(),
                size: 16.0,
                ..Default::default()
            },
        )
        .advance;
        // Inside "beta" (offsets 6..10), nearer the start of its second cell.
        let inside = CssPoint::new(cell * 7.2, 10.0);
        assert_eq!(scene.text_point_at(inside), Some(point(text, 7)));
        assert!(scene.selectable_text_at(inside));
        assert_eq!(
            scene.word_at(inside),
            Some((point(text, 6), point(text, 10)))
        );
        // Right of the first line ends it; below everything is the end.
        let first_line = scene
            .nearest_text_point(CssPoint::new(600.0, 10.0))
            .unwrap();
        assert_eq!(first_line.node, text);
        assert!(first_line.offset > 6 && first_line.offset < 35);
        assert_eq!(
            scene.nearest_text_point(CssPoint::new(2.0, 390.0)),
            Some(point(next, 0)),
            "the nearest line, not the nearest character"
        );
        let paragraph = scene.paragraph_at(inside).unwrap();
        assert_eq!(paragraph, (point(text, 0), point(text, 35)));
    }

    #[test]
    fn copied_text_joins_soft_wraps_and_breaks_lines_and_blocks() {
        let (dom, page) = lay_out(
            "<p id=p style='margin:0 0 16px;font:16px/20px monospace;width:100px'>aaaa bbbb \
             cccc<br>dddd</p><ul style='margin:0'><li>one</li><li id=two>two</li></ul>",
            640.0,
        );
        let (first, last) = page_text_bounds(&page).unwrap();
        assert_eq!(first, point(text_of(&dom, "p", 0), 0));
        assert_eq!(last, point(text_of(&dom, "two", 0), 3));
        let text = selected_page_text(
            &page,
            &PageSelection::Painted {
                anchor: first,
                focus: last,
            },
        );
        assert_eq!(text, "aaaa bbbb cccc\ndddd\n\none\ntwo");
    }

    #[test]
    fn selection_changes_damage_only_what_they_highlight() {
        let (dom, page) = lay_out(
            "<p id=p style='margin:0'>Hello world</p><p style='margin:100px 0'>far below</p>",
            640.0,
        );
        let text = text_of(&dom, "p", 0);
        let before = compose(&page, None);
        let after = compose(
            &page,
            Some(&PageSelection::Painted {
                anchor: point(text, 0),
                focus: point(text, 5),
            }),
        );
        match super::super::scene_damage(&before, &after) {
            super::super::SceneDamage::Partial(rect) => {
                assert!(rect.y < 30.0 && rect.height < 60.0, "{rect:?}")
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn selected_replaced_content_is_washed() {
        // #highlight-replaced: a semi-transparent wash over a selected image.
        let handle = super::super::ImageHandle::for_source("https://example.test/a.png");
        let image = DisplayCommand::Image {
            rect: CssRect::new(0.0, 0.0, 40.0, 30.0),
            handle,
            source_rect: None,
            fit: super::super::ImageFit::Fill,
            sampling: super::super::ImageSampling::Smooth,
            clip: None,
            node: 9,
            link: None,
        };
        let page = PagePaint {
            primitives: vec![image.clone()],
            replaced_images: HashSet::from([9]),
            ..Default::default()
        };
        let selection = PageSelection::Document(Arc::new(SelectionSegments {
            replaced: HashSet::from([9]),
            ..Default::default()
        }));
        let scene = compose(&page, Some(&selection));
        let at = scene.primitives.iter().position(|c| *c == image).unwrap();
        assert!(matches!(
            scene.primitives[at + 1],
            DisplayCommand::FillRect { rect, color: PaintColor::Rgba(0, 120, 215, alpha) }
                if rect == CssRect::new(0.0, 0.0, 40.0, 30.0) && alpha < 255
        ));
    }

    #[test]
    fn painted_ranges_select_whole_runs_between_their_ends() {
        let range = PaintedRange {
            start: PaintedPosition {
                ordinal: 1,
                byte: 3,
            },
            end: PaintedPosition {
                ordinal: 3,
                byte: 2,
            },
        };
        assert_eq!(range.run_bytes(0, 5), None);
        assert_eq!(range.run_bytes(1, 5), Some(3..5));
        assert_eq!(range.run_bytes(2, 5), Some(0..5));
        assert_eq!(range.run_bytes(3, 5), Some(0..2));
        assert!(range.contains_replaced(2));
        assert!(!range.contains_replaced(3));
    }
}
