//! Retained slices of one shaping context (CSS Text 3 #boundary-shaping and
//! #word-break-shaping, CSSWG 81c27f686901). Formatting that changes paint or
//! DOM ownership does not reshape glyphs. The interval index keeps slicing a
//! paragraph split into many inline elements from repeating glyph scans.

use super::{Cluster, ShapedRun, ShapedText, TextStyle};
use skrifa::{
    FontRef, MetadataProvider,
    instance::{NormalizedCoord, Size},
};
use std::ops::Range;
use std::sync::Arc;

#[derive(Debug)]
struct GlyphInterval {
    left: f32,
    right: f32,
    run: usize,
    glyph: usize,
    source: Range<usize>,
}

#[derive(Debug)]
pub(crate) struct ContextShape {
    pub text: String,
    content: Range<usize>,
    shaped: ShapedText,
    style: TextStyle,
    left_to_right: std::sync::OnceLock<Arc<ContextShape>>,
    right_to_left: std::sync::OnceLock<Arc<ContextShape>>,
    logical: Vec<Cluster>,
    advances: Vec<f32>,
    glyphs: Vec<GlyphInterval>,
    max_right: f32,
    min_left: f32,
    max_source_end: Vec<usize>,
    breaks: [std::sync::OnceLock<Vec<usize>>; 3],
}

#[derive(Clone, Debug)]
pub(crate) struct ContextSlice {
    pub context: Arc<ContextShape>,
    pub range: Range<usize>,
}

impl PartialEq for ContextSlice {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.context, &other.context) && self.range == other.range
    }
}

impl ContextShape {
    #[cfg(test)]
    pub fn new(text: String, style: &TextStyle) -> Self {
        let end = text.len();
        Self::with_direction(text, style, None, 0..end)
    }

    pub fn with_content(text: String, style: &TextStyle, content: Range<usize>) -> Self {
        Self::with_direction(text, style, None, content)
    }

    fn with_direction(
        text: String,
        style: &TextStyle,
        direction: Option<bool>,
        content: Range<usize>,
    ) -> Self {
        let (shaped, sources) = super::shape_context(&text, style, direction);
        let mut logical = shaped.clusters.clone();
        logical.sort_by_key(|c| c.text_range.start);
        let mut advances = vec![0.];
        for cluster in &logical {
            advances.push(advances.last().unwrap() + cluster.advance);
        }
        let mut glyphs = Vec::new();
        for (run_index, run) in shaped.runs.iter().enumerate() {
            let font =
                FontRef::from_index(run.font.data().data.as_ref(), run.font.collection_index())
                    .ok();
            let coords: Vec<_> = run
                .normalized_coords
                .iter()
                .copied()
                .map(NormalizedCoord::from_bits)
                .collect();
            let metrics = font
                .as_ref()
                .map(|font| font.glyph_metrics(Size::new(run.font_size), coords.as_slice()));
            for (glyph_index, glyph) in run.glyphs.iter().enumerate() {
                let mut source = sources
                    .get(run_index)
                    .and_then(|run| run.get(glyph_index))
                    .cloned()
                    .unwrap_or_else(|| run.text_range.clone());
                // Keep an extended grapheme whole if the font divides its
                // marks into separate shaping clusters.
                let first =
                    logical.partition_point(|cluster| cluster.text_range.end <= source.start);
                let last = logical.partition_point(|cluster| cluster.text_range.start < source.end);
                if let Some(cluster) = logical.get(first) {
                    source.start = source.start.min(cluster.text_range.start);
                }
                if last > 0 {
                    source.end = source.end.max(logical[last - 1].text_range.end);
                }
                // Joining context is synthetic, not another DOM owner.
                source.start = source.start.clamp(content.start, content.end);
                source.end = source.end.clamp(content.start, content.end);
                let bounds = metrics
                    .as_ref()
                    .and_then(|m| m.bounds(skrifa::GlyphId::new(glyph.id)));
                let (mut left, mut right) = bounds.map_or((0., glyph.advance), |b| {
                    let skew = run.synth_skew_degrees.unwrap_or(0.).to_radians().tan();
                    let shift = [b.y_min * skew, b.y_max * skew];
                    (
                        b.x_min + shift[0].min(shift[1]),
                        b.x_max + shift[0].max(shift[1]),
                    )
                });
                if run.synth_bold {
                    left -= run.font_size / 24.;
                    right += run.font_size / 24.;
                }
                glyphs.push(GlyphInterval {
                    left: glyph.x + left,
                    right: glyph.x + right,
                    run: run_index,
                    glyph: glyph_index,
                    source,
                });
            }
        }
        let max_right = glyphs
            .iter()
            .map(|g| g.right)
            .fold(shaped.advance, f32::max);
        let min_left = glyphs.iter().map(|g| g.left).fold(0., f32::min);
        glyphs.sort_by_key(|glyph| glyph.source.start);
        let mut end = 0;
        let max_source_end = glyphs
            .iter()
            .map(|glyph| {
                end = end.max(glyph.source.end);
                end
            })
            .collect();
        Self {
            text,
            content,
            shaped,
            style: style.clone(),
            left_to_right: Default::default(),
            right_to_left: Default::default(),
            logical,
            advances,
            glyphs,
            max_right,
            min_left,
            max_source_end,
            breaks: Default::default(),
        }
    }

