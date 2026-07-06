//! Float layout for the layout2 engine (CSS 2.1 §9.5).
//!
//! A float is a box taken out of normal flow and shifted to the left or right
//! edge of its containing block; the block boxes before and after it flow "as
//! if the float did not exist", but the line boxes NEXT TO it are shortened to
//! make room for its margin box (§9.5.1). That one sentence is the whole model,
//! and it maps cleanly onto the fragment engine: a per-block-formatting-context
//! [`FloatCtx`] holds the placed float margin boxes and answers three queries —
//! `band` (how much a line box at a given vertical position is shortened),
//! `place` (§9.5.1 placement of a new float), and `clear_y`/`bottom` (§9.5.2
//! clearance and BFC containment). Everything is absolute CSS px in the frame of
//! the block-formatting-context root that owns the context; the px→cell
//! quantization stays at the paint boundary, so floats shorten line boxes at row
//! granularity naturally.

use crate::dom::{Dom, NodeId};

/// Which edge a float is shifted to (CSS 2.1 §9.5.1). `float:none` produces no
/// [`Side`] at all (the element stays in flow).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(crate) enum Side {
    Left,
    Right,
}

/// The `clear` property (§9.5.2): which sides of a box may not be adjacent to
/// an earlier float. `none` is `{ left: false, right: false }`.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub(crate) struct Clear {
    pub left: bool,
    pub right: bool,
}

impl Clear {
    pub fn any(self) -> bool {
        self.left || self.right
    }
}

/// The float side of an element, or `None` (`float:none`, or the property
/// absent). css-logical-1 flow-relative values map by writing direction —
/// LTR-only here, so `inline-start` = left and `inline-end` = right (dom.rs
/// maps logical property NAMES under the same rule). An out-of-flow
/// (`position:absolute`/`fixed`) box computes `float:none` (§9.7) — the caller
/// checks positioning first, so this need not.
pub(crate) fn float_side(dom: &Dom, id: NodeId) -> Option<Side> {
    match dom
        .computed_value(id, "float")
        .as_deref()
        .map(str::trim)
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("left") | Some("inline-start") => Some(Side::Left),
        Some("right") | Some("inline-end") => Some(Side::Right),
        _ => None,
    }
}

/// The `clear` sides of an element (§9.5.2). Logical values map by direction
/// (LTR), same as `float_side`.
pub(crate) fn clear_of(dom: &Dom, id: NodeId) -> Clear {
    match dom
        .computed_value(id, "clear")
        .as_deref()
        .map(str::trim)
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("left") | Some("inline-start") => Clear {
            left: true,
            right: false,
        },
        Some("right") | Some("inline-end") => Clear {
            left: false,
            right: true,
        },
        Some("both") => Clear {
            left: true,
            right: true,
        },
        _ => Clear::default(),
    }
}

/// A placed float's margin-box rectangle, absolute px, in the owning BFC's
/// coordinate frame. Half-open on each axis for overlap tests.
#[derive(Copy, Clone, Debug)]
struct FloatRect {
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
}

/// The float context of one block formatting context: the placed float margin
/// boxes, split by side (CSS 2.1 §9.5.1). A fresh context is created at every
/// BFC root (the ICB, a flex/grid/table item, a `flow-root`/`overflow≠visible`
/// box, a float's own content); every other block threads the ancestor BFC's
/// context down.
#[derive(Debug, Default)]
pub(crate) struct FloatCtx {
    lefts: Vec<FloatRect>,
    rights: Vec<FloatRect>,
}

impl FloatCtx {
    pub fn new() -> FloatCtx {
        FloatCtx::default()
    }

    /// No floats placed: the fast path that keeps a float-free page byte-
    /// identical to the pre-float engine (`band` returns unbounded insets, so a
    /// line box is never shortened).
    pub fn is_empty(&self) -> bool {
        self.lefts.is_empty() && self.rights.is_empty()
    }

    /// The inline band a line box occupying `[y, y+h)` is shortened to: the
    /// inner edge of every float whose margin box overlaps that vertical band.
    /// Returns `(left_inset, right_inset)` in absolute px — the RIGHT edge of
    /// the innermost left float and the LEFT edge of the innermost right float
    /// (§9.5.1). An absent side is unbounded (`f32::MIN`/`MAX`) so the caller's
    /// `own_left.max(left_inset)` / `own_right.min(right_inset)` leaves an
    /// unfloated side at the block's own content edge.
    pub fn band(&self, y: f32, h: f32) -> (f32, f32) {
        let mut left = f32::MIN;
        let mut right = f32::MAX;
        for f in &self.lefts {
            if overlaps(f, y, h) {
                left = left.max(f.x1);
            }
        }
        for f in &self.rights {
            if overlaps(f, y, h) {
                right = right.min(f.x0);
            }
        }
        (left, right)
    }

