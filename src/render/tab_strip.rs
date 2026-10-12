//! Desktop tab strip: the browser chrome that replaces the window's title bar
//! and presents the user agent's top-level traversables as tabs (HTML
//! #nav-traversal-ui: create, close and switch top-level traversables). Like
//! COMMAND and the context menu it is TRust's own chrome: one geometry is
//! shared by paint, pointer hit testing and native accessibility, and nothing
//! of it reaches a page's DOM or layout.

use super::*;

/// The strip's height: the system title bar it replaces. labwc's themes size
/// theirs to the window button height (26 px by default); Windows draws a
/// 32 px caption.
#[cfg(windows)]
pub const TAB_STRIP_HEIGHT: f32 = 32.0;
#[cfg(not(windows))]
pub const TAB_STRIP_HEIGHT: f32 = 26.0;

const STRIP: PaintColor = PaintColor::Rgba(7, 6, 18, 255);
const SURFACE: PaintColor = PaintColor::Rgba(12, 9, 28, 255);
const HOVER: PaintColor = PaintColor::Rgba(20, 16, 40, 255);
const PRESSED: PaintColor = PaintColor::Rgba(5, 8, 19, 255);
const EDGE: PaintColor = PaintColor::Rgba(44, 67, 80, 255);
const RULE: PaintColor = PaintColor::Rgba(36, 48, 66, 255);
const MUTED: PaintColor = PaintColor::Rgba(147, 156, 179, 255);
const DISABLED: PaintColor = PaintColor::Rgba(88, 92, 112, 255);
const TEXT: PaintColor = theme_color(crate::theme::TEXT);

#[cfg(windows)]
const BUTTON_WIDTH: f32 = 46.0;
#[cfg(not(windows))]
const BUTTON_WIDTH: f32 = 40.0;
const TAB_MAX_WIDTH: f32 = 224.0;
/// Narrower tabs scroll instead of shrinking further.
const TAB_MIN_WIDTH: f32 = 112.0;
const ARROW_WIDTH: f32 = 20.0;
/// Empty strip that always remains for moving the window.
const DRAG_RESERVE: f32 = 40.0;
const LEFT_PAD: f32 = 4.0;
/// Tabs start below this margin; the rest of the strip's top moves the window.
const TAB_TOP: f32 = 3.0;
const TITLE_SIZE: f32 = 11.5;
/// From this width every tab shows its close button; narrower inactive tabs
/// show it while hovered.
const CLOSE_ALWAYS_WIDTH: f32 = 140.0;

/// A tab as the strip presents it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TabStripTab {
    /// The page title, or its address when it has none.
    pub title: String,
    /// A Telnet session: a prompt glyph instead of the page jewel.
    pub terminal: bool,
    pub loading: bool,
    /// Its page is playing sound.
    pub audible: bool,
    /// Released from memory until shown again (Page Lifecycle "discarded").
    pub asleep: bool,
}

/// One interactive part of the strip.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TabStripPart {
    Tab(usize),
    CloseTab(usize),
    NewTab,
    ScrollBack,
    ScrollForward,
    Minimize,
    Maximize,
    Close,
    /// Strip without a control: dragging it moves the window.
    Empty,
}

/// Where the strip's parts are for a window `width` wide.
#[derive(Clone, Debug, PartialEq)]
pub struct TabStripGeometry {
    pub strip: CssRect,
    /// The tabs show inside this rectangle; the rest are scrolled away.
    pub viewport: CssRect,
    /// One rectangle per tab, in strip order, at the current scroll.
    pub tabs: Vec<CssRect>,
    pub new_tab: CssRect,
    /// Only while the tabs overflow their viewport.
    pub scroll_back: Option<CssRect>,
    pub scroll_forward: Option<CssRect>,
    pub minimize: CssRect,
    pub maximize: CssRect,
    pub close: CssRect,
    pub scroll: f32,
    pub max_scroll: f32,
}

