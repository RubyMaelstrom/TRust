//! Tabs: each is one top-level traversable of the user agent's top-level
//! traversable set (HTML #top-level-traversable-set), presented in the tab
//! strip that replaces the window's title bar. The window shows one tab at a
//! time; `DesktopApp::tab` holds it and every other tab waits in `TabSet`.

use super::*;
use trust::render::{TAB_STRIP_HEIGHT, TabStripTab};

/// How long a tab may stay unseen before it sleeps: its page is released
/// from memory and loads again when the tab is next shown (Page Lifecycle
/// #discarding).
pub(super) const SLEEP_AFTER: Duration = Duration::from_secs(10 * 60);
/// Closed tabs that Ctrl+Shift+T can reopen in this session.
const REMEMBERED_CLOSED: usize = 25;

/// A tab's identity for the life of the process; never reused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(super) struct TabId(pub(super) u64);

/// One tab's browser state: its controller (session history, live page)
/// and everything the window keeps about the document it shows.
pub(super) struct Tab {
    pub(super) id: TabId,
    /// When the tab was last shown: an unviewed tab sleeps after
    /// `SLEEP_AFTER` (Page Lifecycle #discarding).
    pub(super) last_shown: Instant,
    /// The tab the user opened this one from, which places further tabs
    /// opened from it after this one.
    pub(super) opened_from: Option<TabId>,
    pub(super) browser: BrowserController,
    pub(super) focus: FocusTarget,
    /// The command-line URL starts only after the native window reports its
    /// real CSS viewport and device scale. Responsive scripts must not observe
    /// the constructor's provisional 1x environment during initial parsing.
    pub(super) initial_navigation: Option<String>,
    pub(super) find: TextEditor,
    pub(super) command: TextEditor,
    pub(super) file_action_selected: usize,
    pub(super) form_editor: Option<TextEditor>,
    pub(super) form_scroll_x: f32,
    /// A select's open drop-down box.
    pub(super) select_popup: Option<SelectPopup>,
    /// A press outside the drop-down box closed it; its release is consumed.
    pub(super) select_popup_swallow_release: bool,
    pub(super) page_layout: Option<PageLayoutCache>,
    pub(super) protocol_page: Option<ProtocolPageCache>,
    pub(super) image_sizes: trust::layout2::ImageSizes,
    /// Number of decoded intrinsic sizes successfully queued to the resident
    /// page actor. `try_send` can legitimately find the command queue full;
    /// retain the unsent map and retry from the native draw/event loop.
    pub(super) image_sizes_sent: usize,
    pub(super) image_loads: ImageLoadScheduler,
    pub(super) image_tasks: HashMap<ImageHandle, tokio::task::JoinHandle<()>>,
    pub(super) image_flush_scheduled: bool,
    pub(super) decoded_images_pending_layout: bool,
    pub(super) animation_sources: AnimationSourceStore,
    pub(super) active_animations: HashSet<ImageHandle>,
    pub(super) image_schedule_key: Option<ImageScheduleKey>,
    pub(super) hovered_actor: Option<usize>,
    pub(super) hover_document_generation: u64,
    pub(super) css_animation_generation: u64,
    pub(super) css_animation_started: Instant,
    pub(super) link_preview: String,
    /// The user's selection of page text (`text_selection`).
    pub(super) text_selection: text_selection::TextSelectionState,
    /// The bookmark store's last answer about an address: its bookmarks.
    pub(super) page_bookmark: Option<(String, Vec<u64>)>,
    pub(super) find_matches: Vec<TextSelection>,
    pub(super) find_index: usize,
    pub(super) terminal: Option<TerminalSession>,
    pub(super) bookmark_terminal: Option<TerminalSession>,
    pub(super) bookmark_filter: String,
    /// The Telnet session's RAM-only line-entry history, separate from
    /// COMMAND's and from other tabs' sessions.
    pub(super) session_history: trust::command::History,
    pub(super) gopher_query: Option<trust::gopher::GopherUrl>,
    pub(super) gemini_input: bool,
    pub(super) pending_page_keys: VecDeque<PendingPageKey>,
    pub(super) browser_events_pending: bool,
    pub(super) queued_form_keys: VecDeque<PendingPageKey>,
    pub(super) keyboard_target: Option<PageHit>,
    pub(super) pressed_hit: Option<PageHit>,
    pub(super) heart_hover: Option<ScrollbarAxis>,
    pub(super) heart_drag: Option<HeartDrag>,
    pub(super) heart_glide: Option<HeartGlide>,
    pub(super) last_document_generation: u64,
    pub(super) last_vertical_heart_fraction: Option<f32>,
    pub(super) loading_started: Option<Instant>,
}

