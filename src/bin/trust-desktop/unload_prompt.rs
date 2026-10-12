//! Leaving pages the user agent's way: closing a tab or the window first
//! checks whether unloading is canceled (HTML
//! #definitely-close-a-top-level-traversable), and a page that asks gets
//! the user agent's own question, modal over the window, while it waits
//! (HTML #steps-to-fire-beforeunload).

use super::tabs::{Hidden, TabId};
use super::*;
use trust::render::{
    TabStripPart, UnloadPromptButton, UnloadPromptGeometry, UnloadPromptState, paint_unload_prompt,
};

/// The question shown for one page's request.
pub(super) struct UnloadPromptUi {
    pub(super) tab: TabId,
    prompt: trust::js::UnloadPrompt,
    pub(super) geometry: UnloadPromptGeometry,
    pub(super) state: UnloadPromptState,
    title: String,
}

/// Quitting: the tabs whose pages have yet to allow it.
#[derive(Default)]
pub(super) struct Quitting {
    waiting: HashSet<TabId>,
}

/// Why the window asked a tab's page whether it may be left.
pub(super) enum Leaving {
    Close,
    Quit,
    /// A navigation the window runs itself (a Telnet session, an internal
    /// page), not the controller.
    Navigate(String),
}

impl DesktopApp {
    /// Take what tab `id`'s controller received about leaving its page:
    /// prompts to show and answers to `ask_unload`.
    pub(super) fn collect_unload_requests(&mut self, id: TabId) {
        loop {
            let browser = if id == self.tab.id {
                &mut self.tab.browser
            } else if let Some(tab) = self.tabs.awake_mut(id) {
                &mut tab.browser
            } else {
                return;
            };
            if let Some(prompt) = browser.take_unload_prompt() {
                self.queued_unload_prompts.push_back((id, prompt));
                continue;
            }
            let Some((request, leave)) = browser.take_unload_answer() else {
                break;
            };
            self.unload_answered(id, request, leave);
        }
        self.show_next_unload_prompt();
    }

    /// Tab `id`'s page answered whether it may be left.
    fn unload_answered(&mut self, id: TabId, request: u64, leave: bool) {
        if self
            .leaving
            .get(&id)
            .is_none_or(|(asked, _)| *asked != request)
        {
            return;
        }
        let Some((_, leaving)) = self.leaving.remove(&id) else {
            return;
        };
        match (leaving, leave) {
            (Leaving::Close, true) => self.finish_close_tab(id),
            (Leaving::Quit, true) => {
                if let Some(quit) = &mut self.quitting {
                    quit.waiting.remove(&id);
                }
                self.finish_quit_if_allowed();
            }
            (Leaving::Quit, false) => self.cancel_quit(),
            (Leaving::Navigate(address), true) if id == self.tab.id => {
                self.navigate_now(address);
            }
            (Leaving::Close | Leaving::Navigate(_), _) => {}
        }
        self.request_redraw();
    }

    /// Before the window navigates the shown tab away itself, ask its
    /// page when it listens for beforeunload. Whether to navigate now.
    pub(super) fn may_leave_now(&mut self, address: &str) -> bool {
        if !self.tab.browser.unload_needs_check() {
            return true;
        }
        let id = self.tab.id;
        if self.leaving.contains_key(&id) {
            return false;
        }
        let request = self.tab.browser.ask_unload();
        self.leaving
            .insert(id, (request, Leaving::Navigate(address.to_owned())));
        self.collect_unload_requests(id);
        false
    }

    /// Close tab `id` as the user asked (HTML #close-a-top-level-traversable):
    /// its page decides first when it listens for beforeunload.
    pub(super) fn close_tab(&mut self, id: TabId) {
        if self.tabs.index_of(id).is_none()
            || self
                .leaving
                .get(&id)
                .is_some_and(|(_, leaving)| !matches!(leaving, Leaving::Navigate(_)))
        {
            return;
        }
        // Closing the last tab ends the browser.
        if self.tabs.order.len() == 1 {
            self.request_quit();
            return;
        }
        let browser = if id == self.tab.id {
            Some(&mut self.tab.browser)
        } else {
            self.tabs.awake_mut(id).map(|tab| &mut tab.browser)
        };
        match browser {
            Some(browser) if browser.unload_needs_check() => {
                let request = browser.ask_unload();
                self.leaving.insert(id, (request, Leaving::Close));
                self.collect_unload_requests(id);
            }
            _ => self.finish_close_tab(id),
        }
    }

