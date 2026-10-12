//! The window's own chrome now that the tab strip replaces the system title
//! bar: the strip's pointer interaction (switching, closing, reordering and
//! scrolling tabs, the new-tab button), the window buttons, moving the
//! window by its empty strip, and resizing it from its edges. Like COMMAND
//! and the context menu, none of it reaches a page.

use super::*;

use tabs::TabId;
use trust::render::{TabStripGeometry, TabStripPart, TabStripState, paint_tab_strip};
use winit::window::ResizeDirection;

/// The band along the window's edges that resizes it while it is neither
/// maximized nor fullscreen.
const RESIZE_BORDER: f32 = 5.0;
/// Along the top, between its corners, only this much of the band resizes:
/// the tabs fill the strip below it and must be easy to hit.
const TOP_RESIZE_BORDER: f32 = 2.0;
/// A second press on the empty strip within this time maximizes the window.
const DOUBLE_CLICK: Duration = Duration::from_millis(450);
/// How far a pressed tab moves before the press becomes a drag.
const DRAG_THRESHOLD: f32 = 4.0;

#[derive(Default)]
pub(super) struct StripUi {
    pub(super) scroll: f32,
    pub(super) hovered: Option<TabStripPart>,
    /// The part a press began on, and its button; its release acts on it.
    pressed: Option<(TabStripPart, MouseButton)>,
    drag: Option<TabDrag>,
    last_empty_press: Option<Instant>,
    /// Scroll the shown tab into view at the next paint.
    pub(super) reveal_active: bool,
    /// The edge under the pointer that resizes the window.
    edge: Option<ResizeDirection>,
}

/// A tab the pointer pressed, and how far it has been dragged.
struct TabDrag {
    id: TabId,
    /// The pointer's x where the dragged tab's slot would put it unmoved.
    origin: f32,
    offset: f32,
    moved: bool,
}

impl DesktopApp {
    pub(super) fn strip_geometry(&self) -> TabStripGeometry {
        TabStripGeometry::new(
            self.metrics.css.width,
            self.tabs.order.len(),
            self.strip.scroll,
        )
    }

    fn window_maximized(&self) -> bool {
        self.window
            .as_ref()
            .is_some_and(|window| window.is_maximized())
    }

    /// Paint the strip over the top of the window; while a page is
    /// fullscreen there is none.
    pub(super) fn paint_tab_strip(&mut self, scene: &mut Scene) {
        let start = scene.primitives.len();
        self.strip_range = start..start;
        if self.strip_height() <= 0.0 {
            return;
        }
        let active = self.tabs.index_of(self.tab.id).unwrap_or(0);
        if std::mem::take(&mut self.strip.reveal_active) {
            self.strip.scroll = self.strip_geometry().reveal(active);
        }
        let geometry = self.strip_geometry();
        self.strip.scroll = geometry.scroll;
        let tabs = self.strip_tabs();
        let state = TabStripState {
            active,
            hovered: self.strip.hovered,
            pressed: self.strip.pressed.map(|(part, _)| part),
            maximized: self.window_maximized(),
            window_focused: self.window_focused,
            pulse: self
                .tab
                .loading_started
                .or(self.background_loading_since)
                .map_or(0.6, |started| heartbeat_energy(started.elapsed())),
            dragging: self.strip.drag.as_ref().and_then(|drag| {
                drag.moved
                    .then(|| {
                        self.tabs
                            .index_of(drag.id)
                            .map(|index| (index, drag.offset))
                    })
                    .flatten()
            }),
        };
        paint_tab_strip(&mut scene.primitives, &geometry, &tabs, &state);
        self.strip_range = start..scene.primitives.len();
    }

