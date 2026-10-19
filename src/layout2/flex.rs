//! CSS Flexible Box Layout — the container/item property model and the §9.7
//! flexible-length resolution, implemented FROM THE SPEC TEXT (css-flexbox-1,
//! read 2026-07-05; the quoted step names below are the spec's own).
//!
//! This module is pure math over prepared numbers: flow.rs prepares each
//! item's base/min/max/margins (through the intrinsic-size query) and lays
//! the flexed results. Nothing here guesses a size — the failure class that
//! buried the old engine (`width_is_flex_base`, basis probing, measuring-
//! flag contamination) is structurally absent: base sizes come from §9.2's
//! cases over definite values and honest intrinsic queries, and §9.7 is the
//! spec's freeze loop verbatim.

use crate::dom::{Dom, NodeId};
use crate::layout2::Units;

use super::value::{Len, Vp};

/// Container-level flex style, resolved once per container.
pub(crate) struct FlexStyle {
    /// Main axis is horizontal (`row`/`row-reverse`).
    pub row: bool,
    /// Main-axis direction is reversed (`row-reverse`/`column-reverse`).
    pub reverse: bool,
    pub wrap: bool,
    /// Cross-start is the physical bottom/right edge, including both the
    /// writing direction and `wrap-reverse`.
    pub wrap_reverse: bool,
    /// The writing mode's cross-start edge, before `wrap-reverse`. Auto
    /// margins use this edge when the item overflows (Flexbox §9.6).
    pub cross_normal_reverse: bool,
    pub justify: Justify,
    pub align_items: AlignItem,
    align_items_value: String,
    pub align_content: AlignContent,
    /// Main-axis gap (`column-gap` in a row container, `row-gap` in a
    /// column one) and cross-axis gap.
    pub gap_main: Len,
    pub gap_cross: Len,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(crate) enum Justify {
    Start,
    End,
    Center,
    Between,
    Around,
    Evenly,
    /// CSS Box Alignment 3 #overflow-values: `safe center` / `safe end`
    /// start-align instead when the subject overflows its container.
    SafeCenter,
    SafeEnd,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(crate) enum AlignItem {
    Stretch,
    Start,
    End,
    Center,
    Baseline,
    /// `safe center` / `safe end` (CSS Box Alignment 3 #overflow-values).
    SafeCenter,
    SafeStart,
    SafeEnd,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(crate) enum AlignContent {
    Stretch,
    Start,
    End,
    Center,
    Between,
    Around,
    Evenly,
    /// `safe center` / `safe end` (CSS Box Alignment 3 #overflow-values).
    SafeCenter,
    SafeStart,
    SafeEnd,
}

/// CSS Box Alignment 3 #overflow-values: a leading `safe` or `unsafe`
/// keyword (`Some(true)` for safe) and the rest of an alignment value,
/// ASCII-lowercased and trimmed.
pub(crate) fn overflow_position(value: &str) -> (Option<bool>, String) {
    let value = value.trim().to_ascii_lowercase();
    if let Some(rest) = value.strip_prefix("safe ") {
        (Some(true), rest.trim().to_string())
    } else if let Some(rest) = value.strip_prefix("unsafe ") {
        (Some(false), rest.trim().to_string())
    } else {
        (None, value)
    }
}

impl Justify {
    /// The `safe` form of a positional alignment (#overflow-values).
    pub(crate) fn safe(self, safe: Option<bool>) -> Self {
        match (safe, self) {
            (Some(true), Self::Center) => Self::SafeCenter,
            (Some(true), Self::End) => Self::SafeEnd,
            _ => self,
        }
    }
}

impl AlignItem {
    pub(crate) fn safe(self, safe: Option<bool>) -> Self {
        match (safe, self) {
            (Some(true), Self::Center) => Self::SafeCenter,
            (Some(true), Self::Start) => Self::SafeStart,
            (Some(true), Self::End) => Self::SafeEnd,
            _ => self,
        }
    }
}

impl FlexStyle {
    /// Keep writing-mode-relative `start` distinct from flex cross-start.
    /// CSS Align 3 #positional-values also resolves self-start against the
    /// item's writing direction, rather than the container's direction.
    pub(super) fn item_alignment(&self, dom: &Dom, id: NodeId) -> AlignItem {
        let own = (id != super::NO_NODE)
            .then(|| dom.computed_value_resolved(id, "align-self"))
            .flatten();
        let value = own
            .as_deref()
            .filter(|value| !matches!(value.trim(), "" | "auto"))
            .unwrap_or(&self.align_items_value);
        let (safe, keyword) = overflow_position(value);
        let normal_reverse = if matches!(keyword.as_str(), "self-start" | "self-end") {
            if id == super::NO_NODE {
                self.cross_normal_reverse
            } else {
                !self.row && horizontal_rtl(dom, id)
            }
        } else {
            self.cross_normal_reverse
        };
        let align = align_item_from(value, self.align_items);
        if matches!(
            keyword.as_str(),
            "start" | "end" | "self-start" | "self-end"
        ) && self.wrap_reverse != normal_reverse
        {
            match align {
                AlignItem::Start | AlignItem::SafeStart => AlignItem::End.safe(safe),
                AlignItem::End | AlignItem::SafeEnd => AlignItem::Start.safe(safe),
                _ => align,
            }
        } else {
            align
        }
    }
}

fn horizontal_rtl(dom: &Dom, id: NodeId) -> bool {
    dom.computed_value_resolved(id, "direction").as_deref() == Some("rtl")
        && !dom
            .computed_value_resolved(id, "writing-mode")
            .is_some_and(|mode| mode.starts_with("vertical-") || mode.starts_with("sideways-"))
}

/// Read a container's flex style.
pub(crate) fn container_style(dom: &Dom, id: NodeId, u: Units, vp: Vp) -> FlexStyle {
    // CSS Custom Properties §3: flex longhands and their shorthand/gap values
    // are consumed after var() substitution, just like the box-size snapshot.
    let cv = |prop: &str| dom.computed_value_resolved(id, prop);
    // flex-direction / flex-wrap, with `flex-flow` as their shorthand.
    let flow = cv("flex-flow").unwrap_or_default();
    let dir = cv("flex-direction").unwrap_or_else(|| {
        if matches!(
            dom.computed_display(id).as_deref(),
            Some("-webkit-box" | "-webkit-inline-box")
        ) && dom.legacy_box_vertical(id)
        {
            "column".into()
        } else {
            flow.clone()
        }
    });
    let dir = dir.to_ascii_lowercase();
    let (row, reverse) = if dir.contains("column-reverse") {
        (false, true)
    } else if dir.contains("column") {
        (false, false)
    } else if dir.contains("row-reverse") {
        (true, true)
    } else {
        (true, false)
    };
    let wrap_v = cv("flex-wrap")
        .unwrap_or_else(|| flow.clone())
        .to_ascii_lowercase();
    let (wrap, wrap_reverse) = if wrap_v.contains("wrap-reverse") {
        (true, true)
    } else if wrap_v.contains("nowrap") {
        (false, false)
    } else {
        (wrap_v.contains("wrap"), false)
    };
    // CSS Flexbox 1 #flex-direction-property / #flex-wrap-property:
    // horizontal writing's inline-start follows direction. A row's main
    // axis and a column's cross axis therefore reverse in RTL, independently
    // of the explicit row/column-reverse and wrap-reverse keywords.
    let horizontal_rtl = horizontal_rtl(dom, id);
    let physical_reverse = reverse ^ (row && horizontal_rtl);
    let physical_cross_reverse = wrap_reverse ^ (!row && horizontal_rtl);
    // CSS Box Alignment 3 #positional-values: `flex-start`/`flex-end` name
    // the main-start/main-end sides, `start`/`end` the container's own
    // writing-mode edges, so they swap in a reversed container. `left` and
    // `right` are physical in a row and behave as `start` in a column.
    let (start, end) = if reverse {
        (Justify::End, Justify::Start)
    } else {
        (Justify::Start, Justify::End)
    };
    let (justify_safe, justify_value) =
        overflow_position(&cv("justify-content").unwrap_or_default());
    let justify = match justify_value.as_str() {
        "flex-end" => Justify::End,
        "start" => start,
        "end" => end,
        "left" if row => {
            if physical_reverse {
                Justify::End
            } else {
                Justify::Start
            }
        }
        "left" => start,
        "right" if row => {
            if physical_reverse {
                Justify::Start
            } else {
                Justify::End
            }
        }
        "right" => start,
        "center" => Justify::Center,
        "space-between" => Justify::Between,
        "space-around" => Justify::Around,
        "space-evenly" => Justify::Evenly,
        _ => Justify::Start,
    }
    .safe(justify_safe);
    let align_items_value = cv("align-items").unwrap_or_default();
    let align_items = align_item_from(&align_items_value, AlignItem::Stretch);
    let (content_safe, content_value) = overflow_position(&cv("align-content").unwrap_or_default());
    let align_content = match (content_safe, content_value.as_str()) {
        (Some(true), "start") if wrap_reverse => AlignContent::SafeEnd,
        (_, "start") if wrap_reverse => AlignContent::End,
        (Some(true), "end") if wrap_reverse => AlignContent::SafeStart,
        (_, "end") if wrap_reverse => AlignContent::Start,
        (Some(true), "flex-start" | "start") => AlignContent::SafeStart,
        (_, "flex-start" | "start") => AlignContent::Start,
        (Some(true), "flex-end" | "end") => AlignContent::SafeEnd,
        (_, "flex-end" | "end") => AlignContent::End,
        (Some(true), "center") => AlignContent::SafeCenter,
        (_, "center") => AlignContent::Center,
        (_, "space-between") => AlignContent::Between,
        (_, "space-around") => AlignContent::Around,
        (_, "space-evenly") => AlignContent::Evenly,
        _ => AlignContent::Stretch,
    };
    // `gap` is the `<row-gap> <column-gap>` shorthand; longhands win.
    let (short_row, short_col) = {
        let g = cv("gap").unwrap_or_default();
        let mut parts = g.split_whitespace();
        let a = parts.next().map(str::to_string);
        let b = parts.next().map(str::to_string).or_else(|| a.clone());
        (a, b)
    };
    let parse_gap = |long: Option<String>, short: Option<String>| {
        long.or(short)
            .and_then(|v| Len::parse(&v, u, vp))
            .unwrap_or(Len::px(0.0))
    };
    let row_gap = parse_gap(cv("row-gap"), short_row);
    let col_gap = parse_gap(cv("column-gap"), short_col);
    let (gap_main, gap_cross) = if row {
        (col_gap, row_gap)
    } else {
        (row_gap, col_gap)
    };
    FlexStyle {
        row,
        reverse: physical_reverse,
        wrap,
        wrap_reverse: physical_cross_reverse,
        cross_normal_reverse: !row && horizontal_rtl,
        justify,
        align_items,
        align_items_value,
        align_content,
        gap_main,
        gap_cross,
    }
}

pub(crate) fn align_item_from(v: &str, auto: AlignItem) -> AlignItem {
    let (safe, v) = overflow_position(v);
    match v.as_str() {
        "flex-start" | "start" | "self-start" => AlignItem::Start,
        "flex-end" | "end" | "self-end" => AlignItem::End,
        "center" => AlignItem::Center,
        "stretch" | "normal" => AlignItem::Stretch,
        "baseline" | "first baseline" | "last baseline" => AlignItem::Baseline,
        _ => auto,
    }
    .safe(safe)
}

/// The `flex` shorthand + longhands → (grow, shrink, basis). Longhands win
/// over shorthand components when both are present (the cascade tracks them
/// as separate properties, so per-property winners are already resolved).
pub(crate) fn item_flex(dom: &Dom, id: NodeId, u: Units, vp: Vp) -> (f32, f32, Len) {
    // Shorthand per css-flexbox-1 §7.1: `none` = 0 0 auto; `initial` = 0 1
    // auto; one number = <grow> 1 0; number number = <grow> <shrink> 0;
    // a width alone = 1 1 <basis>.
    let (mut grow, mut shrink, mut basis) = (0.0f32, 1.0f32, Len::Auto);
    let cv = |prop: &str| dom.computed_value_resolved(id, prop);
    if let Some(sh) = cv("flex") {
        let sh = sh.trim().to_ascii_lowercase();
        match sh.as_str() {
            "none" => (grow, shrink, basis) = (0.0, 0.0, Len::Auto),
            "initial" => {}
            "auto" => (grow, shrink, basis) = (1.0, 1.0, Len::Auto),
            _ => {
                let mut nums: Vec<f32> = Vec::new();
                let mut b: Option<Len> = None;
                for tok in sh.split_whitespace() {
                    if let Ok(n) = tok.parse::<f32>() {
                        // A bare number is a flex factor, not a basis (the
                        // `flex: 1` idiom — basis 0), except that `0px` etc.
                        // carry units and fall through to Len below.
                        nums.push(n);
                    } else if let Some(l) = Len::parse(tok, u, vp) {
                        b = Some(l);
                    }
                }
                match (nums.len(), b) {
                    (0, Some(l)) => (grow, shrink, basis) = (1.0, 1.0, l),
                    (1, None) => (grow, shrink, basis) = (nums[0], 1.0, Len::px(0.0)),
                    (1, Some(l)) => (grow, shrink, basis) = (nums[0], 1.0, l),
                    (2, None) => (grow, shrink, basis) = (nums[0], nums[1], Len::px(0.0)),
                    (2, Some(l)) => (grow, shrink, basis) = (nums[0], nums[1], l),
                    _ => {}
                }
            }
        }
    }
    if let Some(g) = cv("flex-grow").and_then(|v| v.trim().parse::<f32>().ok()) {
        grow = g;
    }
    if let Some(s) = cv("flex-shrink").and_then(|v| v.trim().parse::<f32>().ok()) {
        shrink = s;
    }
    if let Some(bv) = cv("flex-basis") {
        let bv = bv.trim();
        if bv.eq_ignore_ascii_case("content") {
            basis = Len::MaxContent; // §9.2.3: treat content as max-content
        } else if let Some(l) = Len::parse(bv, u, vp) {
            basis = l;
        }
    }
    (grow.max(0.0), shrink.max(0.0), basis)
}

/// One item's numbers through §9.7, all CONTENT-box main sizes in px
/// (margins/borders/padding ride separately in `mbp`, forming the OUTER
/// sizes the spec sums).
#[derive(Debug)]
pub(crate) struct FlexCalc {
    pub base: f32,
    /// "hypothetical main size": base clamped by min/max and floored at 0.
    pub hypo: f32,
    pub min: f32,
    pub max: f32,
    pub grow: f32,
    pub shrink: f32,
    /// Outer − content: margins (auto → 0 here) + borders + padding.
    pub mbp: f32,
    /// The resolved used main size (§9.7's final step).
    pub target: f32,
    frozen: bool,
}

impl FlexCalc {
    pub fn new(base: f32, min: f32, max: f32, grow: f32, shrink: f32, mbp: f32) -> FlexCalc {
        FlexCalc {
            base,
            hypo: base.clamp(min, max).max(0.0),
            min,
            max,
            grow,
            shrink,
            mbp,
            target: 0.0,
            frozen: false,
        }
    }

    pub fn outer_hypo(&self) -> f32 {
        self.hypo + self.mbp
    }
}

/// §9.7 "Resolving Flexible Lengths", the spec's freeze loop verbatim.
/// `inner_main` is the container's inner main size available to this line
/// (gaps already subtracted). Sets each item's `target` (used main size).
pub(crate) fn resolve_flexible_lengths(inner_main: f32, items: &mut [FlexCalc]) {
    if items.is_empty() {
        return;
    }
    // 1. Determine the used flex factor: sum the outer hypothetical main
    //    sizes; less than the container's inner main size → grow, else shrink.
    let sum_hypo: f32 = items.iter().map(FlexCalc::outer_hypo).sum();
    let growing = sum_hypo < inner_main;

    // 2. Each item's target main size starts at its flex base size; size
    //    inflexible items: freeze at the hypothetical main size any item
    //    with a zero factor, or whose base already over/undershoots it.
    for it in items.iter_mut() {
        let factor = if growing { it.grow } else { it.shrink };
        let inflexible =
            factor == 0.0 || (growing && it.base > it.hypo) || (!growing && it.base < it.hypo);
        if inflexible {
            it.target = it.hypo;
            it.frozen = true;
        } else {
            it.target = it.base;
        }
    }

    // 3. Initial free space: frozen items count at their outer target,
    //    others at their outer flex base size.
    let free_space = |items: &[FlexCalc]| -> f32 {
        inner_main
            - items
                .iter()
                .map(|it| (if it.frozen { it.target } else { it.base }) + it.mbp)
                .sum::<f32>()
    };
    let initial_free = free_space(items);

    // 4. The loop. The spec guarantees each pass freezes at least one item;
    //    the counter is a belt against float pathology only.
    for _ in 0..items.len() + 2 {
        if items.iter().all(|it| it.frozen) {
            break;
        }
        // Remaining free space; the "sum of flex factors less than 1" rule
        // scales the INITIAL free space and uses it when smaller in
        // magnitude (fractional factors leave space unabsorbed by design).
        let mut remaining = free_space(items);
        let factor_sum: f32 = items
            .iter()
            .filter(|it| !it.frozen)
            .map(|it| if growing { it.grow } else { it.shrink })
            .sum();
        if factor_sum < 1.0 {
            let scaled = initial_free * factor_sum;
            if scaled.abs() < remaining.abs() {
                remaining = scaled;
            }
        }
        // Distribute proportionally to the (scaled) flex factors.
        if remaining != 0.0 {
            if growing {
                if factor_sum > 0.0 {
                    for it in items.iter_mut().filter(|it| !it.frozen) {
                        it.target = it.base + remaining * (it.grow / factor_sum);
                    }
                }
            } else {
                let scaled_sum: f32 = items
                    .iter()
                    .filter(|it| !it.frozen)
                    .map(|it| it.shrink * it.base)
                    .sum();
                if scaled_sum > 0.0 {
                    for it in items.iter_mut().filter(|it| !it.frozen) {
                        let scaled = it.shrink * it.base;
                        it.target = it.base - remaining.abs() * (scaled / scaled_sum);
                    }
                }
            }
        }
        // Fix min/max violations: clamp each unfrozen target and note its
        // adjustment (clamped − unclamped). Positive = min violation,
        // negative = max violation.
        let mut total_violation = 0.0f32;
        let mut adjust = vec![0.0f32; items.len()];
        for (i, it) in items.iter_mut().enumerate() {
            if it.frozen {
                continue;
            }
            let clamped = it.target.clamp(it.min, it.max).max(0.0);
            adjust[i] = clamped - it.target;
            total_violation += clamped - it.target;
            it.target = clamped;
        }
        // Freeze over-flexed items: zero total → all; positive → the min
        // violators; negative → the max violators. (Always ≥1 item, so the
        // loop terminates.)
        for (i, it) in items.iter_mut().enumerate() {
            if it.frozen {
                continue;
            }
            let freeze = total_violation == 0.0
                || (total_violation > 0.0 && adjust[i] > 0.0)
                || (total_violation < 0.0 && adjust[i] < 0.0);
            if freeze {
                it.frozen = true;
            }
        }
    }
}

/// §9.5 justify-content distribution: `(leading offset, extra between each
/// adjacent pair)` for `free` space over `n` items. Negative free space
/// takes css-align-3 #distribution-values' fallbacks: `space-between` is
/// `safe flex-start` and `space-around`/`space-evenly` are `safe center`,
/// which overflow toward the end rather than past the start edge; plain
/// `center` and `end` stay unsafe.
pub(crate) fn justify_offsets(justify: Justify, free: f32, n: usize) -> (f32, f32) {
    if n == 0 {
        return (0.0, 0.0);
    }
    if free <= 0.0 {
        return match justify {
            Justify::End => (free, 0.0),
            Justify::Center => (free / 2.0, 0.0),
            _ => (0.0, 0.0),
        };
    }
    match justify {
        Justify::Start => (0.0, 0.0),
        Justify::End | Justify::SafeEnd => (free, 0.0),
        Justify::Center | Justify::SafeCenter => (free / 2.0, 0.0),
        Justify::Between => {
            if n == 1 {
                (0.0, 0.0)
            } else {
                (0.0, free / (n - 1) as f32)
            }
        }
        Justify::Around => {
            let per = free / n as f32;
            (per / 2.0, per)
        }
        Justify::Evenly => {
            let per = free / (n + 1) as f32;
            (per, per)
        }
    }
}

/// align-content distribution over flex lines — same shape as
/// `justify_offsets` (css-align models them identically).
pub(crate) fn align_content_offsets(align: AlignContent, free: f32, n: usize) -> (f32, f32) {
    let j = match align {
        AlignContent::Start | AlignContent::SafeStart | AlignContent::Stretch => Justify::Start,
        AlignContent::End => Justify::End,
        AlignContent::Center => Justify::Center,
        AlignContent::SafeCenter => Justify::SafeCenter,
        AlignContent::SafeEnd => Justify::SafeEnd,
        AlignContent::Between => Justify::Between,
        AlignContent::Around => Justify::Around,
        AlignContent::Evenly => Justify::Evenly,
    };
    justify_offsets(j, free, n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn servo_sticky_rtl_flex_axes_and_physical_justification_follow_direction() {
        for (direction, justify, expected) in [
            ("row", "flex-start", [(80., 0.), (60., 0.)]),
            ("row-reverse", "flex-start", [(0., 0.), (20., 0.)]),
            ("column", "flex-start", [(80., 0.), (80., 20.)]),
            ("column-reverse", "flex-start", [(80., 80.), (80., 60.)]),
            ("row", "left", [(20., 0.), (0., 0.)]),
            ("row", "right", [(80., 0.), (60., 0.)]),
            ("row", "start", [(80., 0.), (60., 0.)]),
            ("row", "end", [(20., 0.), (0., 0.)]),
        ] {
            let mut dom = Dom::parse_document(&format!(
                r#"<style>body{{margin:0}}
                #box{{display:flex;direction:rtl;flex-direction:{direction};justify-content:{justify};align-items:flex-start;width:100px;height:100px}}
                #box>div{{flex:none;width:20px;height:20px}}</style><div id=box><div id=a></div><div id=b></div></div>"#
            ));
            dom.set_viewport_px(200., 200.);
            let measured = crate::layout2::measure_retained_layout(
                &dom,
                &url::Url::parse("https://flex.test/").unwrap(),
                crate::layout2::Viewport::new(200., 200.),
                &[],
                &Default::default(),
                &Default::default(),
            );
            for (id, expected) in ["a", "b"].into_iter().zip(expected) {
                let rect = measured.boxes[&dom.get_by_id(id).unwrap()];
                assert_eq!(
                    (rect.left, rect.top),
                    expected,
                    "{direction} {justify} {id}"
                );
            }
        }
    }

    #[test]
    fn servo_sticky_flex_cross_edges_wrap_lines_and_physical_margins() {
        for (style, expected) in [
            (
                "direction:rtl;flex-direction:column;flex-wrap:wrap",
                [(80., 0.), (60., 0.)],
            ),
            (
                "direction:rtl;flex-direction:column;flex-wrap:wrap-reverse",
                [(0., 0.), (20., 0.)],
            ),
            (
                "direction:rtl;flex-direction:column;flex-wrap:wrap-reverse;align-content:start",
                [(60., 0.), (80., 0.)],
            ),
            (
                "flex-wrap:wrap-reverse;align-content:stretch",
                [(0., 40.), (20., 40.)],
            ),
            (
                "flex-wrap:wrap-reverse;align-content:stretch;align-items:start",
                [(0., 0.), (20., 0.)],
            ),
            (
                "direction:rtl;flex-direction:column;align-items:flex-start",
                [(80., 0.), (80., 60.)],
            ),
        ] {
            let mut dom = Dom::parse_document(&format!(
                r#"<style>body{{margin:0}}
                #box{{display:flex;align-items:flex-start;align-content:flex-start;width:100px;height:100px;{style}}}
                #box>div{{flex:none;width:20px;height:60px}}</style><div id=box><div id=a></div><div id=b></div></div>"#
            ));
            dom.set_viewport_px(200., 200.);
            let measured = crate::layout2::measure_retained_layout(
                &dom,
                &url::Url::parse("https://flex.test/").unwrap(),
                crate::layout2::Viewport::new(200., 200.),
                &[],
                &Default::default(),
                &Default::default(),
            );
            for (id, expected) in ["a", "b"].into_iter().zip(expected) {
                let rect = measured.boxes[&dom.get_by_id(id).unwrap()];
                assert_eq!((rect.left, rect.top), expected, "{style} {id}");
            }
        }
        for (margin, expected) in [
            ("margin-left:10px;margin-right:5px", 75.),
            ("margin-left:auto", 80.),
            ("margin-right:auto", 0.),
        ] {
            let mut dom = Dom::parse_document(&format!(
                r#"<style>body{{margin:0}}
                #box{{display:flex;direction:rtl;flex-direction:column;align-items:flex-start;width:100px;height:100px}}
                #a{{flex:none;width:20px;height:20px;{margin}}}</style><div id=box><div id=a></div></div>"#
            ));
            dom.set_viewport_px(200., 200.);
            let measured = crate::layout2::measure_retained_layout(
                &dom,
                &url::Url::parse("https://flex.test/").unwrap(),
                crate::layout2::Viewport::new(200., 200.),
                &[],
                &Default::default(),
                &Default::default(),
            );
            assert_eq!(
                measured.boxes[&dom.get_by_id("a").unwrap()].left,
                expected,
                "{margin}"
            );
        }
        for (direction, wrap, align, margin, expected) in [
            ("rtl", "nowrap", "safe flex-end", "", -50.),
            ("rtl", "wrap-reverse", "safe flex-start", "", -50.),
            ("ltr", "wrap-reverse", "safe flex-start", "", 0.),
            ("ltr", "wrap-reverse", "safe end", "", 0.),
            ("ltr", "wrap-reverse", "end", "", -50.),
            ("rtl", "wrap-reverse", "center", "margin-left:auto", -50.),
            ("ltr", "wrap-reverse", "center", "margin-right:auto", 0.),
        ] {
            let mut dom = Dom::parse_document(&format!(
                r#"<style>body{{margin:0}}
                #box{{display:flex;direction:{direction};flex-direction:column;flex-wrap:{wrap};align-items:{align};width:100px;height:100px}}
                #a{{flex:none;width:150px;height:20px;{margin}}}</style><div id=box><div id=a></div></div>"#
            ));
            dom.set_viewport_px(200., 200.);
            let measured = crate::layout2::measure_retained_layout(
                &dom,
                &url::Url::parse("https://flex.test/").unwrap(),
                crate::layout2::Viewport::new(200., 200.),
                &[],
                &Default::default(),
                &Default::default(),
            );
            assert_eq!(
                measured.boxes[&dom.get_by_id("a").unwrap()].left,
                expected,
                "{direction} {wrap} {align} {margin}"
            );
        }
    }

    #[test]
    fn distributed_justification_falls_back_to_safe_alignment() {
        // css-align-3: overflowing space-around/space-evenly items are safely
        // centered, i.e. start-aligned; an explicit center stays unsafe.
        assert_eq!(justify_offsets(Justify::Around, -40.0, 2), (0.0, 0.0));
        assert_eq!(justify_offsets(Justify::Evenly, -40.0, 2), (0.0, 0.0));
        assert_eq!(justify_offsets(Justify::Between, -40.0, 2), (0.0, 0.0));
        assert_eq!(justify_offsets(Justify::Center, -40.0, 2), (-20.0, 0.0));
        assert_eq!(justify_offsets(Justify::Around, 40.0, 2), (10.0, 20.0));
    }

    fn calc(base: f32, min: f32, max: f32, grow: f32, shrink: f32) -> FlexCalc {
        FlexCalc::new(base, min, max, grow, shrink, 0.0)
    }

    #[test]
    fn grow_distributes_proportionally() {
        let mut items = vec![
            calc(100.0, 0.0, f32::INFINITY, 1.0, 1.0),
            calc(100.0, 0.0, f32::INFINITY, 3.0, 1.0),
        ];
        resolve_flexible_lengths(600.0, &mut items);
        assert_eq!(items[0].target, 200.0);
        assert_eq!(items[1].target, 400.0);
    }

    #[test]
    fn grow_respects_max_and_redistributes() {
        let mut items = vec![
            calc(100.0, 0.0, 150.0, 1.0, 1.0),
            calc(100.0, 0.0, f32::INFINITY, 1.0, 1.0),
        ];
        resolve_flexible_lengths(600.0, &mut items);
        assert_eq!(items[0].target, 150.0, "clamped at max, frozen");
        assert_eq!(items[1].target, 450.0, "reclaims the frozen item's share");
    }

    #[test]
    fn shrink_scales_by_base_and_floors_at_min() {
        // 300+300 into 400: equal shrink factors × equal bases → 200 each,
        // but item 0's min 250 floors it; item 1 absorbs the rest.
        let mut items = vec![
            calc(300.0, 250.0, f32::INFINITY, 0.0, 1.0),
            calc(300.0, 0.0, f32::INFINITY, 0.0, 1.0),
        ];
        resolve_flexible_lengths(400.0, &mut items);
        assert_eq!(items[0].target, 250.0);
        assert_eq!(items[1].target, 150.0);
    }

    #[test]
    fn zero_factors_freeze_at_hypothetical() {
        let mut items = vec![
            calc(100.0, 0.0, f32::INFINITY, 0.0, 0.0),
            calc(100.0, 0.0, f32::INFINITY, 1.0, 1.0),
        ];
        resolve_flexible_lengths(500.0, &mut items);
        assert_eq!(items[0].target, 100.0);
        assert_eq!(items[1].target, 400.0);
    }

    #[test]
    fn factor_sum_below_one_leaves_space() {
        // flex-grow: 0.5 absorbs only half the free space (the spec's
        // sum-of-factors-less-than-one rule).
        let mut items = vec![calc(100.0, 0.0, f32::INFINITY, 0.5, 1.0)];
        resolve_flexible_lengths(300.0, &mut items);
        assert_eq!(items[0].target, 200.0, "100 + 0.5 × 200 free");
    }

    #[test]
    fn shrink_is_proportional_to_scaled_factor() {
        // Bigger items give up more (scaled flex shrink factor = factor ×
        // base): 400 and 200 shrinking by 150 total → 100 and 50.
        let mut items = vec![
            calc(400.0, 0.0, f32::INFINITY, 0.0, 1.0),
            calc(200.0, 0.0, f32::INFINITY, 0.0, 1.0),
        ];
        resolve_flexible_lengths(450.0, &mut items);
        assert!((items[0].target - 300.0).abs() < 0.01);
        assert!((items[1].target - 150.0).abs() < 0.01);
    }

    #[test]
    fn justify_distributions() {
        assert_eq!(justify_offsets(Justify::Center, 100.0, 2), (50.0, 0.0));
        assert_eq!(justify_offsets(Justify::Between, 100.0, 3), (0.0, 50.0));
        assert_eq!(justify_offsets(Justify::Around, 100.0, 2), (25.0, 50.0));
        assert_eq!(justify_offsets(Justify::Evenly, 90.0, 2), (30.0, 30.0));
        // Overflow fallbacks: between → safe flex-start, around/evenly →
        // safe center (start-aligned when overflowing; LibreWolf agrees).
        assert_eq!(justify_offsets(Justify::Between, -40.0, 2), (0.0, 0.0));
        assert_eq!(justify_offsets(Justify::Around, -40.0, 2), (0.0, 0.0));
    }
}