impl TabStripGeometry {
    /// Lay out `count` tabs: as wide as fits up to their maximum, then at
    /// their minimum width scrolled by `scroll`, with the scroll arrows at
    /// the far right. The window buttons stay at the right edge and some
    /// empty strip always remains to move the window by.
    pub fn new(width: f32, count: usize, scroll: f32) -> Self {
        let height = TAB_STRIP_HEIGHT;
        let width = width.max(0.0);
        let strip = CssRect::new(0.0, 0.0, width, height);
        let close = CssRect::new((width - BUTTON_WIDTH).max(0.0), 0.0, BUTTON_WIDTH, height);
        let maximize = CssRect::new((close.x - BUTTON_WIDTH).max(0.0), 0.0, BUTTON_WIDTH, height);
        let minimize = CssRect::new(
            (maximize.x - BUTTON_WIDTH).max(0.0),
            0.0,
            BUTTON_WIDTH,
            height,
        );
        let controls_x = minimize.x;
        let new_tab_width = height;
        let room = (controls_x - LEFT_PAD - new_tab_width - DRAG_RESERVE).max(0.0);
        let count_f = count as f32;
        let overflow = count > 0 && count_f * TAB_MIN_WIDTH > room;
        let (tab_width, viewport_width, arrows) = if overflow {
            let arrows = (room - 2.0 * ARROW_WIDTH).max(0.0);
            (TAB_MIN_WIDTH, arrows, true)
        } else if count > 0 {
            let tab = (room / count_f).clamp(TAB_MIN_WIDTH.min(room), TAB_MAX_WIDTH);
            (tab, tab * count_f, false)
        } else {
            (TAB_MAX_WIDTH, 0.0, false)
        };
        let viewport = CssRect::new(LEFT_PAD, TAB_TOP, viewport_width, height - TAB_TOP);
        let max_scroll = if overflow {
            (tab_width * count_f - viewport_width).max(0.0)
        } else {
            0.0
        };
        let scroll = if scroll.is_finite() {
            scroll.clamp(0.0, max_scroll).round()
        } else {
            0.0
        };
        let tabs = (0..count)
            .map(|index| {
                CssRect::new(
                    viewport.x + index as f32 * tab_width - scroll,
                    TAB_TOP,
                    tab_width,
                    height - TAB_TOP,
                )
            })
            .collect();
        let new_tab = CssRect::new(viewport.x + viewport.width, 0.0, new_tab_width, height);
        let (scroll_back, scroll_forward) = if arrows {
            let forward = CssRect::new(controls_x - ARROW_WIDTH, 0.0, ARROW_WIDTH, height);
            let back = CssRect::new(forward.x - ARROW_WIDTH, 0.0, ARROW_WIDTH, height);
            (Some(back), Some(forward))
        } else {
            (None, None)
        };
        Self {
            strip,
            viewport,
            tabs,
            new_tab,
            scroll_back,
            scroll_forward,
            minimize,
            maximize,
            close,
            scroll,
            max_scroll,
        }
    }

    /// Whether the tabs are wider than their viewport (the arrows show).
    pub fn overflows(&self) -> bool {
        self.scroll_back.is_some()
    }

    /// The visible part of tab `index`, if any of it shows.
    pub fn visible_tab(&self, index: usize) -> Option<CssRect> {
        let rect = *self.tabs.get(index)?;
        let left = rect.x.max(self.viewport.x);
        let right = (rect.x + rect.width).min(self.viewport.x + self.viewport.width);
        (right > left).then(|| CssRect::new(left, rect.y, right - left, rect.height))
    }

    /// Tab `index`'s close button.
    pub fn close_button(&self, index: usize) -> Option<CssRect> {
        let tab = *self.tabs.get(index)?;
        let size = 16.0;
        Some(CssRect::new(
            tab.x + tab.width - size - 6.0,
            tab.y + ((tab.height - size) / 2.0).round(),
            size,
            size,
        ))
    }

    /// The part under `point`; `None` outside the strip.
    pub fn part_at(&self, point: CssPoint) -> Option<TabStripPart> {
        if !self.strip.contains(point) {
            return None;
        }
        for (rect, part) in [
            (Some(self.close), TabStripPart::Close),
            (Some(self.maximize), TabStripPart::Maximize),
            (Some(self.minimize), TabStripPart::Minimize),
            (self.scroll_forward, TabStripPart::ScrollForward),
            (self.scroll_back, TabStripPart::ScrollBack),
            (Some(self.new_tab), TabStripPart::NewTab),
        ] {
            if rect.is_some_and(|rect| rect.contains(point)) {
                return Some(part);
            }
        }
        if self.viewport.contains(point) {
            let index = self.tabs.iter().position(|tab| tab.contains(point));
            if let Some(index) = index {
                if self
                    .close_button(index)
                    .is_some_and(|close| close.contains(point))
                {
                    return Some(TabStripPart::CloseTab(index));
                }
                return Some(TabStripPart::Tab(index));
            }
        }
        Some(TabStripPart::Empty)
    }

    /// The scroll offset that shows all of tab `index`, changing `scroll`
    /// as little as possible.
    pub fn reveal(&self, index: usize) -> f32 {
        let Some(tab) = self.tabs.get(index) else {
            return self.scroll;
        };
        let start = tab.x + self.scroll - self.viewport.x;
        let end = start + tab.width;
        let scroll = if start < self.scroll {
            start
        } else if end > self.scroll + self.viewport.width {
            end - self.viewport.width
        } else {
            self.scroll
        };
        scroll.clamp(0.0, self.max_scroll)
    }

    /// The scroll offset one arrow press moves to: by about one tab, toward
    /// the start (`back`) or the end.
    pub fn step(&self, back: bool) -> f32 {
        let tab = self.tabs.first().map_or(TAB_MIN_WIDTH, |tab| tab.width);
        let scroll = if back {
            self.scroll - tab
        } else {
            self.scroll + tab
        };
        scroll.clamp(0.0, self.max_scroll)
    }

    /// The strip index a tab dragged so that its center is at `x` moves to.
    pub fn drop_index(&self, x: f32) -> usize {
        let Some(first) = self.tabs.first() else {
            return 0;
        };
        let offset = x - first.x;
        ((offset / first.width).floor().max(0.0) as usize).min(self.tabs.len() - 1)
    }

    /// What painting the strip covers.
    pub fn damage(&self) -> CssRect {
        self.strip
    }
}

/// The strip's interaction state.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TabStripState {
    /// The tab shown in the window.
    pub active: usize,
    pub hovered: Option<TabStripPart>,
    pub pressed: Option<TabStripPart>,
    pub maximized: bool,
    /// The window has the keyboard focus; the strip dims without it.
    pub window_focused: bool,
    /// The loading jewels' heartbeat, 0 to 1.
    pub pulse: f32,
    /// The tab being dragged to a new position, and how far its paint is
    /// from its slot.
    pub dragging: Option<(usize, f32)>,
}

