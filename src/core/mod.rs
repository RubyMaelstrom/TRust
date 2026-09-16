//! Frontend-neutral browser controller, input vocabulary, and coordinate types.
//!
//! CSSOM View exposes viewport and pointer geometry in CSS pixels, while the
//! native window surface is sized in physical device pixels.  This module keeps
//! those spaces distinct; only a frontend renderer applies [`ScaleFactor`].
//! See CSSOM View §4 and CSS Values and Units §6.2.

use std::collections::VecDeque;
use std::sync::Arc;
#[cfg(test)]
use std::sync::mpsc;

use tokio::runtime::Handle;
use tokio::task::JoinHandle;

use crate::doc::Link;
use crate::{gemini, gopher, http, oneshot};

mod events;

/// A position in logical/CSS pixels relative to the content viewport.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CssPoint {
    pub x: f32,
    pub y: f32,
}

impl CssPoint {
    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }
}

/// A size in logical/CSS pixels. This is the coordinate space layout consumes.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CssSize {
    pub width: f32,
    pub height: f32,
}

impl CssSize {
    pub fn new(width: f32, height: f32) -> Self {
        Self {
            width: finite_non_negative(width),
            height: finite_non_negative(height),
        }
    }
}

/// A size in physical framebuffer pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PhysicalSize {
    pub width: u32,
    pub height: u32,
}

impl PhysicalSize {
    pub const fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }

    pub const fn is_empty(self) -> bool {
        self.width == 0 || self.height == 0
    }
}

/// Device pixels per logical/CSS pixel, normally supplied by the windowing OS.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScaleFactor(f64);

impl ScaleFactor {
    pub fn new(value: f64) -> Self {
        if value.is_finite() && value > 0.0 {
            Self(value)
        } else {
            Self(1.0)
        }
    }

    pub const fn get(self) -> f64 {
        self.0
    }
}

impl Default for ScaleFactor {
    fn default() -> Self {
        Self(1.0)
    }
}

/// The explicit relationship between CSS layout and the physical surface.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ViewportMetrics {
    pub css: CssSize,
    pub physical: PhysicalSize,
    pub scale_factor: ScaleFactor,
}

impl ViewportMetrics {
    pub fn from_physical(physical: PhysicalSize, scale_factor: ScaleFactor) -> Self {
        let scale = scale_factor.get();
        Self {
            css: CssSize::new(
                (f64::from(physical.width) / scale) as f32,
                (f64::from(physical.height) / scale) as f32,
            ),
            physical,
            scale_factor,
        }
    }

    /// Convert a native physical-pixel pointer position to CSS coordinates.
    pub fn physical_to_css(self, x: f64, y: f64) -> CssPoint {
        let scale = self.scale_factor.get();
        CssPoint::new((x / scale) as f32, (y / scale) as f32)
    }
}

