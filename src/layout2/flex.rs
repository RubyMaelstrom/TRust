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
    /// `wrap-reverse`: the cross axis is inverted (line stacking order and
    /// cross alignment).
    pub wrap_reverse: bool,
    pub justify: Justify,
    pub align_items: AlignItem,
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
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(crate) enum AlignItem {
    Stretch,
    Start,
    End,
    Center,
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
}

/// Read a container's flex style.
pub(crate) fn container_style(dom: &Dom, id: NodeId, u: Units, vp: Vp) -> FlexStyle {
    // flex-direction / flex-wrap, with `flex-flow` as their shorthand.
    let flow = dom.computed_value(id, "flex-flow").unwrap_or_default();
    let dir = dom
        .computed_value(id, "flex-direction")
        .unwrap_or_else(|| flow.clone());
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
    let wrap_v = dom
        .computed_value(id, "flex-wrap")
        .unwrap_or_else(|| flow.clone())
        .to_ascii_lowercase();
    let (wrap, wrap_reverse) = if wrap_v.contains("wrap-reverse") {
        (true, true)
    } else if wrap_v.contains("nowrap") {
        (false, false)
    } else {
        (wrap_v.contains("wrap"), false)
    };
    let justify = match dom
        .computed_value(id, "justify-content")
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "flex-end" | "end" | "right" => Justify::End,
        "center" => Justify::Center,
        "space-between" => Justify::Between,
        "space-around" => Justify::Around,
        "space-evenly" => Justify::Evenly,
        _ => Justify::Start,
    };
    let align_items = align_item_from(
        dom.computed_value(id, "align-items")
            .as_deref()
            .unwrap_or(""),
        AlignItem::Stretch,
    );
    let align_content = match dom
        .computed_value(id, "align-content")
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "flex-start" | "start" => AlignContent::Start,
        "flex-end" | "end" => AlignContent::End,
        "center" => AlignContent::Center,
        "space-between" => AlignContent::Between,
        "space-around" => AlignContent::Around,
        "space-evenly" => AlignContent::Evenly,
        _ => AlignContent::Stretch,
    };
    // `gap` is the `<row-gap> <column-gap>` shorthand; longhands win.
    let (short_row, short_col) = {
        let g = dom.computed_value(id, "gap").unwrap_or_default();
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
    let row_gap = parse_gap(dom.computed_value(id, "row-gap"), short_row);
    let col_gap = parse_gap(dom.computed_value(id, "column-gap"), short_col);
    let (gap_main, gap_cross) = if row {
        (col_gap, row_gap)
    } else {
        (row_gap, col_gap)
    };
    FlexStyle {
        row,
        reverse,
        wrap,
        wrap_reverse,
        justify,
        align_items,
        align_content,
        gap_main,
        gap_cross,
    }
}

pub(crate) fn align_item_from(v: &str, auto: AlignItem) -> AlignItem {
    match v.trim().to_ascii_lowercase().as_str() {
        "flex-start" | "start" | "self-start" => AlignItem::Start,
        "flex-end" | "end" | "self-end" => AlignItem::End,
        "center" => AlignItem::Center,
        "stretch" | "normal" => AlignItem::Stretch,
        // Baseline alignment quantized to the line start (a cell grid has no
        // sub-row baselines; text-led items coincide anyway).
        "baseline" | "first baseline" | "last baseline" => AlignItem::Start,
        _ => auto,
    }
}

/// The `flex` shorthand + longhands → (grow, shrink, basis). Longhands win
/// over shorthand components when both are present (the cascade tracks them
/// as separate properties, so per-property winners are already resolved).
pub(crate) fn item_flex(dom: &Dom, id: NodeId, u: Units, vp: Vp) -> (f32, f32, Len) {
    // Shorthand per css-flexbox-1 §7.1: `none` = 0 0 auto; `initial` = 0 1
    // auto; one number = <grow> 1 0; number number = <grow> <shrink> 0;
    // a width alone = 1 1 <basis>.
    let (mut grow, mut shrink, mut basis) = (0.0f32, 1.0f32, Len::Auto);
    if let Some(sh) = dom.computed_value(id, "flex") {
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
    if let Some(g) = dom
        .computed_value(id, "flex-grow")
        .and_then(|v| v.trim().parse::<f32>().ok())
    {
        grow = g;
    }
    if let Some(s) = dom
        .computed_value(id, "flex-shrink")
        .and_then(|v| v.trim().parse::<f32>().ok())
    {
        shrink = s;
    }
    if let Some(bv) = dom.computed_value(id, "flex-basis") {
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
/// falls back per css-align: `space-between` packs to the start,
/// `space-around`/`space-evenly` center.
pub(crate) fn justify_offsets(justify: Justify, free: f32, n: usize) -> (f32, f32) {
    if n == 0 {
        return (0.0, 0.0);
    }
    if free <= 0.0 {
        return match justify {
            Justify::End => (free, 0.0),
            Justify::Center | Justify::Around | Justify::Evenly => (free / 2.0, 0.0),
            _ => (0.0, 0.0),
        };
    }
    match justify {
        Justify::Start => (0.0, 0.0),
        Justify::End => (free, 0.0),
        Justify::Center => (free / 2.0, 0.0),
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
        AlignContent::Start | AlignContent::Stretch => Justify::Start,
        AlignContent::End => Justify::End,
        AlignContent::Center => Justify::Center,
        AlignContent::Between => Justify::Between,
        AlignContent::Around => Justify::Around,
        AlignContent::Evenly => Justify::Evenly,
    };
    justify_offsets(j, free, n)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        // Overflow fallbacks: between → start, around/evenly → center.
        assert_eq!(justify_offsets(Justify::Between, -40.0, 2), (0.0, 0.0));
        assert_eq!(justify_offsets(Justify::Around, -40.0, 2), (-20.0, 0.0));
    }
}