    fn opportunities(&self, mode: super::TextWordBreak) -> &[usize] {
        let key = match mode {
            super::TextWordBreak::Normal => 0,
            super::TextWordBreak::BreakAll => 1,
            super::TextWordBreak::KeepAll => 2,
        };
        self.breaks[key].get_or_init(|| {
            if super::cell_metrics().is_some() {
                return super::cells::opportunities(&self.text, mode).collect();
            }
            // Reuse Parley's cluster/dictionary analysis. Breaking at a
            // tiny width exposes every ordinary opportunity without replacing
            // the retained-context advances used by the actual line solver.
            super::TEXT.with_borrow_mut(|system| {
                system.select_fonts(self.style.font_set.as_ref());
                let prepared = system.prepare(&self.text, &self.style, None);
                let mut layout = system.prepared_layout::<()>(
                    &prepared,
                    &self.style,
                    true,
                    Some(super::TextBreakStyle {
                        wrap: true,
                        word_break: mode,
                        overflow_wrap: super::TextOverflowWrap::Normal,
                    }),
                    &[],
                );
                layout.break_all_lines(Some(0.01));
                let mut ends: Vec<_> = layout
                    .lines()
                    .map(|line| {
                        let at = line.text_range().end;
                        prepared
                            .map
                            .as_ref()
                            .map_or(at, |map| map.to_source(at, true))
                    })
                    .filter(|at| *at > 0 && *at <= self.text.len())
                    .collect();
                ends.push(self.text.len());
                ends.sort_unstable();
                ends.dedup();
                ends
            })
        })
    }

    fn partial(&self, cluster: &Cluster, at: usize) -> f32 {
        if at <= cluster.text_range.start {
            return 0.;
        }
        if at >= cluster.text_range.end {
            return cluster.advance;
        }
        // retain_line and the cell adapter each retain one extended grapheme
        // per cluster. An element boundary inside it keeps its advance with
        // the leading base (CSS Text 3 #characters permits this ownership).
        // Do not rescan a potentially long combining sequence for every
        // source fragment that splits it.
        cluster.advance
    }

    fn cumulative(&self, at: usize) -> f32 {
        let index = self
            .logical
            .partition_point(|cluster| cluster.text_range.end <= at);
        self.advances[index]
            + self
                .logical
                .get(index)
                .map_or(0., |cluster| self.partial(cluster, at))
    }

    pub fn advance(&self, range: Range<usize>) -> f32 {
        (self.cumulative(range.end) - self.cumulative(range.start)).max(0.)
    }