fn finite_non_negative(value: f32) -> f32 {
    if value.is_finite() {
        value.max(0.0)
    } else {
        0.0
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Modifiers {
    pub shift: bool,
    pub control: bool,
    pub alt: bool,
    pub meta: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Key {
    Character(String),
    Enter,
    Escape,
    Backspace,
    Delete,
    Tab,
    ArrowLeft,
    ArrowRight,
    ArrowUp,
    ArrowDown,
    Home,
    End,
    PageUp,
    PageDown,
    Other(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyState {
    Pressed,
    Released,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyInput {
    pub key: Key,
    /// UI Events physical key identity; empty when the input source cannot provide it.
    pub code: String,
    pub location: u32,
    pub state: KeyState,
    pub modifiers: Modifiers,
    pub repeat: bool,
    pub composing: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointerButton {
    Primary,
    Auxiliary,
    Secondary,
    Back,
    Forward,
    Other(u16),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ButtonState {
    Pressed,
    Released,
}

/// Unit for a wheel/touchpad delta, mirroring UI Events' `deltaMode`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollUnit {
    CssPixel,
    Line,
    Page,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScrollDelta {
    pub dx: f32,
    pub dy: f32,
    pub unit: ScrollUnit,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImeAction {
    Enabled,
    Preedit {
        text: String,
        cursor: Option<(usize, usize)>,
    },
    Commit(String),
    Disabled,
}

/// Input and browser commands accepted by the shared controller. Native
/// frontends translate their own event types once at their boundary.
#[derive(Clone, Debug, PartialEq)]
pub enum UserAction {
    Navigate(String),
    Back,
    Forward,
    Reload,
    Stop,
    Resize(CssSize),
    /// Output-device pixels per CSS pixel (monitor scale / browser zoom
    /// density), kept separate from CSS viewport geometry.
    DevicePixelRatio(f32),
    /// Client-window origin in CSS pixels, independent of viewport scrolling.
    ScreenPosition(i32, i32),
    Focus(bool),
    PointerMove(CssPoint),
    PointerButton {
        position: CssPoint,
        button: PointerButton,
        state: ButtonState,
    },
    Scroll(ScrollDelta),
    SetViewportScroll(CssPoint),
    Key(KeyInput),
    TextInput(String),
    Ime(ImeAction),
    /// Activate a semantic page target produced by layout hit testing.
    Activate(Link),
    /// Pointer transition target in the resident JavaScript actor.
    PageHover {
        actor: Option<usize>,
        position: CssPoint,
    },
    PagePointerButton {
        actor: Option<usize>,
        position: CssPoint,
        pressed: bool,
    },
    /// Move native page focus, including clicking non-interactive content.
    /// This is distinct from the window's system-focus notification above.
    PageFocus {
        actor: Option<usize>,
    },
    /// User-edited value/checkedness for a live form control.
    SetFormValue {
        actor: Option<usize>,
        value: String,
        checked: Option<bool>,
    },
    StepNumber {
        node: usize,
        direction: i8,
        click: bool,
    },
    /// Deliver a native key press to the focused live DOM node. The resident
    /// actor reports whether to suppress the frontend's editing/form default.
    PageKey {
        node: usize,
        input: KeyInput,
    },
    /// Submit through the live actor first, falling back to HTML's native
    /// application/x-www-form-urlencoded submission when not canceled.
    SubmitForm {
        form: crate::doc::Form,
        submitter: Option<usize>,
    },
    SetNestedScroll {
        actor: Option<usize>,
        top: f32,
        left: f32,
    },
}

/// Cross-thread invalidation target. The winit frontend implements this with
/// `EventLoopProxy`; the browser controller never depends on winit itself.
pub trait WakeSink: Send + Sync {
    fn wake(&self);
}

/// Cloneable invalidation hook for network, image, JavaScript, and other
/// asynchronous browser workers. Calling it schedules native event-loop work;
/// it never renders on the worker thread.
#[derive(Clone)]
pub struct InvalidationHandle {
    wake: Arc<dyn WakeSink>,
}

impl InvalidationHandle {
    pub fn request_redraw(&self) {
        self.wake.wake();
    }
}

impl<F> WakeSink for F
where
    F: Fn() + Send + Sync,
{
    fn wake(&self) {
        self();
    }
}

#[derive(Clone, Debug)]
struct HistoryEntry {
    target: Link,
    fallback_http: bool,
    /// Trusted in-process documents have no transport to refetch from.
    /// Retain their small source so Back/Forward uses the same history path.
    internal_source: Option<Vec<u8>>,
    dict_view: Option<crate::dict::SavedView>,
    gopher_page: Option<gopher::Page>,
    gemini_page: Option<Box<gemini::Response>>,
    scroll: CssPoint,
}

impl HistoryEntry {
    fn from_page(page: BrowserPage, scroll: CssPoint) -> Self {
        let dict_view = match &page.document {
            FetchedDocument::Dict(dict) => Some(dict.saved_view()),
            _ => None,
        };
        let (internal_source, gopher_page, gemini_page) = match page.document {
            FetchedDocument::Internal(source) => (Some(source), None, None),
            FetchedDocument::Gopher(page) => (None, Some(page), None),
            FetchedDocument::Gemini(response) => (None, None, Some(response)),
            _ => (None, None, None),
        };
        Self {
            target: page.target,
            fallback_http: page.fallback_http,
            internal_source,
            dict_view,
            gopher_page,
            gemini_page,
            scroll,
        }
    }
}

/// Raw protocol result retained by the shared controller. HTTP responses feed
/// the canonical DOM/CSS-pixel graphical layout path; the other protocol
/// presentation models can be added without changing navigation or wakeups.
#[derive(Debug)]
pub enum FetchedDocument {
    Gopher(crate::gopher::Page),
    Gemini(Box<gemini::Response>),
    Http(Box<http::Response>),
    OneShot(Vec<u8>),
    Finger(crate::finger::Page),
    Whois(crate::whois::Page),
    Rdap(crate::rdap::Page),
    Dict(Box<crate::dict::Page>),
    /// A trusted, in-process Gemtext document such as `about:help`.
    Internal(Vec<u8>),
}

#[derive(Debug)]
pub struct BrowserPage {
    target: Link,
    fallback_http: bool,
    pub document: FetchedDocument,
    pub status: String,
    /// Latest canonical actor render. Native frontends consume this typed
    /// CSS-pixel product directly; it is the presentation authority.
    rendered: Option<http::RenderedPage>,
    /// Revision of the latest complete typed actor render. Pure interaction
    /// transforms can advance `revision` without replacing this pixel product.
    rendered_revision: u64,
    revision: u64,
}

impl BrowserPage {
    pub fn address(&self) -> String {
        self.target.to_string()
    }

    pub fn rendered_page(&self) -> Option<&http::RenderedPage> {
        self.rendered.as_ref().or(match &self.document {
            FetchedDocument::Http(response) => response.rendered.as_deref(),
            _ => None,
        })
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn rendered_revision(&self) -> u64 {
        self.rendered_revision
    }

    pub fn target(&self) -> &Link {
        &self.target
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NavigationIntent {
    New,
    Replace,
    Reload,
    Back,
    Forward,
}

impl NavigationIntent {
    fn timing_type(self) -> http::NavigationType {
        match self {
            Self::Reload => http::NavigationType::Reload,
            Self::Back | Self::Forward => http::NavigationType::BackForward,
            Self::New | Self::Replace => http::NavigationType::Navigate,
        }
    }
}

#[derive(Clone, Debug)]
struct PendingNavigation {
    generation: u64,
    target: Link,
    fallback_http: bool,
    intent: NavigationIntent,
}

#[derive(Debug)]
enum CoreEvent {
    Gemini {
        generation: u64,
        response: Box<crate::gemini::Response>,
    },
    GeminiImage {
        generation: u64,
        url: gemini::GeminiUrl,
        response: Box<http::Response>,
        status: String,
    },
    Gopher {
        generation: u64,
        reply: crate::text_reply::Reply,
    },
    Dict {
        generation: u64,
        reply: crate::dict::Reply,
    },
    Whois {
        generation: u64,
        reply: crate::whois::Reply,
    },
    Finger {
        generation: u64,
        reply: crate::finger::Reply,
    },
    UserInputReady {
        generation: u64,
        permit: Option<tokio::sync::mpsc::OwnedPermit<crate::js::PageCmd>>,
    },
    FetchFinished {
        generation: u64,
        result: Result<FetchedDocument, String>,
    },
    ExternalMedia {
        generation: u64,
        url: url::Url,
    },
    Download {
        generation: u64,
        response: Box<http::Response>,
    },
    GopherDownload {
        generation: u64,
        offer: Box<crate::download::DownloadOffer>,
    },
    Page {
        generation: u64,
        event: crate::js::PageEvt,
    },
    DeclarativeRefresh {
        generation: u64,
        url: url::Url,
    },
}

enum InteractiveFetch {
    Document(FetchedDocument),
    ExternalMedia(url::Url),
    Download(Box<http::Response>),
    GopherDownload(Box<crate::download::DownloadOffer>),
}

/// Read-only state used by graphical chrome.
#[derive(Clone, Debug)]
pub struct BrowserSnapshot {
    pub address: String,
    pub status: String,
    pub loading: bool,
    pub can_go_back: bool,
    pub can_go_forward: bool,
    pub focused: bool,
    pub viewport: CssSize,
    pub page_revision: u64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct InteractionState {
    pub focused: bool,
    pub pointer: Option<CssPoint>,
    pub scroll: CssPoint,
    pub ime: Option<ImeAction>,
    pub nested_scroll: std::collections::HashMap<usize, CssPoint>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ActionOutcome {
    pub invalidated: bool,
    /// The current document crossed a load-retirement boundary. Frontends must
    /// stop their own document-scoped work while retaining completed pixels
    /// needed to paint the frozen page until the replacement commits.
    pub loading_retired: bool,
}

/// Durable browser/protocol controller shared by native frontends.
///
/// It owns navigation state and production fetch tasks, but no Ratatui,
/// Crossterm, terminal image, or winit types. Completion is message-driven:
/// background work sends one event and invokes [`WakeSink`], so a frontend can
/// keep its native event loop in a blocking wait state at idle.
pub struct BrowserController {
    runtime: Handle,
    invalidation: InvalidationHandle,
    tx: events::Sender,
    rx: events::Receiver,
    current: Option<BrowserPage>,
    back: Vec<HistoryEntry>,
    forward: Vec<HistoryEntry>,
    pending: Option<PendingNavigation>,
    task: Option<JoinHandle<()>>,
    declarative_refresh_task: Option<JoinHandle<()>>,
    live_task: Option<JoinHandle<()>>,
    live_page: Option<crate::js::PageHandle>,
    /// Whether the current document's presentation is final: neither fetch nor
    /// a resident actor will deliver another render for it. See
    /// [`BrowserController::page_render_is_final`].
    render_is_final: bool,
    pending_live_submit: Option<(crate::doc::Form, Option<usize>)>,
    /// Default-action results for keyboard events delivered to the resident
    /// page. Native frontends consume these after the actor runs `keydown`.
    page_key_defaults: VecDeque<bool>,
    pending_form_values: std::collections::HashMap<usize, usize>,
    /// Preserve native input FIFO while the actor's bounded channel is full.
    pending_user_input: VecDeque<(crate::js::PageCmd, bool)>,
    user_input_retry: Option<JoinHandle<()>>,
    pending_fragment: Option<String>,
    external_media: VecDeque<(url::Url, Option<url::Url>)>,
    download_offer: Option<crate::download::DownloadOffer>,
    gemini_prompt: Option<crate::gemini::Prompt>,
    storage: crate::js::WebStorage,
    external_address: Option<String>,
    generation: u64,
    document_generation: u64,
    status: String,
    viewport: CssSize,
    device_pixel_ratio: f32,
    screen_position: (i32, i32),
    interaction: InteractionState,
    live_regions: Vec<usize>,
    live_boundaries: Vec<usize>,
    /// Most recent page-script outcome, for headless diagnostics: error text,
    /// captured console output, module skips, panic flag, and fetch count.
    last_js_outcome: Option<crate::js::Outcome>,
}

fn event_variant_name(event: &crate::js::PageEvt) -> &'static str {
    match event {
        crate::js::PageEvt::Updated { .. } => "Updated",
        crate::js::PageEvt::Patched { .. } => "Patched",
        crate::js::PageEvt::Static { .. } => "Static",
        crate::js::PageEvt::Navigate(_) => "Navigate",
        crate::js::PageEvt::Replace(_) => "Replace",
        crate::js::PageEvt::Reload(_) => "Reload",
        crate::js::PageEvt::HistoryUpdate { .. } => "HistoryUpdate",
        crate::js::PageEvt::ScrollToFragment(_) => "ScrollToFragment",
        crate::js::PageEvt::Trouble(_) => "Trouble",
        crate::js::PageEvt::Settled => "Settled",
        _ => "Other",
    }
}

impl BrowserController {
    pub fn new(runtime: Handle, wake: impl WakeSink + 'static, viewport: CssSize) -> Self {
        let invalidation = InvalidationHandle {
            wake: Arc::new(wake),
        };
        let (tx, rx) = events::channel(invalidation.clone());
        Self {
            runtime,
            invalidation,
            tx,
            rx,
            current: None,
            back: Vec::new(),
            forward: Vec::new(),
            pending: None,
            task: None,
            declarative_refresh_task: None,
            live_task: None,
            live_page: None,
            render_is_final: false,
            pending_live_submit: None,
            page_key_defaults: VecDeque::new(),
            pending_form_values: Default::default(),
            pending_user_input: VecDeque::new(),
            user_input_retry: None,
            pending_fragment: None,
            external_media: VecDeque::new(),
            download_offer: None,
            gemini_prompt: None,
            storage: crate::site_storage::web_storage(),
            external_address: None,
            generation: 0,
            document_generation: 0,
            status: String::from("Ready"),
            viewport,
            device_pixel_ratio: 1.0,
            screen_position: (0, 0),
            interaction: InteractionState::default(),
            live_regions: Vec::new(),
            live_boundaries: Vec::new(),
            last_js_outcome: None,
        }
    }

    pub fn gemini_prompt(&self) -> Option<&crate::gemini::Prompt> {
        self.gemini_prompt.as_ref()
    }
    pub fn cancel_gemini_prompt(&mut self) {
        self.gemini_prompt = None;
    }
    pub fn submit_gemini_prompt(&mut self, text: &str) -> bool {
        let Some(prompt) = self.gemini_prompt.take() else {
            return false;
        };
        match prompt.submit(text) {
            Ok(url) => self.begin_fetch(Link::Gemini(url), false, NavigationIntent::New),
            Err(error) => {
                self.status = error;
                self.gemini_prompt = Some(prompt);
            }
        }
        self.invalidation.request_redraw();
        true
    }

    pub fn snapshot(&self) -> BrowserSnapshot {
        BrowserSnapshot {
            address: self
                .pending
                .as_ref()
                .map(|p| p.target.to_string())
                .or_else(|| self.current.as_ref().map(BrowserPage::address))
                .or_else(|| self.external_address.clone())
                .unwrap_or_default(),
            status: self.status.clone(),
            loading: self.pending.is_some(),
            can_go_back: !self.back.is_empty(),
            can_go_forward: !self.forward.is_empty(),
            focused: self.interaction.focused,
            viewport: self.viewport,
            page_revision: self.current.as_ref().map_or(0, BrowserPage::revision),
        }
    }

    pub fn current_page(&self) -> Option<&BrowserPage> {
        self.current.as_ref()
    }

    /// Take a playback URL delegated by navigation. External-process spawning
    /// remains a frontend responsibility, while classification is centralized
    /// with navigation so typed, clicked, redirected, and scripted URLs agree.
    pub fn take_external_media(&mut self) -> Option<(url::Url, Option<url::Url>)> {
        self.external_media.pop_front()
    }

    pub fn download_offer(&self) -> Option<&crate::download::DownloadOffer> {
        self.download_offer.as_ref()
    }

    pub fn take_download_offer(&mut self) -> Option<crate::download::DownloadOffer> {
        self.download_offer.take()
    }

    pub fn dismiss_download_offer(&mut self) -> bool {
        let dismissed = self.download_offer.take().is_some();
        if dismissed {
            self.invalidation.request_redraw();
        }
        dismissed
    }

    pub fn page_is_live(&self) -> bool {
        self.live_page.is_some()
    }

    /// Whether the current presentation is final, so a caller can stop waiting
    /// for renders without guessing from silence.
    ///
    /// This is the engine's own quiescence, not a wall-clock heuristic. It is
    /// true in exactly these cases:
    ///
    /// * a document committed and got no resident actor at all (a script-free
    ///   article, a Gopher menu, a Gemini capsule, an internal gemtext page,
    ///   or a failed fetch) — its first render is its last;
    /// * the resident actor classified the document as inert and sent
    ///   [`PageEvt::Static`], which retires the actor.
    ///
    /// * navigation was stopped before a document committed; there is no page
    ///   to wait for in that case;
    ///
    /// It stays false while a fetch is in flight and, crucially, for as long as
    /// the actor keeps a document alive for timers, workers, pending fetches,
    /// hover, or scroll work — a page with a timer loop is never reported final
    /// just because it happens to be quiet right now. It is false before any
    /// navigation, and true after a failed one, because no render can arrive
    /// from a document that never committed.
    ///
    /// The hover and scroll clauses deserve a warning: a page that merely styles
    /// `a:hover` in a way that can change its render keeps the actor resident
    /// forever, so on much of the web this never answers true even though no
    /// further render is in fact pending. Those clauses say why the engine must
    /// stay *reachable*, not why it must keep *painting*, and a caller that
    /// wants to stop waiting is asking the painting question. Until the actor
    /// answers the two separately, callers must keep a bounded fallback wait.
    ///
    /// `PageEvt::Settled` is deliberately not part of this: it acknowledges one
    /// dispatch that changed no pixels, not page quiescence.
    /// Most recent page-script outcome (errors, console output, panic flag):
    /// surfaced by `trust-headless --js-diagnostics` for headless debugging of
    /// pages whose scripts misbehave; `None` before the first render event.
    pub fn last_js_outcome(&self) -> Option<&crate::js::Outcome> {
        self.last_js_outcome.as_ref()
    }

    pub fn page_render_is_final(&self) -> bool {
        self.render_is_final
    }

    /// Take the next resident-page keyboard default result. `true` suppresses
    /// the native default (canceled keydown/keypress, IME composition, or Enter
    /// in a formless input); `false` lets the frontend apply editing/submission.
    pub fn take_page_key_default(&mut self) -> Option<bool> {
        self.page_key_defaults.pop_front()
    }

    /// Whether a native value is still ahead of the actor's canonical DOM.
    pub fn form_value_pending(&self, node: usize) -> bool {
        self.pending_form_values.contains_key(&node)
    }

    /// Publish graphical layout boundaries the resident actor may target.
    /// Values are canonical actor arena ids carried directly by the typed
    /// layout. Duplicate layouts are free; only a changed set crosses the
    /// actor channel.
    pub fn set_live_layout_boundaries(
        &mut self,
        mut regions: Vec<usize>,
        mut boundaries: Vec<usize>,
    ) {
        regions.sort_unstable();
        regions.dedup();
        boundaries.sort_unstable();
        boundaries.dedup();
        if regions != self.live_regions
            && self.send_live(crate::js::PageCmd::LiveRegions(regions.clone()))
        {
            self.live_regions = regions;
        }
        if boundaries != self.live_boundaries
            && self.send_live(crate::js::PageCmd::LiveBoundaries(boundaries.clone()))
        {
            self.live_boundaries = boundaries;
        }
    }

    /// Ask the resident page actor for the always-correct complete typed render.
    /// Retained for compatibility with the legacy patch protocol.
    pub fn request_live_resync(&self) {
        self.send_live(crate::js::PageCmd::Resync);
    }

    /// Identity of the committed document. It changes only when a replacement
    /// document commits, never while the old document remains visibly frozen
    /// behind an in-progress or failed navigation.
    pub fn document_generation(&self) -> u64 {
        self.document_generation
    }

    pub fn invalidation_handle(&self) -> InvalidationHandle {
        self.invalidation.clone()
    }

    pub fn interaction(&self) -> &InteractionState {
        &self.interaction
    }

    pub fn set_status(&mut self, status: impl Into<String>) {
        self.status = status.into();
        self.invalidation.request_redraw();
    }

    /// Apply the process-wide in-memory cookie preference through the shared
    /// browser boundary, so frontends do not reach through to HTTP internals.
    pub fn set_cookies_enabled(&mut self, enabled: bool) {
        http::set_cookies_enabled(enabled);
        self.set_status(if enabled {
            "Cookies on: first-party access; bookmarked sites may remember cookies."
        } else {
            "Cookies off."
        });
    }

    /// Start a typed COMMAND `post` through the same request and navigation
    /// commit path used by an HTML form submission.
    pub fn post(&mut self, url: url::Url, body: String) -> ActionOutcome {
        let generation_before = self.generation;
        self.begin_post(url, body);
        self.invalidation.request_redraw();
        ActionOutcome {
            invalidated: true,
            loading_retired: self.generation != generation_before,
        }
    }

    /// Commit a trusted in-process Gemtext document through the ordinary
    /// graphical document/history path. This is the desktop counterpart of
    /// the terminal frontend's `about:` pages and keeps their shared source
    /// scrollable, selectable, and reachable through browser history.
    pub fn open_internal_gemtext(
        &mut self,
        address: impl Into<String>,
        source: impl Into<Vec<u8>>,
    ) -> ActionOutcome {
        let generation_before = self.generation;
        let address = address.into();
        let intent = if self
            .current
            .as_ref()
            .is_some_and(|p| p.target == Link::External(address.clone()))
        {
            NavigationIntent::Reload
        } else {
            NavigationIntent::New
        };
        self.begin_internal_gemtext(Link::External(address), source.into(), intent);
        ActionOutcome {
            invalidated: true,
            loading_retired: self.generation != generation_before,
        }
    }

    fn begin_internal_gemtext(&mut self, target: Link, source: Vec<u8>, intent: NavigationIntent) {
        self.gemini_prompt = None;
        self.download_offer = None;
        if let Some(task) = self.task.take() {
            task.abort();
        }
        self.retire_query("Incomplete reply: left this page");
        self.abort_declarative_refresh();
        self.drop_live_page();
        self.external_address = None;
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        self.pending = Some(PendingNavigation {
            generation,
            target,
            fallback_http: false,
            intent,
        });
        // `finish_fetch` is deliberately reused so internal documents obey
        // exactly the same history, scroll reset, and commit transitions as a
        // protocol response, without starting a network task.
        let _ = self.finish_fetch(generation, Ok(FetchedDocument::Internal(source)));
        self.invalidation.request_redraw();
    }

    /// Install a frontend-owned protocol view (currently the graphical VT
    /// session) into shared chrome without introducing terminal-emulator state
    /// into the browser/layout core.
    pub fn open_external_session(&mut self, address: impl Into<String>) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
        self.retire_query("Incomplete reply: left this page");
        self.abort_declarative_refresh();
        self.drop_live_page();
        self.pending = None;
        if let Some(old) = self.current.take() {
            self.back
                .push(HistoryEntry::from_page(old, self.interaction.scroll));
            self.forward.clear();
        }
        self.trim_gopher_history();
        self.external_address = Some(address.into());
        self.generation = self.generation.wrapping_add(1);
        self.document_generation = self.generation;
        self.status = String::from("Connecting terminal session …");
        self.invalidation.request_redraw();
    }

    pub fn take_fragment_request(&mut self) -> Option<String> {
        self.pending_fragment.take()
    }

    /// Queue the decoded intrinsic-size map for the resident page. The native
    /// frontend calls this from an event loop and must never block; a full
    /// command queue is therefore a normal retry condition, not a successful
    /// delivery. This mirrors the terminal frontend's `image_sizes_sent`
    /// acknowledgement discipline and keeps CSSOM/layout geometry convergent
    /// with the decoded image resources (CSSOM View + HTML image update).
    pub fn send_image_sizes(&self, sizes: &crate::layout2::ImageSizes) -> bool {
        let Some(handle) = &self.live_page else {
            return false;
        };
        let values = sizes
            .iter()
            .map(|(source, size)| (source.clone(), *size))
            .collect();
        handle
            .cmds
            .try_send(crate::js::PageCmd::ImageSizes(values))
            .is_ok()
    }

    pub fn handle_action(&mut self, action: UserAction) -> ActionOutcome {
        let generation_before = self.generation;
        let invalidated = match action {
            UserAction::Navigate(address) => self.begin_address(&address, NavigationIntent::New),
            UserAction::Back => self.begin_history(false),
            UserAction::Forward => self.begin_history(true),
            UserAction::Reload => {
                let Some(current) = &self.current else {
                    self.status = String::from("Nothing to reload.");
                    return ActionOutcome {
                        invalidated: true,
                        loading_retired: false,
                    };
                };
                self.begin_fetch(
                    current.target.clone(),
                    current.fallback_http,
                    NavigationIntent::Reload,
                );
                true
            }
            UserAction::Stop => self.stop(),
            UserAction::Resize(size) => {
                if self.viewport == size {
                    false
                } else {
                    self.viewport = size;
                    self.send_live(crate::js::PageCmd::Viewport(layout_viewport(size)));
                    true
                }
            }
            UserAction::DevicePixelRatio(ratio) => {
                let ratio = if ratio.is_finite() && ratio > 0.0 {
                    ratio
                } else {
                    1.0
                };
                if self.device_pixel_ratio == ratio {
                    false
                } else {
                    self.device_pixel_ratio = ratio;
                    self.send_live(crate::js::PageCmd::DevicePixelRatio(ratio));
                    true
                }
            }
            UserAction::ScreenPosition(x, y) => {
                if self.screen_position != (x, y) {
                    self.screen_position = (x, y);
                    self.send_live(crate::js::PageCmd::ScreenPosition(x, y));
                }
                false
            }
            UserAction::Focus(focused) => {
                let changed = self.interaction.focused != focused;
                self.interaction.focused = focused;
                changed
            }
            UserAction::PointerMove(point) => {
                self.interaction.pointer = Some(point);
                // Raw pointer coordinates are interaction state, not a visual
                // invalidation. Frontends separately send `PageHover` when
                // hit testing crosses a semantic target; that transition (or
                // the resulting live-DOM wake) is what can change pixels.
                false
            }
            UserAction::PagePointerButton {
                actor,
                position,
                pressed,
            } => {
                self.send_user(crate::js::PageCmd::PointerButton {
                    node: actor,
                    pressed,
                    x: f64::from(position.x),
                    y: f64::from(position.y),
                });
                false
            }
            UserAction::PointerButton { position, .. } => {
                let changed = self.interaction.pointer != Some(position);
                self.interaction.pointer = Some(position);
                changed
            }
            UserAction::Scroll(delta) => {
                let multiplier = match delta.unit {
                    ScrollUnit::CssPixel => 1.0,
                    ScrollUnit::Line => 40.0,
                    ScrollUnit::Page => self.viewport.height.max(1.0) * 0.9,
                };
                self.interaction.scroll.x =
                    (self.interaction.scroll.x + delta.dx * multiplier).max(0.0);
                self.interaction.scroll.y =
                    (self.interaction.scroll.y + delta.dy * multiplier).max(0.0);
                self.send_user(crate::js::PageCmd::Scroll {
                    x: f64::from(self.interaction.scroll.x),
                    y: f64::from(self.interaction.scroll.y),
                });
                true
            }
            UserAction::SetViewportScroll(point) => {
                self.interaction.scroll = CssPoint::new(point.x.max(0.0), point.y.max(0.0));
                self.send_user(crate::js::PageCmd::Scroll {
                    x: f64::from(self.interaction.scroll.x),
                    y: f64::from(self.interaction.scroll.y),
                });
                true
            }
            UserAction::Ime(ime) => {
                self.interaction.ime = Some(ime);
                true
            }
            // Interaction stays renderer/window-system neutral: a desktop hit
            // resolves to a semantic link or actor before crossing this API.
            UserAction::Activate(link) => self.activate(link),
            UserAction::PageHover { actor, position } => {
                if let Some(handle) = &self.live_page {
                    handle.send_hover(actor, f64::from(position.x), f64::from(position.y));
                }
                false
            }
            UserAction::PageFocus { actor } => {
                self.send_user(crate::js::PageCmd::Focus(actor));
                false
            }
            UserAction::SetFormValue {
                actor,
                value,
                checked,
            } => {
                if let Some(node) = actor
                    && self.send_user(crate::js::PageCmd::SetValue {
                        node,
                        value,
                        checked,
                    })
                {
                    *self.pending_form_values.entry(node).or_default() += 1;
                }
                true
            }
            UserAction::PageKey { node, input } => {
                self.send_user(crate::js::PageCmd::Key {
                    node: Some(node),
                    input,
                });
                false
            }
            UserAction::StepNumber {
                node,
                direction,
                click,
            } => {
                self.send_user(crate::js::PageCmd::StepNumber {
                    node,
                    direction,
                    click,
                });
                false
            }
            UserAction::SubmitForm { form, submitter } => {
                let form_node = form.live_node;
                let submitter_node = submitter
                    .and_then(|index| form.fields.get(index))
                    .and_then(|field| field.live_node);
                if let Some(form_node) = form_node
                    && self.live_page.is_some()
                {
                    self.pending_live_submit = Some((form, submitter));
                    self.send_user(crate::js::PageCmd::Submit {
                        form: form_node,
                        submitter: submitter_node,
                    });
                } else {
                    self.submit_static(form, submitter);
                }
                true
            }
            UserAction::SetNestedScroll { actor, top, left } => {
                if let Some(actor) = actor {
                    self.interaction
                        .nested_scroll
                        .insert(actor, CssPoint::new(left.max(0.0), top.max(0.0)));
                    self.send_user(crate::js::PageCmd::SetScroll {
                        node: actor,
                        top: f64::from(top.max(0.0)),
                        left: f64::from(left.max(0.0)),
                    });
                }
                true
            }
            UserAction::Key(input) => {
                // The live actor resolves focus at dispatch time, including shadow trees and
                // child browsing contexts; presentation hit-test state is not DOM focus.
                self.send_user(crate::js::PageCmd::Key { node: None, input });
                false
            }
            UserAction::TextInput(_) => false,
        };
        ActionOutcome {
            invalidated: invalidated || self.generation != generation_before,
            loading_retired: self.generation != generation_before,
        }
    }

    /// Drain all async completions currently queued. Returns whether visible
    /// state changed and therefore a redraw should be requested.
    pub fn process_async_events(&mut self) -> ActionOutcome {
        if let Some(error) = crate::site_storage::take_notice() {
            self.set_status(error);
        }
        let generation_before = self.generation;
        let mut changed = false;
        while let Some(event) = self.rx.pop() {
            match event {
                CoreEvent::UserInputReady { generation, permit } => {
                    if generation == self.generation {
                        self.user_input_retry = None;
                        if let (Some(page), Some(permit)) = (&self.live_page, permit) {
                            if let Some((command, navigation)) = self.pending_user_input.pop_front()
                            {
                                page.send_reserved_user(permit, command, navigation);
                            }
                            self.flush_user_input();
                        } else {
                            changed |= self.stop();
                        }
                    }
                }
                CoreEvent::GeminiImage {
                    generation,
                    url,
                    response,
                    status,
                } => {
                    if let Some(pending) = &mut self.pending
                        && pending.generation == generation
                    {
                        pending.target = Link::Gemini(url);
                    }
                    if self.finish_fetch(generation, Ok(FetchedDocument::Http(response))) {
                        if let Some(page) = &mut self.current {
                            page.status = status.clone();
                        }
                        self.status = status;
                        changed = true;
                    }
                }
                CoreEvent::Gemini {
                    generation,
                    response,
                } => {
                    changed |= self.update_gemini(generation, *response);
                }
                CoreEvent::Gopher { generation, reply } => {
                    changed |= self.update_gopher(generation, reply);
                }
                CoreEvent::Dict { generation, reply } => {
                    changed |= self.update_dict(generation, reply);
                }
                CoreEvent::Whois { generation, reply } => {
                    changed |= self.update_whois(generation, reply);
                }
                CoreEvent::Finger { generation, reply } => {
                    changed |= self.update_finger(generation, reply);
                }
                CoreEvent::FetchFinished { generation, result } => {
                    changed |= self.finish_fetch(generation, result);
                }
                CoreEvent::ExternalMedia { generation, url } => {
                    changed |= self.finish_external_media(generation, url);
                }
                CoreEvent::Download {
                    generation,
                    response,
                } => {
                    changed |= self.finish_download(generation, *response);
                }
                CoreEvent::GopherDownload { generation, offer } => {
                    changed |= self.finish_gopher_download(generation, *offer);
                }
                CoreEvent::Page { generation, event } => {
                    if generation == self.generation {
                        changed |= self.handle_page_event(event);
                    }
                }
                CoreEvent::DeclarativeRefresh { generation, url } => {
                    if generation == self.document_generation {
                        self.declarative_refresh_task = None;
                        self.begin_page_fetch(Link::Http(url), false, NavigationIntent::Replace);
                        changed = true;
                    }
                }
            }
        }
        ActionOutcome {
            invalidated: changed || self.generation != generation_before,
            loading_retired: self.generation != generation_before,
        }
    }

    fn begin_address(&mut self, address: &str, intent: NavigationIntent) -> bool {
        // Classify the address-bar form before generic bare-host handling adds
        // a trailing slash. For `youtube.com/watch?v=id`, appending that slash
        // would alter the query value and hide an otherwise valid video URL.
        if let Some(url) = crate::media::youtube_video_url(address) {
            self.delegate_external_media(url);
            return true;
        }
        match parse_navigation_target(address) {
            Ok((target, fallback_http)) => {
                self.begin_fetch(target, fallback_http, intent);
            }
            // Nothing was fetched, so nothing will ever render this address.
            Err(error) => {
                self.render_is_final = true;
                self.status = error;
            }
        }
        true
    }

    fn begin_page_address(&mut self, address: &str, intent: NavigationIntent) -> bool {
        match parse_navigation_target(address) {
            Ok((target, fallback)) => self.begin_page_fetch(target, fallback, intent),
            Err(error) => self.status = error,
        }
        true
    }

    /// A document-directed navigation is not an address-bar filesystem grant.
    /// Keep this check before retiring the current page or starting any I/O.
    fn begin_page_fetch(&mut self, target: Link, fallback: bool, intent: NavigationIntent) {
        if let Link::Http(url) = &target
            && url.scheme() == "file"
            && !self.current.as_ref().is_some_and(|page| {
                (matches!(&page.document, FetchedDocument::Internal(_))
                    && matches!(&page.target, Link::External(address) if address == "about:bookmarks"))
                    || matches!(&page.target, Link::Http(client) if crate::file::allowed_from(client, url))
            })
        {
            self.status = String::from("Local file navigation blocked for this document.");
            return;
        }
        self.begin_fetch(target, fallback, intent);
    }

    /// Small-net Back/Forward is local while its bounded source is retained.
    /// This is session RAM, with no persistent response cache.
    fn trim_gopher_history(&mut self) {
        let mut bytes = 0usize;
        let mut count = 0;
        for stack in [&mut self.back, &mut self.forward] {
            for entry in stack.iter_mut().rev() {
                if let Some(response) = &entry.gemini_page {
                    let size = response.body.capacity() + std::mem::size_of::<gemini::Response>();
                    if count >= 32 || size > (8 * 1024 * 1024usize).saturating_sub(bytes) {
                        entry.gemini_page = None;
                    } else {
                        bytes += size;
                        count += 1;
                    }
                }
                if let Some(page) = &entry.gopher_page {
                    let size = page.reply.body.capacity() + std::mem::size_of::<gopher::Page>();
                    if count >= 32 || size > (8 * 1024 * 1024usize).saturating_sub(bytes) {
                        entry.gopher_page = None;
                    } else {
                        bytes += size;
                        count += 1;
                    }
                }
            }
        }
    }

    fn begin_history(&mut self, forward: bool) -> bool {
        let entry = if forward {
            self.forward.last()
        } else {
            self.back.last()
        };
        self.gemini_prompt = None;
        let Some(entry) = entry.cloned() else {
            self.status = if forward {
                String::from("Nothing forward in history.")
            } else {
                String::from("History is empty.")
            };
            return true;
        };
        if let Some(document) = entry
            .gemini_page
            .map(FetchedDocument::Gemini)
            .or_else(|| entry.gopher_page.map(FetchedDocument::Gopher))
        {
            if let Some(task) = self.task.take() {
                task.abort();
            }
            self.retire_query("Incomplete reply: left this page");
            self.abort_declarative_refresh();
            self.drop_live_page();
            self.external_address = None;
            self.download_offer = None;
            self.generation = self.generation.wrapping_add(1);
            let generation = self.generation;
            self.pending = Some(PendingNavigation {
                generation,
                target: entry.target,
                fallback_http: false,
                intent: if forward {
                    NavigationIntent::Forward
                } else {
                    NavigationIntent::Back
                },
            });
            self.finish_fetch(generation, Ok(document));
            self.interaction.scroll = entry.scroll;
            return true;
        }
        if let Some(source) = entry.internal_source {
            self.begin_internal_gemtext(
                entry.target,
                source,
                if forward {
                    NavigationIntent::Forward
                } else {
                    NavigationIntent::Back
                },
            );
            return true;
        }
        self.begin_fetch(
            entry.target,
            entry.fallback_http,
            if forward {
                NavigationIntent::Forward
            } else {
                NavigationIntent::Back
            },
        );
        true
    }

    fn begin_fetch(&mut self, target: Link, fallback_http: bool, intent: NavigationIntent) {
        self.gemini_prompt = None;
        let target = if intent == NavigationIntent::Reload
            && let Some(page) = &self.current
            && let FetchedDocument::Rdap(rdap) = &page.document
            && target == page.target
        {
            crate::rdap::direct_action(&rdap.url)
        } else {
            target
        };
        if let Link::Http(url) = &target
            && crate::media::is_youtube_video_url(url)
        {
            self.delegate_external_media(url.clone());
            return;
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
        self.gemini_prompt = None;
        self.retire_query("Incomplete reply: request replaced");
        self.abort_declarative_refresh();
        self.drop_live_page();
        self.external_address = None;
        self.download_offer = None;
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        self.render_is_final = false;
        self.status = format!("Fetching {target} …");
        self.pending = Some(PendingNavigation {
            generation,
            target: target.clone(),
            fallback_http,
            intent,
        });
        let tx = self.tx.clone();
        let viewport = self.viewport;
        let device_pixel_ratio = self.device_pixel_ratio;
        let screen_position = self.screen_position;
        let storage = self.storage.clone();
        self.task = Some(self.runtime.spawn(async move {
            if let Link::Gemini(url) = &target {
                let result = gemini::fetch_updates(url, |response| {
                    let tx = tx.clone();
                    async move {
                        tx.send(CoreEvent::Gemini {
                            generation,
                            response: Box::new(response),
                        })
                        .await
                        .is_ok()
                    }
                })
                .await;
                let event = match result {
                    Ok(response)
                        if response.download.is_none()
                            && (20..30).contains(&response.status)
                            && response.media_type().is_ok_and(|m| m.is_image()) =>
                    {
                        let status = response.status_text();
                        let final_url = response.url.public_url();
                        match gemini::image_response(response) {
                            Ok(response) => {
                                let size = (
                                    viewport.width.round().clamp(1.0, u16::MAX as f32) as u16,
                                    viewport.height.round().clamp(1.0, u16::MAX as f32) as u16,
                                );
                                let response = http::execute_js_for_window(
                                    response,
                                    size,
                                    (1, 1),
                                    device_pixel_ratio,
                                    screen_position,
                                    storage,
                                )
                                .await;
                                CoreEvent::GeminiImage {
                                    generation,
                                    url: final_url,
                                    response: Box::new(response),
                                    status,
                                }
                            }
                            Err(error) => CoreEvent::FetchFinished {
                                generation,
                                result: Err(error),
                            },
                        }
                    }
                    Ok(response) => CoreEvent::Gemini {
                        generation,
                        response: Box::new(response),
                    },
                    Err(error) => CoreEvent::FetchFinished {
                        generation,
                        result: Err(error),
                    },
                };
                let _ = tx.send(event).await;
                return;
            }
            if let Link::Gopher(url) = &target
                && url.is_text()
                && !url.is_html()
            {
                let result = gopher::fetch_updates(url, |reply| {
                    let tx = tx.clone();
                    async move {
                        tx.send(CoreEvent::Gopher { generation, reply })
                            .await
                            .is_ok()
                    }
                })
                .await;
                let event = match result {
                    Ok(reply) => CoreEvent::Gopher { generation, reply },
                    Err(error) => CoreEvent::FetchFinished {
                        generation,
                        result: Err(error),
                    },
                };
                let _ = tx.send(event).await;
                return;
            }
            if let Link::Dict(url) = &target {
                let result = crate::dict::fetch_updates(url, |reply| {
                    let tx = tx.clone();
                    async move { tx.send(CoreEvent::Dict { generation, reply }).await.is_ok() }
                })
                .await;
                let event = match result {
                    Ok(reply) => CoreEvent::Dict { generation, reply },
                    Err(error) => CoreEvent::FetchFinished {
                        generation,
                        result: Err(error),
                    },
                };
                let _ = tx.send(event).await;
                return;
            }
            if let Link::OneShot(url) = &target
                && url.scheme == oneshot::Scheme::Finger
            {
                let result = crate::finger::fetch_updates(url, |reply| {
                    let tx = tx.clone();
                    async move {
                        tx.send(CoreEvent::Finger { generation, reply })
                            .await
                            .is_ok()
                    }
                })
                .await;
                let event = match result {
                    Ok(reply) => CoreEvent::Finger { generation, reply },
                    Err(error) => CoreEvent::FetchFinished {
                        generation,
                        result: Err(error),
                    },
                };
                let _ = tx.send(event).await;
                return;
            }
            if let Link::OneShot(url) = &target
                && url.scheme == oneshot::Scheme::Whois
            {
                let result = crate::whois::fetch_updates(url, |reply| {
                    let tx = tx.clone();
                    async move {
                        tx.send(CoreEvent::Whois { generation, reply })
                            .await
                            .is_ok()
                    }
                })
                .await;
                let event = match result {
                    Ok(reply) => CoreEvent::Whois { generation, reply },
                    Err(error) => CoreEvent::FetchFinished {
                        generation,
                        result: Err(error),
                    },
                };
                let _ = tx.send(event).await;
                return;
            }
            let result = fetch_protocol_interactive(
                &target,
                fallback_http,
                None,
                viewport,
                device_pixel_ratio,
                screen_position,
                storage,
                None,
                intent,
            )
            .await;
            let event = interactive_fetch_event(generation, result);
            let _ = tx.send(event).await;
        }));
    }

    fn queue_external_media(&mut self, url: url::Url) {
        // RFC 9110 §10.1.3: the referrer identifies the resource from which
        // the target URI was obtained. Capture the source with the media
        // request before the frontend drains it and launches the player.
        let referrer = self.current.as_ref().and_then(|page| match &page.target {
            Link::Http(url) => Some(url.clone()),
            _ => None,
        });
        self.queue_external_media_with_referrer(url, referrer);
    }

    fn queue_external_media_with_referrer(&mut self, url: url::Url, referrer: Option<url::Url>) {
        self.status = format!("Opening in mpv: {url}");
        self.external_media.push_back((url, referrer));
        self.invalidation.request_redraw();
    }

    fn delegate_external_media(&mut self, url: url::Url) {
        self.abort_declarative_refresh();
        if self.pending.take().is_some() {
            if let Some(task) = self.task.take() {
                task.abort();
            }
            self.gemini_prompt = None;
            self.retire_query("Incomplete reply: request replaced");
            self.render_is_final = self.live_page.is_none();
            // Ignore a completion already queued by the superseded fetch.
            self.generation = self.generation.wrapping_add(1);
        }
        self.queue_external_media(url);
    }

    fn finish_external_media(&mut self, generation: u64, url: url::Url) -> bool {
        let Some(_pending) = self
            .pending
            .take_if(|pending| pending.generation == generation)
        else {
            return false;
        };
        self.task = None;
        self.queue_external_media(url);
        true
    }

    fn finish_download(&mut self, generation: u64, response: http::Response) -> bool {
        let Some(_pending) = self
            .pending
            .take_if(|pending| pending.generation == generation)
        else {
            return false;
        };
        self.task = None;
        let referrer = self.current.as_ref().and_then(|page| match &page.target {
            Link::Http(url) => Some(url.clone()),
            _ => None,
        });
        let offer = crate::download::DownloadOffer::from_response(response, referrer);
        self.status = format!("Cannot display {}.", offer.content_type);
        self.download_offer = Some(offer);
        self.render_is_final = self.live_page.is_none();
        true
    }

    fn finish_gopher_download(
        &mut self,
        generation: u64,
        offer: crate::download::DownloadOffer,
    ) -> bool {
        if self
            .pending
            .take_if(|p| p.generation == generation)
            .is_none()
        {
            return false;
        }
        self.task = None;
        self.status = format!("Gopher file · {}", offer.summary());
        self.download_offer = Some(offer);
        self.render_is_final = self.live_page.is_none();
        true
    }

    fn retire_query(&mut self, reason: &str) {
        if let Some(page) = &mut self.current {
            match &mut page.document {
                FetchedDocument::Gemini(response) if !response.finished => {
                    response.finished = true;
                    response.notice = Some(reason.into());
                }
                FetchedDocument::Gopher(gopher) if !gopher.reply.finished => {
                    gopher.reply.finished = true;
                    gopher.reply.notice = Some(reason.to_string());
                }
                FetchedDocument::Finger(finger) if !finger.reply.finished => {
                    finger.reply.finished = true;
                    finger.reply.notice = Some(reason.to_string());
                }
                FetchedDocument::Whois(whois) if !whois.reply.finished => whois.stop(reason),
                FetchedDocument::Dict(dict) if !dict.reply.finished => dict.stop(reason),
                _ => return,
            }
            page.revision = page.revision.wrapping_add(1);
        }
    }

    pub fn gemini_width(&mut self, columns: usize) -> bool {
        if !(20..=240).contains(&columns) {
            self.status = "usage: gemini-width 20..240".into();
            return true;
        }
        let Some(page) = &mut self.current else {
            return false;
        };
        let FetchedDocument::Gemini(response) = &mut page.document else {
            return false;
        };
        response.view.reading_columns = columns;
        page.revision = page.revision.wrapping_add(1);
        self.interaction.scroll.x = 0.0;
        self.status = format!("Gemini reading width: {columns} columns.");
        self.invalidation.request_redraw();
        true
    }

    pub fn gopher_encoding(&mut self) -> bool {
        let Some(page) = &mut self.current else {
            return false;
        };
        let FetchedDocument::Gopher(gopher) = &mut page.document else {
            return false;
        };
        gopher.view.encoding = gopher.view.encoding.next();
        page.revision = page.revision.wrapping_add(1);
        self.status = format!("Gopher encoding: {}", gopher.view.encoding.label());
        self.invalidation.request_redraw();
        true
    }

    pub fn offer_gopher_download(&mut self, url: &gopher::GopherUrl) {
        self.stop();
        match crate::download::DownloadOffer::from_gopher(url.clone()) {
            Ok(offer) => {
                self.status = format!("Gopher file · {}", offer.suggested_filename);
                self.download_offer = Some(offer);
            }
            Err(error) => self.status = error,
        }
        self.invalidation.request_redraw();
    }

    pub fn reply_view_action(&mut self, action: &str, enabled: Option<bool>) -> bool {
        let Some(page) = &mut self.current else {
            return false;
        };
        let (result, wrap) = match &mut page.document {
            FetchedDocument::Gemini(response) => (
                gemini::view_action(&mut response.view, action, enabled),
                response.view.controls.wrap,
            ),
            FetchedDocument::Gopher(gopher) => (
                crate::text_reply::view_action(&mut gopher.view.controls, action, enabled, false),
                gopher.view.controls.wrap,
            ),
            FetchedDocument::Finger(finger) => (
                crate::finger::view_action(&mut finger.view, action, enabled),
                finger.view.wrap,
            ),
            FetchedDocument::Whois(whois) => (whois.view_action(action, enabled), whois.view.wrap),
            FetchedDocument::Rdap(rdap) => (rdap.view_action(action, enabled), rdap.view.wrap),
            FetchedDocument::Dict(dict) => (dict.view_action(action, enabled), dict.view.wrap),
            _ => return false,
        };
        match result {
            Ok(status) => {
                page.revision = page.revision.wrapping_add(1);
                self.status = status.to_string();
                if wrap {
                    self.interaction.scroll.x = 0.0;
                }
            }
            Err(status) => self.status = status.to_string(),
        }
        self.invalidation.request_redraw();
        true
    }

    pub fn offer_whois_export(&mut self, server: Option<usize>) -> bool {
        let Some(page) = &self.current else {
            return false;
        };
        if let FetchedDocument::Http(response) = &page.document
            && gemini::MediaType::parse(&response.content_type)
                .is_ok_and(|m| m.essence == "text/gemini")
            && server.is_none()
        {
            self.download_offer = Some(crate::download::DownloadOffer::from_bytes(
                response.url.clone(),
                "gemini-source.gmi".into(),
                response.body.clone(),
            ));
            self.status = "Save received Gemtext source".into();
            self.invalidation.request_redraw();
            return true;
        }
        if let FetchedDocument::Gemini(response) = &page.document {
            if server.is_none() {
                match gemini::source_offer(&response.document(80)) {
                    Ok(offer) => {
                        self.download_offer = Some(offer);
                        self.status = "Save received Gemini source".into();
                    }
                    Err(error) => self.status = error,
                }
            }
            self.invalidation.request_redraw();
            return true;
        }
        if let (FetchedDocument::Gopher(gopher), Link::Gopher(url)) = (&page.document, &page.target)
        {
            if server.is_none() {
                self.download_offer = Some(crate::download::DownloadOffer::from_bytes(
                    url::Url::parse(&url.to_string()).unwrap(),
                    "gopher-source.txt".into(),
                    gopher.reply.body.clone(),
                ));
                self.status = "Save received Gopher source".into();
            }
            return true;
        }
        if let FetchedDocument::Dict(dict) = &page.document {
            if server.is_some() {
                self.status = "DICT has one reply; use save without a server number.".into();
            } else {
                self.download_offer = Some(dict.export());
                self.status = "Save original DICT text.".into();
            }
            self.invalidation.request_redraw();
            return true;
        }
        if let FetchedDocument::Rdap(rdap) = &page.document {
            if server.is_some() {
                self.status = "RDAP has one JSON reply; use save without a server number.".into();
            } else {
                self.download_offer = Some(rdap.export());
                self.status = "Save original RDAP JSON.".into();
            }
            self.invalidation.request_redraw();
            return true;
        }
        let (FetchedDocument::Whois(whois), Link::OneShot(url)) = (&page.document, &page.target)
        else {
            return false;
        };
        match whois.export(url, server) {
            Ok(offer) => {
                self.download_offer = Some(offer);
                self.status = "Save received WHOIS reply.".into();
            }
            Err(error) => self.status = error,
        }
        self.invalidation.request_redraw();
        true
    }

    pub fn whois_encoding(&mut self, encoding: Option<crate::whois::Encoding>) -> bool {
        let Some(page) = &mut self.current else {
            return false;
        };
        let FetchedDocument::Whois(whois) = &mut page.document else {
            return false;
        };
        whois.encoding = encoding.unwrap_or(match whois.encoding {
            crate::whois::Encoding::Auto => crate::whois::Encoding::Utf8,
            crate::whois::Encoding::Utf8 => crate::whois::Encoding::Latin1,
            crate::whois::Encoding::Latin1 => crate::whois::Encoding::Auto,
        });
        page.revision = page.revision.wrapping_add(1);
        self.status = format!("WHOIS encoding: {}", whois.encoding.label());
        self.invalidation.request_redraw();
        true
    }

    fn update_dict(&mut self, generation: u64, reply: crate::dict::Reply) -> bool {
        let Some(pending) = self
            .pending
            .as_ref()
            .filter(|p| p.generation == generation)
            .cloned()
        else {
            return false;
        };
        let finished = reply.finished;
        if self.document_generation == generation
            && let Some(page) = &mut self.current
            && let FetchedDocument::Dict(dict) = &mut page.document
        {
            dict.update(reply);
            page.status = fetched_status(&page.target, &page.document);
            self.status = page.status.clone();
            page.revision = page.revision.wrapping_add(1);
        } else {
            let Link::Dict(target) = &pending.target else {
                return false;
            };
            let mut dict = crate::dict::Page::new(target.clone(), reply);
            if pending.intent == NavigationIntent::Reload
                && let Some(old) = &self.current
                && old.target == pending.target
                && let FetchedDocument::Dict(old) = &old.document
            {
                dict = crate::dict::Page::refreshed(dict.reply, old);
            }
            let task = self.task.take();
            let scroll = self.interaction.scroll;
            self.finish_fetch(generation, Ok(FetchedDocument::Dict(Box::new(dict))));
            if pending.intent == NavigationIntent::Reload {
                self.interaction.scroll = scroll;
            }
            self.task = task;
            self.pending = Some(pending);
        }
        self.render_is_final = finished;
        if finished {
            self.pending = None;
            self.task = None;
        }
        true
    }

    fn update_whois(&mut self, generation: u64, reply: crate::whois::Reply) -> bool {
        let Some(pending) = self
            .pending
            .as_ref()
            .filter(|p| p.generation == generation)
            .cloned()
        else {
            return false;
        };
        let finished = reply.finished;
        if self.document_generation == generation
            && let Some(page) = &mut self.current
            && let FetchedDocument::Whois(whois) = &mut page.document
        {
            whois.update(reply);
            page.status = fetched_status(&page.target, &page.document);
            self.status = page.status.clone();
            page.revision = page.revision.wrapping_add(1);
        } else {
            let mut whois = crate::whois::Page::new(reply);
            if pending.intent == NavigationIntent::Reload
                && let Some(old) = &self.current
                && old.target == pending.target
                && let FetchedDocument::Whois(old) = &old.document
            {
                whois = crate::whois::Page::refreshed(whois.reply, old);
            }
            let task = self.task.take();
            let scroll = self.interaction.scroll;
            self.finish_fetch(generation, Ok(FetchedDocument::Whois(whois)));
            if pending.intent == NavigationIntent::Reload {
                self.interaction.scroll = scroll;
            }
            self.task = task;
            self.pending = Some(pending);
        }
        self.render_is_final = finished;
        if finished {
            self.pending = None;
            self.task = None;
        }
        true
    }

    fn update_gemini(&mut self, generation: u64, mut response: gemini::Response) -> bool {
        let Some(mut pending) = self
            .pending
            .as_ref()
            .filter(|p| p.generation == generation)
            .cloned()
        else {
            return false;
        };
        if let Some(offer) = response.download.take() {
            self.pending = None;
            self.task = None;
            self.render_is_final = true;
            self.status = format!(
                "Gemini file · {}{}",
                offer.summary(),
                response
                    .notice
                    .as_ref()
                    .map_or(String::new(), |note| format!(" · {note}"))
            );
            self.download_offer = Some(offer);
            return true;
        }
        if let Some(prompt) = gemini::Prompt::from_response(&response) {
            self.pending = None;
            self.task = None;
            self.render_is_final = true;
            self.status = prompt.label();
            self.gemini_prompt = Some(prompt);
            return true;
        }
        pending.target = Link::Gemini(response.url.public_url());
        let finished = response.finished;
        if self.document_generation == generation
            && let Some(page) = &mut self.current
            && let FetchedDocument::Gemini(old) = &page.document
        {
            response.view = old.view.clone();
            page.document = FetchedDocument::Gemini(Box::new(response));
            page.status = fetched_status(&page.target, &page.document);
            self.status = page.status.clone();
            page.revision = page.revision.wrapping_add(1);
        } else {
            if pending.intent == NavigationIntent::Reload
                && let Some(page) = &self.current
                && page.target == pending.target
                && let FetchedDocument::Gemini(old) = &page.document
            {
                response.view = old.view.clone();
            }
            let task = self.task.take();
            let scroll = self.interaction.scroll;
            self.finish_fetch(generation, Ok(FetchedDocument::Gemini(Box::new(response))));
            if pending.intent == NavigationIntent::Reload {
                self.interaction.scroll = scroll;
            }
            self.task = task;
            self.pending = Some(pending);
        }
        self.render_is_final = finished;
        if finished {
            self.pending = None;
            self.task = None;
        }
        true
    }

    fn update_gopher(&mut self, generation: u64, reply: crate::text_reply::Reply) -> bool {
        let Some(pending) = self
            .pending
            .as_ref()
            .filter(|p| p.generation == generation)
            .cloned()
        else {
            return false;
        };
        let finished = reply.finished;
        if self.document_generation == generation
            && let Some(page) = &mut self.current
            && let FetchedDocument::Gopher(finger) = &mut page.document
        {
            finger.reply = reply;
            page.status = fetched_status(&page.target, &page.document);
            self.status = page.status.clone();
            page.revision = page.revision.wrapping_add(1);
        } else {
            let mut finger = crate::gopher::Page::new(reply);
            if pending.intent == NavigationIntent::Reload
                && let Some(old) = &self.current
                && old.target == pending.target
                && let FetchedDocument::Gopher(old) = &old.document
            {
                finger.view = old.view.clone();
            }
            // Commit history once, at the first published chunk. Later chunks
            // update that document while the same cancellable task stays live.
            let task = self.task.take();
            let scroll = self.interaction.scroll;
            self.finish_fetch(generation, Ok(FetchedDocument::Gopher(finger)));
            if pending.intent == NavigationIntent::Reload {
                self.interaction.scroll = scroll;
            }
            self.task = task;
            self.pending = Some(pending);
        }
        self.render_is_final = finished;
        if finished {
            self.pending = None;
            self.task = None;
        }
        true
    }

    fn update_finger(&mut self, generation: u64, reply: crate::finger::Reply) -> bool {
        let Some(pending) = self
            .pending
            .as_ref()
            .filter(|p| p.generation == generation)
            .cloned()
        else {
            return false;
        };
        let finished = reply.finished;
        if self.document_generation == generation
            && let Some(page) = &mut self.current
            && let FetchedDocument::Finger(finger) = &mut page.document
        {
            finger.reply = reply;
            page.status = fetched_status(&page.target, &page.document);
            self.status = page.status.clone();
            page.revision = page.revision.wrapping_add(1);
        } else {
            let mut finger = crate::finger::Page::new(reply);
            if pending.intent == NavigationIntent::Reload
                && let Some(old) = &self.current
                && old.target == pending.target
                && let FetchedDocument::Finger(old) = &old.document
            {
                finger.view = old.view.clone();
                if old.reply.finished && old.reply.notice.is_none() {
                    finger.view.previous = Some(Arc::from(old.reply.body.as_slice()));
                }
                if finger.view.previous.is_none() {
                    finger.view.changes = false;
                }
            }
            // Commit history once, at the first published chunk. Later chunks
            // update that document while the same cancellable task stays live.
            let task = self.task.take();
            let scroll = self.interaction.scroll;
            self.finish_fetch(generation, Ok(FetchedDocument::Finger(finger)));
            if pending.intent == NavigationIntent::Reload {
                self.interaction.scroll = scroll;
            }
            self.task = task;
            self.pending = Some(pending);
        }
        self.render_is_final = finished;
        if finished {
            self.pending = None;
            self.task = None;
        }
        true
    }

    fn finish_fetch(&mut self, generation: u64, result: Result<FetchedDocument, String>) -> bool {
        let Some(mut pending) = self.pending.take_if(|p| p.generation == generation) else {
            return false;
        };
        self.task = None;
        match result {
            Ok(mut document) => {
                if let FetchedDocument::Gemini(response) = &document {
                    pending.target = Link::Gemini(response.url.public_url());
                }
                if let FetchedDocument::Dict(dict) = &mut document {
                    let entry = match pending.intent {
                        NavigationIntent::Back => self.back.last(),
                        NavigationIntent::Forward => self.forward.last(),
                        _ => None,
                    };
                    if let Some(view) = entry.and_then(|entry| entry.dict_view.as_ref()) {
                        dict.restore_view(view);
                    }
                }
                if let FetchedDocument::Rdap(rdap) = &mut document {
                    pending.target = Link::Http(rdap.url.clone());
                    if pending.intent == NavigationIntent::Reload
                        && let Some(old) = &self.current
                        && let FetchedDocument::Rdap(old) = &old.document
                        && old.url == rdap.url
                    {
                        rdap.section = old.section;
                        rdap.view = old.view.clone();
                    }
                }
                if matches!(&pending.target, Link::External(address) if crate::rdap::is_action(address))
                    && let FetchedDocument::Http(response) = &document
                {
                    pending.target = Link::Http(response.url.clone());
                }
                let (live, declarative_refresh) = match &mut document {
                    FetchedDocument::Http(response) => {
                        (response.live.take(), response.declarative_refresh.take())
                    }
                    _ => (None, None),
                };
                let initial_rendered = match &document {
                    FetchedDocument::Http(response) => response.rendered.as_deref().cloned(),
                    _ => None,
                };
                let page = BrowserPage {
                    status: fetched_status(&pending.target, &document),
                    target: pending.target.clone(),
                    fallback_http: pending.fallback_http,
                    document,
                    rendered: initial_rendered.clone(),
                    rendered_revision: u64::from(initial_rendered.is_some()),
                    revision: 1,
                };
                self.live_regions.clear();
                self.live_boundaries.clear();
                self.status = page.status.clone();
                let old = self.current.replace(page);
                self.document_generation = generation;
                match pending.intent {
                    NavigationIntent::New => {
                        if let Some(old) = old {
                            self.back
                                .push(HistoryEntry::from_page(old, self.interaction.scroll));
                        }
                        self.forward.clear();
                    }
                    NavigationIntent::Replace | NavigationIntent::Reload => {}
                    NavigationIntent::Back => {
                        let _ = self.back.pop();
                        if let Some(old) = old {
                            self.forward
                                .push(HistoryEntry::from_page(old, self.interaction.scroll));
                        }
                    }
                    NavigationIntent::Forward => {
                        let _ = self.forward.pop();
                        if let Some(old) = old {
                            self.back
                                .push(HistoryEntry::from_page(old, self.interaction.scroll));
                        }
                    }
                }
                self.trim_gopher_history();
                self.interaction.scroll = CssPoint::default();
                self.interaction.nested_scroll.clear();
                // A document that gets no resident actor is final the moment it
                // commits; one that does becomes final only when the actor says
                // so with `PageEvt::Static`.
                self.render_is_final = live.is_none();
                if let Some(live) = live {
                    self.attach_live_page(generation, live);
                    self.send_live(crate::js::PageCmd::Viewport(layout_viewport(self.viewport)));
                    self.send_live(crate::js::PageCmd::DevicePixelRatio(
                        self.device_pixel_ratio,
                    ));
                    self.send_live(crate::js::PageCmd::ScreenPosition(
                        self.screen_position.0,
                        self.screen_position.1,
                    ));
                }
                if let Some(refresh) = declarative_refresh {
                    self.schedule_declarative_refresh(generation, refresh);
                }
            }
            Err(error) => {
                self.render_is_final = true;
                self.status = format!("{} — {error}", pending.target);
            }
        }
        true
    }

    fn stop(&mut self) -> bool {
        if self.gemini_prompt.take().is_some() {
            self.status = "Gemini input cancelled".into();
            return true;
        }
        let pending = self.pending.take();
        let had_live_page = self.live_page.is_some() || self.live_task.is_some();
        let had_refresh = self.declarative_refresh_task.is_some();
        if pending.is_none() && !had_live_page && !had_refresh {
            return false;
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
        self.retire_query("Incomplete reply: stopped");
        self.drop_live_page();
        self.abort_declarative_refresh();
        self.generation = self.generation.wrapping_add(1);
        // Nothing is left to render: the fetch task, the actor, and any
        // scheduled refresh are all gone, so waiting on this document is
        // pointless even though a partial render may well exist.
        self.render_is_final = true;
        self.status = pending.map_or_else(
            || String::from("Stopped — page scripts killed."),
            |pending| format!("Stopped loading {} — page scripts killed.", pending.target),
        );
        true
    }

    fn abort_declarative_refresh(&mut self) {
        if let Some(task) = self.declarative_refresh_task.take() {
            task.abort();
        }
    }

    /// HTML Living Standard, declarative refresh: after the document commits,
    /// wait for the parsed directive and navigate its navigable with history
    /// replacement. The generation rejects a retired document's timer.
    fn schedule_declarative_refresh(&mut self, generation: u64, refresh: http::DeclarativeRefresh) {
        self.abort_declarative_refresh();
        let tx = self.tx.clone();
        self.declarative_refresh_task = Some(self.runtime.spawn(async move {
            tokio::time::sleep(refresh.delay).await;
            let _ = tx
                .send(CoreEvent::DeclarativeRefresh {
                    generation,
                    url: refresh.url,
                })
                .await;
        }));
    }

    fn send_live(&self, command: crate::js::PageCmd) -> bool {
        self.live_page
            .as_ref()
            .is_some_and(|handle| handle.cmds.try_send(command).is_ok())
    }

    fn send_user(&mut self, command: crate::js::PageCmd) -> bool {
        self.queue_user_input(command, false)
    }

    fn send_navigation_click(&mut self, node: usize) -> bool {
        self.queue_user_input(crate::js::PageCmd::Click(node), true)
    }

    fn queue_user_input(&mut self, command: crate::js::PageCmd, navigation: bool) -> bool {
        if self.live_page.is_none() {
            return false;
        }
        // UI Events keydown/keyup MUST be delivered; HTML #task-queue and
        // #user-interaction-task-source preserve ordering within the source.
        // A full transport must not silently drop releases or reorder focus/click
        // relative to keys. Keep a bounded native backlog; pathological overload
        // explicitly stops the page, rather than leaking memory or losing input.
        const MAX_PENDING_INPUT: usize = 4096;
        if self.pending_user_input.len() == MAX_PENDING_INPUT {
            self.stop();
            self.status = String::from("Stopped — page input backlog exceeded its limit.");
            return false;
        }
        self.pending_user_input.push_back((command, navigation));
        self.flush_user_input();
        self.live_page.is_some()
    }

    fn flush_user_input(&mut self) {
        use tokio::sync::mpsc::error::TrySendError;
        if self.user_input_retry.is_some() {
            return;
        }
        while let Some((command, navigation)) = self.pending_user_input.pop_front() {
            let Some(page) = &self.live_page else {
                return;
            };
            let result = if navigation {
                let crate::js::PageCmd::Click(node) = command else {
                    unreachable!()
                };
                page.try_send_navigation_click(node)
            } else {
                page.try_send_user(command)
            };
            match result {
                Ok(()) => {}
                Err(TrySendError::Closed(_)) => {
                    self.stop();
                    return;
                }
                Err(TrySendError::Full(command)) => {
                    self.pending_user_input.push_front((command, navigation));
                    let sender = page.user_input_sender();
                    let tx = self.tx.clone();
                    let generation = self.generation;
                    // Sleep on real channel capacity, not a timer/poll loop. The
                    // reserved slot crosses back with the wake so it cannot be
                    // stolen by a later input. Only one retry task exists per page.
                    self.user_input_retry = Some(self.runtime.spawn(async move {
                        let permit = sender.reserve_owned().await.ok();
                        let _ = tx
                            .send(CoreEvent::UserInputReady { generation, permit })
                            .await;
                    }));
                    return;
                }
            }
        }
    }

    fn drop_live_page(&mut self) {
        if let Some(retry) = self.user_input_retry.take() {
            retry.abort();
        }
        self.pending_user_input.clear();
        if let Some(page) = self.live_page.take() {
            page.retire();
        }
        http::prune_idle_connections();
        self.pending_live_submit = None;
        self.page_key_defaults.clear();
        self.pending_form_values.clear();
        if let Some(task) = self.live_task.take() {
            task.abort();
        }
    }

    fn attach_live_page(&mut self, generation: u64, mut live: http::LivePage) {
        self.live_page = Some(live.handle);
        let tx = self.tx.clone();
        self.live_task = Some(self.runtime.spawn(async move {
            while let Some(event) = live.events.recv().await {
                if tx
                    .send(CoreEvent::Page { generation, event })
                    .await
                    .is_err()
                {
                    break;
                }
            }
        }));
    }

    pub fn dict_filter(&mut self, filter: &str) -> bool {
        let Some(page) = &mut self.current else {
            return false;
        };
        let FetchedDocument::Dict(dict) = &mut page.document else {
            return false;
        };
        match dict.set_filter(filter) {
            Ok(()) => {
                page.revision = page.revision.wrapping_add(1);
                self.interaction.scroll = CssPoint::default();
            }
            Err(error) => self.status = error,
        }
        self.invalidation.request_redraw();
        true
    }

    fn activate(&mut self, link: Link) -> bool {
        if let Some(action) = crate::dict::Action::from_link(&link) {
            if action == crate::dict::Action::Save {
                return self.offer_whois_export(None);
            }
            let Some(page) = &mut self.current else {
                return false;
            };
            let FetchedDocument::Dict(dict) = &mut page.document else {
                return false;
            };
            match dict.apply(&action) {
                Ok(()) => {
                    page.revision = page.revision.wrapping_add(1);
                    self.interaction.scroll = CssPoint::default();
                }
                Err(error) => self.status = error,
            }
            self.invalidation.request_redraw();
            return true;
        }
        if let Some(section) = crate::registration::Section::from_link(&link) {
            let Some(page) = &mut self.current else {
                return false;
            };
            match &mut page.document {
                FetchedDocument::Whois(whois) => {
                    whois.section = section;
                    whois.view.changes = false;
                }
                FetchedDocument::Rdap(rdap) => rdap.section = section,
                _ => return false,
            }
            page.revision = page.revision.wrapping_add(1);
            self.interaction.scroll = CssPoint::default();
            self.invalidation.request_redraw();
            return true;
        }
        match link {
            Link::JsClick { node, href } => {
                if href.is_empty() {
                    self.send_user(crate::js::PageCmd::Click(node));
                } else {
                    self.send_navigation_click(node);
                }
                self.status = String::from("Page action …");
            }
            Link::Form { .. } => return false,
            Link::Media(url) => self.queue_external_media(url),
            Link::External(url) if crate::rdap::is_action(&url) => {
                self.begin_fetch(Link::External(url), false, NavigationIntent::New)
            }
            Link::External(url) => self.status = format!("External target: {url}"),
            target => self.begin_page_fetch(target, false, NavigationIntent::New),
        }
        true
    }

    fn handle_page_event(&mut self, event: crate::js::PageEvt) -> bool {
        if std::env::var_os("TRUST_TRACE_PAGE_EVENTS").is_some() {
            eprintln!("[trace-event] {}", event_variant_name(&event));
        }
        use crate::js::PageEvt;
        match event {
            PageEvt::Updated { html, mut outcome } => {
                self.last_js_outcome = Some(outcome.clone());
                // The native frontends do not pass through `App`, so mirror its
                // gated live-render diagnostic here. Keeping this at the shared
                // controller boundary captures the exact authoritative HTML
                // that every graphical frontend is about to lay out.
                if let Some(dir) = std::env::var_os("TRUST_DUMP_RAW") {
                    static DUMP_SEQUENCE: std::sync::atomic::AtomicU64 =
                        std::sync::atomic::AtomicU64::new(0);
                    let sequence = DUMP_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let _ = std::fs::write(
                        std::path::Path::new(&dir).join(format!("render_{sequence:06}.html")),
                        &html,
                    );
                }
                if let Some(page) = &mut self.current {
                    if let Some(rendered) = outcome.rendered.take() {
                        page.rendered = Some(*rendered);
                    }
                    page.revision = page.revision.wrapping_add(1);
                    page.rendered_revision = page.revision;
                }
                self.render_is_final = false;
                self.status = if outcome.errors.is_empty() {
                    String::from("Page updated · JS")
                } else {
                    format!("Page updated · JS:{}", outcome.errors.len())
                };
                true
            }
            PageEvt::Static { html, mut outcome } => {
                self.last_js_outcome = Some(outcome.clone());
                if let Some(page) = &mut self.current {
                    // The actor has classified this document as inert and is
                    // about to exit.  Preserve its settled serialization as
                    // the static document source: later viewport/image
                    // reflows must not resurrect the server's pre-script DOM.
                    // This remains a presentation snapshot, never a second
                    // mutable DOM authority.
                    if let FetchedDocument::Http(response) = &mut page.document {
                        response.body = html.into_bytes();
                    }
                    if let Some(rendered) = outcome.rendered.take() {
                        page.rendered = Some(*rendered);
                    }
                    page.revision = page.revision.wrapping_add(1);
                    page.rendered_revision = page.revision;
                }
                self.drop_live_page();
                self.render_is_final = true;
                self.status = if outcome.errors.is_empty() {
                    String::from("Page updated · JS")
                } else {
                    format!("Page updated · JS:{}", outcome.errors.len())
                };
                true
            }
            PageEvt::Patched {
                patches,
                mut outcome,
            } => {
                self.last_js_outcome = Some(outcome.clone());
                let _ = patches;
                if let Some(page) = &mut self.current {
                    if let Some(rendered) = outcome.rendered.take() {
                        page.rendered = Some(*rendered);
                        page.revision = page.revision.wrapping_add(1);
                        page.rendered_revision = page.revision;
                    } else {
                        page.revision = page.revision.wrapping_add(1);
                    }
                }
                if !outcome.errors.is_empty() {
                    self.status = format!("Page JS:{}", outcome.errors.len());
                }
                true
            }
            PageEvt::Navigate(address) => self.begin_page_address(&address, NavigationIntent::New),
            PageEvt::Reload(address) => self.begin_page_address(&address, NavigationIntent::Reload),
            PageEvt::Replace(address) => {
                self.begin_page_address(&address, NavigationIntent::Replace)
            }
            PageEvt::HistoryUpdate { url, replace } => {
                self.apply_same_document_history_update(&url, replace)
            }
            PageEvt::ScrollToFragment(fragment) => {
                self.pending_fragment = Some(fragment);
                true
            }
            PageEvt::Trouble(errors) => {
                if let Some(error) = errors.first() {
                    self.status = format!("Page JS: {error}");
                }
                true
            }
            PageEvt::Settled => {
                // A quiescent dispatch acknowledges completion but changes no
                // page pixels. The terminal frontend treats repeated Settled
                // events as redraw-neutral; native frontends must do the same
                // or a page with a mutationless timer/rAF loop turns each actor
                // wake into a full Vello frame.
                let changed = self.status != "Ready";
                self.status = String::from("Ready");
                changed
            }
            PageEvt::KeyDefault { prevented } => {
                self.page_key_defaults.push_back(prevented);
                false
            }
            PageEvt::FormValueApplied { node } => {
                let Some(pending) = self.pending_form_values.get_mut(&node) else {
                    return false;
                };
                *pending -= 1;
                if *pending == 0 {
                    self.pending_form_values.remove(&node);
                    return true;
                }
                false
            }
            PageEvt::Scrolled { node, top, left } => {
                let position = CssPoint::new(left as f32, top as f32);
                let changed = self.interaction.nested_scroll.get(&node) != Some(&position);
                self.interaction.nested_scroll.insert(node, position);
                // Programmatic CSSOM scrolling arrives without an HTML
                // mutation. Native graphical frontends cache the display list,
                // whose nested-scroll transform is baked at extraction time,
                // so advance the render revision exactly when the offset moves.
                // (Wheel input updates its retained display list directly and
                // does not travel through this page-originated event.)
                if changed && let Some(page) = &mut self.current {
                    page.revision = page.revision.wrapping_add(1);
                }
                changed
            }
            PageEvt::SubmitDefault => {
                if let Some((form, submitter)) = self.pending_live_submit.take() {
                    self.submit_static(form, submitter);
                }
                true
            }
            PageEvt::SubmitForm { submission, .. } => {
                if let Some(submission) = submission {
                    self.submit_page_form(submission);
                }
                true
            }
        }
    }

    /// Apply HTML's URL and history update steps without starting a fetch or
    /// replacing the resident Document. The JS realm owns classic-history
    /// state; this controller owns browser chrome and product-level navigation
    /// policy, including the existing YouTube → mpv delegation.
    fn apply_same_document_history_update(&mut self, address: &str, _replace: bool) -> bool {
        let Ok(url) = url::Url::parse(address) else {
            self.status = String::from("Page supplied an invalid same-document URL.");
            return true;
        };
        let Some(page) = self.current.as_mut() else {
            return false;
        };
        let old_url = match &page.target {
            Link::Http(url) => Some(url.clone()),
            _ => None,
        };
        let address_changed = old_url.as_ref() != Some(&url);
        page.target = Link::Http(url.clone());
        if let FetchedDocument::Http(response) = &mut page.document {
            // `Response::url` is also the base used by any later static
            // presentation rebuild. After pushState/replaceState, HTML's
            // active Document URL—not the original request URL—is the base.
            response.url = url.clone();
        }
        if crate::media::is_youtube_video_url(&url) {
            self.queue_external_media_with_referrer(url, old_url);
            true
        } else {
            address_changed
        }
    }

    fn submit_static(&mut self, form: crate::doc::Form, submitter: Option<usize>) {
        use crate::doc::FormMethod;
        let body = form.encode(submitter);
        match form.method {
            FormMethod::Get => {
                let mut target = form.action;
                target.set_query((!body.is_empty()).then_some(body.as_str()));
                self.begin_page_fetch(Link::Http(target), false, NavigationIntent::New);
            }
            FormMethod::Post => self.begin_post(form.action, body),
        }
    }

    fn submit_page_form(&mut self, submission: crate::js::FormSubmission) {
        let Ok(mut action) = url::Url::parse(&submission.action) else {
            self.status = String::from("Invalid form action.");
            return;
        };
        if submission.method.eq_ignore_ascii_case("post") {
            self.begin_post(action, submission.body);
        } else if !submission.method.eq_ignore_ascii_case("dialog") {
            action.set_query((!submission.body.is_empty()).then_some(&submission.body));
            self.begin_page_fetch(Link::Http(action), false, NavigationIntent::New);
        }
    }

    fn begin_post(&mut self, url: url::Url, body: String) {
        if crate::media::is_youtube_video_url(&url) {
            self.delegate_external_media(url);
            return;
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
        self.abort_declarative_refresh();
        self.drop_live_page();
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        let target = Link::Http(url.clone());
        self.status = format!("POSTing to {url} …");
        self.pending = Some(PendingNavigation {
            generation,
            target: target.clone(),
            fallback_http: false,
            intent: NavigationIntent::New,
        });
        let tx = self.tx.clone();
        let viewport = self.viewport;
        let device_pixel_ratio = self.device_pixel_ratio;
        let screen_position = self.screen_position;
        let storage = self.storage.clone();
        self.task = Some(self.runtime.spawn(async move {
            let result = fetch_protocol_interactive(
                &target,
                false,
                None,
                viewport,
                device_pixel_ratio,
                screen_position,
                storage,
                Some(body),
                NavigationIntent::New,
            )
            .await;
            let event = interactive_fetch_event(generation, result);
            let _ = tx.send(event).await;
        }));
    }
}

/// Fetch a browser document through TRust's production protocol stack.
///
/// Both native frontends call this single protocol dispatch. Page-specific
/// DOM/layout processing remains separate from protocol transport so either
/// frontend can choose its presentation adapter.
pub async fn fetch_protocol(
    target: &Link,
    fallback_http: bool,
    referrer: Option<&url::Url>,
) -> Result<FetchedDocument, String> {
    match target {
        Link::Gopher(url) if url.is_binary_file() => match gopher::fetch_file(url).await? {
            gopher::FileResponse::Document(response) => {
                if gopher::file_is_text(&response.content_type) {
                    Ok(FetchedDocument::Gopher(response.body.into()))
                } else {
                    Ok(FetchedDocument::Http(response))
                }
            }
            gopher::FileResponse::Download(offer) => Err(format!(
                "Gopher file requires a download: {}",
                offer.summary()
            )),
        },
        Link::Gopher(url) if url.is_html() => {
            gopher::representation(url, gopher::fetch(url).await?)
                .map(|r| FetchedDocument::Http(Box::new(r)))
        }
        Link::Gopher(url) => gopher::fetch(url).await.map(FetchedDocument::Gopher),
        Link::Gemini(url) => gemini::fetch(url)
            .await
            .map(|response| FetchedDocument::Gemini(Box::new(response))),
        Link::Dict(url) => crate::dict::fetch(url).await.map(|reply| {
            FetchedDocument::Dict(Box::new(crate::dict::Page::new(url.clone(), reply)))
        }),
        Link::Http(url) => {
            let response = if fallback_http {
                http::fetch_web_default_with_referrer(url, referrer).await
            } else {
                let mut request = http::Request::get(url.clone());
                if let Some(referrer) = referrer {
                    http::set_referrer(&mut request, referrer);
                }
                http::set_navigation_metadata(&mut request, referrer);
                http::fetch(&request).await
            }?;
            if crate::rdap::is_response(&response) {
                Ok(FetchedDocument::Rdap(crate::rdap::Page::from_response(
                    response,
                )))
            } else {
                Ok(FetchedDocument::Http(Box::new(response)))
            }
        }
        Link::OneShot(url) if url.scheme == oneshot::Scheme::Finger => crate::finger::fetch(url)
            .await
            .map(|reply| FetchedDocument::Finger(crate::finger::Page::new(reply))),
        Link::OneShot(url) if url.scheme == oneshot::Scheme::Whois => crate::whois::fetch(url)
            .await
            .map(|reply| FetchedDocument::Whois(crate::whois::Page::new(reply))),
        Link::OneShot(url) => oneshot::fetch(url).await.map(FetchedDocument::OneShot),
        Link::Telnet { .. } => Err(String::from("terminal target requires a frontend VT view")),
        Link::External(url) if crate::rdap::is_action(url) => crate::rdap::fetch_action(url)
            .await
            .map(|response| FetchedDocument::Rdap(crate::rdap::Page::from_response(response))),
        Link::External(url) => Err(format!("unsupported URL scheme: {url}")),
        Link::Form { .. } | Link::JsClick { .. } | Link::Media(_) => {
            Err(String::from("target is not directly fetchable"))
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn fetch_protocol_interactive(
    target: &Link,
    fallback_http: bool,
    referrer: Option<&url::Url>,
    viewport: CssSize,
    device_pixel_ratio: f32,
    screen_position: (i32, i32),
    storage: crate::js::WebStorage,
    post_body: Option<String>,
    intent: NavigationIntent,
) -> Result<InteractiveFetch, String> {
    if let Link::Gopher(url) = target
        && (url.is_image() || url.is_html() || url.is_binary_file())
    {
        let response = if url.is_binary_file() {
            match gopher::fetch_file(url).await? {
                gopher::FileResponse::Download(offer) => {
                    return Ok(InteractiveFetch::GopherDownload(offer));
                }
                gopher::FileResponse::Document(response) => {
                    let mime = response.content_type.clone();
                    if gopher::file_is_text(&mime) {
                        return Ok(InteractiveFetch::Document(FetchedDocument::Gopher(
                            response.body.into(),
                        )));
                    }
                    if mime.starts_with("image/") {
                        http::image_navigation_response(*response, &mime)
                    } else {
                        *response
                    }
                }
            }
        } else {
            gopher::representation(url, gopher::fetch(url).await?)?
        };
        let size = (
            viewport.width.round().clamp(1.0, u16::MAX as f32) as u16,
            viewport.height.round().clamp(1.0, u16::MAX as f32) as u16,
        );
        let response = http::execute_js_for_window(
            response,
            size,
            (1, 1),
            device_pixel_ratio,
            screen_position,
            storage,
        )
        .await;
        return Ok(InteractiveFetch::Document(FetchedDocument::Http(Box::new(
            response,
        ))));
    }
    if let Link::Http(url) = target {
        let mut response = if let Some(body) = post_body {
            let mut request = http::Request {
                method: String::from("POST"),
                url: url.clone(),
                body: Some((
                    String::from("application/x-www-form-urlencoded"),
                    body.into_bytes(),
                )),
                headers: Vec::new(),
                fetch_metadata: None,
                cookie_context: None,
                timing_client: None,
                fetch_policy: None,
            };
            if let Some(referrer) = referrer {
                http::set_referrer(&mut request, referrer);
            }
            http::set_navigation_metadata(&mut request, referrer);
            http::fetch(&request).await?
        } else if fallback_http {
            http::fetch_web_default_with_referrer(url, referrer).await?
        } else {
            let mut request = http::Request::get(url.clone());
            if let Some(referrer) = referrer {
                http::set_referrer(&mut request, referrer);
            }
            http::set_navigation_metadata(&mut request, referrer);
            http::fetch(&request).await?
        };
        if let Some(timing) = response.timing.as_mut() {
            timing.navigation_type = intent.timing_type();
        }
        if crate::media::is_youtube_video_url(&response.url) {
            return Ok(InteractiveFetch::ExternalMedia(response.url));
        }
        let computed_type = crate::download::computed_mime_type(&response);
        if crate::download::response_needs_download(&response, true) {
            return Ok(InteractiveFetch::Download(Box::new(response)));
        }
        if crate::rdap::is_response(&response) {
            return Ok(InteractiveFetch::Document(FetchedDocument::Rdap(
                crate::rdap::Page::from_response(response),
            )));
        }
        if computed_type
            .split(';')
            .next()
            .is_some_and(|mime| mime.trim().starts_with("image/"))
        {
            response = crate::http::image_navigation_response(response, &computed_type);
        } else {
            response.content_type = computed_type;
        }
        // The legacy API accepts a terminal viewport and cell size. A one-pixel
        // cell is the explicit desktop adapter, so the actor's CSSOM viewport
        // is exactly the desktop's CSS-pixel viewport and never device pixels.
        let css_viewport = (
            viewport.width.round().clamp(1.0, f32::from(u16::MAX)) as u16,
            viewport.height.round().clamp(1.0, f32::from(u16::MAX)) as u16,
        );
        response = http::execute_js_for_window(
            response,
            css_viewport,
            (1, 1),
            device_pixel_ratio,
            screen_position,
            storage,
        )
        .await;
        Ok(InteractiveFetch::Document(FetchedDocument::Http(Box::new(
            response,
        ))))
    } else {
        fetch_protocol(target, fallback_http, referrer)
            .await
            .map(InteractiveFetch::Document)
    }
}

fn interactive_fetch_event(generation: u64, result: Result<InteractiveFetch, String>) -> CoreEvent {
    match result {
        Ok(InteractiveFetch::Document(document)) => CoreEvent::FetchFinished {
            generation,
            result: Ok(document),
        },
        Ok(InteractiveFetch::ExternalMedia(url)) => CoreEvent::ExternalMedia { generation, url },
        Ok(InteractiveFetch::Download(response)) => CoreEvent::Download {
            generation,
            response,
        },
        Ok(InteractiveFetch::GopherDownload(offer)) => {
            CoreEvent::GopherDownload { generation, offer }
        }
        Err(error) => CoreEvent::FetchFinished {
            generation,
            result: Err(error),
        },
    }
}

fn layout_viewport(size: CssSize) -> crate::layout2::Viewport {
    crate::layout2::Viewport::new(size.width, size.height)
}

fn fetched_status(target: &Link, document: &FetchedDocument) -> String {
    match document {
        FetchedDocument::Http(_) if matches!(target, Link::Gopher(_)) => {
            format!("{target} · Gopher document")
        }
        FetchedDocument::Http(response) => {
            let media = response.content_type.split(';').next().unwrap_or("").trim();
            if response.url.scheme() == "file" {
                format!("{} — local file ({media})", response.url)
            } else {
                format!("{} — HTTP {} ({media})", response.url, response.status)
            }
        }
        FetchedDocument::Gemini(response) => response.status_text(),
        FetchedDocument::Gopher(page) => match target {
            Link::Gopher(url) => gopher::status(url, &page.reply),
            _ => format!("{target} — {} bytes", page.reply.body.len()),
        },
        FetchedDocument::OneShot(bytes) => format!("{target} — {} bytes", bytes.len()),
        FetchedDocument::Whois(page) => match target {
            Link::OneShot(url) => crate::whois::status(url, &page.reply),
            _ => format!("WHOIS — {} bytes", page.reply.bytes()),
        },
        FetchedDocument::Dict(page) => page.status(),
        FetchedDocument::Rdap(page) => {
            format!("RDAP · {} · HTTP {}", page.record.title, page.status)
        }
        FetchedDocument::Finger(page) => format!(
            "{target} — {} bytes{} · W wrap · D changes",
            page.reply.body.len(),
            if page.reply.finished {
                ""
            } else {
                " received …"
            }
        ),
        FetchedDocument::Internal(_) => target.to_string(),
    }
}

/// Parse a typed navigation target into a fetchable protocol target. A bare host is
/// HTTPS with HTTP fallback, matching the existing terminal address behavior.
pub fn parse_navigation_target(address: &str) -> Result<(Link, bool), String> {
    if let Some(url) = crate::file::url_from_input(address)? {
        return Ok((Link::Http(url), false));
    }
    if let Some((host, port, tls)) = crate::command::telnet_target(address) {
        return Ok((Link::Telnet { host, port, tls }, false));
    }
    if address
        .split_once(':')
        .is_some_and(|(scheme, _)| gopher::is_scheme(scheme))
    {
        return gopher::GopherUrl::parse(address)
            .map(|url| (Link::Gopher(url), false))
            .ok_or_else(|| "Invalid Gopher address".into());
    }
    let address = address.trim();
    if address.is_empty() {
        return Err(String::from("Enter an address."));
    }
    if crate::dict::is_address(address) {
        return crate::dict::Target::parse(address).map(|url| (Link::Dict(url), false));
    }
    if crate::rdap::is_action(address) {
        return Ok((Link::External(address.to_string()), false));
    }
    if address
        .split_once(':')
        .is_some_and(|(scheme, _)| scheme.eq_ignore_ascii_case("finger"))
    {
        return crate::finger::parse_url(address)
            .map(|url| (Link::OneShot(url), false))
            .ok_or_else(|| String::from("Invalid Finger address."));
    }
    if address
        .split_once(':')
        .is_some_and(|(scheme, _)| scheme.eq_ignore_ascii_case("whois"))
    {
        return crate::whois::parse_url(address)
            .map(|url| (Link::OneShot(url), false))
            .ok_or_else(|| String::from("Invalid WHOIS address."));
    }
    if let Some(url) = gopher::GopherUrl::parse(address) {
        return Ok((Link::Gopher(url), false));
    }
    if let Some(url) = gemini::GeminiUrl::parse(address) {
        return Ok((Link::Gemini(url), false));
    }
    if let Some(url) = http::parse_url(address) {
        return Ok((Link::Http(url), false));
    }
    if let Some(url) = oneshot::OneShotUrl::parse(address) {
        return Ok((Link::OneShot(url), false));
    }
    if address.starts_with("telnet://") || address.starts_with("telnets://") {
        return Err(String::from("Invalid Telnet address."));
    }
    let (host, port) = split_host_port(address);
    match port {
        Some(70) => Ok((
            Link::Gopher(gopher::GopherUrl {
                host: host.to_string(),
                port: 70,
                tls: false,
                item_type: '1',
                query: None,
                gopher_plus: None,
                selector: Vec::new(),
            }),
            false,
        )),
        Some(1965) => Ok((
            Link::Gemini(crate::gemini::GeminiUrl::new(host, 1965, "/")),
            false,
        )),
        Some(79) => Ok((
            Link::OneShot(oneshot::OneShotUrl {
                scheme: oneshot::Scheme::Finger,
                host: host.to_string(),
                port: 79,
                query: String::new(),
            }),
            false,
        )),
        Some(80) => http::parse_url(&format!("http://{host}/"))
            .map(|url| (Link::Http(url), false))
            .ok_or_else(|| format!("Invalid address: {address}")),
        Some(443) => http::parse_url(&format!("https://{host}/"))
            .map(|url| (Link::Http(url), false))
            .ok_or_else(|| format!("Invalid address: {address}")),
        Some(port) => Err(format!(
            "Port {port} is a terminal session; open it with the terminal frontend."
        )),
        None => http::parse_url(&format!("https://{host}/"))
            .map(|url| (Link::Http(url), true))
            .ok_or_else(|| format!("Invalid address: {address}")),
    }
}

fn split_host_port(address: &str) -> (&str, Option<u16>) {
    if let Some((host, port)) = address.rsplit_once(':')
        && !host.is_empty()
        && !host.contains(':')
        && let Ok(port) = port.parse()
    {
        return (host, Some(port));
    }
    (address, None)
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn gopher_plus_desktop_routes_views_and_information_by_representation() {
        for (tls, (kind, command, bytes, expected)) in [
            (
                '0',
                b"+text/html".as_slice(),
                b"<p>A page</p>\r\n.\r\n<p>Still here</p>".to_vec(),
                "html",
            ),
            (
                '0',
                b"+image/webp",
                crate::gopher::file_tests::webp(),
                "image",
            ),
            (
                '1',
                b"+text/plain",
                b"Text view\r\n.\r\nAfter dot".to_vec(),
                "text",
            ),
            (
                '9',
                b"!",
                b"+INFO: 9A file\t/item\te\t70\t+\r\n+VIEWS:\r\n image/webp: <1k>\r\n".to_vec(),
                "info",
            ),
        ]
        .into_iter()
        .flat_map(|case| [(false, case.clone()), (true, case)])
        {
            let response = [format!("+{}\r\n", bytes.len()).as_bytes(), &bytes].concat();
            let (mut url, server) =
                crate::gopher::file_tests::serve_transport(response, b"/item", tls).await;
            url.item_type = kind;
            let url = url.with_plus(command);
            let mut browser = BrowserController::new(
                tokio::runtime::Handle::current(),
                || {},
                CssSize::new(800.0, 600.0),
            );
            browser.open_internal_gemtext("about:plus-test", b"Previous page".to_vec());
            let previous = browser.current_page().unwrap().target().clone();
            browser.handle_action(UserAction::Activate(Link::Gopher(url.clone())));
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                browser.task.take().unwrap(),
            )
            .await
            .unwrap()
            .unwrap();
            browser.process_async_events();
            assert!(browser.download_offer().is_none());
            let page = browser.current_page().unwrap();
            assert_eq!(page.target(), &Link::Gopher(url.clone()));
            match (&page.document, expected) {
                (FetchedDocument::Http(response), "html") => {
                    // The page actor serializes the parsed HTML document.
                    assert_eq!(response.content_type.split(';').next(), Some("text/html"));
                    let html = String::from_utf8_lossy(&response.body);
                    assert!(html.contains("A page") && html.contains("Still here"));
                }
                (FetchedDocument::Http(response), "image") => assert!(
                    String::from_utf8_lossy(&response.body).contains("data:image/webp;base64,")
                ),
                (FetchedDocument::Gopher(page), "text") => {
                    let doc = crate::gopher::render(&url, page.clone(), 80);
                    assert_eq!(
                        doc.lines
                            .iter()
                            .map(|l| l.text.as_str())
                            .collect::<Vec<_>>(),
                        ["Text view", ".", "After dot"]
                    );
                }
                (FetchedDocument::Gopher(page), "info") => {
                    let doc = crate::gopher::render(&url, page.clone(), 80);
                    assert!(
                        doc.lines
                            .iter()
                            .any(|l| matches!(&l.link, Some(Link::Gopher(u)) if u.is_image()))
                    );
                }
                _ => panic!("Wrong presentation for {expected}"),
            }
            assert_eq!(server.await.unwrap(), url.request().unwrap());
            browser.handle_action(UserAction::Back);
            assert_eq!(browser.current_page().unwrap().target(), &previous);
            assert!(browser.task.is_none());
        }
    }

    #[tokio::test]
    async fn gopher_generic_menu_files_open_in_the_desktop_and_restore_the_menu() {
        for (tls, (bytes, selector, image)) in [
            (
                crate::gopher::file_tests::webp(),
                b"/comic panel 1.webp".as_slice(),
                true,
            ),
            (
                b"PK\x03\x04\0archive".to_vec(),
                b"/archive.zip".as_slice(),
                false,
            ),
        ]
        .into_iter()
        .flat_map(|case| [(false, case.clone()), (true, case)])
        {
            let (url, server) =
                crate::gopher::file_tests::serve_transport(bytes, selector, tls).await;
            let mut parent = url.clone();
            parent.item_type = '1';
            parent.selector = b"/menu".to_vec();
            let menu = format!(
                "9File\t{}\t{}\t{}\r\n.\r\n",
                String::from_utf8_lossy(selector),
                url.host,
                url.port
            );
            let mut browser = BrowserController::new(
                tokio::runtime::Handle::current(),
                || {},
                CssSize::new(800.0, 600.0),
            );
            browser.pending = Some(PendingNavigation {
                generation: 0,
                target: Link::Gopher(parent.clone()),
                fallback_http: false,
                intent: NavigationIntent::New,
            });
            assert!(browser.finish_fetch(0, Ok(FetchedDocument::Gopher(menu.into_bytes().into()))));
            let target = Link::Gopher(url.clone());
            browser.handle_action(UserAction::Activate(target.clone()));
            assert!(browser.download_offer().is_none());
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                browser.task.take().expect("file fetch started"),
            )
            .await
            .unwrap()
            .unwrap();
            browser.process_async_events();
            assert_eq!(server.await.unwrap(), url.request().unwrap());
            assert!(browser.pending.is_none());
            if image {
                assert!(browser.download_offer().is_none());
                let page = browser.current_page().unwrap();
                assert_eq!(page.target(), &target);
                let FetchedDocument::Http(response) = &page.document else {
                    panic!("WebP needs a native image document");
                };
                assert_eq!(response.content_type, "text/html; charset=utf-8");
                assert!(
                    String::from_utf8_lossy(&response.body).contains("data:image/webp;base64,")
                );
                browser.handle_action(UserAction::Back);
            } else {
                assert_eq!(
                    browser.download_offer().unwrap().content_type,
                    "application/zip"
                );
                browser.dismiss_download_offer();
            }
            assert_eq!(
                browser.current_page().unwrap().target(),
                &Link::Gopher(parent)
            );
            assert!(
                browser.task.is_none(),
                "returning to the menu must not refetch it"
            );
        }
    }

    #[tokio::test]
    async fn gopher_history_restores_source_and_view_without_network() {
        let mut browser = super::BrowserController::new(
            tokio::runtime::Handle::current(),
            || {},
            super::CssSize::new(800.0, 600.0),
        );
        for i in 0..3 {
            let url =
                crate::gopher::GopherUrl::parse(&format!("gopher://nonexistent.invalid/0/{i}"))
                    .unwrap();
            browser.generation += 1;
            let generation = browser.generation;
            browser.pending = Some(super::PendingNavigation {
                generation,
                target: crate::doc::Link::Gopher(url),
                fallback_http: false,
                intent: super::NavigationIntent::New,
            });
            let mut page = crate::gopher::Page::from(format!("Page {i}\r\n.\r\n").into_bytes());
            page.view.controls.wrap = false;
            assert!(browser.finish_fetch(generation, Ok(super::FetchedDocument::Gopher(page))));
            browser.interaction.scroll.y = 100.0 + i as f32;
        }
        browser.begin_history(false);
        browser.begin_history(false);
        assert!(browser.pending.is_none() && browser.task.is_none());
        let page = browser.current.as_ref().unwrap();
        assert!(page.target.to_string().ends_with("/0/0"));
        assert_eq!(browser.interaction.scroll.y, 100.0);
        assert!(
            matches!(&page.document, super::FetchedDocument::Gopher(p) if !p.view.controls.wrap && p.reply.body.starts_with(b"Page 0"))
        );
        browser.begin_history(true);
        assert!(
            browser
                .current
                .as_ref()
                .unwrap()
                .target
                .to_string()
                .ends_with("/0/1")
        );
    }

    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn css_and_physical_coordinates_remain_separate() {
        let metrics =
            ViewportMetrics::from_physical(PhysicalSize::new(1500, 900), ScaleFactor::new(1.5));
        assert_eq!(metrics.css, CssSize::new(1000.0, 600.0));
        assert_eq!(
            metrics.physical_to_css(300.0, 150.0),
            CssPoint::new(200.0, 100.0)
        );
        assert_eq!(ScaleFactor::new(0.0), ScaleFactor::default());
    }

    #[test]
    fn action_boundary_tracks_native_input_without_native_types() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let wakes = Arc::new(AtomicUsize::new(0));
        let wake_count = Arc::clone(&wakes);
        let mut browser = BrowserController::new(
            runtime.handle().clone(),
            move || {
                wake_count.fetch_add(1, Ordering::Relaxed);
            },
            CssSize::new(800.0, 600.0),
        );

        assert!(browser.handle_action(UserAction::Focus(true)).invalidated);
        assert!(
            !browser
                .handle_action(UserAction::PointerMove(CssPoint::new(12.5, 44.0)))
                .invalidated
        );
        assert_eq!(
            browser.interaction().pointer,
            Some(CssPoint::new(12.5, 44.0))
        );
        assert!(
            browser
                .handle_action(UserAction::Scroll(ScrollDelta {
                    dx: 2.0,
                    dy: 18.0,
                    unit: ScrollUnit::CssPixel,
                }))
                .invalidated
        );
        assert!(
            browser
                .handle_action(UserAction::Ime(ImeAction::Preedit {
                    text: String::from("é"),
                    cursor: Some((0, 2)),
                }))
                .invalidated
        );
        assert_eq!(
            browser.interaction().pointer,
            Some(CssPoint::new(12.5, 44.0))
        );
        assert_eq!(browser.interaction().scroll, CssPoint::new(2.0, 18.0));
        assert_eq!(wakes.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn pixel_scroll_actions_clamp_and_retain_nested_container_state() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut browser =
            BrowserController::new(runtime.handle().clone(), || {}, CssSize::new(640.0, 480.0));

        browser.handle_action(UserAction::SetViewportScroll(CssPoint::new(-8.0, 125.5)));
        browser.handle_action(UserAction::SetNestedScroll {
            actor: Some(17),
            top: 44.25,
            left: -3.0,
        });

        assert_eq!(browser.interaction().scroll, CssPoint::new(0.0, 125.5));
        assert_eq!(
            browser.interaction().nested_scroll.get(&17),
            Some(&CssPoint::new(0.0, 44.25))
        );
    }

    #[test]
    fn repeated_page_settlement_is_redraw_neutral() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut browser =
            BrowserController::new(runtime.handle().clone(), || {}, CssSize::new(640.0, 480.0));
        browser.status = String::from("Page action …");

        assert!(browser.handle_page_event(crate::js::PageEvt::Settled));
        assert_eq!(browser.snapshot().status, "Ready");
        assert!(!browser.handle_page_event(crate::js::PageEvt::Settled));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn declarative_refresh_replaces_the_committed_history_entry() {
        let mut browser = BrowserController::new(
            tokio::runtime::Handle::current(),
            || {},
            CssSize::new(640.0, 480.0),
        );
        let source = url::Url::parse("https://source.example/").unwrap();
        let destination = url::Url::parse("https://destination.example/").unwrap();
        browser.generation = 1;
        browser.pending = Some(PendingNavigation {
            generation: 1,
            target: Link::Http(source.clone()),
            fallback_http: false,
            intent: NavigationIntent::New,
        });
        assert!(browser.finish_fetch(
            1,
            Ok(FetchedDocument::Http(Box::new(crate::http::Response {
                url: source,
                status: 200,
                content_type: String::from("text/html"),
                headers: Vec::new(),
                body: b"<p>redirecting</p>".to_vec(),
                rendered: None,
                js: None,
                blobs: None,
                live: None,
                declarative_refresh: Some(crate::http::DeclarativeRefresh {
                    delay: std::time::Duration::ZERO,
                    url: destination.clone(),
                }),
                challenge: None,
                from_post: false,
                timing: None,
            })))
        ));

        browser
            .declarative_refresh_task
            .take()
            .expect("refresh timer installed")
            .await
            .unwrap();
        assert!(browser.process_async_events().invalidated);
        let pending = browser.pending.as_ref().expect("refresh starts navigation");
        assert_eq!(pending.target, Link::Http(destination));
        assert_eq!(pending.intent, NavigationIntent::Replace);
        assert!(browser.back.is_empty(), "the source is not added twice");
        browser.task.take().unwrap().abort();
    }

    #[test]
    fn unsupported_download_offer_leaves_the_desktop_document_committed() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut browser =
            BrowserController::new(runtime.handle().clone(), || {}, CssSize::new(640.0, 480.0));
        let current = Link::External(String::from("about:help"));
        browser.current = Some(BrowserPage {
            target: current.clone(),
            fallback_http: false,
            document: FetchedDocument::Internal(b"current page".to_vec()),
            status: String::from("Ready"),
            rendered: None,
            rendered_revision: 1,
            revision: 1,
        });
        browser.generation = 7;
        browser.pending = Some(PendingNavigation {
            generation: 7,
            target: Link::Http(url::Url::parse("https://example.test/report.pdf").unwrap()),
            fallback_http: false,
            intent: NavigationIntent::New,
        });

        assert!(browser.finish_download(
            7,
            crate::http::Response {
                url: url::Url::parse("https://example.test/report.pdf").unwrap(),
                status: 200,
                content_type: String::from("application/pdf"),
                headers: vec![("content-length".into(), "9".into())],
                body: b"%PDF-1.7".to_vec(),
                rendered: None,
                js: None,
                blobs: None,
                live: None,
                declarative_refresh: None,
                challenge: None,
                from_post: false,
                timing: None,
            },
        ));

        assert_eq!(browser.current.as_ref().unwrap().target, current);
        assert_eq!(
            browser.download_offer().unwrap().suggested_filename,
            "report.pdf"
        );
        assert!(browser.pending.is_none());
    }

    #[test]
    fn form_value_acknowledgements_do_not_release_newer_edits() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut browser =
            BrowserController::new(runtime.handle().clone(), || {}, CssSize::new(640., 480.));
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        browser.live_page = Some(crate::js::PageHandle::from_test_sender(tx));
        for value in ["a", "ab"] {
            browser.handle_action(UserAction::SetFormValue {
                actor: Some(42),
                value: value.into(),
                checked: None,
            });
        }
        for _ in 0..2 {
            assert!(matches!(
                rx.try_recv(),
                Ok(crate::js::PageCmd::SetValue { node: 42, .. })
            ));
        }
        assert!(browser.form_value_pending(42));
        assert!(!browser.handle_page_event(crate::js::PageEvt::FormValueApplied { node: 42 }));
        assert!(browser.form_value_pending(42));
        assert!(browser.handle_page_event(crate::js::PageEvt::FormValueApplied { node: 42 }));
        assert!(!browser.form_value_pending(42));
        assert!(!browser.handle_page_event(crate::js::PageEvt::FormValueApplied { node: 42 }));
    }

    #[test]
    fn page_keyboard_actions_preserve_focus_routing_and_release_state() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut browser =
            BrowserController::new(runtime.handle().clone(), || {}, CssSize::new(640.0, 480.0));
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        browser.live_page = Some(crate::js::PageHandle::from_test_sender(tx));
        for state in [KeyState::Pressed, KeyState::Released] {
            let input = KeyInput {
                key: Key::Character(String::from("z")),
                code: String::from("KeyY"),
                location: 0,
                state,
                modifiers: Modifiers::default(),
                repeat: false,
                composing: false,
            };
            browser.handle_action(UserAction::Key(input.clone()));
            assert!(
                matches!(rx.try_recv(), Ok(crate::js::PageCmd::Key { node: None, input: actual }) if actual == input)
            );
            browser.handle_action(UserAction::PageKey {
                node: 42,
                input: input.clone(),
            });
            assert!(
                matches!(rx.try_recv(), Ok(crate::js::PageCmd::Key { node: Some(42), input: actual }) if actual == input)
            );
        }
        browser.handle_page_event(crate::js::PageEvt::KeyDefault { prevented: true });
        browser.handle_page_event(crate::js::PageEvt::KeyDefault { prevented: false });
        assert_eq!(browser.take_page_key_default(), Some(true));
        assert_eq!(browser.take_page_key_default(), Some(false));
        assert_eq!(browser.take_page_key_default(), None);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn native_input_backpressure_preserves_bursts_focus_clicks_and_releases() {
        for capacity in [1, 16] {
            let wakes = Arc::new(AtomicUsize::new(0));
            let wake_count = wakes.clone();
            let mut browser = BrowserController::new(
                Handle::current(),
                move || {
                    wake_count.fetch_add(1, Ordering::Relaxed);
                },
                CssSize::new(640.0, 480.0),
            );
            let (tx, mut rx) = tokio::sync::mpsc::channel(capacity);
            browser.live_page = Some(crate::js::PageHandle::from_test_sender(tx));
            browser.send_user(crate::js::PageCmd::Focus(Some(42)));
            for index in 0..100 {
                for state in [KeyState::Pressed, KeyState::Released] {
                    browser.handle_action(UserAction::Key(KeyInput {
                        key: Key::Character(index.to_string()),
                        code: "KeyA".into(),
                        location: 0,
                        state,
                        modifiers: Default::default(),
                        repeat: false,
                        composing: false,
                    }));
                }
            }
            browser.send_user(crate::js::PageCmd::Focus(None));
            browser.send_navigation_click(99);
            assert!(!browser.pending_user_input.is_empty());
            assert!(browser.user_input_retry.is_some());
            // No page acknowledgements are sent. Capacity itself must wake the
            // controller, including when the lane contains only focus/click tasks.
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                for ordinal in 0..203 {
                    let command = loop {
                        browser.process_async_events();
                        if let Ok(command) = rx.try_recv() {
                            break command;
                        }
                        tokio::task::yield_now().await;
                    };
                    match (ordinal, command) {
                        (0, crate::js::PageCmd::Focus(Some(42))) => {}
                        (201, crate::js::PageCmd::Focus(None)) => {}
                        (202, crate::js::PageCmd::Click(99)) => {}
                        (ordinal, crate::js::PageCmd::Key { node: None, input })
                            if (1..201).contains(&ordinal) =>
                        {
                            assert_eq!(input.key, Key::Character(((ordinal - 1) / 2).to_string()));
                            assert_eq!(
                                input.state,
                                if ordinal % 2 == 1 {
                                    KeyState::Pressed
                                } else {
                                    KeyState::Released
                                }
                            );
                        }
                        (ordinal, command) => panic!("input {ordinal} out of order: {command:?}"),
                    }
                }
            })
            .await
            .expect("input queue did not wake on capacity");
            assert!(browser.pending_user_input.is_empty());
            assert!(browser.user_input_retry.is_none());
            assert!(wakes.load(Ordering::Relaxed) > 0);
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn native_input_backpressure_retires_stale_capacity_and_bounds_overload() {
        let mut browser =
            BrowserController::new(Handle::current(), || {}, CssSize::new(640.0, 480.0));
        let (old_tx, mut old_rx) = tokio::sync::mpsc::channel(1);
        browser.live_page = Some(crate::js::PageHandle::from_test_sender(old_tx));
        browser.send_user(crate::js::PageCmd::Focus(Some(1)));
        browser.send_user(crate::js::PageCmd::Focus(Some(2)));
        assert!(old_rx.try_recv().is_ok());
        tokio::task::yield_now().await; // The old generation now owns a reserved slot/wake.
        browser.stop();
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        browser.live_page = Some(crate::js::PageHandle::from_test_sender(tx));
        browser.send_user(crate::js::PageCmd::Focus(Some(3)));
        browser.process_async_events();
        assert!(matches!(
            rx.try_recv(),
            Ok(crate::js::PageCmd::Focus(Some(3)))
        ));
        assert!(
            old_rx.try_recv().is_err(),
            "retired input must not be delivered"
        );
        for _ in 0..4097 {
            browser.send_user(crate::js::PageCmd::Focus(None));
        }
        let overflow = browser.handle_action(UserAction::PageFocus { actor: None });
        assert!(overflow.invalidated && overflow.loading_retired);
        assert!(!browser.page_is_live());
        assert!(browser.pending_user_input.is_empty() && browser.user_input_retry.is_none());
        assert!(browser.status.contains("input backlog"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn native_input_backpressure_releases_closed_actor_and_waiter() {
        let mut browser =
            BrowserController::new(Handle::current(), || {}, CssSize::new(640.0, 480.0));
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        browser.live_page = Some(crate::js::PageHandle::from_test_sender(tx));
        browser.send_user(crate::js::PageCmd::Focus(None));
        browser.send_user(crate::js::PageCmd::Focus(None));
        drop(rx);
        tokio::task::yield_now().await;
        browser.process_async_events();
        assert!(!browser.page_is_live());
        assert!(browser.pending_user_input.is_empty() && browser.user_input_retry.is_none());
    }

    #[test]
    fn image_size_delivery_reports_a_full_page_queue_for_retry() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut browser =
            BrowserController::new(runtime.handle().clone(), || {}, CssSize::new(640.0, 480.0));
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        tx.try_send(crate::js::PageCmd::Click(0)).unwrap();
        browser.live_page = Some(crate::js::PageHandle::from_test_sender(tx));
        let sizes = crate::layout2::ImageSizes::from([(
            String::from("https://example.test/hero.jpg"),
            (640, 360),
        )]);

        assert!(!browser.send_image_sizes(&sizes));
        assert!(matches!(rx.try_recv(), Ok(crate::js::PageCmd::Click(0))));
        assert!(browser.send_image_sizes(&sizes));
        assert!(matches!(
            rx.try_recv(),
            Ok(crate::js::PageCmd::ImageSizes(_))
        ));
    }

    #[test]
    fn static_actor_event_retains_the_settled_document_source() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut browser =
            BrowserController::new(runtime.handle().clone(), || {}, CssSize::new(640.0, 480.0));
        browser.current = Some(BrowserPage {
            target: Link::Http(url::Url::parse("https://example.com/").unwrap()),
            fallback_http: false,
            document: FetchedDocument::Http(Box::new(crate::http::Response {
                url: url::Url::parse("https://example.com/").unwrap(),
                status: 200,
                content_type: String::from("text/html"),
                headers: Vec::new(),
                body: b"<p>server source</p>".to_vec(),
                rendered: None,
                js: None,
                blobs: None,
                live: None,
                declarative_refresh: None,
                challenge: None,
                from_post: false,
                timing: None,
            })),
            status: String::from("Ready"),
            rendered: None,
            rendered_revision: 1,
            revision: 1,
        });

        assert!(browser.handle_page_event(crate::js::PageEvt::Static {
            html: String::from("<html><body><p>settled DOM</p></body></html>"),
            outcome: Default::default(),
        }));
        let FetchedDocument::Http(response) = &browser.current.as_ref().unwrap().document else {
            panic!("expected HTTP document");
        };
        assert_eq!(
            response.body,
            b"<html><body><p>settled DOM</p></body></html>"
        );
    }

    #[test]
    fn a_static_actor_event_makes_the_render_final() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut browser =
            BrowserController::new(runtime.handle().clone(), || {}, CssSize::new(640.0, 480.0));
        browser.current = Some(BrowserPage {
            target: Link::Http(url::Url::parse("https://example.com/").unwrap()),
            fallback_http: false,
            document: FetchedDocument::Internal(Vec::new()),
            status: String::from("Ready"),
            rendered: None,
            rendered_revision: 1,
            revision: 1,
        });
        assert!(!browser.page_render_is_final());

        assert!(browser.handle_page_event(crate::js::PageEvt::Static {
            html: String::from("<p>settled</p>"),
            outcome: Default::default(),
        }));
        assert!(
            browser.page_render_is_final(),
            "the actor retired itself, so nothing further can arrive"
        );
    }

    #[test]
    fn a_document_that_gets_no_actor_is_final_as_it_commits() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut browser =
            BrowserController::new(runtime.handle().clone(), || {}, CssSize::new(640.0, 480.0));

        // Internal gemtext never starts a resident actor, so its first render
        // is its last; a driver must not have to guess that from a clock.
        browser.begin_internal_gemtext(
            Link::Http(url::Url::parse("about:help").unwrap()),
            b"= Help\n".to_vec(),
            NavigationIntent::New,
        );
        assert!(!browser.page_is_live());
        assert!(browser.page_render_is_final());

        // A navigation reopens the question for the new document. This runtime
        // is never driven, so the fetch task is dropped before it can reach the
        // network — a unit test must not probe a live site.
        browser.begin_fetch(
            Link::Http(url::Url::parse("https://example.com/").unwrap()),
            false,
            NavigationIntent::New,
        );
        assert!(!browser.page_render_is_final());
    }

    #[test]
    fn stopping_leaves_nothing_to_wait_for() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut browser =
            BrowserController::new(runtime.handle().clone(), || {}, CssSize::new(640.0, 480.0));
        let generation = browser.generation;
        browser.pending = Some(PendingNavigation {
            generation,
            target: Link::Http(url::Url::parse("https://example.com/").unwrap()),
            fallback_http: false,
            intent: NavigationIntent::New,
        });

        assert!(browser.stop());
        assert!(
            browser.page_render_is_final(),
            "a stopped document's fetch, actor, and refresh timer are all gone"
        );
    }

    #[test]
    fn an_unparsable_address_leaves_nothing_to_wait_for() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut browser =
            BrowserController::new(runtime.handle().clone(), || {}, CssSize::new(640.0, 480.0));

        assert!(browser.begin_address("not a valid address", NavigationIntent::New));
        assert!(
            browser.page_render_is_final(),
            "an address that never became a fetch can never produce a render"
        );
    }

    #[test]
    fn a_failed_fetch_leaves_nothing_to_wait_for() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut browser =
            BrowserController::new(runtime.handle().clone(), || {}, CssSize::new(640.0, 480.0));
        let generation = browser.generation;
        browser.pending = Some(PendingNavigation {
            generation,
            target: Link::Http(url::Url::parse("https://example.com/").unwrap()),
            fallback_http: false,
            intent: NavigationIntent::New,
        });

        assert!(browser.finish_fetch(generation, Err(String::from("connection reset"))));
        assert!(
            browser.page_render_is_final(),
            "a document that never committed cannot render again"
        );
    }

    #[test]
    fn page_originated_horizontal_scroll_invalidates_the_cached_display_list() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut browser =
            BrowserController::new(runtime.handle().clone(), || {}, CssSize::new(640.0, 480.0));
        browser.current = Some(BrowserPage {
            target: Link::Http(url::Url::parse("https://example.com/").unwrap()),
            fallback_http: false,
            document: FetchedDocument::Internal(Vec::new()),
            status: String::from("Ready"),
            rendered: None,
            rendered_revision: 7,
            revision: 7,
        });

        assert!(browser.handle_page_event(crate::js::PageEvt::Scrolled {
            node: 17,
            top: 0.0,
            left: 100.0,
        }));
        assert_eq!(browser.current.as_ref().unwrap().revision, 8);
        assert_eq!(
            browser.interaction().nested_scroll.get(&17),
            Some(&CssPoint::new(100.0, 0.0))
        );

        // A duplicate notification changes no pixels and must not churn the
        // retained desktop layout cache.
        assert!(!browser.handle_page_event(crate::js::PageEvt::Scrolled {
            node: 17,
            top: 0.0,
            left: 100.0,
        }));
        assert_eq!(browser.current.as_ref().unwrap().revision, 8);
    }

    #[test]
    fn address_parser_preserves_protocol_and_web_fallback_intent() {
        let (web, fallback) = parse_navigation_target("example.com").unwrap();
        assert!(matches!(web, Link::Http(_)));
        assert!(fallback);

        let (gemini, fallback) = parse_navigation_target("gemini://geminiprotocol.net/").unwrap();
        assert!(matches!(gemini, Link::Gemini(_)));
        assert!(!fallback);

        let (gopher, fallback) = parse_navigation_target("example.com:70").unwrap();
        assert!(matches!(gopher, Link::Gopher(_)));
        assert!(!fallback);

        let (local, fallback) = parse_navigation_target("/tmp/IdleHeart.png").unwrap();
        assert!(!fallback);
        assert!(
            matches!(local, Link::Http(url) if url.scheme() == "file" && url.path() == "/tmp/IdleHeart.png")
        );
        let (file_url, fallback) = parse_navigation_target("file:///tmp/IdleHeart.png").unwrap();
        assert!(!fallback);
        assert!(matches!(file_url, Link::Http(url) if url.scheme() == "file"));

        assert!(matches!(
            parse_navigation_target("telnet://example.com"),
            Ok((
                Link::Telnet {
                    port: 23,
                    tls: false,
                    ..
                },
                false
            ))
        ));
        let (whois, fallback) =
            parse_navigation_target("WHOIS://[::1]:4343/%65xample.com#local").unwrap();
        assert!(!fallback);
        assert!(
            matches!(whois, Link::OneShot(url) if url.host == "::1" && url.query == "example.com")
        );
        assert!(parse_navigation_target("WHOIS://host:bad/query").is_err());
        let action = crate::rdap::action("example.com").unwrap();
        assert_eq!(
            parse_navigation_target(&action.to_string()).unwrap(),
            (action, false)
        );
    }

    #[test]
    fn typed_clicked_and_scripted_youtube_players_delegate_without_fetching() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut browser =
            BrowserController::new(runtime.handle().clone(), || {}, CssSize::new(640.0, 480.0));

        browser.handle_action(UserAction::Navigate(String::from(
            "https://127.0.0.1:9/superseded",
        )));
        assert!(browser.snapshot().loading);

        let outcome = browser.handle_action(UserAction::Navigate(String::from(
            "youtube.com/watch?v=typed123",
        )));
        assert!(outcome.invalidated);
        assert!(outcome.loading_retired);
        assert!(!browser.snapshot().loading);
        let (url, referrer) = browser.take_external_media().unwrap();
        assert_eq!(url.as_str(), "https://youtube.com/watch?v=typed123");
        assert!(referrer.is_none());

        browser.handle_action(UserAction::Activate(Link::Http(
            url::Url::parse("https://youtu.be/clicked123?si=share").unwrap(),
        )));
        let (url, _) = browser.take_external_media().unwrap();
        assert_eq!(url.host_str(), Some("youtu.be"));

        assert!(
            browser.handle_page_event(crate::js::PageEvt::Replace(String::from(
                "https://www.youtube.com/shorts/scripted123",
            )))
        );
        let (url, _) = browser.take_external_media().unwrap();
        assert_eq!(url.path(), "/shorts/scripted123");
        assert!(browser.current_page().is_none());

        browser.begin_post(
            url::Url::parse("https://www.youtube.com/watch?v=posted123").unwrap(),
            String::from("ignored=body"),
        );
        let (url, _) = browser.take_external_media().unwrap();
        assert_eq!(url.query(), Some("v=posted123"));

        browser.handle_action(UserAction::Navigate(String::from(
            "https://www.youtube.com/results?search_query=rust",
        )));
        assert!(browser.snapshot().loading);
        assert!(browser.take_external_media().is_none());
    }

    #[test]
    fn media_activation_carries_the_source_page_referrer_and_keeps_it_live() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut browser =
            BrowserController::new(runtime.handle().clone(), || {}, CssSize::new(640.0, 480.0));
        let source = url::Url::parse("https://www.example.test/album/123").unwrap();
        browser.current = Some(BrowserPage {
            target: Link::Http(source.clone()),
            fallback_http: false,
            document: FetchedDocument::Internal(Vec::new()),
            status: String::from("Ready"),
            rendered: None,
            rendered_revision: 1,
            revision: 1,
        });
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        browser.live_page = Some(crate::js::PageHandle::from_test_sender(tx));

        for target in [
            "https://cdn.example.test/video.mp4",
            "https://cdn.example.test/audio.ogg",
        ] {
            let target = url::Url::parse(target).unwrap();
            let outcome = browser.handle_action(UserAction::Activate(Link::Media(target.clone())));
            assert!(outcome.invalidated);
            assert!(!outcome.loading_retired);
            assert_eq!(
                browser.take_external_media(),
                Some((target, Some(source.clone())))
            );
            assert!(browser.page_is_live());
            assert!(!browser.snapshot().loading);
            assert_eq!(
                browser.current_page().unwrap().target(),
                &Link::Http(source.clone())
            );
            assert!(
                rx.try_recv().is_err(),
                "external playback must not click the page player"
            );
            assert!(browser.back.is_empty());
        }

        browser.open_internal_gemtext("about:help", Vec::new());
        let target = url::Url::parse("https://cdn.example.test/video.mp4").unwrap();
        browser.handle_action(UserAction::Activate(Link::Media(target.clone())));
        assert_eq!(browser.take_external_media(), Some((target, None)));
    }

    #[tokio::test]
    async fn file_desktop_navigation_distinguishes_user_input_from_document_requests() {
        use std::time::Duration;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Idle Heart.PNG");
        std::fs::write(&path, include_bytes!("../assets/IdleHeart30.png")).unwrap();
        let target = url::Url::from_file_path(&path).unwrap();
        let mut browser = BrowserController::new(
            tokio::runtime::Handle::current(),
            || {},
            CssSize::new(640.0, 480.0),
        );
        let remote = Link::Http(url::Url::parse("https://example.test/").unwrap());
        browser.current = Some(BrowserPage {
            target: remote.clone(),
            fallback_http: false,
            document: FetchedDocument::Internal(Vec::new()),
            status: "Ready".into(),
            rendered: None,
            rendered_revision: 1,
            revision: 1,
        });
        browser.activate(Link::Http(target.clone()));
        assert!(!browser.snapshot().loading);
        assert!(
            browser
                .snapshot()
                .status
                .contains("Local file navigation blocked")
        );
        for event in [
            crate::js::PageEvt::Navigate(target.to_string()),
            crate::js::PageEvt::Replace(target.to_string()),
            crate::js::PageEvt::Reload(target.to_string()),
        ] {
            browser.handle_page_event(event);
            assert!(!browser.snapshot().loading);
            assert_eq!(browser.current_page().unwrap().target, remote);
        }
        // The exact desktop CLI path -> Navigate route is a user grant.
        browser.handle_action(UserAction::Navigate(path.display().to_string()));
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                browser.process_async_events();
                if !browser.snapshot().loading {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let page = browser.current_page().unwrap();
        assert_eq!(
            page.target,
            Link::Http(target.clone()),
            "{}",
            browser.snapshot().status
        );
        let FetchedDocument::Http(response) = &page.document else {
            panic!("file image document")
        };
        assert_eq!(response.url, target);
        assert_eq!(response.content_type, "text/html; charset=utf-8");
        assert!(
            page.rendered_page().is_some(),
            "local image has a graphical document"
        );
        browser.open_internal_gemtext("about:bookmarks", Vec::new());
        browser.activate(Link::Http(target));
        assert!(
            browser.snapshot().loading,
            "trusted bookmarks remain a user grant"
        );
        browser.stop();
    }

    #[test]
    fn spa_history_watch_update_preserves_page_and_delegates_to_mpv() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut browser =
            BrowserController::new(runtime.handle().clone(), || {}, CssSize::new(640.0, 480.0));
        let source =
            url::Url::parse("https://www.youtube.com/results?search_query=squirrels").unwrap();
        browser.current = Some(BrowserPage {
            target: Link::Http(source.clone()),
            fallback_http: false,
            document: FetchedDocument::Internal(Vec::new()),
            status: String::from("Ready"),
            rendered: None,
            rendered_revision: 1,
            revision: 1,
        });
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        browser.live_page = Some(crate::js::PageHandle::from_test_sender(tx));

        assert!(
            browser.handle_page_event(crate::js::PageEvt::HistoryUpdate {
                url: String::from("https://www.youtube.com/watch?v=spa123"),
                replace: false,
            })
        );

        assert!(
            browser.page_is_live(),
            "pushState must retain the Document realm"
        );
        assert!(
            !browser.snapshot().loading,
            "pushState must not start a fetch"
        );
        assert_eq!(
            browser.snapshot().address,
            "https://www.youtube.com/watch?v=spa123"
        );
        let (video, referrer) = browser.take_external_media().unwrap();
        assert_eq!(video.as_str(), "https://www.youtube.com/watch?v=spa123");
        assert_eq!(referrer.as_ref(), Some(&source));
    }

    #[test]
    fn redirected_youtube_result_is_delegated_before_document_commit() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut browser =
            BrowserController::new(runtime.handle().clone(), || {}, CssSize::new(640.0, 480.0));
        browser.handle_action(UserAction::Navigate(String::from(
            "https://127.0.0.1:9/redirect-source",
        )));
        let generation = browser.generation;
        browser.task.take().unwrap().abort();

        let url = url::Url::parse("https://www.youtube.com/watch?v=redirect123").unwrap();
        let event =
            interactive_fetch_event(generation, Ok(InteractiveFetch::ExternalMedia(url.clone())));
        runtime.block_on(browser.tx.send(event)).unwrap();
        let outcome = browser.process_async_events();

        assert!(outcome.invalidated);
        assert!(!browser.snapshot().loading);
        assert!(browser.current_page().is_none());
        assert_eq!(browser.take_external_media().unwrap().0, url);
    }

    #[test]
    fn async_fetch_completion_uses_the_explicit_invalidation_hook() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::time::Duration;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 2048];
            let _ = stream.read(&mut request).unwrap();
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                )
                .unwrap();
        });
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let (wake_tx, wake_rx) = mpsc::channel();
        let mut browser = BrowserController::new(
            runtime.handle().clone(),
            move || {
                let _ = wake_tx.send(());
            },
            CssSize::new(800.0, 600.0),
        );
        browser.handle_action(UserAction::Navigate(format!("http://{address}/")));

        wake_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        assert!(browser.process_async_events().invalidated);
        assert!(!browser.snapshot().loading);
        assert!(matches!(
            browser.current_page().map(|page| &page.document),
            Some(FetchedDocument::Http(_))
        ));
        server.join().unwrap();
    }

    #[test]
    fn successful_navigation_commits_back_and_forward_only_on_arrival() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut browser =
            BrowserController::new(runtime.handle().clone(), || {}, CssSize::new(800.0, 600.0));
        let target = |host: &str| {
            Link::Gopher(gopher::GopherUrl {
                host: host.to_string(),
                port: 70,
                tls: false,
                item_type: '1',
                query: None,
                gopher_plus: None,
                selector: Vec::new(),
            })
        };
        let arrive = |browser: &mut BrowserController,
                      generation: u64,
                      target: Link,
                      intent: NavigationIntent| {
            browser.pending = Some(PendingNavigation {
                generation,
                target,
                fallback_http: false,
                intent,
            });
            assert!(browser.finish_fetch(
                generation,
                Ok(FetchedDocument::Gopher(vec![generation as u8].into()))
            ));
        };

        arrive(
            &mut browser,
            1,
            target("one.example"),
            NavigationIntent::New,
        );
        arrive(
            &mut browser,
            2,
            target("two.example"),
            NavigationIntent::New,
        );
        assert!(browser.snapshot().can_go_back);
        assert!(!browser.snapshot().can_go_forward);

        // Location.replace() commits the destination in the current slot: the
        // replaced intermediary never grows the back trail.
        arrive(
            &mut browser,
            3,
            target("replacement.example"),
            NavigationIntent::Replace,
        );
        assert_eq!(browser.back.len(), 1);
        assert_eq!(
            browser.current_page().unwrap().address(),
            "gopher://replacement.example/1"
        );

        // Merely beginning travel leaves the trail intact; successful arrival
        // is the commit point, matching the terminal browser's failure-safe
        // history behavior.
        browser.pending = Some(PendingNavigation {
            generation: 4,
            target: target("one.example"),
            fallback_http: false,
            intent: NavigationIntent::Back,
        });
        assert!(browser.snapshot().can_go_back);
        assert!(browser.finish_fetch(4, Ok(FetchedDocument::Gopher(vec![4].into()))));
        assert!(!browser.snapshot().can_go_back);
        assert!(browser.snapshot().can_go_forward);
        assert_eq!(
            browser.current_page().unwrap().address(),
            "gopher://one.example/1"
        );
    }

    #[tokio::test]
    async fn gemini_image_handoff_keeps_encoded_path_and_completion_notice() {
        let mut browser =
            BrowserController::new(Handle::current(), || {}, CssSize::new(800.0, 600.0));
        let url = gemini::GeminiUrl::new("127.0.0.8", 1, "/dir/%2e/image.png");
        browser.begin_fetch(Link::Gemini(url.clone()), false, NavigationIntent::New);
        browser.task.take().unwrap().abort();
        let mut response = gemini::Response::new(url.clone(), 20, "image/png".into());
        response.body = include_bytes!("../assets/IdleHeart30.png").to_vec();
        response.notice = Some("Incomplete response".into());
        let status = response.status_text();
        browser
            .tx
            .send(CoreEvent::GeminiImage {
                generation: browser.generation,
                url: url.clone(),
                response: Box::new(gemini::image_response(response).unwrap()),
                status,
            })
            .await
            .unwrap();
        browser.process_async_events();
        assert_eq!(browser.current_page().unwrap().target(), &Link::Gemini(url));
        assert!(browser.snapshot().status.contains("Incomplete"));
        assert!(
            matches!(&browser.current_page().unwrap().document, FetchedDocument::Http(r) if r.body.starts_with(b"<!doctype html>"))
        );
    }

    #[tokio::test]
    async fn gemini_controller_prompts_final_urls_progress_errors_and_history() {
        let mut browser =
            BrowserController::new(Handle::current(), || {}, CssSize::new(800.0, 600.0));
        let original = gemini::GeminiUrl::new("127.0.0.8", 1, "/original");
        browser.begin_fetch(Link::Gemini(original.clone()), false, NavigationIntent::New);
        browser.task.take().unwrap().abort();
        let final_url = gemini::GeminiUrl::new("127.0.0.8", 1, "/final/page");
        let mut response = gemini::Response::new(final_url.clone(), 20, "text/gemini".into());
        response.body = b"# Heading\n=> sibling Relative link\n".to_vec();
        response.finished = false;
        assert!(browser.update_gemini(browser.generation, response.clone()));
        assert_eq!(
            browser.current_page().unwrap().address(),
            final_url.to_string()
        );
        let doc = crate::render::documents::document(browser.current_page().unwrap()).unwrap();
        assert_eq!(
            doc.lines[1].link,
            Some(Link::Gemini(gemini::GeminiUrl::new(
                "127.0.0.8",
                1,
                "/final/sibling"
            )))
        );
        browser.reply_view_action("alt", Some(true));
        response.finished = true;
        browser.update_gemini(browser.generation, response);
        assert!(browser.back.is_empty());
        browser.begin_fetch(Link::Gemini(original.clone()), false, NavigationIntent::New);
        browser.task.take().unwrap().abort();
        browser.update_gemini(
            browser.generation,
            gemini::Response::new(original.clone(), 11, "Password".into()),
        );
        assert!(browser.gemini_prompt().unwrap().sensitive);
        browser.submit_gemini_prompt("private value");
        browser.task.take().unwrap().abort();
        assert!(!browser.snapshot().address.contains("private"));
        let secret = original.with_input("private value", true).unwrap();
        browser.update_gemini(
            browser.generation,
            gemini::Response::new(secret, 51, "Not found".into()),
        );
        assert!(browser.gemini_prompt().is_none());
        let doc = crate::render::documents::document(browser.current_page().unwrap()).unwrap();
        assert_eq!(doc.lines[0].text, "Not found");
        assert!(!browser.snapshot().address.contains('?'));
        browser.begin_history(false);
        let page = browser.current_page().unwrap();
        assert_eq!(page.address(), final_url.to_string());
        let FetchedDocument::Gemini(response) = &page.document else {
            panic!("Gemini history lost")
        };
        assert!(response.view.show_alt);
        assert!(browser.task.is_none());
    }

    #[test]
    fn internal_documents_use_the_normal_back_and_forward_trail() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut browser =
            BrowserController::new(runtime.handle().clone(), || {}, CssSize::new(800.0, 600.0));

        browser.open_internal_gemtext("about:help", b"# help".to_vec());
        browser.open_internal_gemtext("about:status", b"# status".to_vec());
        assert!(browser.snapshot().can_go_back);

        browser.handle_action(UserAction::Back);
        let page = browser.current_page().expect("back commits synchronously");
        assert_eq!(page.address(), "about:help");
        assert!(matches!(&page.document, FetchedDocument::Internal(source) if source == b"# help"));

        browser.handle_action(UserAction::Forward);
        let page = browser
            .current_page()
            .expect("forward commits synchronously");
        assert_eq!(page.address(), "about:status");
        assert!(
            matches!(&page.document, FetchedDocument::Internal(source) if source == b"# status")
        );
    }

    #[test]
    fn navigation_retires_loading_without_reidentifying_the_visible_document() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut browser =
            BrowserController::new(runtime.handle().clone(), || {}, CssSize::new(800.0, 600.0));
        let target = |host: &str| {
            Link::Gopher(gopher::GopherUrl {
                host: host.to_string(),
                port: 70,
                tls: false,
                item_type: '1',
                query: None,
                gopher_plus: None,
                selector: Vec::new(),
            })
        };
        browser.generation = 1;
        browser.pending = Some(PendingNavigation {
            generation: 1,
            target: target("old.example"),
            fallback_http: false,
            intent: NavigationIntent::New,
        });
        assert!(browser.finish_fetch(1, Ok(FetchedDocument::Gopher(vec![1].into()))));
        assert_eq!(browser.document_generation(), 1);

        let outcome = browser.handle_action(UserAction::Activate(target("next.example")));
        assert!(outcome.loading_retired);
        assert_eq!(
            browser.document_generation(),
            1,
            "starting navigation must leave the frozen document's identity intact"
        );
        assert_eq!(
            browser.current_page().unwrap().address(),
            "gopher://old.example/1"
        );

        let generation = browser.pending.as_ref().unwrap().generation;
        assert!(browser.finish_fetch(generation, Err(String::from("offline"))));
        assert_eq!(browser.document_generation(), 1);
        assert_eq!(
            browser.current_page().unwrap().address(),
            "gopher://old.example/1"
        );
    }
}

