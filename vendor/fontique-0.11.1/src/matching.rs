// Copyright 2024 the Parley Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Implementation of the CSS font matching algorithm.
//!
//! TRust: CSS Fonts 4 #font-style-matching (csswg-drafts 81c27f686901) with
//! ranges. A face supports a range of widths, weights, and italic values or
//! oblique angles: a variable font through its `wdth`, `wght`, `ital` and
//! `slnt` axes, an `@font-face` face through its descriptors. "The algorithm
//! proceeds as if each supported combination of values is a unique font in
//! the set": each step finds the value nearest the desired one in the
//! specified search order among those the remaining faces support, then
//! removes the faces that do not support it.

use super::font::{FontInfo, StyleRange};
use crate::{FontStyle, FontWeight, FontWidth};
use smallvec::SmallVec;

/// The default `oblique` angle (#font-style-prop: "the lack of an <angle>
/// represents 14deg").
pub(crate) const DEFAULT_OBLIQUE_ANGLE: f32 = 14.0;

/// The angle at which an `oblique` request prefers larger angles and the
/// italic search looks for oblique faces (#font-style-matching uses 11deg).
const OBLIQUE_THRESHOLD: f32 = 11.0;

#[derive(Copy, Clone)]
struct Candidate {
    index: usize,
    width: [f32; 2],
    weight: [f32; 2],
    style: StyleRange,
}

pub fn match_font(
    set: &[FontInfo],
    width: FontWidth,
    style: FontStyle,
    weight: FontWeight,
    synthesize_style: bool,
) -> Option<usize> {
    match set.len() {
        0 => return None,
        1 => return Some(0),
        _ => {}
    }
    let mut set: SmallVec<[Candidate; 16]> = set
        .iter()
        .enumerate()
        .map(|(index, font)| Candidate {
            index,
            width: font.width_range(),
            weight: font.weight_range(),
            style: font.style_range(),
        })
        .collect();
    // Step 4.1: 'font-width' is tried first.
    let desired = width.ratio();
    let widths: SmallVec<[[f32; 2]; 16]> = set.iter().map(|f| f.width).collect();
    let found = if contains_any(&widths, desired) {
        Some(desired)
    } else if desired <= 1.0 {
        below(&widths, desired, f32::NEG_INFINITY).or_else(|| above(&widths, desired, f32::INFINITY))
    } else {
        above(&widths, desired, f32::INFINITY).or_else(|| below(&widths, desired, f32::NEG_INFINITY))
    };
    if let Some(found) = found {
        set.retain(|f| contains(f.width, found));
    }
    // Step 4.2: 'font-style' is tried next.
    match style_match(&set, style, synthesize_style) {
        Some(StyleMatch::Oblique(angle)) => {
            set.retain(|f| f.style.oblique.is_some_and(|range| contains(range, angle)));
        }
        Some(StyleMatch::Italic(value)) => {
            set.retain(|f| f.style.italic.is_some_and(|range| contains(range, value)));
        }
        None => {}
    }
    // Step 4.3: 'font-weight' is matched next.
    let desired = weight.value();
    let weights: SmallVec<[[f32; 2]; 16]> = set.iter().map(|f| f.weight).collect();
    let found = if contains_any(&weights, desired) {
        Some(desired)
    } else if (400.0..=500.0).contains(&desired) {
        // Weights at or above the target up to 500, then below the target
        // in descending order, then above 500.
        above(&weights, desired, 500.0)
            .or_else(|| below(&weights, desired, f32::NEG_INFINITY))
            .or_else(|| above(&weights, 500.0, f32::INFINITY))
    } else if desired < 400.0 {
        below(&weights, desired, f32::NEG_INFINITY)
            .or_else(|| above(&weights, desired, f32::INFINITY))
    } else {
        above(&weights, desired, f32::INFINITY)
            .or_else(|| below(&weights, desired, f32::NEG_INFINITY))
    };
    if let Some(found) = found {
        set.retain(|f| contains(f.weight, found));
    }
    // "more than one font might be remaining ... it must not differ between
    // two elements in the same document": the first in registration order.
    set.first().map(|f| f.index)
}

/// The value the style search settled on: the faces kept are those whose
/// oblique (or italic) range includes it.
#[derive(Copy, Clone, Debug, PartialEq)]
enum StyleMatch {
    Oblique(f32),
    Italic(f32),
}