impl Tab {
    pub(super) fn new(
        id: TabId,
        browser: BrowserController,
        initial_navigation: Option<String>,
    ) -> Self {
        let style = chrome_text_style();
        Self {
            id,
            last_shown: Instant::now(),
            opened_from: None,
            browser,
            focus: FocusTarget::for_startup(initial_navigation.as_deref()),
            initial_navigation,
            find: TextEditor::new("", &style, 500.0, false),
            command: TextEditor::new("", &style, 700.0, false),
            file_action_selected: 0,
            form_editor: None,
            form_scroll_x: 0.0,
            select_popup: None,
            select_popup_swallow_release: false,
            page_layout: None,
            protocol_page: None,
            image_sizes: trust::layout2::ImageSizes::new(),
            image_sizes_sent: 0,
            image_loads: ImageLoadScheduler::default(),
            image_tasks: HashMap::new(),
            image_flush_scheduled: false,
            decoded_images_pending_layout: false,
            animation_sources: AnimationSourceStore::default(),
            active_animations: HashSet::new(),
            image_schedule_key: None,
            hovered_actor: None,
            hover_document_generation: 0,
            css_animation_generation: 0,
            css_animation_started: Instant::now(),
            link_preview: String::new(),
            text_selection: Default::default(),
            page_bookmark: None,
            find_matches: Vec::new(),
            find_index: 0,
            terminal: None,
            bookmark_terminal: None,
            bookmark_filter: String::new(),
            session_history: trust::command::History::default(),
            gopher_query: None,
            gemini_input: false,
            pending_page_keys: VecDeque::new(),
            browser_events_pending: false,
            queued_form_keys: VecDeque::new(),
            keyboard_target: None,
            pressed_hit: None,
            heart_hover: None,
            heart_drag: None,
            heart_glide: None,
            last_document_generation: 0,
            last_vertical_heart_fraction: None,
            loading_started: None,
        }
    }
}

/// A tab that is not shown.
pub(super) enum Hidden {
    Awake(Box<Tab>),
    /// Released from memory (Page Lifecycle "discarded"); shown again by
    /// loading its page anew.
    Asleep(Box<SleepingTab>),
}

/// What a sleeping tab keeps: enough to present it in the strip and to load
/// its page again where the user left it.
pub(super) struct SleepingTab {
    pub(super) title: String,
    pub(super) history: trust::core::SessionHistory,
    pub(super) opened_from: Option<TabId>,
}

/// A closed tab Ctrl+Shift+T can reopen where it was, with its history.
pub(super) struct ClosedTab {
    pub(super) history: trust::core::SessionHistory,
    pub(super) index: usize,
}

/// The window's tabs: their strip order and every tab that is not shown.
#[derive(Default)]
pub(super) struct TabSet {
    next_id: u64,
    /// Strip order, the shown tab included.
    pub(super) order: Vec<TabId>,
    pub(super) hidden: HashMap<TabId, Hidden>,
    /// Background tabs whose controllers have events to process.
    woken: Vec<TabId>,
    /// Recently closed tabs, the most recent last.
    closed: Vec<ClosedTab>,
}

impl TabSet {
    pub(super) fn allocate_id(&mut self) -> TabId {
        self.next_id += 1;
        TabId(self.next_id)
    }

    /// A background tab's controller has events to process.
    pub(super) fn wake(&mut self, tab: TabId) {
        if matches!(self.hidden.get(&tab), Some(Hidden::Awake(_))) && !self.woken.contains(&tab) {
            self.woken.push(tab);
        }
    }

    pub(super) fn index_of(&self, tab: TabId) -> Option<usize> {
        self.order.iter().position(|&id| id == tab)
    }

    pub(super) fn hidden(&self, tab: TabId) -> Option<&Hidden> {
        self.hidden.get(&tab)
    }

    pub(super) fn awake_mut(&mut self, tab: TabId) -> Option<&mut Tab> {
        match self.hidden.get_mut(&tab) {
            Some(Hidden::Awake(tab)) => Some(tab),
            _ => None,
        }
    }

    pub(super) fn awake_tabs_mut(&mut self) -> impl Iterator<Item = &mut Tab> {
        self.hidden.values_mut().filter_map(|hidden| match hidden {
            Hidden::Awake(tab) => Some(&mut **tab),
            Hidden::Asleep(_) => None,
        })
    }

    fn take_hidden(&mut self, tab: TabId) -> Option<Hidden> {
        self.woken.retain(|&id| id != tab);
        self.hidden.remove(&tab)
    }

    fn park(&mut self, tab: Box<Tab>) {
        self.hidden.insert(tab.id, Hidden::Awake(tab));
    }

    fn remember_closed(&mut self, closed: ClosedTab) {
        if self.closed.len() == REMEMBERED_CLOSED {
            self.closed.remove(0);
        }
        self.closed.push(closed);
    }