#[cfg(test)]
mod finger_tests {
    use super::*;
    use crate::finger::Reply;
    fn reply(text: &str, finished: bool) -> Reply {
        Reply {
            body: text.as_bytes().to_vec(),
            finished,
            notice: None,
        }
    }
    fn start(browser: &mut BrowserController, name: &str, intent: NavigationIntent) {
        browser.generation += 1;
        browser.pending = Some(PendingNavigation {
            generation: browser.generation,
            target: Link::OneShot(
                oneshot::OneShotUrl::parse(&format!("finger://example.test/{name}")).unwrap(),
            ),
            fallback_http: false,
            intent,
        });
        browser.task = Some(tokio::spawn(std::future::pending()));
    }
    #[tokio::test]
    async fn finger_stream_history_stop_and_stale_updates() {
        let mut browser =
            BrowserController::new(Handle::current(), || {}, CssSize::new(800.0, 600.0));
        start(&mut browser, "alice", NavigationIntent::New);
        assert!(browser.update_finger(1, reply("old\r\n", true)));
        start(&mut browser, "bob", NavigationIntent::New);
        assert!(browser.update_finger(2, reply("partial\r\n", false)));
        assert_eq!(browser.back.len(), 1);
        assert!(browser.pending.is_some() && browser.task.is_some());
        assert!(!browser.render_is_final);
        let revision = browser.current.as_ref().unwrap().revision;
        assert!(browser.update_finger(2, reply("partial\r\nmore\r\n", false)));
        assert_eq!(browser.back.len(), 1);
        assert!(browser.current.as_ref().unwrap().revision > revision);
        assert!(browser.stop());
        assert!(!browser.update_finger(2, reply("late", true)));
        let FetchedDocument::Finger(page) = &browser.current.as_ref().unwrap().document else {
            panic!()
        };
        assert_eq!(page.reply.body, b"partial\r\nmore\r\n");
        assert!(page.reply.notice.as_ref().unwrap().contains("stopped"));
    }
    #[tokio::test]
    async fn finger_reload_retains_one_comparison_and_view_preferences() {
        let mut browser =
            BrowserController::new(Handle::current(), || {}, CssSize::new(800.0, 600.0));
        start(&mut browser, "alice", NavigationIntent::New);
        browser.update_finger(1, reply("old", true));
        assert!(browser.reply_view_action("wrap", Some(true)));
        for (generation, text) in [(2, "new"), (3, "newest")] {
            start(&mut browser, "alice", NavigationIntent::Reload);
            browser.update_finger(generation, reply(text, false));
            browser.update_finger(generation, reply(text, true));
        }
        assert!(browser.back.is_empty());
        let FetchedDocument::Finger(page) = &browser.current.as_ref().unwrap().document else {
            panic!()
        };
        assert!(page.view.wrap);
        assert_eq!(page.view.previous.as_deref(), Some(b"new".as_slice()));
        assert!(browser.reply_view_action("changes", Some(true)));
        let doc = crate::render::documents::document(browser.current.as_ref().unwrap()).unwrap();
        assert!(doc.lines.iter().any(|line| line.text == "+ newest"));
    }
}