    /// Quit (Ctrl+Q, the window's close button, closing the last tab): every
    /// tab's page is asked first, and one that keeps the user cancels it.
    pub(super) fn request_quit(&mut self) {
        if self.quitting.is_some() {
            return;
        }
        self.quitting = Some(Quitting::default());
        let ids: Vec<TabId> = self.tabs.order.clone();
        for id in ids {
            let browser = if id == self.tab.id {
                Some(&mut self.tab.browser)
            } else {
                self.tabs.awake_mut(id).map(|tab| &mut tab.browser)
            };
            let Some(browser) = browser.filter(|browser| browser.unload_needs_check()) else {
                continue;
            };
            // A question already asked serves the quit.
            match self.leaving.get_mut(&id) {
                Some((_, leaving)) => *leaving = Leaving::Quit,
                None => {
                    let request = browser.ask_unload();
                    self.leaving.insert(id, (request, Leaving::Quit));
                }
            }
            if let Some(quit) = &mut self.quitting {
                quit.waiting.insert(id);
            }
        }
        let asked: Vec<TabId> = self
            .quitting
            .as_ref()
            .map(|quit| quit.waiting.iter().copied().collect())
            .unwrap_or_default();
        for id in asked {
            self.collect_unload_requests(id);
        }
        self.finish_quit_if_allowed();
    }

    fn finish_quit_if_allowed(&mut self) {
        if self
            .quitting
            .as_ref()
            .is_none_or(|quit| !quit.waiting.is_empty())
        {
            return;
        }
        // Every page unloads (HTML #unload-a-document-and-its-descendants)
        // as the browser ends.
        self.tab.browser.close();
        for tab in self.tabs.awake_tabs_mut() {
            tab.browser.close();
        }
        self.exit_requested = true;
    }

    fn cancel_quit(&mut self) {
        let Some(quit) = self.quitting.take() else {
            return;
        };
        // The other pages' answers no longer matter.
        for id in quit.waiting {
            self.leaving.remove(&id);
        }
        self.tab.browser.set_status("A page asked to stay open.");
    }

    /// Show the next queued prompt over its tab, which comes to the front:
    /// the user must see the page that asks.
    pub(super) fn show_next_unload_prompt(&mut self) {
        while self.unload_prompt.is_none() {
            let Some((id, prompt)) = self.queued_unload_prompts.pop_front() else {
                return;
            };
            if self.tabs.index_of(id).is_none() {
                prompt.answer(true);
                continue;
            }
            if id != self.tab.id {
                if !matches!(self.tabs.hidden(id), Some(Hidden::Awake(_))) {
                    prompt.answer(true);
                    continue;
                }
                self.activate_tab(id);
            }
            self.close_context_menu();
            self.close_select_popup();
            self.release_native_pointer_lock(true);
            let bounds = self.content_bounds();
            self.unload_prompt = Some(UnloadPromptUi {
                tab: id,
                prompt,
                geometry: UnloadPromptGeometry::new(bounds),
                state: UnloadPromptState {
                    focused: UnloadPromptButton::Leave,
                    hovered: None,
                    pressed: None,
                },
                title: tabs::tab_title(&self.tab),
            });
            self.apply_cursor_icon(CursorIcon::Default);
            self.request_overlay_redraw();
            self.update_accessibility(false);
        }
    }

    /// The page's content viewport in window CSS pixels.
    fn content_bounds(&self) -> CssRect {
        self.scene.as_ref().map_or_else(
            || {
                let viewport = self.browser_viewport();
                CssRect::new(0.0, self.strip_height(), viewport.width, viewport.height)
            },
            |scene| scene.content_viewport,
        )
    }

    /// The shown tab changed under the prompt: it waits for its tab.
    pub(super) fn requeue_unload_prompt_of_hidden_tab(&mut self) {
        if let Some(ui) = self.unload_prompt.take_if(|ui| ui.tab != self.tab.id) {
            self.queued_unload_prompts.push_front((ui.tab, ui.prompt));
            self.request_overlay_redraw();
        }
    }

    /// Answer the shown prompt and show the next one.
    pub(super) fn answer_unload_prompt(&mut self, button: UnloadPromptButton) {
        let Some(ui) = self.unload_prompt.take() else {
            return;
        };
        ui.prompt.answer(button == UnloadPromptButton::Leave);
        self.request_overlay_redraw();
        self.update_accessibility(false);
        self.show_next_unload_prompt();
    }

    /// Keys while the prompt shows, all of which it takes: Enter or Space
    /// answers with the focused button, Escape stays, Tab and the arrow
    /// keys move between the buttons, L and S answer directly.
    pub(super) fn unload_prompt_key(&mut self, input: &KeyInput) -> bool {
        let Some(ui) = &mut self.unload_prompt else {
            return false;
        };
        if input.state != KeyState::Pressed {
            return true;
        }
        let answer = match &input.key {
            Key::Enter => Some(ui.state.focused),
            Key::Character(text) if text == " " => Some(ui.state.focused),
            Key::Escape => Some(UnloadPromptButton::Stay),
            Key::Character(text) if text.eq_ignore_ascii_case("l") => {
                Some(UnloadPromptButton::Leave)
            }
            Key::Character(text) if text.eq_ignore_ascii_case("s") => {
                Some(UnloadPromptButton::Stay)
            }
            Key::Tab | Key::ArrowLeft | Key::ArrowRight => {
                ui.state.focused = ui.state.focused.other();
                self.request_overlay_redraw();
                self.update_accessibility(false);
                None
            }
            _ => None,
        };
        if let Some(button) = answer {
            self.answer_unload_prompt(button);
        }
        true
    }

