//! Gesture-start decisions for direct manipulation. Pointer Events
//! #determining-supported-direct-manipulation-behavior fixes both the allowed
//! behavior and its axis for the lifetime of the contact. Gesture recognition
//! thresholds are a UA choice; the policy and coordinate spaces are not.

use super::{PanAxis, TouchAction};
use crate::core::CssPoint;
use crate::render::Affine2d;

/// One applicable ancestor's policy and coordinate system at pointerdown.
/// Callers stop at the nearest inclusive scroll container and at document
/// boundaries when building this list.
#[derive(Clone, Copy, Debug)]
pub struct PanConstraint {
    action: TouchAction,
    to_screen: Affine2d,
    from_screen: Option<Affine2d>,
}

impl PanConstraint {
    pub fn new(action: TouchAction, to_screen: Affine2d) -> Self {
        Self {
            action,
            to_screen,
            from_screen: to_screen.inverse(),
        }
    }

    fn single_axis(self) -> Option<CssPoint> {
        let [x, y] = self.action.axes();
        match (x != PanAxis::None, y != PanAxis::None) {
            (true, false) => unit(vector(self.to_screen, CssPoint::new(1.0, 0.0))),
            (false, true) => unit(vector(self.to_screen, CssPoint::new(0.0, 1.0))),
            _ => None,
        }
    }