#[cfg(test)]
mod whois_tests {
    use super::*;

    fn target(query: &str) -> Link {
        Link::OneShot(crate::whois::server_target("example.test", query).unwrap())
    }
    fn reply(query: &str, bytes: &[u8], finished: bool) -> crate::whois::Reply {
        let Link::OneShot(url) = target(query) else {
            unreachable!()
        };
        let mut reply = crate::whois::Reply::from_bytes(url, bytes.to_vec());
        reply.finished = finished;
        if !finished {
            reply.hops[0].state = crate::whois::HopState::Receiving;
        }
        reply
    }
    fn start(browser: &mut BrowserController, query: &str, intent: NavigationIntent) {
        browser.generation += 1;
        browser.pending = Some(PendingNavigation {
            generation: browser.generation,
            target: target(query),
            fallback_http: false,
            intent,
        });
        browser.task = Some(tokio::spawn(std::future::pending()));
    }

    #[tokio::test]
    async fn whois_local_sections_keep_the_pending_stream_and_history() {
        let mut browser =
            BrowserController::new(Handle::current(), || {}, CssSize::new(800.0, 600.0));
        start(&mut browser, "example.com", NavigationIntent::New);
        browser.update_whois(
            1,
            reply(
                "example.com",
                b"Domain Name: example.com\nRegistrar: Early\n",
                false,
            ),
        );
        browser.activate(crate::registration::Section::Raw.action());
        assert!(browser.pending.is_some() && browser.task.is_some());
        assert!(browser.back.is_empty());
        browser.update_whois(
            1,
            reply(
                "example.com",
                b"Domain Name: example.com\nRegistrar: Later\n",
                true,
            ),
        );
        let FetchedDocument::Whois(page) = &browser.current.as_ref().unwrap().document else {
            panic!()
        };
        assert_eq!(page.section, crate::registration::Section::Raw);
        assert!(browser.back.is_empty());
    }