    /// The tab a tab opened from `source` goes to: after `source` and the
    /// tabs already opened from it, so they keep the order they were opened.
    fn related_insert_index(&self, source: TabId, shown: &Tab) -> usize {
        let Some(mut index) = self.index_of(source) else {
            return self.order.len();
        };
        index += 1;
        while let Some(&id) = self.order.get(index) {
            let opened_from = if id == shown.id {
                shown.opened_from
            } else {
                match self.hidden.get(&id) {
                    Some(Hidden::Awake(tab)) => tab.opened_from,
                    Some(Hidden::Asleep(sleeping)) => sleeping.opened_from,
                    None => None,
                }
            };
            if opened_from != Some(source) {
                break;
            }
            index += 1;
        }
        index
    }
}

/// The keyboard's tab commands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TabShortcut {
    New,
    Close,
    Reopen,
    Next,
    Previous,
    /// Ctrl+1 to Ctrl+8: the tab at that place; Ctrl+9: the last one.
    Select(usize),
    Last,
    Quit,
}

impl TabShortcut {
    pub(super) fn of(key: &WinitKey, modifiers: ModifiersState) -> Option<Self> {
        if !modifiers.control_key() || modifiers.alt_key() || modifiers.super_key() {
            return None;
        }
        let shift = modifiers.shift_key();
        match key {
            WinitKey::Named(NamedKey::Tab) => Some(if shift { Self::Previous } else { Self::Next }),
            WinitKey::Named(NamedKey::PageDown) if !shift => Some(Self::Next),
            WinitKey::Named(NamedKey::PageUp) if !shift => Some(Self::Previous),
            WinitKey::Character(text) => match text.to_ascii_lowercase().as_str() {
                "t" => Some(if shift { Self::Reopen } else { Self::New }),
                "w" if !shift => Some(Self::Close),
                "q" if !shift => Some(Self::Quit),
                digit @ ("1" | "2" | "3" | "4" | "5" | "6" | "7" | "8") if !shift => {
                    Some(Self::Select(digit.parse::<usize>().ok()? - 1))
                }
                "9" if !shift => Some(Self::Last),
                _ => None,
            },
            _ => None,
        }
    }

    /// Holding the key repeats switching tabs, never opening or closing.
    pub(super) fn repeats(self) -> bool {
        matches!(self, Self::Next | Self::Previous)
    }
}

/// A browser controller for tab `id`: its wakes name the tab, so background
/// tabs are serviced without waking the shown one.
pub(super) fn new_controller(
    runtime: &Handle,
    proxy: &EventLoopProxy<DesktopEvent>,
    id: TabId,
) -> BrowserController {
    let proxy = proxy.clone();
    let mut browser = BrowserController::new(
        runtime.clone(),
        move || {
            let _ = proxy.send_event(DesktopEvent::BrowserWake(Some(id)));
        },
        CssSize::new(960.0, 640.0),
    );
    browser.set_fullscreen_available(true);
    browser.set_file_picker_available(true);
    browser.set_new_traversables_available(true);
    browser.set_unload_prompts_available(true);
    browser
}

/// Whether following `link` needs its tab's own chrome rather than a page
/// load: a Telnet session, a Gopher search, or a download offer.
fn needs_tab_chrome(link: &Link) -> bool {
    match link {
        Link::Telnet { .. } => true,
        Link::External(target) => parse_telnet_target(target).is_some(),
        Link::Gopher(url) => url.needs_query() || url.item_type == '8' || url.is_download(),
        _ => false,
    }
}

/// The window's presentation of one tab, which its controller hands to the
/// page: viewport, density, position, focus, and whether it can be seen.
#[derive(Clone, Copy)]
pub(super) struct Presentation {
    pub(super) viewport: CssSize,
    pub(super) device_pixel_ratio: f32,
    pub(super) screen_position: (i32, i32),
    pub(super) focused: bool,
    pub(super) visible: bool,
}

impl Presentation {
    fn apply(self, browser: &mut BrowserController) {
        browser.handle_action(UserAction::DevicePixelRatio(self.device_pixel_ratio));
        browser.handle_action(UserAction::Resize(self.viewport));
        browser.handle_action(UserAction::ScreenPosition(
            self.screen_position.0,
            self.screen_position.1,
        ));
        browser.handle_action(UserAction::Focus(self.focused));
        browser.handle_action(UserAction::Visible(self.visible));
    }
}

