//! Native touchscreen contacts. Pointer Events snapshot the policy before
//! pointerdown, keep the initial axis, and suppress pointer delivery when
//! direct manipulation takes over. An idle contact schedules no work.

use super::*;
use trust::render::Affine2d;
use trust::touch::{PanGesture, PanUpdate, TouchInput, TouchPhase as Phase};

#[derive(Default)]
pub(super) struct TouchState {
    contacts: HashMap<(DeviceId, u64), Contact>,
    next_id: i32,
}

struct Contact {
    input: TouchInput,
    generation: u64,
    route: Route,
}

enum Route {
    Page {
        gesture: PanGesture,
        chain: Vec<usize>,
        transforms: HashMap<usize, Affine2d>,
        activation: Option<Box<PageHit>>,
        delivering: bool,
    },
    Browser {
        gesture: PanGesture,
        target: BrowserTarget,
    },
    Scrollbar(HeartDrag),
    Terminal(PanGesture),
    Suppressed,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BrowserTarget {
    Select,
    ContextMenu,
    UnloadPrompt,
    Strip,
    Control(ControlId),
    Outside,
}

impl TouchState {
    fn allocate_id(&mut self) -> i32 {
        loop {
            self.next_id = self.next_id.checked_add(1).unwrap_or(2).max(2);
            if !self
                .contacts
                .values()
                .any(|contact| contact.input.id == self.next_id)
            {
                return self.next_id;
            }
        }
    }
}

impl DesktopWindow {
    fn touch_browser_target(&self, point: CssPoint) -> Option<BrowserTarget> {
        if self.unload_prompt.is_some() {
            return Some(BrowserTarget::UnloadPrompt);
        }
        if self.context_menu.is_some() {
            return Some(BrowserTarget::ContextMenu);
        }
        if self.tab.select_popup.is_some() {
            return Some(BrowserTarget::Select);
        }
        if point.y < self.strip_height() {
            return Some(BrowserTarget::Strip);
        }
        let Some(scene) = &self.scene else {
            return Some(BrowserTarget::Outside);
        };
        if let Some(control) = scene.control_at(point) {
            return Some(BrowserTarget::Control(control));
        }
        (!scene.content_viewport.contains(point)).then_some(BrowserTarget::Outside)
    }

    fn touch_sample(&self, id: i32, primary: bool, point: CssPoint, pressure: f64) -> TouchInput {
        let position = self.scene.as_ref().map_or(point, |scene| {
            CssPoint::new(
                point.x - scene.content_viewport.x,
                point.y - scene.content_viewport.y,
            )
        });
        TouchInput {
            id,
            primary,
            phase: Phase::Down,
            node: None,
            position,
            metadata: self.pointer_metadata_at(point),
            pressure,
            mouse: false,
            tap: false,
        }
    }

