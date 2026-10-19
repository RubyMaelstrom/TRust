//! Direct-manipulation policy shared by CSS and native touchscreen input.
//!
//! Pointer Events, local W3C snapshot 49c398264c8d, #the-touch-action-css-property
//! and #determining-supported-direct-manipulation-behavior; WHATWG Compatibility
//! d2417c512256, #touch-action adds `pinch-zoom` to the grammar.

mod gesture;
pub use gesture::{PanConstraint, PanGesture, PanUpdate};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TouchPhase {
    Down,
    Move,
    Up,
    Cancel,
}

/// One native sample in top-level viewport CSS pixels. The frontend owns
/// contact identity and gesture recognition; the page owns DOM hit testing,
/// pointer capture, event dispatch and activation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TouchInput {
    pub id: i32,
    pub phase: TouchPhase,
    pub node: Option<usize>,
    pub position: crate::core::CssPoint,
    pub metadata: crate::js::PointerMetadata,
    pub pressure: f64,
    pub primary: bool,
    /// Start delayed compatibility mouse delivery for a page-owned drag.
    pub mouse: bool,
    /// An uncanceled tap, determined by native gesture recognition.
    pub tap: bool,
}

/// Permitted initial scrolling directions in an element's local axis.
/// The sign is the scroll direction, opposite to the finger's movement.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PanAxis {
    #[default]
    None,
    Negative,
    Positive,
    Both,
}

impl PanAxis {
    pub fn permits(self, delta: f32) -> bool {
        delta.is_finite()
            && delta != 0.0
            && match self {
                Self::None => false,
                Self::Negative => delta < 0.0,
                Self::Positive => delta > 0.0,
                Self::Both => true,
            }
    }
}

/// The computed (non-inherited, non-animatable) `touch-action` value.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TouchAction {
    #[default]
    Auto,
    None,
    Manipulation,
    Pan {
        x: PanAxis,
        y: PanAxis,
        pinch_zoom: bool,
    },
}

impl TouchAction {
    /// Parse the property grammar, including escaped CSS identifiers and
    /// comments. Each of the three `||` groups may occur at most once.
    pub fn parse(value: &str) -> Option<Self> {
        let mut parser = cssparser::Parser::new(value);
        let mut x = PanAxis::None;
        let mut y = PanAxis::None;
        let mut pinch_zoom = false;
        let mut count = 0;
        while !parser.is_exhausted() {
            let word = parser.expect_ident_cloned().ok()?;
            count += 1;
            match word.to_ascii_lowercase().as_str() {
                "auto" | "none" | "manipulation" => {
                    if count != 1 || !parser.is_exhausted() {
                        return None;
                    }
                    return Some(if word.eq_ignore_ascii_case("auto") {
                        Self::Auto
                    } else if word.eq_ignore_ascii_case("none") {
                        Self::None
                    } else {
                        Self::Manipulation
                    });
                }
                "pan-x" | "pan-left" | "pan-right" if x == PanAxis::None => {
                    x = if word.eq_ignore_ascii_case("pan-left") {
                        PanAxis::Negative
                    } else if word.eq_ignore_ascii_case("pan-right") {
                        PanAxis::Positive
                    } else {
                        PanAxis::Both
                    };
                }
                "pan-y" | "pan-up" | "pan-down" if y == PanAxis::None => {
                    y = if word.eq_ignore_ascii_case("pan-up") {
                        PanAxis::Negative
                    } else if word.eq_ignore_ascii_case("pan-down") {
                        PanAxis::Positive
                    } else {
                        PanAxis::Both
                    };
                }
                "pinch-zoom" if !pinch_zoom => pinch_zoom = true,
                _ => return None,
            }
        }
        (count != 0).then_some(Self::Pan { x, y, pinch_zoom })
    }