/// Paint the strip: its tabs with their titles and states, the new-tab
/// button, the scroll arrows while the tabs overflow, and the window
/// buttons, in COMMAND's surfaces, etched edges and accents.
pub fn paint_tab_strip(
    primitives: &mut Vec<Primitive>,
    geometry: &TabStripGeometry,
    tabs: &[TabStripTab],
    state: &TabStripState,
) {
    let strip = geometry.strip;
    if strip.width <= 0.0 {
        return;
    }
    let focused = state.window_focused;
    let accent = |color: PaintColor| if focused { color } else { UI_DIM };
    fill(primitives, strip, STRIP);
    // The strip's lower edge separates the chrome from the page; the shown
    // tab opens through it.
    fill(
        primitives,
        CssRect::new(strip.x, strip.y + strip.height - 1.0, strip.width, 1.0),
        EDGE,
    );
    fill(
        primitives,
        CssRect::new(
            (geometry.minimize.x - 132.0).max(0.0),
            strip.y + strip.height - 1.0,
            108.0,
            1.0,
        ),
        accent(UI_CYAN),
    );

    let clip = PaintShape::Rect(geometry.viewport);
    primitives.push(Primitive::PushClip(clip));
    let dragged = state.dragging.map(|(index, _)| index);
    for (index, tab) in tabs.iter().enumerate() {
        if Some(index) == dragged || index == state.active {
            continue;
        }
        let Some(rect) = geometry.tabs.get(index).copied() else {
            continue;
        };
        if geometry.visible_tab(index).is_none() {
            continue;
        }
        let hovered = matches!(
            state.hovered,
            Some(TabStripPart::Tab(i) | TabStripPart::CloseTab(i)) if i == index
        );
        if hovered {
            fill_tab_shape(primitives, rect, HOVER);
            fill(
                primitives,
                CssRect::new(rect.x + 4.0, rect.y, (rect.width * 0.3).min(40.0), 1.0),
                accent(UI_CYAN),
            );
        } else if index + 1 != state.active && index + 1 < tabs.len() {
            // A rule between two inactive neighbors.
            fill(
                primitives,
                CssRect::new(
                    rect.x + rect.width - 0.5,
                    rect.y + 6.0,
                    1.0,
                    rect.height - 11.0,
                ),
                RULE,
            );
        }
        paint_tab_contents(
            primitives, geometry, index, rect, tab, state, false, hovered,
        );
    }
    for index in [Some(state.active), dragged].into_iter().flatten() {
        let (Some(tab), Some(slot)) = (tabs.get(index), geometry.tabs.get(index).copied()) else {
            continue;
        };
        let offset = state
            .dragging
            .filter(|(dragging, _)| *dragging == index)
            .map_or(0.0, |(_, offset)| offset);
        let rect = CssRect::new(slot.x + offset, slot.y, slot.width, slot.height);
        let active = index == state.active;
        let outline = tab_outline(rect);
        primitives.push(Primitive::Fill {
            shape: outline.clone(),
            brush: PaintBrush::Solid(if active { SURFACE } else { HOVER }),
        });
        primitives.push(Primitive::Stroke {
            shape: tab_edge(rect),
            brush: PaintBrush::Solid(EDGE),
            style: StrokeStyle::solid(1.0),
        });
        if active {
            fill(
                primitives,
                CssRect::new(rect.x + 4.0, rect.y, (rect.width * 0.4).min(64.0), 2.0),
                accent(UI_PINK),
            );
        }
        let hovered = matches!(
            state.hovered,
            Some(TabStripPart::Tab(i) | TabStripPart::CloseTab(i)) if i == index
        );
        paint_tab_contents(
            primitives, geometry, index, rect, tab, state, active, hovered,
        );
    }
    primitives.push(Primitive::PopClip);

    paint_button(
        primitives,
        geometry.new_tab,
        state,
        TabStripPart::NewTab,
        false,
        |primitives, center, color| {
            stroke(
                primitives,
                vec![
                    PathElement::MoveTo(CssPoint::new(center.x - 5.0, center.y)),
                    PathElement::LineTo(CssPoint::new(center.x + 5.0, center.y)),
                    PathElement::MoveTo(CssPoint::new(center.x, center.y - 5.0)),
                    PathElement::LineTo(CssPoint::new(center.x, center.y + 5.0)),
                ],
                color,
                1.5,
            );
        },
        accent(UI_CYAN),
    );
    if let (Some(back), Some(forward)) = (geometry.scroll_back, geometry.scroll_forward) {
        for (rect, part, enabled, sign) in [
            (back, TabStripPart::ScrollBack, geometry.scroll > 0.0, -1.0),
            (
                forward,
                TabStripPart::ScrollForward,
                geometry.scroll < geometry.max_scroll,
                1.0,
            ),
        ] {
            paint_button(
                primitives,
                rect,
                state,
                part,
                false,
                |primitives, center, color| {
                    stroke(
                        primitives,
                        vec![
                            PathElement::MoveTo(CssPoint::new(
                                center.x - 2.5 * sign,
                                center.y - 5.0,
                            )),
                            PathElement::LineTo(CssPoint::new(center.x + 2.5 * sign, center.y)),
                            PathElement::LineTo(CssPoint::new(
                                center.x - 2.5 * sign,
                                center.y + 5.0,
                            )),
                        ],
                        color,
                        1.5,
                    );
                },
                if enabled { accent(UI_CYAN) } else { DISABLED },
            );
        }
    }
    // Decorative traces in the empty strip, as on COMMAND's frame.
    let traces_end = geometry
        .scroll_back
        .map_or(geometry.minimize.x, |back| back.x)
        - 10.0;
    if traces_end - (geometry.new_tab.x + geometry.new_tab.width) > 48.0 {
        let mut traces = Vec::new();
        for i in 0..3 {
            let x = traces_end - 24.0 + i as f32 * 8.0;
            traces.extend([
                PathElement::MoveTo(CssPoint::new(x, strip.y + 8.0)),
                PathElement::LineTo(CssPoint::new(x + 8.0, strip.y + 16.0)),
            ]);
        }
        primitives.push(Primitive::Stroke {
            shape: PaintShape::Path(traces),
            brush: PaintBrush::Solid(EDGE),
            style: StrokeStyle::solid(2.0),
        });
    }

    let text = if focused { TEXT } else { MUTED };
    paint_button(
        primitives,
        geometry.minimize,
        state,
        TabStripPart::Minimize,
        false,
        |primitives, center, color| {
            stroke(
                primitives,
                vec![
                    PathElement::MoveTo(CssPoint::new(center.x - 5.0, center.y + 0.5)),
                    PathElement::LineTo(CssPoint::new(center.x + 5.0, center.y + 0.5)),
                ],
                color,
                1.25,
            );
        },
        text,
    );
    let maximized = state.maximized;
    paint_button(
        primitives,
        geometry.maximize,
        state,
        TabStripPart::Maximize,
        false,
        |primitives, center, color| {
            if maximized {
                // Restore: the window behind the window.
                stroke(
                    primitives,
                    vec![
                        PathElement::MoveTo(CssPoint::new(center.x - 2.5, center.y - 2.5)),
                        PathElement::LineTo(CssPoint::new(center.x - 2.5, center.y - 4.5)),
                        PathElement::LineTo(CssPoint::new(center.x + 4.5, center.y - 4.5)),
                        PathElement::LineTo(CssPoint::new(center.x + 4.5, center.y + 2.5)),
                        PathElement::LineTo(CssPoint::new(center.x + 2.5, center.y + 2.5)),
                    ],
                    color,
                    1.25,
                );
                stroke(
                    primitives,
                    rect_path(CssRect::new(center.x - 4.5, center.y - 2.5, 7.0, 7.0)),
                    color,
                    1.25,
                );
            } else {
                stroke(
                    primitives,
                    rect_path(CssRect::new(center.x - 4.5, center.y - 4.5, 9.0, 9.0)),
                    color,
                    1.25,
                );
            }
        },
        text,
    );
    paint_button(
        primitives,
        geometry.close,
        state,
        TabStripPart::Close,
        true,
        |primitives, center, color| cross(primitives, center, 4.5, color, 1.25),
        text,
    );
}