fn style_match(set: &[Candidate], style: FontStyle, synthesize: bool) -> Option<StyleMatch> {
    let obliques: SmallVec<[[f32; 2]; 16]> = set.iter().filter_map(|f| f.style.oblique).collect();
    let italics: SmallVec<[[f32; 2]; 16]> = set.iter().filter_map(|f| f.style.italic).collect();
    let oblique = |value: Option<f32>| value.map(StyleMatch::Oblique);
    let italic = |value: Option<f32>| value.map(StyleMatch::Italic);
    // The faces a synthetic oblique of the default (upright) face can serve.
    let synthetic = || {
        (synthesize && contains_any(&obliques, 0.0)).then_some(StyleMatch::Oblique(0.0))
    };
    let inf = f32::INFINITY;
    match style {
        FontStyle::Italic => {
            // 1. Faces whose italic values include 1.
            if contains_any(&italics, 1.0) {
                return Some(StyleMatch::Italic(1.0));
            }
            // 2. Italic values above 1 ascending, then below 1 descending
            //    until 0 (positive values only). 3. An `ital` axis already
            //    makes its range include 1.
            italic(above(&italics, 1.0, inf))
                .or_else(|| italic(below_exclusive(&italics, 1.0, 0.0)))
                // 4. Oblique values >= 11deg ascending, then below 11deg
                //    descending until 0.
                .or_else(|| oblique(above(&obliques, OBLIQUE_THRESHOLD, inf)))
                .or_else(|| oblique(below_exclusive(&obliques, OBLIQUE_THRESHOLD, 0.0)))
                // 5. Italic values <= 0 descending. 6. Oblique values <= 0deg
                //    descending.
                .or_else(|| italic(below(&italics, 0.0, f32::NEG_INFINITY)))
                .or_else(|| oblique(below(&obliques, 0.0, f32::NEG_INFINITY)))
        }
        FontStyle::Oblique(angle) => {
            let angle = angle.unwrap_or(DEFAULT_OBLIQUE_ANGLE);
            if contains_any(&obliques, angle) {
                return Some(StyleMatch::Oblique(angle));
            }
            // Negative requests follow the same steps "with the negated
            // values and opposite directions".
            let sign = if angle < 0.0 { -1.0 } else { 1.0 };
            let flipped: SmallVec<[[f32; 2]; 16]> =
                obliques.iter().map(|r| negate(*r, sign)).collect();
            let a = angle * sign;
            let found = if a >= OBLIQUE_THRESHOLD {
                // 2. Above the angle ascending, then below it descending
                //    until 0 (positive values only).
                above(&flipped, a, inf).or_else(|| below_exclusive(&flipped, a, 0.0))
            } else {
                // 2. Below the angle descending until 0, then above it.
                below_exclusive(&flipped, a, 0.0).or_else(|| above(&flipped, a, inf))
            };
            if let Some(found) = found {
                return Some(StyleMatch::Oblique(found * sign));
            }
            // 3. A `slnt` axis already makes its range include the angle;
            //    otherwise a synthetic oblique of an upright face.
            if let Some(found) = synthetic() {
                return Some(found);
            }
            // 4. Italic values >= 1 ascending then below 1 descending until
            //    0 (for angles >= 11deg); below 1 descending until 0 then
            //    above 1 (for smaller angles).
            let italic_found = if a >= OBLIQUE_THRESHOLD {
                above(&italics, 1.0, inf).or_else(|| below_exclusive(&italics, 1.0, 0.0))
            } else {
                below_exclusive(&italics, 1.0, 0.0).or_else(|| above(&italics, 1.0, inf))
            };
            italic(italic_found)
                // 5. Oblique values <= 0deg descending (with the opposite
                //    direction for negative requests). 6. Italic values <= 0.
                .or_else(|| {
                    oblique(below(&flipped, 0.0, f32::NEG_INFINITY).map(|found| found * sign))
                })
                .or_else(|| italic(below(&italics, 0.0, f32::NEG_INFINITY)))
        }
        FontStyle::Normal => {
            // 1. Oblique values >= 0 ascending. 2. Italic values >= 0
            //    ascending. 3. Oblique values < 0 descending. 4. Italic
            //    values < 0 descending.
            oblique(above(&obliques, 0.0, inf))
                .or_else(|| italic(above(&italics, 0.0, inf)))
                .or_else(|| oblique(below(&obliques, 0.0, f32::NEG_INFINITY)))
                .or_else(|| italic(below(&italics, 0.0, f32::NEG_INFINITY)))
        }
    }
}

fn negate(range: [f32; 2], sign: f32) -> [f32; 2] {
    if sign < 0.0 {
        [-range[1], -range[0]]
    } else {
        range
    }
}

fn contains(range: [f32; 2], value: f32) -> bool {
    range[0] <= value && value <= range[1]
}

fn contains_any(ranges: &[[f32; 2]], value: f32) -> bool {
    ranges.iter().any(|range| contains(*range, value))
}

/// The smallest supported value in `[from, to]` ("values above `from`
/// checked in ascending order", up to `to`).
fn above(ranges: &[[f32; 2]], from: f32, to: f32) -> Option<f32> {
    ranges
        .iter()
        .filter(|range| range[1] >= from && range[0] <= to)
        .map(|range| range[0].max(from))
        .min_by(f32::total_cmp)
}

/// The largest supported value in `[to, from]` ("values below `from`
/// checked in descending order", down to `to`).
fn below(ranges: &[[f32; 2]], from: f32, to: f32) -> Option<f32> {
    ranges
        .iter()
        .filter(|range| range[0] <= from && range[1] >= to)
        .map(|range| range[1].min(from))
        .max_by(f32::total_cmp)
}