    pub(crate) fn retained_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.text.capacity()
            + self.shaped.retained_bytes()
            + self.style.family.capacity()
            + self.style.language.as_ref().map_or(0, String::capacity)
            + self.logical.capacity() * std::mem::size_of::<Cluster>()
            + self.advances.capacity() * std::mem::size_of::<f32>()
            + self.max_source_end.capacity() * std::mem::size_of::<usize>()
            + self.glyphs.capacity() * std::mem::size_of::<GlyphInterval>()
            + self
                .breaks
                .iter()
                .filter_map(|cell| cell.get())
                .map(|v| v.capacity() * std::mem::size_of::<usize>())
                .sum::<usize>()
            + [&self.left_to_right, &self.right_to_left]
                .into_iter()
                .filter_map(|cell| cell.get())
                .map(|context| context.retained_bytes())
                .sum::<usize>()
    }

    fn bounds(&self, range: &Range<usize>) -> [f32; 2] {
        let start = self
            .logical
            .partition_point(|cluster| cluster.text_range.end <= range.start);
        let mut left = f32::INFINITY;
        let mut right = f32::NEG_INFINITY;
        for cluster in self.logical[start..]
            .iter()
            .take_while(|c| c.text_range.start < range.end)
        {
            let a = self.partial(cluster, range.start);
            let b = self.partial(cluster, range.end);
            let (a, b) = if cluster.rtl {
                (cluster.advance - b, cluster.advance - a)
            } else {
                (a, b)
            };
            left = left.min(cluster.x + a);
            right = right.max(cluster.x + b);
        }
        if left.is_finite() {
            [left, right]
        } else {
            [0., 0.]
        }
    }
}

impl ContextSlice {
    pub fn new(context: Arc<ContextShape>, range: Range<usize>) -> Self {
        Self { context, range }
    }

    pub fn advance(&self) -> f32 {
        self.context.advance(self.range.clone())
    }

    pub fn subrange(&self, range: Range<usize>) -> Self {
        Self::new(
            self.context.clone(),
            self.range.start + range.start..self.range.start + range.end,
        )
    }

    pub fn reads_direction(&self, rtl: bool) -> bool {
        let start = self
            .context
            .logical
            .partition_point(|cluster| cluster.text_range.end <= self.range.start);
        self.context.logical[start..]
            .iter()
            .take_while(|cluster| cluster.text_range.start < self.range.end)
            .all(|cluster| cluster.rtl == rtl)
    }

    pub fn directed(&self, rtl: bool) -> Self {
        if self.reads_direction(rtl) {
            return self.clone();
        }
        let cache = if rtl {
            &self.context.right_to_left
        } else {
            &self.context.left_to_right
        };
        let context = cache.get_or_init(|| {
            Arc::new(ContextShape::with_direction(
                self.context.text.clone(),
                &self.context.style,
                Some(rtl),
                self.context.content.clone(),
            ))
        });
        Self::new(context.clone(), self.range.clone())
    }

    pub fn joins(&self, next: &Self) -> bool {
        Arc::ptr_eq(&self.context, &next.context) && self.range.end <= next.range.start
    }

    pub fn is_grapheme_boundary(&self) -> bool {
        let at = self
            .context
            .logical
            .partition_point(|cluster| cluster.text_range.start < self.range.start);
        self.range.start == self.context.content.start
            || self
                .context
                .logical
                .get(at)
                .is_some_and(|cluster| cluster.text_range.start == self.range.start)
    }

    fn owned_range(&self) -> Range<usize> {
        // CSS Text 3 #characters permits either inline element to own a
        // grapheme split by the DOM. Keep it with its first component,
        // including zero-advance marks and full ink. N source fragments must
        // not each retain all N glyphs of one long combining sequence.
        let after = |at| {
            if at == self.context.content.start || at == self.context.content.end {
                return at;
            }
            let index = self
                .context
                .logical
                .partition_point(|cluster| cluster.text_range.end <= at);
            self.context
                .logical
                .get(index)
                .filter(|cluster| cluster.text_range.start < at)
                .map_or(at, |cluster| {
                    cluster.text_range.end.min(self.context.content.end)
                })
        };
        after(self.range.start)..after(self.range.end)
    }

    fn selected_glyphs(&self) -> impl Iterator<Item = &GlyphInterval> {
        let range = self.owned_range();
        let end = self
            .context
            .glyphs
            .partition_point(|glyph| glyph.source.start < range.end);
        let start = self.context.max_source_end[..end].partition_point(|end| *end <= range.start);
        self.context.glyphs[start..end]
            .iter()
            .filter(move |glyph| range.start < range.end && glyph.source.end > range.start)
    }