/// The tab's shape: COMMAND's chamfered corners, small at the top left and
/// larger at the top right, open at the bottom where the page begins.
fn tab_outline(rect: CssRect) -> PaintShape {
    let (right, bottom) = (rect.x + rect.width, rect.y + rect.height);
    PaintShape::Path(vec![
        PathElement::MoveTo(CssPoint::new(rect.x, bottom)),
        PathElement::LineTo(CssPoint::new(rect.x, rect.y + 4.0)),
        PathElement::LineTo(CssPoint::new(rect.x + 4.0, rect.y)),
        PathElement::LineTo(CssPoint::new(right - 9.0, rect.y)),
        PathElement::LineTo(CssPoint::new(right, rect.y + 9.0)),
        PathElement::LineTo(CssPoint::new(right, bottom)),
        PathElement::Close,
    ])
}

/// The tab outline's etched edge, without the open bottom.
fn tab_edge(rect: CssRect) -> PaintShape {
    let (right, bottom) = (rect.x + rect.width - 0.5, rect.y + rect.height);
    let left = rect.x + 0.5;
    let top = rect.y + 0.5;
    PaintShape::Path(vec![
        PathElement::MoveTo(CssPoint::new(left, bottom)),
        PathElement::LineTo(CssPoint::new(left, top + 4.0)),
        PathElement::LineTo(CssPoint::new(left + 4.0, top)),
        PathElement::LineTo(CssPoint::new(right - 9.0, top)),
        PathElement::LineTo(CssPoint::new(right, top + 9.0)),
        PathElement::LineTo(CssPoint::new(right, bottom)),
    ])
}

fn fill_tab_shape(primitives: &mut Vec<Primitive>, rect: CssRect, color: PaintColor) {
    primitives.push(Primitive::Fill {
        shape: tab_outline(rect),
        brush: PaintBrush::Solid(color),
    });
}