    /// Repaint only the strip: its hover, press, drag and scroll, and
    /// background tabs' titles and loading jewels, change nothing else.
    pub(super) fn request_strip_redraw(&mut self) {
        self.strip_redraw_pending = true;
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    /// While a background tab loads and the shown one does not, its jewel
    /// keeps the heartbeat: wake for the strip's next frame.
    pub(super) fn schedule_strip_tick(&mut self) {
        let Some(since) = self.background_loading_since else {
            return;
        };
        if self.strip_tick_scheduled || self.tab.loading_started.is_some() {
            return;
        }
        self.strip_tick_scheduled = true;
        let delay = heartbeat_tick_delay(since.elapsed());
        let proxy = self.event_proxy.clone();
        self.runtime.spawn(async move {
            tokio::time::sleep(delay).await;
            let _ = proxy.send_event(DesktopEvent::StripTick);
        });
    }

    /// The window edge under `point` that resizes the window, if any.
    pub(super) fn resize_edge_at(&self, point: CssPoint) -> Option<ResizeDirection> {
        if self.page_fullscreen || self.window_maximized() {
            return None;
        }
        let size = self.metrics.css;
        let west = point.x < RESIZE_BORDER;
        let east = point.x >= size.width - RESIZE_BORDER;
        let north = point.y < TOP_RESIZE_BORDER || (point.y < RESIZE_BORDER && (west || east));
        let south = point.y >= size.height - RESIZE_BORDER;
        match (north, south, west, east) {
            (true, _, true, _) => Some(ResizeDirection::NorthWest),
            (true, _, _, true) => Some(ResizeDirection::NorthEast),
            (_, true, true, _) => Some(ResizeDirection::SouthWest),
            (_, true, _, true) => Some(ResizeDirection::SouthEast),
            (true, ..) => Some(ResizeDirection::North),
            (_, true, ..) => Some(ResizeDirection::South),
            (_, _, true, _) => Some(ResizeDirection::West),
            (_, _, _, true) => Some(ResizeDirection::East),
            _ => None,
        }
    }

    /// Pointer motion over the strip or a resizing edge. Returns whether
    /// the window's chrome took it from the page.
    pub(super) fn chrome_pointer_moved(&mut self, point: CssPoint) -> bool {
        // A press on the page keeps the pointer for the page until it ends.
        let page_owned = self.page_pointer_buttons != 0 || self.tab.heart_drag.is_some();
        if let Some(drag) = &mut self.strip.drag {
            let offset = point.x - drag.origin;
            drag.moved |= offset.abs() >= DRAG_THRESHOLD;
            drag.offset = offset;
            if drag.moved {
                self.reorder_dragged_tab();
            }
            self.request_strip_redraw();
            return true;
        }
        if self.strip.pressed.is_some() {
            let geometry = self.strip_geometry();
            let hovered = geometry.part_at(point);
            if hovered != self.strip.hovered {
                self.strip.hovered = hovered;
                self.request_strip_redraw();
            }
            return true;
        }
        let edge = (!page_owned).then(|| self.resize_edge_at(point)).flatten();
        if edge != self.strip.edge {
            self.strip.edge = edge;
        }
        if let Some(edge) = edge {
            self.clear_strip_hover();
            self.apply_cursor_icon(match edge {
                ResizeDirection::East => CursorIcon::EResize,
                ResizeDirection::North => CursorIcon::NResize,
                ResizeDirection::NorthEast => CursorIcon::NeResize,
                ResizeDirection::NorthWest => CursorIcon::NwResize,
                ResizeDirection::South => CursorIcon::SResize,
                ResizeDirection::SouthEast => CursorIcon::SeResize,
                ResizeDirection::SouthWest => CursorIcon::SwResize,
                ResizeDirection::West => CursorIcon::WResize,
            });
            return true;
        }
        if page_owned || point.y >= self.strip_height() {
            self.clear_strip_hover();
            return false;
        }
        let hovered = self.strip_geometry().part_at(point);
        if hovered != self.strip.hovered {
            self.strip.hovered = hovered;
            self.request_strip_redraw();
        }
        self.apply_cursor_icon(CursorIcon::Default);
        // The page's hover state ends at its viewport's edge.
        if self.tab.hovered_actor.take().is_some() {
            self.dispatch(UserAction::PageHover {
                actor: None,
                position: CssPoint::default(),
                metadata: self.pointer_metadata(),
            });
        }
        if !self.tab.link_preview.is_empty() {
            self.tab.link_preview.clear();
            self.request_redraw();
        }
        true
    }

    pub(super) fn clear_strip_hover(&mut self) {
        if self.strip.hovered.take().is_some() {
            self.request_strip_redraw();
        }
    }

    /// A button press or release over the strip or a resizing edge.
    /// Returns whether the window's chrome took it.
    pub(super) fn chrome_pointer_button(
        &mut self,
        state: ElementState,
        button: MouseButton,
    ) -> bool {
        let point = self.pointer;
        let pressed = state == ElementState::Pressed;
        if !pressed {
            return self.release_strip_press(button, point);
        }
        if self.page_pointer_buttons != 0 || self.tab.heart_drag.is_some() {
            return false;
        }
        if let Some(edge) = self.resize_edge_at(point) {
            if button == MouseButton::Left
                && let Some(window) = &self.window
            {
                let _ = window.drag_resize_window(edge);
            }
            return true;
        }
        if point.y >= self.strip_height() {
            return false;
        }
        let Some(part) = self.strip_geometry().part_at(point) else {
            return false;
        };
        self.close_context_menu();
        self.close_select_popup();
        match (part, button) {
            (TabStripPart::Empty, MouseButton::Left) => {
                let now = Instant::now();
                if self
                    .strip
                    .last_empty_press
                    .take()
                    .is_some_and(|last| now.duration_since(last) <= DOUBLE_CLICK)
                {
                    self.toggle_maximized();
                } else {
                    self.strip.last_empty_press = Some(now);
                    if let Some(window) = &self.window {
                        let _ = window.drag_window();
                    }
                }
            }
            (TabStripPart::Empty, MouseButton::Right) => {
                if let Some(window) = &self.window {
                    window.show_window_menu(winit::dpi::LogicalPosition::new(
                        f64::from(point.x),
                        f64::from(point.y),
                    ));
                }
            }
            (TabStripPart::Tab(index), MouseButton::Left) => {
                if let Some(&id) = self.tabs.order.get(index) {
                    self.activate_tab(id);
                    let geometry = self.strip_geometry();
                    let slot = geometry.tabs.get(index).map_or(point.x, |tab| tab.x);
                    self.strip.drag = Some(TabDrag {
                        id,
                        origin: point.x - (geometry.tabs[index].x - slot),
                        offset: 0.0,
                        moved: false,
                    });
                }
                self.strip.pressed = Some((part, button));
            }
            (TabStripPart::ScrollBack | TabStripPart::ScrollForward, MouseButton::Left) => {
                self.strip.scroll = self.strip_geometry().step(part == TabStripPart::ScrollBack);
                self.strip.pressed = Some((part, button));
            }
            (_, MouseButton::Left | MouseButton::Middle) => {
                self.strip.pressed = Some((part, button));
            }
            _ => {}
        }
        self.strip.hovered = Some(part);
        self.request_strip_redraw();
        true
    }

    fn release_strip_press(&mut self, button: MouseButton, point: CssPoint) -> bool {
        let Some((part, pressed_button)) = self.strip.pressed else {
            return false;
        };
        if pressed_button != button {
            return true;
        }
        self.strip.pressed = None;
        let drag = self.strip.drag.take();
        self.request_redraw();
        if drag.is_some_and(|drag| drag.moved) {
            return true;
        }
        let released_on = self.strip_geometry().part_at(point);
        if released_on != Some(part) {
            return true;
        }
        match (part, button) {
            (TabStripPart::CloseTab(index) | TabStripPart::Tab(index), MouseButton::Middle)
            | (TabStripPart::CloseTab(index), MouseButton::Left) => {
                if let Some(&id) = self.tabs.order.get(index) {
                    self.close_tab(id);
                }
            }
            (TabStripPart::NewTab, MouseButton::Left) => self.open_blank_tab(),
            (TabStripPart::NewTab, MouseButton::Middle) => self.open_blank_tab(),
            (TabStripPart::Minimize, MouseButton::Left) => {
                if let Some(window) = &self.window {
                    window.set_minimized(true);
                }
            }
            (TabStripPart::Maximize, MouseButton::Left) => self.toggle_maximized(),
            (TabStripPart::Close, MouseButton::Left) => self.request_quit(),
            _ => {}
        }
        true
    }

    fn toggle_maximized(&mut self) {
        if let Some(window) = &self.window {
            window.set_maximized(!window.is_maximized());
        }
        self.request_redraw();
    }

    /// Move a dragged tab to the slot its center is over.
    fn reorder_dragged_tab(&mut self) {
        let Some(drag) = &self.strip.drag else {
            return;
        };
        let id = drag.id;
        let geometry = self.strip_geometry();
        let Some(from) = self.tabs.index_of(id) else {
            return;
        };
        let slot = geometry.tabs[from];
        let center = slot.x + drag.offset + slot.width / 2.0;
        let to = geometry.drop_index(center);
        if to == from {
            return;
        }
        let moved = self.tabs.order.remove(from);
        self.tabs.order.insert(to, moved);
        // The tab keeps its place under the pointer: its new slot is that
        // many widths away from the old one.
        let shift = (to as f32 - from as f32) * slot.width;
        if let Some(drag) = &mut self.strip.drag {
            drag.origin += shift;
            drag.offset -= shift;
        }
    }

    /// The wheel over the strip scrolls tabs that overflow it.
    pub(super) fn chrome_scroll(&mut self, delta: MouseScrollDelta) -> bool {
        if self.pointer.y >= self.strip_height() || self.page_pointer_buttons != 0 {
            return false;
        }
        let geometry = self.strip_geometry();
        if geometry.overflows() {
            let amount = match delta {
                MouseScrollDelta::LineDelta(x, y) => {
                    (if x.abs() > y.abs() { x } else { -y }) * 60.0
                }
                MouseScrollDelta::PixelDelta(position) => {
                    let scale = self.metrics.scale_factor.get();
                    (if position.x.abs() > position.y.abs() {
                        -position.x
                    } else {
                        -position.y
                    } / scale) as f32
                }
            };
            self.strip.scroll = (geometry.scroll + amount).clamp(0.0, geometry.max_scroll);
            self.request_strip_redraw();
        }
        true
    }
}

/// Accessibility node ids of the strip, above every other node's range.
const ACCESS_TAB_LIST: AccessNodeId = AccessNodeId(1 << 40);
const ACCESS_NEW_TAB: AccessNodeId = AccessNodeId((1 << 40) + 1);
const ACCESS_SCROLL_BACK: AccessNodeId = AccessNodeId((1 << 40) + 2);
const ACCESS_SCROLL_FORWARD: AccessNodeId = AccessNodeId((1 << 40) + 3);
const ACCESS_MINIMIZE: AccessNodeId = AccessNodeId((1 << 40) + 4);
const ACCESS_MAXIMIZE: AccessNodeId = AccessNodeId((1 << 40) + 5);
const ACCESS_CLOSE_WINDOW: AccessNodeId = AccessNodeId((1 << 40) + 6);
/// A tab's node is this plus its id; its close button's is the next range.
const ACCESS_TABS: u64 = 1 << 41;
const ACCESS_TAB_CLOSE: u64 = 1 << 42;

/// What accessibility needs of the strip.
pub(super) struct AccessTabStrip {
    geometry: TabStripGeometry,
    tabs: Vec<(TabId, String)>,
    active: usize,
    maximized: bool,
}

impl DesktopApp {
    pub(super) fn access_tab_strip(&self) -> Option<AccessTabStrip> {
        (self.strip_height() > 0.0).then(|| AccessTabStrip {
            geometry: self.strip_geometry(),
            tabs: self
                .tabs
                .order
                .iter()
                .copied()
                .zip(self.strip_tabs())
                .map(|(id, tab)| (id, tab.title))
                .collect(),
            active: self.tabs.index_of(self.tab.id).unwrap_or(0),
            maximized: self.window_maximized(),
        })
    }