    pub(super) fn handle_touch(&mut self, touch: winit::event::Touch) {
        let key = (touch.device_id, touch.id);
        let point = self
            .metrics
            .physical_to_css(touch.location.x, touch.location.y);
        if !point.x.is_finite() || !point.y.is_finite() {
            if matches!(touch.phase, TouchPhase::Ended | TouchPhase::Cancelled)
                && let Some(mut contact) = self.touches.contacts.remove(&key)
            {
                self.cancel_touch(&mut contact);
            }
            return;
        }
        let pressure = touch.force.map_or(0.5, |force| force.normalized());
        let pressure = if pressure.is_finite() {
            pressure.clamp(0.0, 1.0)
        } else {
            0.5
        };
        if touch.phase == TouchPhase::Started {
            // Native duplicate starts must first release the old stream.
            if let Some(mut old) = self.touches.contacts.remove(&key) {
                self.cancel_touch(&mut old);
            }
            // A UA limit on simultaneous contacts; never allocate per move.
            if self.touches.contacts.len() >= 32 {
                return;
            }
            let primary = self.touches.contacts.is_empty();
            let id = self.touches.allocate_id();
            let mut input = self.touch_sample(id, primary, point, pressure);
            let route = if let Some(target) = self.touch_browser_target(point) {
                if let Some(drag) = primary
                    .then(|| self.touch_scrollbar_start(target, point))
                    .flatten()
                {
                    self.touch_scrollbar_move(drag, point);
                    Route::Scrollbar(drag)
                } else {
                    Route::Browser {
                        gesture: PanGesture::new(point, Affine2d::IDENTITY, [false; 2], Vec::new()),
                        target,
                    }
                }
            } else if self.tab.terminal.is_some() {
                Route::Terminal(PanGesture::new(
                    point,
                    Affine2d::IDENTITY,
                    [false, primary],
                    Vec::new(),
                ))
            } else {
                let target = self
                    .scene
                    .as_ref()
                    .zip(self.tab.page_layout.as_ref())
                    .and_then(|(scene, page)| scene.touch_target_at(point, &page.document.parents));
                let (constraints, transforms, chain) = target.map_or_else(
                    || (Vec::new(), HashMap::new(), Vec::new()),
                    |target| {
                        input.node = target.hit.actor;
                        (target.constraints, target.transforms, target.scroll_chain)
                    },
                );
                let (basis, axes) = self
                    .scene
                    .as_ref()
                    .map_or((Affine2d::IDENTITY, [false; 2]), |scene| {
                        pan_scroll_space(scene, &chain, &transforms)
                    });
                let activation = self
                    .scene
                    .as_ref()
                    .and_then(|scene| scene.page_hit_at(point));
                self.cancel_heart_glide();
                let keeps_editor = activation
                    .as_ref()
                    .and_then(|hit| self.form_target_for_hit(hit))
                    .is_some_and(|(form, field)| {
                        self.tab.focus == FocusTarget::Form { form, field }
                    });
                if primary && !keeps_editor {
                    self.finish_text_edit();
                    self.tab.form_editor = None;
                    self.tab.keyboard_target = None;
                    self.set_focus(FocusTarget::Page);
                }
                let delivering = self.tab.browser.page_is_live();
                if delivering {
                    self.dispatch(UserAction::PageTouch(input));
                }
                Route::Page {
                    gesture: PanGesture::new(
                        point,
                        basis,
                        axes.map(|available| available && primary),
                        constraints,
                    ),
                    chain,
                    transforms,
                    activation: activation.map(Box::new),
                    delivering,
                }
            };
            self.touches.contacts.insert(
                key,
                Contact {
                    input,
                    generation: self.tab.browser.document_generation(),
                    route,
                },
            );
            return;
        }
        let Some(mut contact) = self.touches.contacts.remove(&key) else {
            return;
        };
        if touch.phase == TouchPhase::Cancelled {
            self.cancel_touch(&mut contact);
            return;
        }
        if contact.generation != self.tab.browser.document_generation() {
            contact.route = Route::Suppressed;
        }
        // Opening browser UI suppresses an existing page stream, including
        // a contact whose next native sample is its release.
        if matches!(contact.route, Route::Page { .. })
            && (self.unload_prompt.is_some()
                || self.context_menu.is_some()
                || self.tab.select_popup.is_some())
        {
            self.cancel_touch(&mut contact);
        }
        let ended = touch.phase == TouchPhase::Ended;
        let mut input = self.touch_sample(contact.input.id, contact.input.primary, point, pressure);
        input.phase = if ended { Phase::Up } else { Phase::Move };
        match &mut contact.route {
            Route::Page {
                gesture,
                chain,
                transforms,
                activation,
                delivering,
            } => {
                let update = gesture.update(point);
                match update {
                    PanUpdate::Scroll { delta, started } => {
                        if started && *delivering {
                            let mut cancel = contact.input;
                            cancel.phase = Phase::Cancel;
                            self.dispatch(UserAction::PageTouch(cancel));
                            *delivering = false;
                        }
                        self.scroll_touch(chain, transforms, delta);
                    }
                    PanUpdate::Pointer => {
                        // The resident page hit-tests movement against its
                        // current tree (or uses its captured target).
                        input.tap = ended && gesture.is_tap();
                        input.mouse = !gesture.is_tap();
                        if *delivering {
                            self.dispatch(UserAction::PageTouch(input));
                        }
                        if input.tap && input.primary {
                            let released = self
                                .scene
                                .as_ref()
                                .and_then(|scene| scene.page_hit_at(point));
                            let target = activation.as_ref().zip(released.as_ref()).and_then(
                                |(down, up)| {
                                    click_target_for_hits(
                                        down,
                                        up,
                                        self.tab
                                            .page_layout
                                            .as_ref()
                                            .map(|page| &page.document.parents),
                                    )
                                },
                            );
                            if let Some(target) = target {
                                // Native form UI needs the tap position, but
                                // the mouse's own pointer and hover stay put.
                                let mouse = std::mem::replace(&mut self.pointer, point);
                                if *delivering {
                                    self.native_pointer_activation(target);
                                } else {
                                    self.activate_page_hit(target);
                                }
                                self.pointer = mouse;
                            }
                        }
                    }
                }
            }
            Route::Browser { gesture, target } => {
                gesture.update(point);
                if ended
                    && input.primary
                    && gesture.is_tap()
                    && self.touch_browser_target(point) == Some(*target)
                {
                    self.touch_browser_tap(*target, point);
                }
            }
            Route::Scrollbar(drag) => self.touch_scrollbar_move(*drag, point),
            Route::Terminal(gesture) => {
                if let PanUpdate::Scroll { delta, .. } = gesture.update(point) {
                    if let Some(terminal) = &mut self.tab.terminal {
                        let (_, height) = terminal.view.cell_size();
                        terminal.wheel_remainder -= delta.y / height;
                        let lines = terminal.wheel_remainder.trunc() as i32;
                        terminal.wheel_remainder -= lines as f32;
                        terminal.view.scroll(lines);
                        self.request_redraw();
                    }
                } else if ended && input.primary && gesture.is_tap() {
                    let mouse = std::mem::replace(&mut self.pointer, point);
                    self.terminal_mouse(trust::terminal::MouseAction::Press(0));
                    self.terminal_mouse(trust::terminal::MouseAction::Release(0));
                    self.pointer = mouse;
                }
            }
            Route::Suppressed => {}
        }
        contact.input = input;
        if !ended {
            self.touches.contacts.insert(key, contact);
        }
    }