    pub fn clip(&self, outset: f32) -> Option<[f32; 2]> {
        let mut before = false;
        let mut after = false;
        let range = self.owned_range();
        for glyph in self.selected_glyphs() {
            let rtl = self.context.shaped.runs[glyph.run].rtl;
            let earlier = glyph.source.start < range.start;
            let later = glyph.source.end > range.end;
            before |= if rtl { later } else { earlier };
            after |= if rtl { earlier } else { later };
        }
        if !before && !after {
            return None;
        }
        let [left, right] = self.context.bounds(&self.range);
        Some([
            if before {
                0.
            } else {
                self.context.min_left - left - outset
            },
            if after {
                right - left
            } else {
                self.context.max_right - left + outset
            },
        ])
    }

    pub fn shape(&self, style: &TextStyle) -> ShapedText {
        let context = &self.context;
        let full = &context.shaped;
        // Line-height is not part of a shaping context, but each inline box
        // keeps its own font metrics and half-leading (CSS 2 #leading).
        // Reuse the ordinary metrics path so its pixel-quantized baseline
        // agrees with the containing block's strut. Centering the retained
        // normal line by an unrounded half-pixel instead grows equal-height
        // lines when their independently quantized struts are aligned.
        let metrics = super::shape(&context.text[self.range.clone()], style);
        let baseline_shift = metrics.baseline - full.baseline;
        let [left, _] = context.bounds(&self.range);
        let mut selected: Vec<_> = self
            .selected_glyphs()
            .map(|glyph| (glyph.run, glyph.glyph))
            .collect();
        selected.sort_unstable();
        let mut runs: Vec<ShapedRun> = Vec::new();
        let mut active = None;
        for (run_index, glyph_index) in selected {
            if active != Some(run_index) {
                let run = &full.runs[run_index];
                let mut part = run.clone();
                part.glyphs.clear();
                part.text_range = run.text_range.start.clamp(self.range.start, self.range.end)
                    - self.range.start
                    ..run.text_range.end.clamp(self.range.start, self.range.end) - self.range.start;
                runs.push(part);
                active = Some(run_index);
            }
            let mut glyph = full.runs[run_index].glyphs[glyph_index];
            glyph.x -= left;
            glyph.y += baseline_shift;
            runs.last_mut().unwrap().glyphs.push(glyph);
        }
        let start = context
            .logical
            .partition_point(|cluster| cluster.text_range.end <= self.range.start);
        let clusters = context.logical[start..]
            .iter()
            .take_while(|cluster| cluster.text_range.start < self.range.end)
            .map(|cluster| {
                let from = cluster.text_range.start.max(self.range.start);
                let to = cluster.text_range.end.min(self.range.end);
                let a = context.partial(cluster, from);
                let b = context.partial(cluster, to);
                Cluster {
                    text_range: from - self.range.start..to - self.range.start,
                    x: cluster.x + if cluster.rtl { cluster.advance - b } else { a } - left,
                    advance: b - a,
                    rtl: cluster.rtl,
                }
            })
            .collect();
        super::ShapedTextData {
            text: context.text[self.range.clone()].to_owned(),
            advance: self.advance(),
            ascent: metrics.ascent,
            descent: metrics.descent,
            leading: metrics.leading,
            line_height: metrics.line_height,
            baseline: metrics.baseline,
            underline: style.underline,
            strikethrough: style.strikethrough,
            runs,
            clusters,
        }
        .into()
    }
}