    pub fn axes(self) -> [PanAxis; 2] {
        match self {
            Self::Auto | Self::Manipulation => [PanAxis::Both; 2],
            Self::None => [PanAxis::None; 2],
            Self::Pan { x, y, .. } => [x, y],
        }
    }

    /// CSSOM #serialize-a-css-value uses grammar order for `||` groups;
    /// WHATWG Compatibility defines manipulation as the full pan/zoom alias.
    pub(crate) fn to_css(self) -> String {
        let (x, y, pinch) = match self {
            Self::Auto => return "auto".into(),
            Self::None => return "none".into(),
            Self::Manipulation => return "manipulation".into(),
            Self::Pan {
                x: PanAxis::Both,
                y: PanAxis::Both,
                pinch_zoom: true,
            } => return "manipulation".into(),
            Self::Pan { x, y, pinch_zoom } => (x, y, pinch_zoom),
        };
        let mut words = Vec::with_capacity(3);
        match x {
            PanAxis::None => {}
            PanAxis::Negative => words.push("pan-left"),
            PanAxis::Positive => words.push("pan-right"),
            PanAxis::Both => words.push("pan-x"),
        }
        match y {
            PanAxis::None => {}
            PanAxis::Negative => words.push("pan-up"),
            PanAxis::Positive => words.push("pan-down"),
            PanAxis::Both => words.push("pan-y"),
        }
        if pinch {
            words.push("pinch-zoom");
        }
        words.join(" ")
    }

    pub fn pinch_zoom(self) -> bool {
        match self {
            Self::Auto | Self::Manipulation => true,
            Self::None => false,
            Self::Pan { pinch_zoom, .. } => pinch_zoom,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn touch_action_grammar_accepts_permutations_and_css_identifiers() {
        let expected = TouchAction::Pan {
            x: PanAxis::Negative,
            y: PanAxis::Both,
            pinch_zoom: true,
        };
        for text in [
            "pan-left pan-y pinch-zoom",
            "pinch-zoom pan-y pan-left",
            "PAN-Y PAN-LEFT PINCH-ZOOM",
            r"pan-\6c eft/**/pan-y pinch-zoom",
        ] {
            assert_eq!(TouchAction::parse(text), Some(expected), "{text}");
        }
        assert_eq!(TouchAction::parse("none"), Some(TouchAction::None));
        assert_eq!(TouchAction::parse("AUTO"), Some(TouchAction::Auto));
        assert_eq!(
            TouchAction::parse("manipulation"),
            Some(TouchAction::Manipulation)
        );
        assert_eq!(
            TouchAction::parse("pinch-zoom").unwrap().axes(),
            [PanAxis::None; 2]
        );
    }

    #[test]
    fn touch_action_grammar_rejects_repeated_groups_and_extra_tokens() {
        for text in [
            "",
            "pan-x pan-left",
            "pan-left pan-right",
            "pan-up pan-down",
            "pan-y pan-y",
            "pinch-zoom pinch-zoom",
            "auto pan-x",
            "pan-y none",
            "manipulation pinch-zoom",
            "pan-x,pan-y",
            "pan-inline",
            "pan-x 1",
            "var(--action)",
        ] {
            assert_eq!(TouchAction::parse(text), None, "{text}");
        }
    }

    #[test]
    fn touch_action_directions_describe_scrolling_and_keep_zoom_separate() {
        let [x, y] = TouchAction::parse("pan-left pan-down").unwrap().axes();
        assert!(x.permits(-3.0) && !x.permits(3.0));
        assert!(y.permits(3.0) && !y.permits(-3.0));
        assert!(!y.permits(f32::NAN) && !y.permits(f32::INFINITY) && !y.permits(0.0));
        assert!(!TouchAction::parse("pan-x pan-y").unwrap().pinch_zoom());
        assert!(TouchAction::Manipulation.pinch_zoom());
        assert!(TouchAction::Auto.pinch_zoom());
        assert!(!TouchAction::None.pinch_zoom());
    }
}
