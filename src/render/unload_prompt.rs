//! The desktop's unload prompt: the user agent's question whether to leave
//! a page that asked for it (HTML #steps-to-fire-beforeunload). Like the
//! context menu it is browser chrome painted over the page; the message is
//! the user agent's own, never the page's.

use super::context_menu::{fill, label, paint_menu_panel};
use super::*;

const WIDTH: f32 = 372.0;
const HEIGHT: f32 = 148.0;
const BUTTON_WIDTH: f32 = 96.0;
const BUTTON_HEIGHT: f32 = 28.0;
/// The panel's distance below the top of the page.
const TOP: f32 = 56.0;
const MARGIN: f32 = 4.0;

const BUTTON: PaintColor = PaintColor::Rgba(31, 27, 48, 255);
const BUTTON_INSET: PaintColor = PaintColor::Rgba(5, 8, 19, 255);
const BUTTON_EDGE: PaintColor = PaintColor::Rgba(44, 67, 80, 255);
const MUTED: PaintColor = PaintColor::Rgba(147, 156, 179, 255);
const TEXT: PaintColor = theme_color(crate::theme::TEXT);

/// The prompt's answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnloadPromptButton {
    Leave,
    Stay,
}

impl UnloadPromptButton {
    pub fn label(self) -> &'static str {
        match self {
            Self::Leave => "Leave",
            Self::Stay => "Stay",
        }
    }

    /// The other button, which Tab and the arrow keys move to.
    pub fn other(self) -> Self {
        match self {
            Self::Leave => Self::Stay,
            Self::Stay => Self::Leave,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct UnloadPromptGeometry {
    pub panel: CssRect,
    pub leave: CssRect,
    pub stay: CssRect,
}

impl UnloadPromptGeometry {
    /// Centered near the top of `bounds`, the page's content viewport.
    pub fn new(bounds: CssRect) -> Self {
        let width = WIDTH.min((bounds.width - 2.0 * MARGIN).max(0.0));
        let height = HEIGHT.min((bounds.height - 2.0 * MARGIN).max(0.0));
        let x = (bounds.x + (bounds.width - width) / 2.0).round();
        let y = (bounds.y + TOP.min((bounds.height - height) / 2.0).max(MARGIN)).round();
        let panel = CssRect::new(x, y, width, height);
        let bottom = panel.y + panel.height - 14.0 - BUTTON_HEIGHT;
        let stay = CssRect::new(
            panel.x + panel.width - 14.0 - BUTTON_WIDTH,
            bottom,
            BUTTON_WIDTH,
            BUTTON_HEIGHT,
        );
        let leave = CssRect::new(
            stay.x - 10.0 - BUTTON_WIDTH,
            bottom,
            BUTTON_WIDTH,
            BUTTON_HEIGHT,
        );
        Self { panel, leave, stay }
    }

    pub fn button(&self, button: UnloadPromptButton) -> CssRect {
        match button {
            UnloadPromptButton::Leave => self.leave,
            UnloadPromptButton::Stay => self.stay,
        }
    }

    pub fn button_at(&self, point: CssPoint) -> Option<UnloadPromptButton> {
        [UnloadPromptButton::Leave, UnloadPromptButton::Stay]
            .into_iter()
            .find(|&button| self.button(button).contains(point))
    }

    /// The panel with its shadow: what showing, focusing or hiding it
    /// repaints.
    pub fn damage(&self) -> CssRect {
        expand_rect(self.panel, 14.0 * 2.0 + 6.0 + 2.0)
    }
}

/// The prompt's state as painted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UnloadPromptState {
    /// The button Enter answers with.
    pub focused: UnloadPromptButton,
    pub hovered: Option<UnloadPromptButton>,
    pub pressed: Option<UnloadPromptButton>,
}

/// Paint the prompt: the context menu's panel naming the page, the user
/// agent's question, and its two answers.
pub fn paint_unload_prompt(
    primitives: &mut Vec<Primitive>,
    geometry: &UnloadPromptGeometry,
    state: UnloadPromptState,
    title: &str,
) {
    let panel = geometry.panel;
    paint_menu_panel(primitives, panel, "LEAVE PAGE", title);
    let text = CssRect::new(panel.x + 18.0, panel.y + 38.0, panel.width - 36.0, 22.0);
    label(primitives, "Leave this page?", text, 15.0, TEXT, false);
    label(
        primitives,
        "Changes you made may not be saved.",
        CssRect::new(text.x, text.y + 24.0, text.width, 18.0),
        12.5,
        MUTED,
        false,
    );
    for button in [UnloadPromptButton::Leave, UnloadPromptButton::Stay] {
        let rect = geometry.button(button);
        let focused = state.focused == button;
        let lit = focused || state.hovered == Some(button);
        fill(
            primitives,
            rect,
            if state.pressed == Some(button) {
                BUTTON_INSET
            } else {
                BUTTON
            },
        );
        primitives.push(Primitive::Stroke {
            shape: PaintShape::Rect(rect),
            brush: PaintBrush::Solid(if focused { UI_CYAN } else { BUTTON_EDGE }),
            style: StrokeStyle::solid(1.0),
        });
        if lit {
            // Leaving ends the page: the amber accent the context menu
            // gives actions that end something.
            fill(
                primitives,
                CssRect::new(rect.x, rect.y, 2.0, rect.height),
                match button {
                    UnloadPromptButton::Leave => UI_AMBER,
                    UnloadPromptButton::Stay => UI_CYAN,
                },
            );
        }
        label(
            primitives,
            button.label(),
            rect,
            12.5,
            if lit { HEART_LIGHT } else { TEXT },
            true,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_prompt_sits_inside_the_page_with_its_buttons_inside_it() {
        for bounds in [
            CssRect::new(0.0, 26.0, 1280.0, 774.0),
            CssRect::new(0.0, 26.0, 300.0, 120.0),
        ] {
            let geometry = UnloadPromptGeometry::new(bounds);
            let panel = geometry.panel;
            assert!(panel.x >= bounds.x && panel.y >= bounds.y);
            assert!(panel.x + panel.width <= bounds.x + bounds.width);
            for button in [UnloadPromptButton::Leave, UnloadPromptButton::Stay] {
                let rect = geometry.button(button);
                assert_eq!(geometry.button_at(center(rect)), Some(button));
            }
            assert_eq!(
                geometry.button_at(CssPoint::new(panel.x + 2.0, panel.y + 2.0)),
                None
            );
            let damage = geometry.damage();
            assert!(damage.contains(CssPoint::new(panel.x, panel.y)));
        }
    }

    fn center(rect: CssRect) -> CssPoint {
        CssPoint::new(rect.x + rect.width / 2.0, rect.y + rect.height / 2.0)
    }

    #[test]
    fn painting_marks_the_focused_answer() {
        let geometry = UnloadPromptGeometry::new(CssRect::new(0.0, 26.0, 800.0, 600.0));
        let paint = |focused| {
            let mut primitives = Vec::new();
            paint_unload_prompt(
                &mut primitives,
                &geometry,
                UnloadPromptState {
                    focused,
                    hovered: None,
                    pressed: None,
                },
                "Example",
            );
            primitives
        };
        let leave = paint(UnloadPromptButton::Leave);
        let stay = paint(UnloadPromptButton::Stay);
        assert_ne!(leave, stay);
        let accent = |primitives: &[Primitive], button: UnloadPromptButton| {
            let rect = geometry.button(button);
            primitives.iter().any(|primitive| {
                matches!(primitive, Primitive::Stroke {
                    shape: PaintShape::Rect(stroked),
                    brush: PaintBrush::Solid(color),
                    ..
                } if *stroked == rect && *color == UI_CYAN)
            })
        };
        assert!(accent(&leave, UnloadPromptButton::Leave));
        assert!(!accent(&leave, UnloadPromptButton::Stay));
        assert!(accent(&stay, UnloadPromptButton::Stay));
    }
}