#[allow(clippy::too_many_arguments)]
fn paint_tab_contents(
    primitives: &mut Vec<Primitive>,
    geometry: &TabStripGeometry,
    index: usize,
    rect: CssRect,
    tab: &TabStripTab,
    state: &TabStripState,
    active: bool,
    hovered: bool,
) {
    let focused = state.window_focused;
    let center_y = rect.y + rect.height / 2.0;
    // The jewel: the page's state, as COMMAND's status light shows it.
    let jewel = CssRect::new(rect.x + 11.0, (center_y - 2.5).round(), 5.0, 5.0);
    if tab.terminal {
        let at = |x: f32, y: f32| CssPoint::new(rect.x + 9.0 + x, center_y - 5.0 + y);
        stroke(
            primitives,
            vec![
                PathElement::MoveTo(at(0.5, 1.5)),
                PathElement::LineTo(at(4.0, 4.5)),
                PathElement::LineTo(at(0.5, 7.5)),
                PathElement::MoveTo(at(5.0, 9.0)),
                PathElement::LineTo(at(9.5, 9.0)),
            ],
            if tab.loading {
                UI_AMBER
            } else if focused {
                UI_CYAN
            } else {
                UI_DIM
            },
            1.25,
        );
    } else if tab.asleep {
        primitives.push(Primitive::Stroke {
            shape: PaintShape::Rect(CssRect::new(
                jewel.x + 0.5,
                jewel.y + 0.5,
                jewel.width - 1.0,
                jewel.height - 1.0,
            )),
            brush: PaintBrush::Solid(DISABLED),
            style: StrokeStyle::solid(1.0),
        });
    } else if tab.loading {
        let alpha = (110.0 + 145.0 * state.pulse.clamp(0.0, 1.0)) as u8;
        let PaintColor::Rgba(r, g, b, _) = UI_AMBER else {
            unreachable!()
        };
        fill(primitives, jewel, PaintColor::Rgba(r, g, b, alpha));
    } else {
        fill(
            primitives,
            jewel,
            if active && focused { UI_CYAN } else { UI_DIM },
        );
    }

    let show_close = active || hovered || rect.width >= CLOSE_ALWAYS_WIDTH;
    let close = geometry.close_button(index).map(|close| {
        CssRect::new(
            close.x + (rect.x - geometry.tabs[index].x),
            close.y,
            close.width,
            close.height,
        )
    });
    let mut title_right = rect.x + rect.width - 8.0;
    if show_close && let Some(close) = close {
        title_right = close.x - 3.0;
        let close_hovered = state.hovered == Some(TabStripPart::CloseTab(index));
        let pressed = state.pressed == Some(TabStripPart::CloseTab(index));
        if close_hovered || pressed {
            fill(primitives, close, if pressed { PRESSED } else { UI_PINK });
        }
        cross(
            primitives,
            CssPoint::new(close.x + close.width / 2.0, close.y + close.height / 2.0),
            3.5,
            if close_hovered || pressed {
                HEART_LIGHT
            } else if active && focused {
                MUTED
            } else {
                DISABLED
            },
            1.25,
        );
    }
    if tab.audible {
        let size = 12.0;
        let x = title_right - size;
        speaker(
            primitives,
            CssRect::new(x, (center_y - size / 2.0).round(), size, size),
            if focused { UI_CYAN } else { UI_DIM },
        );
        title_right = x - 4.0;
    }
    let title_left = rect.x + 23.0;
    let color = match (tab.asleep, active, focused) {
        (true, ..) => DISABLED,
        (false, true, true) => TEXT,
        (false, true, false) | (false, false, true) => MUTED,
        (false, false, false) => DISABLED,
    };
    fitted_label(
        primitives,
        &tab.title,
        CssRect::new(
            title_left,
            rect.y,
            (title_right - title_left).max(0.0),
            rect.height,
        ),
        TITLE_SIZE,
        color,
    );
}

#[allow(clippy::too_many_arguments)]
fn paint_button(
    primitives: &mut Vec<Primitive>,
    rect: CssRect,
    state: &TabStripState,
    part: TabStripPart,
    danger: bool,
    icon: impl FnOnce(&mut Vec<Primitive>, CssPoint, PaintColor),
    color: PaintColor,
) {
    let hovered = state.hovered == Some(part);
    let pressed = state.pressed == Some(part);
    let color = if pressed && danger {
        fill(primitives, rect, PRESSED);
        UI_PINK
    } else if hovered && danger {
        fill(primitives, rect, UI_PINK);
        HEART_LIGHT
    } else if pressed {
        fill(primitives, rect, PRESSED);
        color
    } else if hovered {
        fill(primitives, rect, HOVER);
        color
    } else {
        color
    };
    icon(
        primitives,
        CssPoint::new(
            (rect.x + rect.width / 2.0).round(),
            (rect.y + rect.height / 2.0).round(),
        ),
        color,
    );
}

fn speaker(primitives: &mut Vec<Primitive>, rect: CssRect, color: PaintColor) {
    let at = |x: f32, y: f32| CssPoint::new(rect.x + x, rect.y + y);
    primitives.push(Primitive::Fill {
        shape: PaintShape::Path(vec![
            PathElement::MoveTo(at(1.0, 4.0)),
            PathElement::LineTo(at(3.5, 4.0)),
            PathElement::LineTo(at(6.5, 1.0)),
            PathElement::LineTo(at(6.5, 11.0)),
            PathElement::LineTo(at(3.5, 8.0)),
            PathElement::LineTo(at(1.0, 8.0)),
            PathElement::Close,
        ]),
        brush: PaintBrush::Solid(color),
    });
    stroke(
        primitives,
        vec![
            PathElement::MoveTo(at(8.5, 4.0)),
            PathElement::CurveTo(at(9.5, 5.0), at(9.5, 7.0), at(8.5, 8.0)),
            PathElement::MoveTo(at(10.0, 2.25)),
            PathElement::CurveTo(at(12.25, 4.5), at(12.25, 7.5), at(10.0, 9.75)),
        ],
        color,
        1.2,
    );
}