/// A background page keeps running (HTML: its tasks still run), but what
/// needs the user's attention is refused, as for a document that is not
/// focused and cannot be seen: fullscreen, pointer lock, clipboard writes,
/// file prompts and context menus. Answers the page waits for are given.
fn refuse_background_requests(browser: &mut BrowserController) {
    while let Some((request, node, _)) = browser.take_pointer_lock_request() {
        if node.is_some() {
            browser.handle_action(UserAction::PointerLockResult {
                request,
                error: Some(String::from("WrongDocumentError")),
            });
        }
    }
    while let Some(change) = browser.take_fullscreen_change() {
        match change {
            FullscreenChange::Enter(request) => {
                browser.handle_action(UserAction::FullscreenResult { request, ok: false });
            }
            FullscreenChange::Exit(Some(request)) => {
                browser.handle_action(UserAction::FullscreenResult { request, ok: true });
            }
            FullscreenChange::Exit(None) => {}
        }
    }
    while let Some((request, _)) = browser.take_clipboard_write() {
        browser.handle_action(UserAction::ClipboardResult { request, ok: false });
    }
    while let Some(request) = browser.take_file_picker() {
        browser.handle_action(UserAction::FilePickerResult {
            request: request.request,
            files: None,
        });
    }
    while browser.take_context_menu_result().is_some() {}
    while browser.take_close_request_result().is_some() {}
    while browser.take_external_media().is_some() {}
}

/// Release what a tab keeps only to present its page: the frontend's copy
/// of the page's layout and interaction facts, and its image work.
fn release_presentation(tab: &mut Tab) {
    tab.page_layout = None;
    tab.protocol_page = None;
    tab.find_matches.clear();
    tab.image_schedule_key = None;
    tab.image_loads.suspend();
    for (_, task) in tab.image_tasks.drain() {
        task.abort();
    }
    tab.image_flush_scheduled = false;
    tab.decoded_images_pending_layout = true;
    tab.animation_sources.clear();
    tab.active_animations.clear();
}

/// The title a tab shows: its page's title, or its address when the page
/// has none.
pub(super) fn tab_title(tab: &Tab) -> String {
    if let Some(terminal) = &tab.terminal {
        return terminal.address.clone();
    }
    tab.browser
        .title()
        .filter(|title| !title.trim().is_empty())
        .unwrap_or_else(|| {
            let address = tab.browser.address();
            if address.is_empty() {
                String::from("New Tab")
            } else {
                address
            }
        })
}

impl DesktopApp {
    /// The strip's height: it hides while a page is fullscreen.
    pub(super) fn strip_height(&self) -> f32 {
        if self.page_fullscreen {
            0.0
        } else {
            TAB_STRIP_HEIGHT
        }
    }

    /// The page viewport of a tab whose focus is `focus`: the window below
    /// the strip, less any panel the focus reserves.
    pub(super) fn viewport_for(&self, focus: FocusTarget) -> CssSize {
        let css = self.metrics.css;
        focus.viewport(CssSize::new(
            css.width,
            (css.height - self.strip_height()).max(0.0),
        ))
    }

    fn presentation(&self, focus: FocusTarget, visible: bool) -> Presentation {
        Presentation {
            viewport: self.viewport_for(focus),
            device_pixel_ratio: self.metrics.scale_factor.get() as f32,
            screen_position: self.screen_origin,
            focused: visible && self.window_focused,
            visible,
        }
    }

    /// A new tab in this window's environment.
    fn create_tab(&mut self, opened_from: Option<TabId>, visible: bool) -> Box<Tab> {
        let id = self.tabs.allocate_id();
        let mut browser = new_controller(&self.runtime, &self.event_proxy, id);
        browser.set_clipboard_available(self.clipboard.is_some());
        let mut tab = Box::new(Tab::new(id, browser, None));
        tab.opened_from = opened_from;
        tab.focus = FocusTarget::Page;
        self.presentation(tab.focus, visible)
            .apply(&mut tab.browser);
        tab
    }

    /// Ctrl+T and the new-tab button: a fresh top-level traversable (HTML
    /// #nav-traversal-ui) at the end of the strip, with COMMAND open for
    /// its address.
    pub(super) fn open_blank_tab(&mut self) {
        let mut tab = self.create_tab(None, true);
        tab.focus = FocusTarget::Command;
        let index = self.tabs.order.len();
        self.show_new_tab(tab, index);
        self.tab.command.set_text("");
    }

    /// Open `link` from the shown tab's document in a new tab: HTML
    /// #following-hyperlinks-2 in a fresh top-level traversable the user
    /// asked for, with that document as the navigation's source and no
    /// opener. Background tabs go after the tabs already opened from the
    /// shown one.
    pub(super) fn open_link_in_new_tab(&mut self, link: Link, foreground: bool) {
        if !self.tab.browser.may_open_link(&link) {
            self.tab
                .browser
                .set_status("Local file navigation blocked for this document.");
            self.request_redraw();
            return;
        }
        let source = self.tab.id;
        let initiator = self.tab.browser.link_initiator();
        let index = self.tabs.related_insert_index(source, &self.tab);
        // What needs the tab's own chrome opens in front.
        if needs_tab_chrome(&link) {
            let tab = self.create_tab(Some(source), true);
            self.show_new_tab(tab, index);
            self.activate_link(link);
        } else if foreground {
            let tab = self.create_tab(Some(source), true);
            self.show_new_tab(tab, index);
            self.tab.browser.open_link_from(link, initiator);
            self.request_redraw();
        } else {
            let mut tab = self.create_tab(Some(source), false);
            tab.browser.open_link_from(link, initiator);
            self.tabs.order.insert(index, tab.id);
            self.tabs.park(tab);
            self.request_redraw();
        }
    }

