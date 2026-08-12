//! Frontend-neutral browser controller, input vocabulary, and coordinate types.
//!
//! CSSOM View exposes viewport and pointer geometry in CSS pixels, while the
//! native window surface is sized in physical device pixels.  This module keeps
//! those spaces distinct; only a frontend renderer applies [`ScaleFactor`].
//! See CSSOM View §4 and CSS Values and Units §6.2.

use std::sync::Arc;
use std::sync::mpsc;

use tokio::runtime::Handle;
use tokio::task::JoinHandle;

use crate::doc::Link;
use crate::{gemini, gopher, http, oneshot};

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
    /// User-edited value/checkedness for a live form control.
    SetFormValue {
        actor: Option<usize>,
        value: String,
        checked: Option<bool>,
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
}

impl HistoryEntry {
    fn from_page(page: BrowserPage) -> Self {
        let internal_source = match page.document {
            FetchedDocument::Internal(source) => Some(source),
            _ => None,
        };
        Self {
            target: page.target,
            fallback_http: page.fallback_http,
            internal_source,
        }
    }
}

/// Raw protocol result retained by the shared controller. HTTP responses feed
/// the canonical DOM/CSS-pixel graphical layout path; the other protocol
/// presentation models can be added without changing navigation or wakeups.
#[derive(Debug)]
pub enum FetchedDocument {
    Gopher(Vec<u8>),
    Gemini(gemini::Response),
    Http(http::Response),
    OneShot(Vec<u8>),
    /// A trusted, in-process Gemtext document such as `about:help`.
    Internal(Vec<u8>),
}

#[derive(Debug)]
pub struct BrowserPage {
    target: Link,
    fallback_http: bool,
    pub document: FetchedDocument,
    pub status: String,
    /// Latest full serialization from the resident page actor. `None` means
    /// the response body is still authoritative.
    rendered_html: Option<String>,
    /// Revision of the latest complete actor serialization. Targeted patches
    /// advance `revision` but not this value, allowing graphical frontends to
    /// update their retained presentation DOM instead of reparsing stale HTML.
    rendered_revision: u64,
    pending_patches: Vec<crate::js::SubtreePatch>,
    revision: u64,
}

impl BrowserPage {
    pub fn address(&self) -> String {
        self.target.to_string()
    }

    pub fn rendered_html(&self) -> Option<&str> {
        self.rendered_html.as_deref()
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
    Reload,
    Back,
    Forward,
}

#[derive(Debug)]
struct PendingNavigation {
    generation: u64,
    target: Link,
    fallback_http: bool,
    intent: NavigationIntent,
}

#[derive(Debug)]
enum CoreEvent {
    FetchFinished {
        generation: u64,
        result: Result<FetchedDocument, String>,
    },
    Page {
        generation: u64,
        event: crate::js::PageEvt,
    },
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
    tx: mpsc::Sender<CoreEvent>,
    rx: mpsc::Receiver<CoreEvent>,
    current: Option<BrowserPage>,
    back: Vec<HistoryEntry>,
    forward: Vec<HistoryEntry>,
    pending: Option<PendingNavigation>,
    task: Option<JoinHandle<()>>,
    live_task: Option<JoinHandle<()>>,
    live_page: Option<crate::js::PageHandle>,
    pending_live_submit: Option<(crate::doc::Form, Option<usize>)>,
    pending_fragment: Option<String>,
    storage: crate::js::WebStorage,
    external_address: Option<String>,
    generation: u64,
    document_generation: u64,
    status: String,
    viewport: CssSize,
    device_pixel_ratio: f32,
    interaction: InteractionState,
    live_regions: Vec<usize>,
    live_boundaries: Vec<usize>,
}

impl BrowserController {
    pub fn new(runtime: Handle, wake: impl WakeSink + 'static, viewport: CssSize) -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            runtime,
            invalidation: InvalidationHandle {
                wake: Arc::new(wake),
            },
            tx,
            rx,
            current: None,
            back: Vec::new(),
            forward: Vec::new(),
            pending: None,
            task: None,
            live_task: None,
            live_page: None,
            pending_live_submit: None,
            pending_fragment: None,
            storage: Default::default(),
            external_address: None,
            generation: 0,
            document_generation: 0,
            status: String::from("Ready"),
            viewport,
            device_pixel_ratio: 1.0,
            interaction: InteractionState::default(),
            live_regions: Vec::new(),
            live_boundaries: Vec::new(),
        }
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