    /// Pointer motion while the prompt shows lights the button under it;
    /// the page sees nothing. The tab strip keeps only the window's own
    /// controls (`unload_prompt_pointer_button`).
    pub(super) fn unload_prompt_pointer_moved(&mut self) -> bool {
        let Some(ui) = &mut self.unload_prompt else {
            return false;
        };
        let hovered = ui.geometry.button_at(self.pointer);
        if hovered != ui.state.hovered {
            ui.state.hovered = hovered;
            self.request_overlay_redraw();
        }
        self.apply_cursor_icon(CursorIcon::Default);
        !self.over_window_controls()
    }

    /// A button while the prompt shows: a press and release over one of
    /// its buttons answers; the window's own controls (moving, resizing,
    /// minimizing, maximizing) still work, and everything else waits.
    pub(super) fn unload_prompt_pointer_button(
        &mut self,
        state: ElementState,
        button: MouseButton,
    ) -> bool {
        if self.unload_prompt.is_none() {
            return false;
        }
        if self.over_window_controls() {
            return false;
        }
        let Some(ui) = &mut self.unload_prompt else {
            return false;
        };
        if button != MouseButton::Left {
            return true;
        }
        let under = ui.geometry.button_at(self.pointer);
        match state {
            ElementState::Pressed => {
                ui.state.pressed = under;
                if let Some(under) = under {
                    ui.state.focused = under;
                }
                self.request_overlay_redraw();
            }
            ElementState::Released => {
                let pressed = ui.state.pressed.take();
                self.request_overlay_redraw();
                if let Some(answer) = under.filter(|&under| pressed == Some(under)) {
                    self.answer_unload_prompt(answer);
                }
            }
        }
        true
    }

    /// Whether the pointer is on the parts of the tab strip that act on the
    /// window rather than on tabs.
    fn over_window_controls(&self) -> bool {
        if self.resize_edge_at(self.pointer).is_some() {
            return true;
        }
        let strip = self.strip_geometry();
        matches!(
            strip.part_at(self.pointer),
            Some(TabStripPart::Minimize | TabStripPart::Maximize | TabStripPart::Empty)
        )
    }

    /// Paint the shown prompt above the rest of the window's chrome.
    pub(super) fn paint_unload_prompt(
        &self,
        primitives: &mut Vec<DisplayCommand>,
    ) -> Option<CssRect> {
        let ui = self.unload_prompt.as_ref()?;
        paint_unload_prompt(primitives, &ui.geometry, ui.state, &ui.title);
        Some(ui.geometry.damage())
    }

    /// An assistive technology's action on the prompt's buttons.
    pub(super) fn unload_prompt_access_action(&mut self, request: &ActionRequest) -> bool {
        let button = match request.target_node.0.checked_sub(UNLOAD_PROMPT_ACCESS_ID) {
            Some(1) => UnloadPromptButton::Leave,
            Some(2) => UnloadPromptButton::Stay,
            _ => return false,
        };
        let Some(ui) = &mut self.unload_prompt else {
            return false;
        };
        match request.action {
            AccessAction::Click => self.answer_unload_prompt(button),
            AccessAction::Focus => {
                ui.state.focused = button;
                self.request_overlay_redraw();
                self.update_accessibility(false);
            }
            _ => return false,
        }
        true
    }
}

/// The prompt as an AccessKit alert dialog (ARIA's alertdialog pattern)
/// with its two buttons; returns the node keyboard focus is on.
pub(super) fn unload_prompt_access_nodes(
    ui: &UnloadPromptUi,
    nodes: &mut Vec<(AccessNodeId, AccessNode)>,
    children: &mut Vec<AccessNodeId>,
) -> AccessNodeId {
    let rect = |rect: CssRect| {
        AccessRect::new(
            f64::from(rect.x),
            f64::from(rect.y),
            f64::from(rect.x + rect.width),
            f64::from(rect.y + rect.height),
        )
    };
    let dialog_id = AccessNodeId(UNLOAD_PROMPT_ACCESS_ID);
    let mut dialog = AccessNode::new(AccessRole::AlertDialog);
    dialog.set_label("Leave this page? Changes you made may not be saved.");
    dialog.set_modal();
    dialog.set_bounds(rect(ui.geometry.panel));
    let mut buttons = Vec::new();
    let mut focus = dialog_id;
    for (offset, button) in [UnloadPromptButton::Leave, UnloadPromptButton::Stay]
        .into_iter()
        .enumerate()
    {
        let id = AccessNodeId(UNLOAD_PROMPT_ACCESS_ID + 1 + offset as u64);
        let mut node = AccessNode::new(AccessRole::Button);
        node.set_label(button.label());
        node.set_bounds(rect(ui.geometry.button(button)));
        node.add_action(AccessAction::Click);
        node.add_action(AccessAction::Focus);
        nodes.push((id, node));
        buttons.push(id);
        if ui.state.focused == button {
            focus = id;
        }
    }
    dialog.set_children(buttons);
    nodes.push((dialog_id, dialog));
    children.push(dialog_id);
    focus
}

/// AccessKit ids of the prompt (the dialog, then its two buttons), apart
/// from the page's and the strip's.
const UNLOAD_PROMPT_ACCESS_ID: u64 = 1 << 43;