fn cross(
    primitives: &mut Vec<Primitive>,
    center: CssPoint,
    half: f32,
    color: PaintColor,
    width: f32,
) {
    stroke(
        primitives,
        vec![
            PathElement::MoveTo(CssPoint::new(center.x - half, center.y - half)),
            PathElement::LineTo(CssPoint::new(center.x + half, center.y + half)),
            PathElement::MoveTo(CssPoint::new(center.x + half, center.y - half)),
            PathElement::LineTo(CssPoint::new(center.x - half, center.y + half)),
        ],
        color,
        width,
    );
}

fn rect_path(rect: CssRect) -> Vec<PathElement> {
    vec![
        PathElement::MoveTo(CssPoint::new(rect.x, rect.y)),
        PathElement::LineTo(CssPoint::new(rect.x + rect.width, rect.y)),
        PathElement::LineTo(CssPoint::new(rect.x + rect.width, rect.y + rect.height)),
        PathElement::LineTo(CssPoint::new(rect.x, rect.y + rect.height)),
        PathElement::Close,
    ]
}

fn stroke(primitives: &mut Vec<Primitive>, path: Vec<PathElement>, color: PaintColor, width: f32) {
    primitives.push(Primitive::Stroke {
        shape: PaintShape::Path(path),
        brush: PaintBrush::Solid(color),
        style: StrokeStyle {
            cap: LineCap::Round,
            ..StrokeStyle::solid(width)
        },
    });
}

fn fill(primitives: &mut Vec<Primitive>, rect: CssRect, color: PaintColor) {
    if rect.width > 0.0 && rect.height > 0.0 {
        primitives.push(Primitive::FillRect { rect, color });
    }
}

/// `text` on one line in `rect`, vertically centered; a title wider than
/// the rectangle ends in an ellipsis after the longest prefix of whole
/// clusters, in logical order, that fits beside it.
fn fitted_label(
    primitives: &mut Vec<Primitive>,
    text: &str,
    rect: CssRect,
    size: f32,
    color: PaintColor,
) {
    if rect.width <= 0.0 || text.is_empty() {
        return;
    }
    let style = command_text_style_with_size(size);
    let shaped = fit_text(text, &style, rect.width);
    let origin = CssPoint::new(
        rect.x,
        (rect.y + (rect.height - shaped.line_height) / 2.0).round(),
    );
    primitives.push(Primitive::GlyphRun {
        origin,
        shaped,
        color,
        decoration: TextDecorationPaint {
            color,
            style: DecorationStyle::Solid,
            thickness: None,
            underline_offset: None,
        },
        shadows: Vec::new(),
        clip: Some(rect),
        node: 0,
        link: None,
        selection: Default::default(),
    });
}