    /// Place a `w`×`h` float margin box on `side` within the containing block's
    /// inline range `[cb_l, cb_r]`, as high as possible at or below `top_min`
    /// (§9.5.1 rules 2/3/4/5/6/7/8/9): scan the candidate shelf tops (the given
    /// minimum plus every existing float's bottom), and take the first where the
    /// float fits the band left after existing same-side floats and without
    /// crossing an opposite float. Returns the placed margin-box top-left. When
    /// nothing fits (a float wider than any open band — a desktop column in a
    /// terminal viewport), it lands at the lowest shelf and overflows, which the
    /// terminal simply cannot h-scroll to (the documented structural clip).
    pub fn place(
        &mut self,
        side: Side,
        w: f32,
        h: f32,
        top_min: f32,
        cb_l: f32,
        cb_r: f32,
    ) -> (f32, f32) {
        // Candidate shelf tops: the minimum, plus each float bottom below it
        // (moving down past a float can only widen the band). Sorted ascending
        // and de-duplicated; "as high as possible" takes the first that fits.
        let mut shelves: Vec<f32> = vec![top_min];
        for f in self.lefts.iter().chain(self.rights.iter()) {
            if f.y1 > top_min {
                shelves.push(f.y1);
            }
        }
        shelves.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        shelves.dedup();

        let mut best_y = top_min;
        for &y in &shelves {
            let (li, ri) = self.band(y, h);
            let l = cb_l.max(li);
            let r = cb_r.min(ri);
            best_y = y;
            if r - l >= w {
                let x = match side {
                    Side::Left => l,
                    Side::Right => r - w,
                };
                self.push(side, x, y, w, h);
                return (x, y);
            }
        }
        // Overflow fallback: the lowest shelf, flush to the containing-block
        // edge (§9.5.1 rule 7 — a float must be as far to the side as possible).
        let x = match side {
            Side::Left => cb_l,
            Side::Right => (cb_r - w).max(cb_l),
        };
        self.push(side, x, best_y, w, h);
        (x, best_y)
    }

    fn push(&mut self, side: Side, x: f32, y: f32, w: f32, h: f32) {
        let rect = FloatRect {
            x0: x,
            y0: y,
            x1: x + w,
            y1: y + h,
        };
        match side {
            Side::Left => self.lefts.push(rect),
            Side::Right => self.rights.push(rect),
        }
    }

    /// The y a box with `clear` set must start at or below so its top margin
    /// edge clears the named-side floats (§9.5.2): the lowest bottom among them,
    /// or `f32::MIN` when there is nothing to clear (a no-op under `.max`).
    pub fn clear_y(&self, sides: Clear) -> f32 {
        let mut y = f32::MIN;
        if sides.left {
            for f in &self.lefts {
                y = y.max(f.y1);
            }
        }
        if sides.right {
            for f in &self.rights {
                y = y.max(f.y1);
            }
        }
        y
    }

    /// The lowest float bottom in this context — a BFC container's used content
    /// height maxes this (§9.5, containment). `f32::MIN` when empty.
    pub fn bottom(&self) -> f32 {
        self.lefts
            .iter()
            .chain(self.rights.iter())
            .map(|f| f.y1)
            .fold(f32::MIN, f32::max)
    }
}

/// A float overlaps the vertical band `[y, y+h)` when their open intervals
/// intersect (a float whose bottom is exactly `y` no longer shortens the line).
fn overlaps(f: &FloatRect, y: f32, h: f32) -> bool {
    f.y0 < y + h && f.y1 > y
}

/// The resolved margin-box size of a float, pre-laid by the block flow and
/// handed to the IFC for placement (the IFC shortens line boxes but the layout
/// engine owns box laying). Only the placement geometry lives here — the
/// leading margins that offset the border box within the margin box stay on the
/// block flow's `PrelaidFloat`.
#[derive(Copy, Clone, Debug)]
pub(crate) struct FloatBox {
    pub side: Side,
    pub mw: f32,
    pub mh: f32,
}

/// A resolved float placement returned from the IFC: which pre-laid float
/// (index into the walk-order float list) and its margin-box top-left in the
/// content frame the IFC was given.
#[derive(Copy, Clone, Debug)]
pub(crate) struct FloatPlace {
    pub index: usize,
    pub x: f32,
    pub y: f32,
}