    /// Open the new top-level traversables tab `source`'s page asked for
    /// (HTML #the-rules-for-choosing-a-navigable: "create a new top-level
    /// traversable"), each after the tabs already opened from `source`. A
    /// tab asked for in front comes to the front only from the shown tab.
    pub(super) fn open_requested_traversables(&mut self, source: TabId) {
        loop {
            let request = if source == self.tab.id {
                self.tab.browser.take_new_traversable()
            } else {
                self.tabs
                    .awake_mut(source)
                    .and_then(|tab| tab.browser.take_new_traversable())
            };
            let Some(request) = request else {
                return;
            };
            let index = self.tabs.related_insert_index(source, &self.tab);
            let request = match request {
                trust::core::NewTraversableRequest::Navigate(request) => request,
                // An auxiliary browsing context the page opened in its group
                // (window.open, a named target): a tab in front, unless a
                // page's question whether to leave it keeps its own there.
                trust::core::NewTraversableRequest::Auxiliary(auxiliary) => {
                    let foreground = source == self.tab.id && self.unload_prompt.is_none();
                    let mut tab = self.create_tab(Some(source), foreground);
                    tab.browser.adopt_auxiliary(auxiliary);
                    if foreground {
                        self.show_new_tab(tab, index);
                    } else {
                        self.tabs.order.insert(index, tab.id);
                        self.tabs.park(tab);
                    }
                    self.request_redraw();
                    continue;
                }
            };
            if request.submission.is_none()
                && let Ok((link, _)) = trust::core::parse_navigation_target(&request.url)
                && needs_tab_chrome(&link)
            {
                let tab = self.create_tab(Some(source), true);
                self.show_new_tab(tab, index);
                self.activate_link(link);
                continue;
            }
            // A page's question whether to leave it keeps its tab in front.
            let foreground =
                request.foreground && source == self.tab.id && self.unload_prompt.is_none();
            let mut tab = self.create_tab(Some(source), foreground);
            tab.browser.open_new_traversable(request);
            if foreground {
                self.show_new_tab(tab, index);
            } else {
                self.tabs.order.insert(index, tab.id);
                self.tabs.park(tab);
            }
            self.request_redraw();
        }
    }

    /// The page of tab `id` asked to close its traversable or to show it
    /// (HTML #definitely-close-a-top-level-traversable, #dom-window-focus):
    /// close it as its user would (its page may still keep it), or show it.
    pub(super) fn service_traversable_requests(&mut self, id: TabId) {
        let browser = if id == self.tab.id {
            Some(&mut self.tab.browser)
        } else {
            self.tabs.awake_mut(id).map(|tab| &mut tab.browser)
        };
        let Some(browser) = browser else {
            return;
        };
        let close = browser.take_close_request();
        let focus = browser.take_focus_request();
        if close {
            self.close_tab(id);
        } else if focus {
            self.activate_tab(id);
        }
    }

    /// Insert `tab` at `index` in the strip and show it.
    fn show_new_tab(&mut self, tab: Box<Tab>, index: usize) {
        let index = index.min(self.tabs.order.len());
        self.tabs.order.insert(index, tab.id);
        self.swap_shown(tab);
    }

    /// Show tab `id`, waking it if it sleeps.
    pub(super) fn activate_tab(&mut self, id: TabId) {
        if id == self.tab.id {
            return;
        }
        let Some(hidden) = self.tabs.take_hidden(id) else {
            return;
        };
        let incoming = match hidden {
            Hidden::Awake(tab) => tab,
            Hidden::Asleep(sleeping) => self.wake_sleeping_tab(id, *sleeping),
        };
        self.swap_shown(incoming);
    }

    fn swap_shown(&mut self, incoming: Box<Tab>) {
        self.hide_shown_tab();
        let outgoing = std::mem::replace(&mut self.tab, incoming);
        self.tabs.park(outgoing);
        self.show_current_tab();
    }