    /// An assistive technology's action on a strip node. Returns whether
    /// the node was the strip's.
    pub(super) fn tab_strip_access_action(&mut self, request: &ActionRequest) -> bool {
        let AccessNodeId(node) = request.target_node;
        let clicked = matches!(request.action, AccessAction::Click);
        if (ACCESS_TABS..ACCESS_TAB_CLOSE).contains(&node) {
            if clicked || matches!(request.action, AccessAction::Focus) {
                self.activate_tab(TabId(node - ACCESS_TABS));
            }
            return true;
        }
        if (ACCESS_TAB_CLOSE..ACCESS_TAB_CLOSE * 2).contains(&node) {
            if clicked {
                self.close_tab(TabId(node - ACCESS_TAB_CLOSE));
            }
            return true;
        }
        let part = match request.target_node {
            ACCESS_NEW_TAB => TabStripPart::NewTab,
            ACCESS_SCROLL_BACK => TabStripPart::ScrollBack,
            ACCESS_SCROLL_FORWARD => TabStripPart::ScrollForward,
            ACCESS_MINIMIZE => TabStripPart::Minimize,
            ACCESS_MAXIMIZE => TabStripPart::Maximize,
            ACCESS_CLOSE_WINDOW => TabStripPart::Close,
            ACCESS_TAB_LIST => return true,
            _ => return false,
        };
        if clicked {
            match part {
                TabStripPart::NewTab => self.open_blank_tab(),
                TabStripPart::ScrollBack | TabStripPart::ScrollForward => {
                    self.strip.scroll =
                        self.strip_geometry().step(part == TabStripPart::ScrollBack);
                    self.request_redraw();
                }
                TabStripPart::Minimize => {
                    if let Some(window) = &self.window {
                        window.set_minimized(true);
                    }
                }
                TabStripPart::Maximize => self.toggle_maximized(),
                TabStripPart::Close => self.request_quit(),
                _ => {}
            }
        }
        true
    }
}

/// The strip as accessibility nodes: a tab list of tabs (ARIA's tabs
/// pattern), each with its close button, and the strip's buttons.
pub(super) fn tab_strip_access_nodes(
    strip: &AccessTabStrip,
    nodes: &mut Vec<(AccessNodeId, AccessNode)>,
    children: &mut Vec<AccessNodeId>,
) {
    let rect = |rect: CssRect| {
        AccessRect::new(
            f64::from(rect.x),
            f64::from(rect.y),
            f64::from(rect.x + rect.width),
            f64::from(rect.y + rect.height),
        )
    };
    let geometry = &strip.geometry;
    let mut list = AccessNode::new(AccessRole::TabList);
    list.set_label("Tabs");
    list.set_bounds(rect(geometry.viewport));
    let mut tabs = Vec::new();
    for (index, (id, title)) in strip.tabs.iter().enumerate() {
        let tab_id = AccessNodeId(ACCESS_TABS + id.0);
        let close_id = AccessNodeId(ACCESS_TAB_CLOSE + id.0);
        let mut tab = AccessNode::new(AccessRole::Tab);
        tab.set_label(title.as_str());
        // A tab scrolled out of the strip keeps its place beyond it.
        if let Some(bounds) = geometry
            .visible_tab(index)
            .or_else(|| geometry.tabs.get(index).copied())
        {
            tab.set_bounds(rect(bounds));
        }
        tab.set_selected(index == strip.active);
        tab.add_action(AccessAction::Click);
        tab.add_action(AccessAction::Focus);
        let mut close = AccessNode::new(AccessRole::Button);
        close.set_label(format!("Close {title}"));
        if let Some(bounds) = geometry.close_button(index) {
            close.set_bounds(rect(bounds));
        }
        close.add_action(AccessAction::Click);
        tab.set_children(vec![close_id]);
        nodes.push((close_id, close));
        nodes.push((tab_id, tab));
        tabs.push(tab_id);
    }
    list.set_children(tabs);
    nodes.push((ACCESS_TAB_LIST, list));
    children.push(ACCESS_TAB_LIST);
    let buttons = [
        (Some(geometry.new_tab), ACCESS_NEW_TAB, "New tab"),
        (geometry.scroll_back, ACCESS_SCROLL_BACK, "Scroll tabs back"),
        (
            geometry.scroll_forward,
            ACCESS_SCROLL_FORWARD,
            "Scroll tabs forward",
        ),
        (Some(geometry.minimize), ACCESS_MINIMIZE, "Minimize"),
        (
            Some(geometry.maximize),
            ACCESS_MAXIMIZE,
            if strip.maximized {
                "Restore"
            } else {
                "Maximize"
            },
        ),
        (Some(geometry.close), ACCESS_CLOSE_WINDOW, "Close"),
    ];
    for (bounds, id, label) in buttons {
        let Some(bounds) = bounds else {
            continue;
        };
        let mut button = AccessNode::new(AccessRole::Button);
        button.set_label(label);
        button.set_bounds(rect(bounds));
        button.add_action(AccessAction::Click);
        nodes.push((id, button));
        children.push(id);
    }
}