    #[tokio::test]
    async fn rdap_http_navigation_and_typed_links_use_the_record_presenter() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url =
            url::Url::parse(&format!("http://{}/record", listener.local_addr().unwrap())).unwrap();
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            for mime in [
                "application/rdap+json",
                "application/json",
                "application/json",
            ] {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    socket.read_exact(&mut byte).await.unwrap();
                    request.push(byte[0]);
                    assert!(request.len() < 16384);
                }
                requests.push(String::from_utf8(request).unwrap());
                let body = r#"{"objectClassName":"domain","ldhName":"example.test"}"#;
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            }
            requests
        });
        let response = fetch_protocol_interactive(
            &Link::Http(url.clone()),
            false,
            None,
            CssSize::new(800.0, 600.0),
            1.0,
            (0, 0),
            Default::default(),
            None,
            NavigationIntent::New,
        )
        .await
        .unwrap();
        assert!(matches!(
            response,
            InteractiveFetch::Document(FetchedDocument::Rdap(_))
        ));
        let response = fetch_protocol(&crate::rdap::direct_action(&url), false, None)
            .await
            .unwrap();
        assert!(matches!(response, FetchedDocument::Rdap(_)));
        let response = fetch_protocol(&Link::Http(url), false, None).await.unwrap();
        assert!(
            matches!(response, FetchedDocument::Http(_)),
            "ordinary JSON keeps its normal HTTP presentation"
        );
        let requests = server.await.unwrap();
        assert!(
            requests[1]
                .to_ascii_lowercase()
                .contains("accept: application/rdap+json")
        );
    }

    #[tokio::test]
    async fn rdap_sections_save_and_reload_preserve_the_received_record() {
        let mut browser =
            BrowserController::new(Handle::current(), || {}, CssSize::new(800.0, 600.0));
        start(&mut browser, "example.test", NavigationIntent::New);
        let page = crate::rdap::Page::from_response(crate::rdap::tests::response(
            serde_json::json!({"objectClassName":"domain", "ldhName":"example.test"}),
        ));
        let raw = page.raw.clone();
        browser.finish_fetch(1, Ok(FetchedDocument::Rdap(page.clone())));
        assert_eq!(
            browser.current.as_ref().unwrap().target,
            Link::Http(page.url.clone())
        );
        browser.activate(crate::registration::Section::Raw.action());
        assert!(browser.back.is_empty() && browser.pending.is_none() && browser.task.is_none());
        assert!(browser.offer_whois_export(None));
        assert_eq!(browser.download_offer.as_ref().unwrap().body, raw.as_ref());
        start(&mut browser, "example.test", NavigationIntent::Reload);
        browser.finish_fetch(2, Ok(FetchedDocument::Rdap(page)));
        let FetchedDocument::Rdap(page) = &browser.current.as_ref().unwrap().document else {
            panic!()
        };
        assert_eq!(page.section, crate::registration::Section::Raw);
        assert!(browser.back.is_empty());
    }

    #[tokio::test]
    async fn whois_stream_history_stop_and_stale_events() {
        let mut browser =
            BrowserController::new(Handle::current(), || {}, CssSize::new(800.0, 600.0));
        start(&mut browser, "old", NavigationIntent::New);
        assert!(browser.update_whois(1, reply("old", b"old", true)));
        start(&mut browser, "new", NavigationIntent::New);
        assert!(browser.update_whois(2, reply("new", b"first\n", false)));
        assert_eq!(browser.back.len(), 1);
        assert!(!browser.render_is_final);
        browser.interaction.scroll.y = 20.0;
        browser.whois_encoding(Some(crate::whois::Encoding::Latin1));
        browser.reply_view_action("wrap", Some(true));
        assert!(browser.update_whois(2, reply("new", b"first\nsecond\n", false)));
        assert_eq!(browser.back.len(), 1);
        assert_eq!(browser.interaction.scroll.y, 20.0);
        assert!(browser.pending.is_some() && browser.task.is_some());
        assert!(browser.stop());
        assert!(!browser.update_whois(2, reply("new", b"late", true)));
        let FetchedDocument::Whois(page) = &browser.current.as_ref().unwrap().document else {
            panic!()
        };
        assert_eq!(&*page.reply.hops[0].body, b"first\nsecond\n");
        assert!(page.reply.notice.as_ref().unwrap().contains("stopped"));
        assert_eq!(page.encoding, crate::whois::Encoding::Latin1);
        assert!(page.view.wrap && !page.view.loading);
        assert!(browser.offer_whois_export(Some(1)));
        assert_eq!(
            browser.download_offer.as_ref().unwrap().body,
            b"first\nsecond\n"
        );
    }

    #[tokio::test]
    async fn whois_reload_compares_one_success_and_encoding_does_not_fetch() {
        let mut browser =
            BrowserController::new(Handle::current(), || {}, CssSize::new(800.0, 600.0));
        start(&mut browser, "same", NavigationIntent::New);
        browser.update_whois(1, reply("same", b"old", true));
        browser.whois_encoding(Some(crate::whois::Encoding::Latin1));
        browser.reply_view_action("wrap", Some(true));
        for (generation, bytes) in [(2, b"new".as_slice()), (3, b"newest")] {
            start(&mut browser, "same", NavigationIntent::Reload);
            browser.update_whois(generation, reply("same", bytes, false));
            browser.update_whois(generation, reply("same", bytes, true));
        }
        assert!(browser.back.is_empty());
        assert!(browser.pending.is_none());
        let generation = browser.generation;
        browser.whois_encoding(None);
        assert_eq!(browser.generation, generation);
        assert!(browser.task.is_none());
        browser.reply_view_action("changes", Some(true));
        let doc = crate::render::documents::document(browser.current.as_ref().unwrap()).unwrap();
        assert!(doc.lines.iter().any(|line| line.text == "+ newest"));
        let page = doc.whois.unwrap();
        assert_eq!(&*page.previous.unwrap().hops[0].body, b"new");
        assert!(page.view.wrap);
    }
}