    pub(super) fn run_tab_shortcut(&mut self, shortcut: TabShortcut) {
        match shortcut {
            TabShortcut::New => self.open_blank_tab(),
            TabShortcut::Close => self.close_tab(self.tab.id),
            TabShortcut::Reopen => self.reopen_closed_tab(),
            TabShortcut::Next => self.cycle_tab(true),
            TabShortcut::Previous => self.cycle_tab(false),
            TabShortcut::Select(index) => {
                if let Some(&id) = self.tabs.order.get(index) {
                    self.activate_tab(id);
                }
            }
            TabShortcut::Last => {
                if let Some(&id) = self.tabs.order.last() {
                    self.activate_tab(id);
                }
            }
            TabShortcut::Quit => self.request_quit(),
        }
        self.request_redraw();
    }

    /// Ctrl+Tab and Ctrl+Shift+Tab: show the next or the previous tab.
    pub(super) fn cycle_tab(&mut self, forward: bool) {
        let count = self.tabs.order.len();
        let Some(index) = self.tabs.index_of(self.tab.id) else {
            return;
        };
        if count < 2 {
            return;
        }
        let next = if forward {
            (index + 1) % count
        } else {
            (index + count - 1) % count
        };
        self.activate_tab(self.tabs.order[next]);
    }

    /// The shown tab goes to the background: end the window's interactions
    /// with its page, tell the page it is hidden, and release what only
    /// presentation needs.
    fn hide_shown_tab(&mut self) {
        self.close_context_menu();
        self.close_select_popup();
        self.release_native_pointer_lock(true);
        if std::mem::take(&mut self.page_fullscreen) {
            // Fullscreen API: a document that is hidden exits fullscreen.
            self.fullscreen_reply = None;
            if let Some(window) = &self.window {
                window.set_fullscreen(None);
            }
            self.tab.browser.handle_action(UserAction::ExitFullscreen);
        }
        self.composing = false;
        self.page_pointer_buttons = 0;
        self.pressed_control = None;
        self.tab.text_selection.gesture = None;
        self.tab.heart_drag = None;
        self.tab.heart_hover = None;
        self.tab.pressed_hit = None;
        if self.tab.hovered_actor.take().is_some() {
            let metadata = self.pointer_metadata();
            self.tab.browser.handle_action(UserAction::PageHover {
                actor: None,
                position: CssPoint::default(),
                metadata,
            });
        }
        self.tab.link_preview.clear();
        self.tab.last_shown = Instant::now();
        release_presentation(&mut self.tab);
        self.set_active_animations(HashSet::new());
        self.tab.browser.handle_action(UserAction::Focus(false));
        self.tab.browser.handle_action(UserAction::Visible(false));
        self.schedule_sleep_check(Instant::now() + SLEEP_AFTER);
    }

    /// `self.tab` was just swapped in: present it.
    fn show_current_tab(&mut self) {
        self.requeue_unload_prompt_of_hidden_tab();
        self.tab.last_shown = Instant::now();
        let presentation = self.presentation(self.tab.focus, true);
        presentation.apply(&mut self.tab.browser);
        // The retained scene shows the previous tab; nothing may hit-test it.
        self.scene = None;
        self.force_full_raster = true;
        self.tab.image_schedule_key = None;
        self.tab.browser_events_pending = true;
        self.strip.reveal_active = true;
        let focus = self.tab.focus;
        self.set_focus(focus);
        // A tab woken from sleep loads its page now that it is shown.
        if self.window.is_some()
            && let Some(address) = self.tab.initial_navigation.take()
        {
            self.navigate(address);
        }
        self.track_background_loading();
        self.request_redraw();
        self.update_accessibility(false);
    }

    /// Background pages keep the window's geometry: a tab shown later
    /// presents in the viewport the window has then, and its scripts see
    /// the window's size meanwhile.
    pub(super) fn present_background_tabs(&mut self) {
        let presentations: Vec<(TabId, Presentation)> = self
            .tabs
            .order
            .iter()
            .filter(|&&id| id != self.tab.id)
            .filter_map(|&id| match self.tabs.hidden(id) {
                Some(Hidden::Awake(tab)) => Some((id, self.presentation(tab.focus, false))),
                _ => None,
            })
            .collect();
        for (id, presentation) in presentations {
            if let Some(tab) = self.tabs.awake_mut(id) {
                presentation.apply(&mut tab.browser);
            }
        }
    }

    /// A sleeping tab is shown again: its page loads anew, as a Document
    /// that replaces a discarded one (Page Lifecycle #discarding), at the
    /// place in its history and on the page where the user left it.
    fn wake_sleeping_tab(&mut self, id: TabId, sleeping: SleepingTab) -> Box<Tab> {
        let mut browser = new_controller(&self.runtime, &self.event_proxy, id);
        browser.set_clipboard_available(self.clipboard.is_some());
        let mut tab = Box::new(Tab::new(id, browser, None));
        tab.opened_from = sleeping.opened_from;
        tab.focus = FocusTarget::Page;
        self.presentation(tab.focus, true).apply(&mut tab.browser);
        tab.browser.restore_session_history(sleeping.history, true);
        tab
    }