    /// Drain graphical subtree patches queued since the frontend last updated
    /// its retained presentation DOM. A full `Updated` clears this queue.
    pub fn take_page_patches(&mut self) -> Vec<crate::js::SubtreePatch> {
        self.current
            .as_mut()
            .map_or_else(Vec::new, |page| std::mem::take(&mut page.pending_patches))
    }

    /// Publish the graphical layout boundaries the resident actor may target.
    /// Values are actor arena ids baked as `data-trust-node`. Duplicate layouts
    /// are free; only a changed set crosses the actor channel.
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

    /// Ask the resident page actor for the always-correct complete snapshot.
    /// Used when a native frontend cannot prove a queued patch safe.
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
            "Cookies on: RAM-only, exact-host only."
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
        self.begin_internal_gemtext(
            Link::External(address.into()),
            source.into(),
            NavigationIntent::New,
        );
        ActionOutcome {
            invalidated: true,
            loading_retired: self.generation != generation_before,
        }
    }

    fn begin_internal_gemtext(&mut self, target: Link, source: Vec<u8>, intent: NavigationIntent) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
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
        self.drop_live_page();
        self.pending = None;
        if let Some(old) = self.current.take() {
            self.back.push(HistoryEntry::from_page(old));
            self.forward.clear();
        }
        self.external_address = Some(address.into());
        self.generation = self.generation.wrapping_add(1);
        self.document_generation = self.generation;
        self.status = String::from("Connecting terminal session …");
        self.invalidation.request_redraw();
    }

    pub fn take_fragment_request(&mut self) -> Option<String> {
        self.pending_fragment.take()
    }

    pub fn send_image_sizes(&self, sizes: &crate::layout2::ImageSizes) {
        let Some(handle) = &self.live_page else {
            return;
        };
        let values = sizes
            .iter()
            .map(|(source, size)| (source.clone(), *size))
            .collect();
        let _ = handle.cmds.try_send(crate::js::PageCmd::ImageSizes(values));
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
                self.send_live(crate::js::PageCmd::Scroll {
                    x: f64::from(self.interaction.scroll.x),
                    y: f64::from(self.interaction.scroll.y),
                });
                true
            }
            UserAction::SetViewportScroll(point) => {
                self.interaction.scroll = CssPoint::new(point.x.max(0.0), point.y.max(0.0));
                self.send_live(crate::js::PageCmd::Scroll {
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
            UserAction::SetFormValue {
                actor,
                value,
                checked,
            } => {
                if let Some(node) = actor {
                    self.send_live(crate::js::PageCmd::SetValue {
                        node,
                        value,
                        checked,
                    });
                }
                true
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
                    self.send_live(crate::js::PageCmd::Submit {
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
                    self.send_live(crate::js::PageCmd::SetScroll {
                        node: actor,
                        top: f64::from(top.max(0.0)),
                        left: f64::from(left.max(0.0)),
                    });
                }
                true
            }
            UserAction::Key(_) | UserAction::TextInput(_) => false,
        };
        ActionOutcome {
            invalidated,
            loading_retired: self.generation != generation_before,
        }
    }

    /// Drain all async completions currently queued. Returns whether visible
    /// state changed and therefore a redraw should be requested.
    pub fn process_async_events(&mut self) -> ActionOutcome {
        let generation_before = self.generation;
        let mut changed = false;
        while let Ok(event) = self.rx.try_recv() {
            match event {
                CoreEvent::FetchFinished { generation, result } => {
                    changed |= self.finish_fetch(generation, result);
                }
                CoreEvent::Page { generation, event } => {
                    if generation == self.generation {
                        changed |= self.handle_page_event(event);
                    }
                }
            }
        }
        ActionOutcome {
            invalidated: changed,
            loading_retired: self.generation != generation_before,
        }
    }

    fn begin_address(&mut self, address: &str, intent: NavigationIntent) -> bool {
        match parse_navigation_target(address) {
            Ok((target, fallback_http)) => {
                self.begin_fetch(target, fallback_http, intent);
            }
            Err(error) => {
                self.status = error;
            }
        }
        true
    }

    fn begin_history(&mut self, forward: bool) -> bool {
        let entry = if forward {
            self.forward.last()
        } else {
            self.back.last()
        };
        let Some(entry) = entry.cloned() else {
            self.status = if forward {
                String::from("Nothing forward in history.")
            } else {
                String::from("History is empty.")
            };
            return true;
        };
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
        if let Some(task) = self.task.take() {
            task.abort();
        }
        self.drop_live_page();
        self.external_address = None;
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        self.status = format!("Fetching {target} …");
        self.pending = Some(PendingNavigation {
            generation,
            target: target.clone(),
            fallback_http,
            intent,
        });
        let tx = self.tx.clone();
        let invalidation = self.invalidation.clone();
        let viewport = self.viewport;
        let device_pixel_ratio = self.device_pixel_ratio;
        let storage = self.storage.clone();
        self.task = Some(self.runtime.spawn(async move {
            let result = fetch_protocol_interactive(
                &target,
                fallback_http,
                None,
                viewport,
                device_pixel_ratio,
                storage,
                None,
            )
            .await;
            if tx
                .send(CoreEvent::FetchFinished { generation, result })
                .is_ok()
            {
                invalidation.request_redraw();
            }
        }));
    }

    fn finish_fetch(&mut self, generation: u64, result: Result<FetchedDocument, String>) -> bool {
        let Some(pending) = self.pending.take_if(|p| p.generation == generation) else {
            return false;
        };
        self.task = None;
        match result {
            Ok(mut document) => {
                let live = match &mut document {
                    FetchedDocument::Http(response) => response.live.take(),
                    _ => None,
                };
                let page = BrowserPage {
                    status: fetched_status(&pending.target, &document),
                    target: pending.target.clone(),
                    fallback_http: pending.fallback_http,
                    document,
                    rendered_html: None,
                    rendered_revision: 0,
                    pending_patches: Vec::new(),
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
                            self.back.push(HistoryEntry::from_page(old));
                        }
                        self.forward.clear();
                    }
                    NavigationIntent::Reload => {}
                    NavigationIntent::Back => {
                        let _ = self.back.pop();
                        if let Some(old) = old {
                            self.forward.push(HistoryEntry::from_page(old));
                        }
                    }
                    NavigationIntent::Forward => {
                        let _ = self.forward.pop();
                        if let Some(old) = old {
                            self.back.push(HistoryEntry::from_page(old));
                        }
                    }
                }
                self.interaction.scroll = CssPoint::default();
                self.interaction.nested_scroll.clear();
                if let Some(live) = live {
                    self.attach_live_page(generation, live);
                    self.send_live(crate::js::PageCmd::Viewport(layout_viewport(self.viewport)));
                    self.send_live(crate::js::PageCmd::DevicePixelRatio(
                        self.device_pixel_ratio,
                    ));
                }
            }
            Err(error) => {
                self.status = format!("{} — {error}", pending.target);
            }
        }
        true
    }

    fn stop(&mut self) -> bool {
        let pending = self.pending.take();
        let had_live_page = self.live_page.is_some() || self.live_task.is_some();
        if pending.is_none() && !had_live_page {
            return false;
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
        self.drop_live_page();
        self.generation = self.generation.wrapping_add(1);
        self.status = pending.map_or_else(
            || String::from("Stopped — page scripts killed."),
            |pending| format!("Stopped loading {} — page scripts killed.", pending.target),
        );
        true
    }

    fn send_live(&self, command: crate::js::PageCmd) -> bool {
        self.live_page
            .as_ref()
            .is_some_and(|handle| handle.cmds.try_send(command).is_ok())
    }

    fn drop_live_page(&mut self) {
        if let Some(page) = self.live_page.take() {
            page.retire();
        }
        self.pending_live_submit = None;
        if let Some(task) = self.live_task.take() {
            task.abort();
        }
    }

    fn attach_live_page(&mut self, generation: u64, mut live: http::LivePage) {
        self.live_page = Some(live.handle);
        let tx = self.tx.clone();
        let invalidation = self.invalidation.clone();
        self.live_task = Some(self.runtime.spawn(async move {
            while let Some(event) = live.events.recv().await {
                if tx.send(CoreEvent::Page { generation, event }).is_err() {
                    break;
                }
                invalidation.request_redraw();
            }
        }));
    }

    fn activate(&mut self, link: Link) -> bool {
        match link {
            Link::JsClick { node, .. } => {
                self.send_live(crate::js::PageCmd::Click(node));
                self.status = String::from("Page action …");
            }
            Link::Form { .. } => return false,
            Link::Media(url) => self.status = format!("Media: {url}"),
            Link::External(url) => self.status = format!("External target: {url}"),
            target => self.begin_fetch(target, false, NavigationIntent::New),
        }
        true
    }

    fn handle_page_event(&mut self, event: crate::js::PageEvt) -> bool {
        use crate::js::PageEvt;
        match event {
            PageEvt::Updated { html, outcome } | PageEvt::Static { html, outcome } => {
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
                    page.rendered_html = Some(html);
                    page.revision = page.revision.wrapping_add(1);
                    page.rendered_revision = page.revision;
                    page.pending_patches.clear();
                }
                self.status = if outcome.errors.is_empty() {
                    String::from("Page updated · JS")
                } else {
                    format!("Page updated · JS:{}", outcome.errors.len())
                };
                true
            }
            PageEvt::Patched { patches, outcome } => {
                // Preserve the actor's attributed subtree updates for the
                // graphical frontend. Newest-per-boundary wins; a later full
                // Updated clears the queue. The frontend requests Resync only
                // if its retained presentation DOM cannot apply one safely.
                if let Some(page) = &mut self.current {
                    for patch in patches {
                        page.pending_patches.retain(|old| old.node != patch.node);
                        page.pending_patches.push(patch);
                    }
                    page.revision = page.revision.wrapping_add(1);
                }
                if !outcome.errors.is_empty() {
                    self.status = format!("Page JS:{}", outcome.errors.len());
                }
                true
            }
            PageEvt::Navigate(address) => self.begin_address(&address, NavigationIntent::New),
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

    fn submit_static(&mut self, form: crate::doc::Form, submitter: Option<usize>) {
        use crate::doc::FormMethod;
        let body = form.encode(submitter);
        match form.method {
            FormMethod::Get => {
                let mut target = form.action;
                target.set_query((!body.is_empty()).then_some(body.as_str()));
                self.begin_fetch(Link::Http(target), false, NavigationIntent::New);
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
            self.begin_fetch(Link::Http(action), false, NavigationIntent::New);
        }
    }

    fn begin_post(&mut self, url: url::Url, body: String) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
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
        let invalidation = self.invalidation.clone();
        let viewport = self.viewport;
        let device_pixel_ratio = self.device_pixel_ratio;
        let storage = self.storage.clone();
        self.task = Some(self.runtime.spawn(async move {
            let result = fetch_protocol_interactive(
                &target,
                false,
                None,
                viewport,
                device_pixel_ratio,
                storage,
                Some(body),
            )
            .await;
            if tx
                .send(CoreEvent::FetchFinished { generation, result })
                .is_ok()
            {
                invalidation.request_redraw();
            }
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
        Link::Gopher(url) => gopher::fetch(url).await.map(FetchedDocument::Gopher),
        Link::Gemini(url) => gemini::fetch(url).await.map(FetchedDocument::Gemini),
        Link::Http(url) => {
            let response = if fallback_http {
                http::fetch_web_default(url).await
            } else {
                let mut request = http::Request::get(url.clone());
                if let Some(referrer) = referrer {
                    http::set_referrer(&mut request, referrer);
                }
                http::fetch(&request).await
            }?;
            Ok(FetchedDocument::Http(response))
        }
        Link::OneShot(url) => oneshot::fetch(url).await.map(FetchedDocument::OneShot),
        Link::Telnet { .. } => Err(String::from("terminal target requires a frontend VT view")),
        Link::External(url) => Err(format!("unsupported URL scheme: {url}")),
        Link::Form { .. } | Link::JsClick { .. } | Link::Media(_) => {
            Err(String::from("target is not directly fetchable"))
        }
    }
}

async fn fetch_protocol_interactive(
    target: &Link,
    fallback_http: bool,
    referrer: Option<&url::Url>,
    viewport: CssSize,
    device_pixel_ratio: f32,
    storage: crate::js::WebStorage,
    post_body: Option<String>,
) -> Result<FetchedDocument, String> {
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
            };
            if let Some(referrer) = referrer {
                http::set_referrer(&mut request, referrer);
            }
            http::fetch(&request).await?
        } else if fallback_http {
            http::fetch_web_default(url).await?
        } else {
            let mut request = http::Request::get(url.clone());
            if let Some(referrer) = referrer {
                http::set_referrer(&mut request, referrer);
            }
            http::fetch(&request).await?
        };
        // The legacy API accepts a terminal viewport and cell size. A one-pixel
        // cell is the explicit desktop adapter, so the actor's CSSOM viewport
        // is exactly the desktop's CSS-pixel viewport and never device pixels.
        let css_viewport = (
            viewport.width.round().clamp(1.0, f32::from(u16::MAX)) as u16,
            viewport.height.round().clamp(1.0, f32::from(u16::MAX)) as u16,
        );
        response = http::execute_js_for_device(
            response,
            css_viewport,
            (1, 1),
            device_pixel_ratio,
            storage,
        )
        .await;
        Ok(FetchedDocument::Http(response))
    } else {
        fetch_protocol(target, fallback_http, referrer).await
    }
}

fn layout_viewport(size: CssSize) -> crate::layout2::Viewport {
    crate::layout2::Viewport::new(size.width, size.height)
}

fn fetched_status(target: &Link, document: &FetchedDocument) -> String {
    match document {
        FetchedDocument::Http(response) => {
            let media = response.content_type.split(';').next().unwrap_or("").trim();
            format!("{} — HTTP {} ({media})", response.url, response.status)
        }
        FetchedDocument::Gemini(response) => {
            format!(
                "{} — Gemini {} {}",
                response.url, response.status, response.meta
            )
        }
        FetchedDocument::Gopher(bytes) => format!("{target} — {} bytes", bytes.len()),
        FetchedDocument::OneShot(bytes) => format!("{target} — {} bytes", bytes.len()),
        FetchedDocument::Internal(_) => target.to_string(),
    }
}

/// Parse a typed navigation target into a fetchable protocol target. A bare host is
/// HTTPS with HTTP fallback, matching the existing terminal address behavior.
pub fn parse_navigation_target(address: &str) -> Result<(Link, bool), String> {
    let address = address.trim();
    if address.is_empty() {
        return Err(String::from("Enter an address."));
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
        return Err(String::from(
            "Telnet sessions currently open in the terminal frontend.",
        ));
    }
    let (host, port) = split_host_port(address);
    match port {
        Some(70) => Ok((
            Link::Gopher(gopher::GopherUrl {
                host: host.to_string(),
                port: 70,
                item_type: '1',
                selector: String::new(),
            }),
            false,
        )),
        Some(1965) => Ok((
            Link::Gemini(gemini::GeminiUrl {
                host: host.to_string(),
                port: 1965,
                path: String::from("/"),
            }),
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
            rendered_html: Some(String::from("<div data-trust-node=17></div>")),
            rendered_revision: 7,
            pending_patches: Vec::new(),
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
    fn graphical_controller_queues_incremental_page_patches() {
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
            rendered_html: Some(String::from("<div data-trust-node=17>old</div>")),
            rendered_revision: 7,
            pending_patches: Vec::new(),
            revision: 7,
        });
        let patch = crate::js::SubtreePatch {
            node: 17,
            html: String::from("<div><div data-trust-node=17>new</div></div>"),
            tier: crate::js::BoundaryTier::WidthStable,
        };

        assert!(browser.handle_page_event(crate::js::PageEvt::Patched {
            patches: vec![patch],
            outcome: crate::js::Outcome::default(),
        }));
        assert_eq!(browser.current.as_ref().unwrap().revision, 8);
        assert_eq!(browser.current.as_ref().unwrap().rendered_revision, 7);
        let queued = browser.take_page_patches();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].node, 17);
        assert!(browser.take_page_patches().is_empty());
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

        assert!(parse_navigation_target("telnet://example.com").is_err());
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
                item_type: '1',
                selector: String::new(),
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
                Ok(FetchedDocument::Gopher(vec![generation as u8]))
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

        // Merely beginning travel leaves the trail intact; successful arrival
        // is the commit point, matching the terminal browser's failure-safe
        // history behavior.
        browser.pending = Some(PendingNavigation {
            generation: 3,
            target: target("one.example"),
            fallback_http: false,
            intent: NavigationIntent::Back,
        });
        assert!(browser.snapshot().can_go_back);
        assert!(browser.finish_fetch(3, Ok(FetchedDocument::Gopher(vec![3]))));
        assert!(!browser.snapshot().can_go_back);
        assert!(browser.snapshot().can_go_forward);
        assert_eq!(
            browser.current_page().unwrap().address(),
            "gopher://one.example/1"
        );
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
                item_type: '1',
                selector: String::new(),
            })
        };
        browser.generation = 1;
        browser.pending = Some(PendingNavigation {
            generation: 1,
            target: target("old.example"),
            fallback_http: false,
            intent: NavigationIntent::New,
        });
        assert!(browser.finish_fetch(1, Ok(FetchedDocument::Gopher(vec![1]))));
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