#[cfg(test)]
mod dict_tests {
    use super::*;
    fn target() -> Link {
        Link::Dict(crate::dict::Target::parse("dict://example.test/d:word:*").unwrap())
    }
    fn reply(finished: bool) -> crate::dict::Reply {
        let mut reply = crate::dict::Reply {
            raw: std::sync::Arc::new(b"original\r\n".to_vec()),
            finished,
            complete: finished,
            ..Default::default()
        };
        for db in ["gcide", "wn"] {
            reply.definitions.push(crate::dict::Definition {
                word: "word".into(),
                database: db.into(),
                description: db.into(),
                body: std::sync::Arc::new(format!("{db} definition\n")),
                complete: true,
            });
        }
        reply
    }
    fn start(browser: &mut BrowserController, generation: u64, intent: NavigationIntent) {
        browser.pending = Some(PendingNavigation {
            generation,
            target: target(),
            fallback_http: false,
            intent,
        });
    }
    #[tokio::test]
    async fn dict_local_actions_stream_reload_and_history_preserve_the_reader() {
        let mut browser = BrowserController::new(
            tokio::runtime::Handle::current(),
            || {},
            CssSize::new(800.0, 600.0),
        );
        start(&mut browser, 1, NavigationIntent::New);
        assert!(browser.update_dict(1, reply(false)));
        browser.activate(crate::dict::Action::Select(1).link());
        browser.reply_view_action("wrap", Some(false));
        assert!(browser.pending.is_some());
        assert!(browser.back.is_empty());
        browser.update_dict(1, reply(true));
        let FetchedDocument::Dict(page) = &browser.current.as_ref().unwrap().document else {
            panic!()
        };
        assert_eq!(page.selected, 1);
        assert!(!page.view.wrap);
        assert!(browser.offer_whois_export(None));
        assert_eq!(browser.download_offer().unwrap().body, b"original\r\n");
        browser.dismiss_download_offer();
        browser.activate(crate::dict::Action::Section(crate::dict::Section::Raw).link());
        start(&mut browser, 2, NavigationIntent::Reload);
        browser.update_dict(2, reply(true));
        let FetchedDocument::Dict(page) = &browser.current.as_ref().unwrap().document else {
            panic!()
        };
        assert_eq!(page.section, crate::dict::Section::Raw);
        assert_eq!(page.selected, 1);
        browser.open_internal_gemtext("about:help", b"# Help".to_vec());
        start(&mut browser, 3, NavigationIntent::Back);
        browser.update_dict(3, reply(true));
        let FetchedDocument::Dict(page) = &browser.current.as_ref().unwrap().document else {
            panic!()
        };
        assert_eq!(page.section, crate::dict::Section::Raw);
        assert_eq!(page.selected, 1);
        assert!(!page.view.wrap);
        assert!(
            !browser.update_dict(1, reply(false)),
            "retired generation ignored"
        );
    }
    #[tokio::test]
    async fn dict_filter_and_stop_preserve_partial_results_without_fetching() {
        let mut browser = BrowserController::new(
            tokio::runtime::Handle::current(),
            || {},
            CssSize::new(800.0, 600.0),
        );
        start(&mut browser, 1, NavigationIntent::New);
        browser.update_dict(1, reply(false));
        assert!(browser.dict_filter("wn"));
        assert!(browser.pending.is_some());
        browser.stop();
        let FetchedDocument::Dict(page) = &browser.current.as_ref().unwrap().document else {
            panic!()
        };
        assert_eq!(page.filter, "wn");
        assert!(page.reply.finished && !page.reply.complete);
        assert_eq!(page.reply.definitions.len(), 2);
        assert!(page.reply.notice.is_some());
    }
}