    fn cancel_touch(&mut self, contact: &mut Contact) {
        if matches!(
            contact.route,
            Route::Page {
                delivering: true,
                ..
            }
        ) && contact.generation == self.tab.browser.document_generation()
        {
            let mut input = contact.input;
            input.phase = Phase::Cancel;
            self.dispatch(UserAction::PageTouch(input));
        }
        contact.route = Route::Suppressed;
    }

    pub(super) fn cancel_touches(&mut self) {
        let mut contacts = std::mem::take(&mut self.touches.contacts);
        for contact in contacts.values_mut() {
            self.cancel_touch(contact);
        }
        self.touches.contacts = contacts;
    }

    fn touch_browser_tap(&mut self, target: BrowserTarget, point: CssPoint) {
        let mouse = std::mem::replace(&mut self.pointer, point);
        match target {
            BrowserTarget::Select => {
                self.select_popup_pointer_button(true, true);
                self.select_popup_pointer_button(false, true);
            }
            BrowserTarget::ContextMenu => {
                self.context_menu_pointer_button(ElementState::Pressed, MouseButton::Left);
                self.context_menu_pointer_button(ElementState::Released, MouseButton::Left);
            }
            BrowserTarget::UnloadPrompt => {
                self.unload_prompt_pointer_button(ElementState::Pressed, MouseButton::Left);
                self.unload_prompt_pointer_button(ElementState::Released, MouseButton::Left);
            }
            BrowserTarget::Strip => {
                self.chrome_pointer_button(ElementState::Pressed, MouseButton::Left);
                self.chrome_pointer_button(ElementState::Released, MouseButton::Left);
            }
            BrowserTarget::Control(control) => self.activate_control(control),
            BrowserTarget::Outside => {}
        }
        self.pointer = mouse;
    }

    fn touch_scrollbar_start(
        &mut self,
        target: BrowserTarget,
        point: CssPoint,
    ) -> Option<HeartDrag> {
        let BrowserTarget::Control(control) = target else {
            return None;
        };
        let (axis, heart) = match control {
            ControlId::VerticalHeart | ControlId::VerticalRail => {
                (ScrollbarAxis::Vertical, ControlId::VerticalHeart)
            }
            ControlId::HorizontalHeart | ControlId::HorizontalRail => {
                (ScrollbarAxis::Horizontal, ControlId::HorizontalHeart)
            }
            _ => return None,
        };
        let scene = self.scene.as_ref()?;
        let offset = if control == heart {
            let rect = scene
                .controls
                .iter()
                .find(|region| region.id == heart)?
                .rect;
            match axis {
                ScrollbarAxis::Vertical => point.y - (rect.y + rect.height / 2.0),
                ScrollbarAxis::Horizontal => point.x - (rect.x + rect.width / 2.0),
            }
        } else {
            0.0
        };
        self.cancel_heart_glide();
        Some(HeartDrag {
            axis,
            pointer_offset: offset,
        })
    }