    /// Whether a background tab must stay in memory however long it goes
    /// unseen: a Telnet session, a page making sound or holding the user's
    /// unsubmitted edits, a form's POST result (loading it again would
    /// resubmit), a download offer, or a page still loading.
    fn keeps_awake(tab: &Tab) -> bool {
        tab.terminal.is_some()
            || tab.bookmark_terminal.is_some()
            || tab.browser.is_audible()
            || tab.browser.has_user_edits()
            || tab.browser.is_post_result()
            || tab.browser.download_offer().is_some()
            || tab.browser.is_loading()
            // Other traversables of its browsing context group reach its
            // Windows (HTML #browsing-context-group-set).
            || tab.browser.in_shared_group()
    }

    /// Put to sleep every background tab unseen for `SLEEP_AFTER`, and
    /// schedule the next check for the earliest tab that could sleep then.
    pub(super) fn sleep_unseen_tabs(&mut self) {
        let now = Instant::now();
        let mut next: Option<Instant> = None;
        let candidates: Vec<TabId> = self
            .tabs
            .order
            .iter()
            .copied()
            .filter(|&id| id != self.tab.id)
            .collect();
        for id in candidates {
            // A page being asked whether it may be left stays for its answer.
            let asked = self.leaving.contains_key(&id)
                || self.queued_unload_prompts.iter().any(|(tab, _)| *tab == id);
            let Some(tab) = self.tabs.awake_mut(id) else {
                continue;
            };
            let due = tab.last_shown + SLEEP_AFTER;
            if asked || Self::keeps_awake(tab) {
                // Check again later: the exemption may end.
                let later = now + SLEEP_AFTER;
                next = Some(next.map_or(later, |next| next.min(later)));
                continue;
            }
            if due > now {
                next = Some(next.map_or(due, |next| next.min(due)));
                continue;
            }
            self.sleep_tab(id);
        }
        if let Some(at) = next {
            self.schedule_sleep_check(at);
        }
    }

    /// Release a background tab from memory (Page Lifecycle "discard"):
    /// its page actor and everything presenting it go; its title and
    /// session history stay for the strip and for waking it.
    fn sleep_tab(&mut self, id: TabId) {
        let Some(Hidden::Awake(tab)) = self.tabs.take_hidden(id) else {
            return;
        };
        let sleeping = SleepingTab {
            title: tab_title(&tab),
            history: tab.browser.session_history(),
            opened_from: tab.opened_from,
        };
        // Decoded pixels only this tab showed go with it.
        for handle in &tab.image_loads.completed {
            if !self.tab.image_loads.completed.contains(handle) {
                self.image_store.remove(*handle);
            }
        }
        let mut tab = tab;
        for (_, task) in tab.image_tasks.drain() {
            task.abort();
        }
        // Page Lifecycle #discarding: the page is frozen, not unloaded.
        tab.browser.discard();
        drop(tab);
        self.tabs
            .hidden
            .insert(id, Hidden::Asleep(Box::new(sleeping)));
        trust::release_allocator_memory();
        self.request_redraw();
    }

    /// Wake the event loop at `at` to put unseen tabs to sleep.
    pub(super) fn schedule_sleep_check(&mut self, at: Instant) {
        if self
            .sleep_check_scheduled
            .is_some_and(|scheduled| scheduled <= at)
        {
            return;
        }
        self.sleep_check_scheduled = Some(at);
        let proxy = self.event_proxy.clone();
        self.runtime.spawn(async move {
            tokio::time::sleep_until(tokio::time::Instant::from_std(at)).await;
            let _ = proxy.send_event(DesktopEvent::SleepCheck);
        });
    }

    /// Close tab `id` once its page allowed it (`close_tab`): its page
    /// unloads and the tab goes, to be reopened with Ctrl+Shift+T. Closing
    /// the shown one shows its right neighbor, else its left; the last tab
    /// is never closed alone (`close_tab` quits instead).
    pub(super) fn finish_close_tab(&mut self, id: TabId) {
        let Some(index) = self.tabs.index_of(id) else {
            return;
        };
        if self.tabs.order.len() == 1 {
            self.request_quit();
            return;
        }
        self.leaving.remove(&id);
        self.queued_unload_prompts.retain(|(tab, _)| *tab != id);
        if id == self.tab.id {
            let next = self.tabs.order[if index + 1 < self.tabs.order.len() {
                index + 1
            } else {
                index - 1
            }];
            let Some(hidden) = self.tabs.take_hidden(next) else {
                return;
            };
            let incoming = match hidden {
                Hidden::Awake(tab) => tab,
                Hidden::Asleep(sleeping) => self.wake_sleeping_tab(next, *sleeping),
            };
            self.hide_shown_tab();
            let closed = std::mem::replace(&mut self.tab, incoming);
            self.tabs.order.remove(index);
            self.tabs.remember_closed(ClosedTab {
                history: closed_history(&closed),
                index,
            });
            end_closed_tab(closed);
            self.show_current_tab();
        } else {
            self.tabs.order.remove(index);
            match self.tabs.take_hidden(id) {
                Some(Hidden::Awake(tab)) => {
                    self.tabs.remember_closed(ClosedTab {
                        history: closed_history(&tab),
                        index,
                    });
                    end_closed_tab(tab);
                }
                Some(Hidden::Asleep(sleeping)) => self.tabs.remember_closed(ClosedTab {
                    history: sleeping.history,
                    index,
                }),
                None => {}
            }
            self.request_redraw();
        }
    }