    fn permits(self, direction: CssPoint) -> bool {
        let Some(inverse) = self.from_screen else {
            return false;
        };
        let local = vector(inverse, direction);
        let [x, y] = self.action.axes();
        finite(local)
            && component_allowed(local.x, local.y, |value| x.permits(value))
            && component_allowed(local.y, local.x, |value| y.permits(value))
            && (x != PanAxis::None || y != PanAxis::None)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PanUpdate {
    /// Continue ordinary pointer delivery; no scrolling has taken over.
    Pointer,
    /// Scroll by a vector in viewport CSS pixels. On the first update, the
    /// caller suppresses the pointer stream before applying this delta.
    Scroll { delta: CssPoint, started: bool },
}

#[derive(Clone, Copy, Debug)]
enum Decision {
    Pending,
    Pointer,
    Pan(CssPoint),
}

/// A snapshot for one contact. Motion alone drives it; an idle contact needs
/// no timer. Style changes and subsequent direction changes cannot replace a
/// decision already made for this contact.
#[derive(Debug)]
pub struct PanGesture {
    start: CssPoint,
    previous: CssPoint,
    scroll_to_screen: Affine2d,
    scroll_from_screen: Option<Affine2d>,
    scroll_axes: [bool; 2],
    constraints: Vec<PanConstraint>,
    decision: Decision,
    moved: bool,
}

impl PanGesture {
    /// Six viewport CSS pixels distinguish incidental movement from a pan.
    const SLOP: f32 = 6.0;

    pub fn new(
        start: CssPoint,
        scroll_to_screen: Affine2d,
        scroll_axes: [bool; 2],
        constraints: Vec<PanConstraint>,
    ) -> Self {
        Self {
            start,
            previous: start,
            scroll_to_screen,
            scroll_from_screen: scroll_to_screen.inverse(),
            scroll_axes,
            constraints,
            decision: if finite(start) {
                Decision::Pending
            } else {
                Decision::Pointer
            },
            moved: !finite(start),
        }
    }

    pub fn is_tap(&self) -> bool {
        !self.moved
    }

    pub fn is_panning(&self) -> bool {
        matches!(self.decision, Decision::Pan(_))
    }

    pub fn update(&mut self, point: CssPoint) -> PanUpdate {
        // A finger moves opposite to the scrolling area's offset.
        let total = difference(self.start, point);
        let step = difference(self.previous, point);
        if !finite(point) || !finite(total) || !finite(step) {
            return if self.is_panning() {
                PanUpdate::Scroll {
                    delta: CssPoint::default(),
                    started: false,
                }
            } else {
                PanUpdate::Pointer
            };
        }
        self.previous = point;
        if total.x.hypot(total.y) >= Self::SLOP {
            self.moved = true;
        }
        match self.decision {
            Decision::Pending if !self.moved => PanUpdate::Pointer,
            Decision::Pending => match self.choose_axis(total) {
                Some(axis) => {
                    self.decision = Decision::Pan(axis);
                    PanUpdate::Scroll {
                        delta: project(total, axis),
                        started: true,
                    }
                }
                None => {
                    self.decision = Decision::Pointer;
                    PanUpdate::Pointer
                }
            },
            Decision::Pointer => PanUpdate::Pointer,
            Decision::Pan(axis) => PanUpdate::Scroll {
                delta: project(step, axis),
                started: false,
            },
        }
    }

    fn choose_axis(&self, motion: CssPoint) -> Option<CssPoint> {
        let inverse = self.scroll_from_screen?;
        // A restrictive local axis can be diagonal in viewport coordinates.
        // Keep that line rather than rounding a rotation to a screen axis.
        let mut axis = self.constraints.iter().find_map(|rule| rule.single_axis());
        if axis.is_none() {
            let local = vector(inverse, motion);
            let basis = if local.x.abs() > local.y.abs() {
                CssPoint::new(1.0, 0.0)
            } else {
                CssPoint::new(0.0, 1.0)
            };
            axis = unit(vector(self.scroll_to_screen, basis));
        }
        let mut axis = axis?;
        let along = dot(motion, axis);
        let across = motion.x * axis.y - motion.y * axis.x;
        // A predominantly perpendicular start remains a pointer gesture,
        // even if the user turns toward the permitted axis later.
        if along.abs() < across.abs() || along == 0.0 {
            return None;
        }
        if along < 0.0 {
            axis = CssPoint::new(-axis.x, -axis.y);
        }
        let local = vector(inverse, axis);
        if !finite(local)
            || !component_allowed(local.x, local.y, |_| self.scroll_axes[0])
            || !component_allowed(local.y, local.x, |_| self.scroll_axes[1])
            || !self.constraints.iter().all(|rule| rule.permits(axis))
        {
            return None;
        }
        Some(axis)
    }
}

fn finite(point: CssPoint) -> bool {
    point.x.is_finite() && point.y.is_finite()
}

fn difference(a: CssPoint, b: CssPoint) -> CssPoint {
    CssPoint::new(a.x - b.x, a.y - b.y)
}

/// Coordinate differences do not include the transform's translation.
fn vector(transform: Affine2d, delta: CssPoint) -> CssPoint {
    let [a, b, c, d, _, _] = transform.0;
    CssPoint::new(a * delta.x + c * delta.y, b * delta.x + d * delta.y)
}

fn unit(vector: CssPoint) -> Option<CssPoint> {
    let length = vector.x.hypot(vector.y);
    (length.is_finite() && length > 0.0)
        .then(|| CssPoint::new(vector.x / length, vector.y / length))
}

fn dot(a: CssPoint, b: CssPoint) -> f32 {
    a.x * b.x + a.y * b.y
}

fn project(delta: CssPoint, axis: CssPoint) -> CssPoint {
    let amount = dot(delta, axis);
    CssPoint::new(axis.x * amount, axis.y * amount)
}

fn component_allowed(value: f32, other: f32, allowed: impl FnOnce(f32) -> bool) -> bool {
    // Ignore only roundoff on an intended zero component (e.g. cos(90deg)).
    value.abs() <= other.abs() * 1.0e-5 || allowed(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gesture(action: &str) -> PanGesture {
        PanGesture::new(
            CssPoint::default(),
            Affine2d::IDENTITY,
            [true; 2],
            vec![PanConstraint::new(
                TouchAction::parse(action).unwrap(),
                Affine2d::IDENTITY,
            )],
        )
    }

    fn scroll(update: PanUpdate, expected: CssPoint, first: bool) {
        let PanUpdate::Scroll { delta, started } = update else {
            panic!("expected scrolling");
        };
        assert!((delta.x - expected.x).abs() < 0.001, "{delta:?}");
        assert!((delta.y - expected.y).abs() < 0.001, "{delta:?}");
        assert_eq!(started, first);
    }

    #[test]
    fn touch_action_pan_decision_and_axis_stay_fixed_until_contact_ends() {
        let mut blocked = gesture("pan-y");
        assert_eq!(
            blocked.update(CssPoint::new(-20.0, 1.0)),
            PanUpdate::Pointer
        );
        assert_eq!(
            blocked.update(CssPoint::new(-20.0, -80.0)),
            PanUpdate::Pointer
        );
        assert!(!blocked.is_tap());
        let mut allowed = gesture("pan-y");
        scroll(
            allowed.update(CssPoint::new(1.0, -20.0)),
            CssPoint::new(0.0, 20.0),
            true,
        );
        scroll(
            allowed.update(CssPoint::new(80.0, -25.0)),
            CssPoint::new(0.0, 5.0),
            false,
        );
        assert!(allowed.is_panning());
    }

    #[test]
    fn touch_action_directional_start_allows_reversal_after_panning() {
        let mut allowed = gesture("pan-down");
        scroll(
            allowed.update(CssPoint::new(0.0, -12.0)),
            CssPoint::new(0.0, 12.0),
            true,
        );
        scroll(
            allowed.update(CssPoint::new(0.0, 8.0)),
            CssPoint::new(0.0, -20.0),
            false,
        );
        let mut blocked = gesture("pan-down");
        assert_eq!(blocked.update(CssPoint::new(0.0, 12.0)), PanUpdate::Pointer);
        assert_eq!(
            blocked.update(CssPoint::new(0.0, -20.0)),
            PanUpdate::Pointer
        );
    }

    #[test]
    fn touch_action_tap_and_small_motion_do_not_start_panning() {
        for action in ["auto", "none", "pan-x", "pinch-zoom"] {
            let mut pan = gesture(action);
            assert_eq!(pan.update(CssPoint::new(2.0, 3.0)), PanUpdate::Pointer);
            assert!(pan.is_tap());
        }
        for action in ["none", "pinch-zoom"] {
            let mut pan = gesture(action);
            assert_eq!(pan.update(CssPoint::new(-20.0, -30.0)), PanUpdate::Pointer);
        }
    }

    #[test]
    fn touch_action_uses_each_ancestors_transformed_axes() {
        let quarter_turn = Affine2d([0.0, 1.0, -1.0, 0.0, 40.0, 70.0]);
        let child = PanConstraint::new(TouchAction::parse("pan-x").unwrap(), quarter_turn);
        for (ancestor, allowed) in [("pan-y", true), ("pan-x", false)] {
            let mut pan = PanGesture::new(
                CssPoint::default(),
                Affine2d::IDENTITY,
                [true; 2],
                vec![
                    child,
                    PanConstraint::new(TouchAction::parse(ancestor).unwrap(), Affine2d::IDENTITY),
                ],
            );
            let update = pan.update(CssPoint::new(0.0, -20.0));
            if allowed {
                scroll(update, CssPoint::new(0.0, 20.0), true);
            } else {
                assert_eq!(update, PanUpdate::Pointer);
            }
        }
    }

    #[test]
    fn touch_action_retains_diagonal_local_axes_and_scroll_orientation() {
        let q = std::f32::consts::FRAC_1_SQRT_2;
        let transform = Affine2d([q, q, -q, q, 100.0, 200.0]);
        let rule = PanConstraint::new(TouchAction::parse("pan-x").unwrap(), transform);
        let mut pan = PanGesture::new(CssPoint::default(), transform, [true, false], vec![rule]);
        scroll(
            pan.update(CssPoint::new(-12.0, -12.0)),
            CssPoint::new(12.0, 12.0),
            true,
        );
        scroll(
            pan.update(CssPoint::new(-22.0, -12.0)),
            CssPoint::new(5.0, 5.0),
            false,
        );
    }

    #[test]
    fn touch_action_unavailable_or_degenerate_axes_do_not_pan() {
        let mut pan = PanGesture::new(
            CssPoint::default(),
            Affine2d::IDENTITY,
            [false, true],
            vec![],
        );
        assert_eq!(pan.update(CssPoint::new(-20.0, 0.0)), PanUpdate::Pointer);
        let mut singular = PanGesture::new(
            CssPoint::default(),
            Affine2d::scale(0.0, 1.0),
            [true; 2],
            vec![],
        );
        assert_eq!(
            singular.update(CssPoint::new(-20.0, 0.0)),
            PanUpdate::Pointer
        );
        let mut normal = gesture("auto");
        assert_eq!(
            normal.update(CssPoint::new(f32::NAN, 0.0)),
            PanUpdate::Pointer
        );
        scroll(
            normal.update(CssPoint::new(0.0, -10.0)),
            CssPoint::new(0.0, 10.0),
            true,
        );
        scroll(
            normal.update(CssPoint::new(f32::NAN, 0.0)),
            CssPoint::default(),
            false,
        );
    }
}