    fn touch_scrollbar_move(&mut self, drag: HeartDrag, point: CssPoint) {
        let mouse_drag = self.tab.heart_drag.replace(drag);
        self.seek_heart_drag(point);
        self.tab.heart_drag = mouse_drag;
    }

    fn scroll_touch(
        &mut self,
        chain: &[usize],
        transforms: &HashMap<usize, Affine2d>,
        delta: CssPoint,
    ) {
        let mut remaining = delta;
        for &node in chain {
            let Some(mut container) = self
                .scene
                .as_ref()
                .and_then(|scene| {
                    scene
                        .page_scroll_containers
                        .iter()
                        .find(|container| container.node == node)
                })
                .cloned()
            else {
                continue;
            };
            let Some(basis) = transforms.get(&node).copied() else {
                continue;
            };
            let Some(inverse) = basis.inverse() else {
                continue;
            };
            let key = container.actor.unwrap_or(container.node);
            if let Some(offset) = self
                .tab
                .page_layout
                .as_ref()
                .and_then(|page| page.scroll_offsets.get(&key))
            {
                container.offset = *offset;
            }
            let (next, residual) = scroll_container_delta(&container, vector(inverse, remaining));
            if next != container.offset {
                if let Some(page) = &mut self.tab.page_layout {
                    page.scroll_offsets.insert(key, next);
                }
                self.tab.image_schedule_key = None;
                self.dispatch(UserAction::SetNestedScroll {
                    actor: container.actor,
                    top: next.y,
                    left: next.x,
                });
                self.request_redraw();
            }
            remaining = vector(basis, residual);
            if remaining.x.abs() <= f32::EPSILON && remaining.y.abs() <= f32::EPSILON {
                return;
            }
        }
        let Some(scene) = &self.scene else {
            return;
        };
        let max_x = (scene.page_size.width - scene.content_viewport.width).max(0.0);
        let max_y = (scene.page_size.height - scene.content_viewport.height).max(0.0);
        let current = self.tab.browser.interaction().scroll;
        self.dispatch(UserAction::SetViewportScroll(CssPoint::new(
            (current.x + remaining.x).clamp(0.0, max_x),
            (current.y + remaining.y).clamp(0.0, max_y),
        )));
    }
}

fn vector(transform: Affine2d, point: CssPoint) -> CssPoint {
    let [a, b, c, d, _, _] = transform.0;
    CssPoint::new(a * point.x + c * point.y, b * point.x + d * point.y)
}

/// The initial axis belongs to a scrollport that can move. Empty overflow
/// wrappers still impose touch-action/chaining policy, but must not turn an
/// otherwise page-owned drag into a canceled, motionless native pan.
fn pan_scroll_space(
    scene: &Scene,
    chain: &[usize],
    transforms: &HashMap<usize, Affine2d>,
) -> (Affine2d, [bool; 2]) {
    let container = |node| scene.page_scroll_containers.iter().find(|c| c.node == node);
    let basis = chain
        .iter()
        .find_map(|&node| {
            let c = container(node)?;
            if scroll_range(c).into_iter().any(|axis| axis) {
                transforms.get(&node).copied()
            } else {
                None
            }
        })
        .unwrap_or(Affine2d::IDENTITY);
    let axes = [CssPoint::new(1.0, 0.0), CssPoint::new(0.0, 1.0)].map(|axis| {
        let direction = vector(basis, axis);
        let length = direction.x.hypot(direction.y);
        if !length.is_finite() || length <= f32::EPSILON {
            return false;
        }
        let mut remaining = CssPoint::new(direction.x / length, direction.y / length);
        let mut possible = false;
        for &node in chain {
            let Some(c) = container(node) else {
                continue;
            };
            let Some(basis) = transforms.get(&node).copied() else {
                return false;
            };
            let Some(inverse) = basis.inverse() else {
                return false;
            };
            let mut local = vector(inverse, remaining);
            if !local.x.is_finite() || !local.y.is_finite() {
                return false;
            }
            trim_roundoff(&mut local);
            let [x, y] = scroll_range(c);
            if local.x != 0.0 {
                if x {
                    local.x = 0.0;
                    possible = true;
                } else if c.horizontal && c.contain_overscroll[0] {
                    return false;
                }
            }
            if local.y != 0.0 {
                if y {
                    local.y = 0.0;
                    possible = true;
                } else if c.vertical && c.contain_overscroll[1] {
                    return false;
                }
            }
            remaining = vector(basis, local);
        }
        trim_roundoff(&mut remaining);
        if remaining.x != 0.0 && scene.page_size.width > scene.content_viewport.width + f32::EPSILON
        {
            remaining.x = 0.0;
            possible = true;
        }
        if remaining.y != 0.0
            && scene.page_size.height > scene.content_viewport.height + f32::EPSILON
        {
            remaining.y = 0.0;
            possible = true;
        }
        possible && remaining.x == 0.0 && remaining.y == 0.0
    });
    (basis, axes)
}

fn scroll_range(container: &ScrollContainer) -> [bool; 2] {
    [
        container.horizontal && container.content.width > container.viewport.width + f32::EPSILON,
        container.vertical && container.content.height > container.viewport.height + f32::EPSILON,
    ]
}

fn trim_roundoff(point: &mut CssPoint) {
    let epsilon = point.x.abs().max(point.y.abs()) * 1.0e-5;
    if point.x.abs() <= epsilon {
        point.x = 0.0;
    }
    if point.y.abs() <= epsilon {
        point.y = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scene(height: f32) -> Scene {
        Scene {
            viewport: ViewportMetrics::from_physical(
                PhysicalSize::new(400, 300),
                Default::default(),
            ),
            primitives: Vec::new(),
            controls: Vec::new(),
            content_viewport: CssRect::new(0.0, 0.0, 400.0, 300.0),
            image_store: Default::default(),
            canvas_images: Default::default(),
            page_scroll_containers: Vec::new(),
            page_size: CssSize::new(400.0, height),
        }
    }

    #[test]
    fn touch_empty_scrollports_preserve_non_scrollable_gestures_and_chaining() {
        let mut scene = scene(600.0);
        scene.page_scroll_containers.push(ScrollContainer {
            node: 1,
            viewport: CssRect::new(0.0, 0.0, 80.0, 80.0),
            content: CssSize::new(80.0, 80.0),
            horizontal: true,
            vertical: true,
            ..Default::default()
        });
        let transforms = HashMap::from([(1, Affine2d([0.0, 1.0, -1.0, 0.0, 0.0, 0.0]))]);
        let (basis, axes) = pan_scroll_space(&scene, &[1], &transforms);
        assert_eq!((basis, axes), (Affine2d::IDENTITY, [false, true]));
        let mut pan = PanGesture::new(CssPoint::new(50.0, 50.0), basis, axes, Vec::new());
        assert_eq!(pan.update(CssPoint::new(30.0, 50.0)), PanUpdate::Pointer);
        // An initially unavailable axis stays with the page after turning.
        assert_eq!(pan.update(CssPoint::new(30.0, 10.0)), PanUpdate::Pointer);
        let mut pan = PanGesture::new(CssPoint::new(50.0, 50.0), basis, axes, Vec::new());
        assert!(matches!(
            pan.update(CssPoint::new(50.0, 30.0)),
            PanUpdate::Scroll { .. }
        ));
        scene.page_scroll_containers[0].contain_overscroll = [true; 2];
        assert_eq!(pan_scroll_space(&scene, &[1], &transforms).1, [false; 2]);
    }

    #[test]
    fn touch_scroll_availability_follows_transformed_local_axes() {
        let mut scene = scene(300.0);
        scene.page_scroll_containers.push(ScrollContainer {
            node: 1,
            viewport: CssRect::new(0.0, 0.0, 80.0, 80.0),
            content: CssSize::new(200.0, 80.0),
            horizontal: true,
            vertical: true,
            ..Default::default()
        });
        let rotated = Affine2d([0.0, 1.0, -1.0, 0.0, 0.0, 0.0]);
        let transforms = HashMap::from([(1, rotated)]);
        let (basis, axes) = pan_scroll_space(&scene, &[1], &transforms);
        assert_eq!((basis, axes), (rotated, [true, false]));
        let mut pan = PanGesture::new(CssPoint::new(50.0, 50.0), basis, axes, Vec::new());
        assert!(matches!(
            pan.update(CssPoint::new(50.0, 30.0)),
            PanUpdate::Scroll { .. }
        ));
        let mut pan = PanGesture::new(CssPoint::new(50.0, 50.0), basis, axes, Vec::new());
        assert_eq!(pan.update(CssPoint::new(30.0, 50.0)), PanUpdate::Pointer);
    }
}