    /// Ctrl+Shift+T: reopen the most recently closed tab where it was,
    /// with its session history.
    pub(super) fn reopen_closed_tab(&mut self) {
        let Some(closed) = self.tabs.closed.pop() else {
            return;
        };
        let tab = self.create_tab(None, true);
        self.show_new_tab(tab, closed.index);
        match closed.history.current_address() {
            // A Telnet session connects again.
            Some(address) if parse_telnet_target(&address).is_some() => self.navigate(address),
            Some(_) => {
                self.tab
                    .browser
                    .restore_session_history(closed.history, false);
                self.request_redraw();
            }
            None => {
                self.tab.focus = FocusTarget::Command;
                let focus = self.tab.focus;
                self.set_focus(focus);
            }
        }
    }

    /// Process the events background tabs' controllers received. Their
    /// pages keep running; requests that need a visible, focused document
    /// are refused.
    pub(super) fn service_background_tabs(&mut self) {
        let woken = std::mem::take(&mut self.tabs.woken);
        let mut strip_changed = false;
        for id in woken {
            let Some(tab) = self.tabs.awake_mut(id) else {
                continue;
            };
            let before = strip_entry(tab);
            let outcome = tab.browser.process_async_events();
            refuse_background_requests(&mut tab.browser);
            if outcome.loading_retired {
                tab.pending_page_keys.clear();
                tab.queued_form_keys.clear();
                release_presentation(tab);
            }
            strip_changed |= before != strip_entry(tab);
            self.open_requested_traversables(id);
            self.collect_unload_requests(id);
            self.service_traversable_requests(id);
        }
        self.track_background_loading();
        if strip_changed {
            self.request_strip_redraw();
            self.update_accessibility(false);
        }
    }

    /// Note whether a background tab is loading, for the strip's jewels.
    pub(super) fn track_background_loading(&mut self) {
        let loading = self
            .tabs
            .hidden
            .values()
            .any(|hidden| matches!(hidden, Hidden::Awake(tab) if tab.browser.is_loading()));
        match (loading, self.background_loading_since) {
            (true, None) => self.background_loading_since = Some(Instant::now()),
            (false, Some(_)) => self.background_loading_since = None,
            _ => {}
        }
        self.schedule_strip_tick();
    }

    /// The strip's view of every tab, in strip order.
    pub(super) fn strip_tabs(&self) -> Vec<TabStripTab> {
        self.tabs
            .order
            .iter()
            .map(|&id| {
                if id == self.tab.id {
                    return strip_entry(&self.tab);
                }
                match self.tabs.hidden(id) {
                    Some(Hidden::Awake(tab)) => strip_entry(tab),
                    Some(Hidden::Asleep(sleeping)) => TabStripTab {
                        title: sleeping.title.clone(),
                        asleep: true,
                        ..Default::default()
                    },
                    None => TabStripTab::default(),
                }
            })
            .collect()
    }
}

fn strip_entry(tab: &Tab) -> TabStripTab {
    TabStripTab {
        title: tab_title(tab),
        terminal: tab.terminal.is_some(),
        loading: tab.browser.is_loading(),
        audible: tab.browser.is_audible(),
        asleep: false,
    }
}

/// The history a closed tab reopens with; a Telnet session's is its
/// address.
fn closed_history(tab: &Tab) -> trust::core::SessionHistory {
    match &tab.terminal {
        Some(terminal) => trust::core::SessionHistory::for_address(&terminal.address),
        None => tab.browser.session_history(),
    }
}

/// Drop a closed tab and everything it holds: its page unloads (HTML
/// #unload-a-document-and-its-descendants), and its Telnet session and
/// image work end.
fn end_closed_tab(mut tab: Box<Tab>) {
    for (_, task) in tab.image_tasks.drain() {
        task.abort();
    }
    tab.browser.close();
    drop(tab);
}