/// Choose line breaks using retained-context advances rather than reshaping
/// every candidate prefix (CSS Text 3 #word-break-shaping).
pub(crate) fn first_line_end(
    slice: &ContextSlice,
    text: &str,
    width: f32,
    breaks: super::TextBreakStyle,
    hang_spaces: bool,
) -> usize {
    let opportunities = slice.context.opportunities(breaks.word_break);
    let first = opportunities.partition_point(|at| *at <= slice.range.start);
    let last = opportunities.partition_point(|at| *at <= slice.range.end);
    let opportunities = opportunities[first..last]
        .iter()
        .copied()
        .map(|at| at - slice.range.start)
        .chain(std::iter::once(text.len()));
    super::cells::first_line_end_at(
        text,
        width,
        breaks,
        |end| {
            let end = if hang_spaces {
                text[..end].trim_end_matches(' ').len()
            } else {
                end
            };
            slice
                .context
                .advance(slice.range.start..slice.range.start + end)
        },
        opportunities,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn servo_audit_shaping_assigns_split_combining_units_to_their_first_source() {
        let style = TextStyle::default();
        let text = "a".to_owned() + &"\u{301}".repeat(64) + "b";
        let end = text.len() - 1;
        let context = Arc::new(ContextShape::new(text, &style));
        let first = ContextSlice::new(context.clone(), 0..1);
        assert!(
            first.clip(2.).is_none(),
            "a source inside one grapheme owns the whole unit's ink"
        );
        assert!(
            first
                .shape(&style)
                .runs
                .iter()
                .map(|r| r.glyphs.len())
                .sum::<usize>()
                > 1
        );
        for start in (1..end).step_by(2) {
            let mark = ContextSlice::new(context.clone(), start..start + 2);
            assert_eq!(mark.advance(), 0.);
            assert_eq!(
                mark.selected_glyphs().count(),
                0,
                "marks are not duplicated in every zero-advance source fragment"
            );
        }
        let last = ContextSlice::new(context, end..end + 1);
        assert_eq!(
            last.shape(&style)
                .runs
                .iter()
                .map(|r| r.glyphs.len())
                .sum::<usize>(),
            1
        );
    }

    #[test]
    fn servo_audit_shaping_keeps_italic_overhang_with_its_source_and_rtl_ligature_carets() {
        let style = TextStyle {
            family: "serif".into(),
            size: 48.,
            italic: true,
            ..Default::default()
        };
        let context = Arc::new(ContextShape::new("fx".into(), &style));
        let first = ContextSlice::new(context.clone(), 0..1);
        let second = ContextSlice::new(context, 1..2);
        assert!(
            first.clip(2.).is_none() && second.clip(2.).is_none(),
            "independent glyph overhangs are painted by their own source"
        );
        let a = first.shape(&style);
        let b = second.shape(&style);
        assert_eq!(a.runs.iter().flat_map(|r| &r.glyphs).count(), 1);
        assert_eq!(b.runs.iter().flat_map(|r| &r.glyphs).count(), 1);
        let style = TextStyle {
            family: "sans-serif".into(),
            size: 48.,
            ..Default::default()
        };
        let context = Arc::new(ContextShape::new("لا".into(), &style));
        assert_eq!(
            context.shaped.runs.iter().flat_map(|r| &r.glyphs).count(),
            1,
            "fixture must use lam-alef ligature"
        );
        let first = ContextSlice::new(context.clone(), 0..2);
        let second = ContextSlice::new(context, 2..4);
        assert_eq!(
            first.clip(2.).unwrap()[0],
            0.,
            "the logically first component owns the visual right"
        );
        assert!(
            (second.clip(2.).unwrap()[1] - second.advance()).abs() < 0.01,
            "the logically last component owns the visual left"
        );
    }

    #[test]
    fn servo_audit_shaping_context_keeps_offsets_past_64k_and_caps_expansion() {
        // The pinned Parley correction keeps run-relative source offsets
        // wide enough for one shared context, not just individual DOM leaves.
        for caps in [
            super::super::typography::Caps::Normal,
            super::super::typography::Caps::SmallCaps,
        ] {
            let style = TextStyle {
                family: "serif".into(),
                typography: Some(Arc::new(super::super::FontTypography {
                    caps,
                    ..Default::default()
                })),
                ..Default::default()
            };
            let text = "x".repeat(70_000) + "ffi";
            let context = Arc::new(ContextShape::new(text.clone(), &style));
            assert_eq!(context.logical.last().unwrap().text_range.end, text.len());
            let mut previous = 0;
            for cluster in &context.logical {
                assert!(
                    cluster.text_range.start >= previous,
                    "source offsets must not wrap"
                );
                assert!(cluster.text_range.end <= text.len());
                previous = cluster.text_range.end;
            }
            let slice = ContextSlice::new(context, 70_000..text.len());
            let expected = super::super::shape("ffi", &style);
            let actual = slice.shape(&style);
            assert!(
                (slice.advance() - expected.advance).abs() < 0.1,
                "{caps:?}: {} vs {}",
                slice.advance(),
                expected.advance
            );
            assert_eq!(
                actual
                    .runs
                    .iter()
                    .flat_map(|r| r.glyphs.iter().map(|g| g.id))
                    .collect::<Vec<_>>(),
                expected
                    .runs
                    .iter()
                    .flat_map(|r| r.glyphs.iter().map(|g| g.id))
                    .collect::<Vec<_>>()
            );
        }
    }
}