/// Shape `text` to fit `width`, ellipsizing it when it is wider. Titles are
/// repainted with every frame, so the last few fittings are remembered.
fn fit_text(text: &str, style: &crate::text::TextStyle, width: f32) -> crate::text::ShapedText {
    type Key = (String, u32, u32);
    thread_local! {
        static FITTED: std::cell::RefCell<VecDeque<(Key, crate::text::ShapedText)>> =
            const { std::cell::RefCell::new(VecDeque::new()) };
    }
    const REMEMBERED: usize = 64;
    let key = (text.to_owned(), style.size.to_bits(), width.to_bits());
    if let Some(shaped) = FITTED.with_borrow(|fitted| {
        fitted
            .iter()
            .find(|(cached, _)| *cached == key)
            .map(|(_, shaped)| shaped.clone())
    }) {
        return shaped;
    }
    // A single line: break characters would start another.
    let line: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let full = crate::text::shape(&line, style);
    let shaped = if full.advance <= width {
        full
    } else {
        const ELLIPSIS: &str = "\u{2026}";
        let room = width - crate::text::shape(ELLIPSIS, style).advance;
        let mut clusters: Vec<&crate::text::Cluster> = full.clusters.iter().collect();
        clusters.sort_by_key(|cluster| cluster.text_range.start);
        let mut used = 0.0;
        let mut end = 0;
        for cluster in clusters {
            if used + cluster.advance > room {
                break;
            }
            used += cluster.advance;
            end = cluster.text_range.end;
        }
        let mut prefix = line[..end].trim_end().to_owned();
        prefix.push_str(ELLIPSIS);
        crate::text::shape(&prefix, style)
    };
    FITTED.with_borrow_mut(|fitted| {
        if fitted.len() == REMEMBERED {
            fitted.pop_front();
        }
        fitted.push_back((key, shaped.clone()));
    });
    shaped
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tabs(count: usize) -> Vec<TabStripTab> {
        (0..count)
            .map(|index| TabStripTab {
                title: format!("Tab number {index}"),
                ..Default::default()
            })
            .collect()
    }

    #[test]
    fn tabs_share_the_strip_until_they_reach_their_minimum_width() {
        let geometry = TabStripGeometry::new(1280.0, 3, 0.0);
        assert!(!geometry.overflows());
        assert_eq!(geometry.tabs[0].width, TAB_MAX_WIDTH);
        assert_eq!(geometry.tabs[1].x, geometry.tabs[0].x + TAB_MAX_WIDTH);
        assert_eq!(
            geometry.new_tab.x,
            geometry.tabs[2].x + geometry.tabs[2].width,
            "the new-tab button follows the last tab"
        );
        assert_eq!(geometry.close.x + geometry.close.width, 1280.0);
        let many = TabStripGeometry::new(1280.0, 9, 0.0);
        assert!(!many.overflows());
        assert!(many.tabs[0].width < TAB_MAX_WIDTH && many.tabs[0].width >= TAB_MIN_WIDTH);
    }

    #[test]
    fn arrows_appear_only_when_the_tabs_overflow() {
        let fits = TabStripGeometry::new(1280.0, 8, 0.0);
        assert!(fits.scroll_back.is_none() && fits.scroll_forward.is_none());
        let overflow = TabStripGeometry::new(1280.0, 30, 0.0);
        assert!(overflow.overflows());
        let (back, forward) = (
            overflow.scroll_back.unwrap(),
            overflow.scroll_forward.unwrap(),
        );
        assert_eq!(
            forward.x + forward.width,
            overflow.minimize.x,
            "the arrows sit at the far right, beside the window buttons"
        );
        assert!(back.x < forward.x);
        assert!(overflow.viewport.x + overflow.viewport.width <= overflow.new_tab.x);
        assert!(
            overflow.new_tab.x + overflow.new_tab.width + DRAG_RESERVE <= back.x + 0.01,
            "some empty strip always remains to move the window by"
        );
        assert_eq!(overflow.tabs[0].width, TAB_MIN_WIDTH);
        assert!(overflow.max_scroll > 0.0);
    }

    #[test]
    fn scrolling_reveals_tabs_and_clamps() {
        let geometry = TabStripGeometry::new(900.0, 40, 0.0);
        let last = geometry.tabs.len() - 1;
        let scroll = geometry.reveal(last);
        assert_eq!(scroll, geometry.max_scroll);
        let scrolled = TabStripGeometry::new(900.0, 40, scroll);
        let visible = scrolled.visible_tab(last).unwrap();
        assert_eq!(visible.width, TAB_MIN_WIDTH, "the whole tab shows");
        assert!(scrolled.visible_tab(0).is_none());
        assert_eq!(
            scrolled.reveal(last),
            scroll,
            "a visible tab needs no scroll"
        );
        assert_eq!(scrolled.reveal(0), 0.0);
        assert_eq!(
            TabStripGeometry::new(900.0, 40, 1e9).scroll,
            geometry.max_scroll
        );
        assert_eq!(TabStripGeometry::new(900.0, 40, f32::NAN).scroll, 0.0);
        assert!(geometry.step(false) > 0.0 && geometry.step(true) == 0.0);
    }

    #[test]
    fn hit_testing_finds_each_part() {
        let geometry = TabStripGeometry::new(1280.0, 3, 0.0);
        let center =
            |rect: CssRect| CssPoint::new(rect.x + rect.width / 2.0, rect.y + rect.height / 2.0);
        assert_eq!(
            geometry.part_at(center(geometry.tabs[1])),
            Some(TabStripPart::Tab(1))
        );
        assert_eq!(
            geometry.part_at(center(geometry.close_button(2).unwrap())),
            Some(TabStripPart::CloseTab(2))
        );
        assert_eq!(
            geometry.part_at(center(geometry.new_tab)),
            Some(TabStripPart::NewTab)
        );
        assert_eq!(
            geometry.part_at(center(geometry.minimize)),
            Some(TabStripPart::Minimize)
        );
        assert_eq!(
            geometry.part_at(center(geometry.maximize)),
            Some(TabStripPart::Maximize)
        );
        assert_eq!(
            geometry.part_at(center(geometry.close)),
            Some(TabStripPart::Close)
        );
        assert_eq!(
            geometry.part_at(CssPoint::new(geometry.new_tab.x + 80.0, 10.0)),
            Some(TabStripPart::Empty)
        );
        assert_eq!(
            geometry.part_at(CssPoint::new(geometry.tabs[0].x + 30.0, 1.0)),
            Some(TabStripPart::Empty),
            "the margin above the tabs moves the window"
        );
        assert_eq!(
            geometry.part_at(CssPoint::new(10.0, TAB_STRIP_HEIGHT + 1.0)),
            None
        );
        let overflow = TabStripGeometry::new(900.0, 40, 0.0);
        let hidden = overflow.tabs[39];
        assert_ne!(
            overflow.part_at(center(hidden)),
            Some(TabStripPart::Tab(39)),
            "a scrolled-away tab cannot be hit"
        );
        assert_eq!(overflow.drop_index(overflow.tabs[3].x + 5.0), 3);
        assert_eq!(overflow.drop_index(-1e6), 0);
        assert_eq!(overflow.drop_index(1e6), 39);
    }

    #[test]
    fn long_titles_end_in_an_ellipsis_that_fits() {
        let style = command_text_style_with_size(TITLE_SIZE);
        let short = fit_text("Short", &style, 400.0);
        assert_eq!(short.text, "Short");
        let long = fit_text(
            "A very long page title that cannot possibly fit into a narrow tab",
            &style,
            90.0,
        );
        assert!(long.text.ends_with('\u{2026}'), "{}", long.text);
        assert!(long.advance <= 90.0 + 0.5);
        let control = fit_text("line\nbreak", &style, 400.0);
        assert_eq!(control.text, "line break");
    }

    #[test]
    fn painting_stays_inside_the_strip() {
        let geometry = TabStripGeometry::new(1024.0, 12, 30.0);
        let list = tabs(12);
        let mut primitives = Vec::new();
        paint_tab_strip(
            &mut primitives,
            &geometry,
            &list,
            &TabStripState {
                active: 2,
                hovered: Some(TabStripPart::CloseTab(3)),
                window_focused: true,
                ..Default::default()
            },
        );
        let damage = geometry.damage();
        let mut clip: Option<CssRect> = None;
        for primitive in &primitives {
            match primitive {
                Primitive::PushClip(PaintShape::Rect(rect)) => {
                    assert!(clip.is_none(), "one clip scope");
                    clip = Some(*rect);
                }
                Primitive::PopClip => clip = None,
                _ => {}
            }
            if let Primitive::FillRect { rect, .. } = primitive {
                // Inside the tabs' clip only the visible part paints.
                let rect = match clip {
                    Some(clip) => {
                        let left = rect.x.max(clip.x);
                        let right = (rect.x + rect.width).min(clip.x + clip.width);
                        if right <= left {
                            continue;
                        }
                        CssRect::new(left, rect.y, right - left, rect.height)
                    }
                    None => *rect,
                };
                assert!(
                    rect.x >= damage.x - 0.01
                        && rect.y >= damage.y - 0.01
                        && rect.x + rect.width <= damage.x + damage.width + 0.01
                        && rect.y + rect.height <= damage.y + damage.height + 0.01,
                    "{rect:?} outside {damage:?}"
                );
            }
        }
        let pushes = primitives
            .iter()
            .filter(|p| matches!(p, Primitive::PushClip(_)))
            .count();
        let pops = primitives
            .iter()
            .filter(|p| matches!(p, Primitive::PopClip))
            .count();
        assert_eq!(pushes, pops, "balanced clip scopes");
    }

    /// `TRUST_TAB_STRIP_PREVIEW=DIR cargo test --lib tab_strip_preview -- --ignored`
    /// writes strips as the CPU reference renderer paints them.
    #[test]
    #[ignore = "writes preview images; set TRUST_TAB_STRIP_PREVIEW"]
    fn tab_strip_preview() {
        let Some(dir) = std::env::var_os("TRUST_TAB_STRIP_PREVIEW") else {
            return;
        };
        let dir = std::path::PathBuf::from(dir);
        let titles = [
            (
                "Ruby's House — strawberries and tea",
                false,
                false,
                false,
                false,
            ),
            (
                "https://rubymaelstrom.com/blog/a-very-long-address-that-goes-on",
                false,
                true,
                false,
                false,
            ),
            ("telnet://bbs.example.org:23", true, false, false, false),
            ("Radio Paradise — Main Mix", false, false, true, false),
            ("Sleeping page", false, false, false, true),
        ];
        for (name, width, count, active, hovered, focused, maximized, scroll) in [
            (
                "few",
                1100.0,
                5,
                0,
                Some(TabStripPart::Tab(1)),
                true,
                false,
                0.0,
            ),
            (
                "hover-close",
                1100.0,
                5,
                3,
                Some(TabStripPart::CloseTab(3)),
                true,
                true,
                0.0,
            ),
            ("unfocused", 1100.0, 5, 1, None, false, false, 0.0),
            (
                "overflow",
                900.0,
                14,
                6,
                Some(TabStripPart::ScrollForward),
                true,
                false,
                300.0,
            ),
            (
                "window-close",
                700.0,
                2,
                0,
                Some(TabStripPart::Close),
                true,
                false,
                0.0,
            ),
        ] {
            let list: Vec<TabStripTab> = (0..count)
                .map(|index| {
                    let (title, terminal, loading, audible, asleep) = titles[index % titles.len()];
                    TabStripTab {
                        title: title.to_string(),
                        terminal,
                        loading,
                        audible,
                        asleep,
                    }
                })
                .collect();
            let geometry = TabStripGeometry::new(width, count, scroll);
            // Twice the CSS size, as a HiDPI screen shows it.
            let height = 60u32;
            let physical = PhysicalSize::new(width as u32 * 2, height * 2);
            let mut scene = Scene {
                viewport: ViewportMetrics::from_physical(
                    physical,
                    crate::core::ScaleFactor::new(2.0),
                ),
                primitives: vec![Primitive::FillRect {
                    rect: CssRect::new(0.0, 0.0, width, height as f32),
                    color: PaintColor::Rgba(250, 250, 252, 255),
                }],
                controls: Vec::new(),
                content_viewport: CssRect::new(
                    0.0,
                    TAB_STRIP_HEIGHT,
                    width,
                    height as f32 - TAB_STRIP_HEIGHT,
                ),
                image_store: Default::default(),
                canvas_images: Default::default(),
                page_scroll_containers: Vec::new(),
                page_size: Default::default(),
            };
            paint_tab_strip(
                &mut scene.primitives,
                &geometry,
                &list,
                &TabStripState {
                    active,
                    hovered,
                    pressed: None,
                    maximized,
                    window_focused: focused,
                    pulse: 0.6,
                    dragging: None,
                },
            );
            let frame = super::super::vello_cpu::VelloCpuRenderer::new()
                .render_rgba(&scene)
                .unwrap();
            super::super::headless::write_png(&frame, dir.join(format!("tab-strip-{name}.png")))
                .unwrap();
        }
    }
}