/// [`below`], excluding `to` itself: "until 0 is hit. Only positive values
/// ... are checked in this stage".
fn below_exclusive(ranges: &[[f32; 2]], from: f32, to: f32) -> Option<f32> {
    below(ranges, from, to).filter(|found| *found > to)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(width: [f32; 2], weight: [f32; 2], style: StyleRange) -> Candidate {
        Candidate {
            index: 0,
            width,
            weight,
            style,
        }
    }

    const NORMAL: StyleRange = StyleRange {
        italic: None,
        oblique: Some([0.0, 0.0]),
    };
    const ITALIC: StyleRange = StyleRange {
        italic: Some([1.0, 1.0]),
        oblique: None,
    };

    #[test]
    fn weight_search_orders_follow_the_desired_weight() {
        // #font-style-matching step 4.3, "weight: 450": 450-500 ascending,
        // then below 450 descending, then above 500.
        let ranges = [[300.0, 300.0], [400.0, 400.0], [500.0, 500.0], [600.0, 600.0]];
        assert_eq!(above(&ranges, 450.0, 500.0), Some(500.0));
        assert_eq!(below(&ranges, 450.0, f32::NEG_INFINITY), Some(400.0));
        // A range containing the desired value supplies it exactly.
        assert!(contains_any(&[[100.0, 900.0]], 750.0));
        // Ranges contribute their nearest endpoint.
        assert_eq!(above(&[[620.0, 800.0]], 550.0, f32::INFINITY), Some(620.0));
        assert_eq!(below(&[[100.0, 399.0]], 700.0, f32::NEG_INFINITY), Some(399.0));
    }

    #[test]
    fn italic_prefers_italic_then_large_obliques() {
        let set = [
            candidate([1.0; 2], [400.0; 2], NORMAL),
            candidate(
                [1.0; 2],
                [400.0; 2],
                StyleRange {
                    italic: None,
                    oblique: Some([12.0, 12.0]),
                },
            ),
        ];
        assert_eq!(
            style_match(&set, FontStyle::Italic, true),
            Some(StyleMatch::Oblique(12.0))
        );
        let set = [set[0], candidate([1.0; 2], [400.0; 2], ITALIC)];
        assert_eq!(
            style_match(&set, FontStyle::Italic, true),
            Some(StyleMatch::Italic(1.0))
        );
    }

    #[test]
    fn oblique_requests_use_slant_ranges_or_synthesize_from_upright() {
        // A `slnt` axis -15..0 supports clockwise obliques 0..15deg.
        let variable = StyleRange {
            italic: None,
            oblique: Some([0.0, 15.0]),
        };
        let set = [candidate([1.0; 2], [400.0; 2], variable), candidate([1.0; 2], [400.0; 2], ITALIC)];
        assert_eq!(
            style_match(&set, FontStyle::Oblique(Some(10.0)), true),
            Some(StyleMatch::Oblique(10.0))
        );
        // Beyond the range: the nearest supported angle below.
        assert_eq!(
            style_match(&set, FontStyle::Oblique(Some(20.0)), true),
            Some(StyleMatch::Oblique(15.0))
        );
        // Upright and italic faces only: a synthetic oblique of the upright
        // face, unless synthesis is disabled, when the italic face serves.
        let set = [candidate([1.0; 2], [400.0; 2], NORMAL), candidate([1.0; 2], [400.0; 2], ITALIC)];
        assert_eq!(
            style_match(&set, FontStyle::Oblique(Some(20.0)), true),
            Some(StyleMatch::Oblique(0.0))
        );
        assert_eq!(
            style_match(&set, FontStyle::Oblique(Some(20.0)), false),
            Some(StyleMatch::Italic(1.0))
        );
        // Negative angles search with negated values.
        let set = [
            candidate([1.0; 2], [400.0; 2], NORMAL),
            candidate(
                [1.0; 2],
                [400.0; 2],
                StyleRange {
                    italic: None,
                    oblique: Some([-20.0, -12.0]),
                },
            ),
        ];
        assert_eq!(
            style_match(&set, FontStyle::Oblique(Some(-14.0)), true),
            Some(StyleMatch::Oblique(-14.0))
        );
        assert_eq!(
            style_match(&set, FontStyle::Oblique(Some(-30.0)), true),
            Some(StyleMatch::Oblique(-20.0))
        );
    }

    #[test]
    fn normal_prefers_upright_then_italic() {
        let set = [candidate([1.0; 2], [400.0; 2], ITALIC)];
        assert_eq!(
            style_match(&set, FontStyle::Normal, true),
            Some(StyleMatch::Italic(1.0))
        );
        let ital_axis = StyleRange {
            italic: Some([0.0, 1.0]),
            oblique: None,
        };
        let set = [candidate([1.0; 2], [400.0; 2], ital_axis)];
        assert_eq!(
            style_match(&set, FontStyle::Normal, true),
            Some(StyleMatch::Italic(0.0))
        );
    }
}
