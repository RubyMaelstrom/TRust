//! Native TRust desktop frontend: winit events → shared browser controller →
//! renderer-neutral scene → selected Vello backend → native surface.

use std::collections::{HashMap, HashSet, VecDeque};
use std::error::Error;
use std::num::NonZeroU32;
use std::sync::Arc;

use accesskit::{
    Action as AccessAction, ActionData, ActionRequest, Affine, Node as AccessNode,
    NodeId as AccessNodeId, Rect as AccessRect, Role as AccessRole, Tree, TreeId, TreeUpdate,
    Vec2 as AccessVec2,
};
use accesskit_winit::{Adapter as AccessAdapter, Event as AccessEvent};
use softbuffer::{Context, Surface};
use tokio::runtime::Handle;
use trust::accessibility::{Role as SemanticRole, SemanticTree};
use trust::core::{
    BrowserController, ButtonState, CssPoint, CssSize, FetchedDocument, ImeAction, Key, KeyInput,
    KeyState, Modifiers, PhysicalSize, PointerButton, ScaleFactor, UserAction, ViewportMetrics,
};
use trust::doc::{FieldKind, Link};
use trust::render::vello_cpu::VelloCpuRenderer;
use trust::render::vello_hybrid::{PresentOutcome, VelloHybridRenderer};
use trust::render::{
    ChromeModel, ControlId, CssRect, DisplayCommand, EditorVisual, ImageHandle, ImageResource,
    ImageStore, PageHit, PaintBrush, PaintColor, PaintShape, RasterBackend, RendererKind,
    RendererPreference, Scene, StrokeStyle, TextSelection, desktop_chrome, paint_text_editor,
};
use trust::text::{TextEditor, TextStyle};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalPosition, LogicalSize};
use winit::event::{ElementState, Ime, MouseButton, MouseScrollDelta, TouchPhase, WindowEvent};
use winit::event_loop::{
    ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy, OwnedDisplayHandle,
};
use winit::keyboard::{Key as WinitKey, ModifiersState, NamedKey};
use winit::window::{CursorIcon, Window, WindowId};

const PAGE_ACCESS_BASE: u64 = 10_000;
const ACCESS_ROOT: AccessNodeId = AccessNodeId(0);
const ACCESS_ADDRESS: AccessNodeId = AccessNodeId(2);
const ACCESS_BACK: AccessNodeId = AccessNodeId(3);
const ACCESS_FORWARD: AccessNodeId = AccessNodeId(4);
const ACCESS_RELOAD: AccessNodeId = AccessNodeId(5);
const ACCESS_FIND: AccessNodeId = AccessNodeId(6);
const ACCESS_CONSOLE: AccessNodeId = AccessNodeId(7);
const IMAGE_FETCH_CONCURRENCY: usize = 8;
/// Coalesce a slow image stream into an occasional progressive relayout. A
/// fast page bypasses this timer and relayouts as soon as its whole request set
/// reaches available/broken state. Full CSS layout can be substantially more
/// expensive than fetching another bounded network wave, so this fallback must
/// never interrupt an ordinary gallery burst every few hundred milliseconds.
const IMAGE_PROGRESS_RELAYOUT_DELAY: std::time::Duration = std::time::Duration::from_secs(5);
/// Internal `ImageSizes` sentinel. Layout turns it into HTML's 300×150 CSS
/// default object size without applying a responsive candidate's density; no
/// decodable image can reach this pair (the decoder caps each axis at 12,000).
const PENDING_IMAGE_SIZE: (u32, u32) = (u32::MAX, u32::MAX);

#[derive(Debug)]
enum DesktopEvent {
    BrowserWake,
    ImageLoaded {
        generation: u64,
        image_epoch: u64,
        handle: ImageHandle,
        source: String,
        result: Result<ImageResource, String>,
    },
    ImagesReady {
        generation: u64,
        image_epoch: u64,
    },
    Access(AccessEvent),
    Telnet(trust::telnet::Event),
}

impl From<AccessEvent> for DesktopEvent {
    fn from(event: AccessEvent) -> Self {
        Self::Access(event)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum FocusTarget {
    #[default]
    Page,
    Address,
    Find,
    Console,
    Form {
        form: usize,
        field: usize,
    },
}

struct PageLayoutCache {
    generation: u64,
    revision: u64,
    viewport: CssSize,
    device_pixel_ratio: f32,
    document: trust::http::GraphicalDocument,
    layout: trust::layout2::GraphicalLayout,
    title: String,
}

struct ProtocolPageCache {
    generation: u64,
    viewport: CssSize,
    document: trust::doc::Doc,
    layout: trust::render::documents::ProtocolPaint,
    /// Parsed line index highlighted by the Gopherus keyboard model. This is
    /// present only for Gopher/Gemini; one-shot text protocols remain plain.
    selected: Option<usize>,
}

/// Per-document image request state. The HTML image processing model keeps a
/// request available/broken once it reaches that state; this scheduler mirrors
/// that lifecycle while bounding the native frontend's network/decode work.
#[derive(Default)]
struct ImageLoadScheduler {
    generation: Option<u64>,
    page: Option<String>,
    epoch: u64,
    queued: VecDeque<trust::render::ImageRequest>,
    pending: HashSet<ImageHandle>,
    failed: HashSet<ImageHandle>,
    completed: HashSet<ImageHandle>,
    retired: bool,
}

impl ImageLoadScheduler {
    fn reset(&mut self, generation: u64, page: &url::Url) -> bool {
        if self.generation == Some(generation) && self.page.as_deref() == Some(page.as_str()) {
            return false;
        }
        self.generation = Some(generation);
        self.page = Some(page.to_string());
        self.epoch = self.epoch.wrapping_add(1);
        self.queued.clear();
        self.pending.clear();
        self.failed.clear();
        self.completed.clear();
        self.retired = false;
        true
    }

    fn retire(&mut self) -> bool {
        if self.retired {
            return false;
        }
        self.retired = true;
        self.epoch = self.epoch.wrapping_add(1);
        self.queued.clear();
        self.pending.clear();
        true
    }

    fn epoch(&self) -> u64 {
        self.epoch
    }

    fn accepts(&self, generation: u64, image_epoch: u64) -> bool {
        !self.retired && self.generation == Some(generation) && self.epoch == image_epoch
    }

    fn enqueue(&mut self, request: trust::render::ImageRequest) {
        if self.retired {
            return;
        }
        if self.pending.contains(&request.handle)
            || self.failed.contains(&request.handle)
            || self.completed.contains(&request.handle)
        {
            return;
        }
        self.pending.insert(request.handle);
        self.queued.push_back(request);
    }

    fn take_ready(&mut self, slots: usize) -> Vec<trust::render::ImageRequest> {
        if self.retired {
            return Vec::new();
        }
        (0..slots).filter_map(|_| self.queued.pop_front()).collect()
    }

    fn finish(&mut self, handle: ImageHandle, success: bool) -> bool {
        if !self.pending.remove(&handle) {
            return false;
        }
        if success {
            self.completed.insert(handle);
        } else {
            self.failed.insert(handle);
        }
        true
    }

    fn mark_cached(&mut self, handle: ImageHandle) -> bool {
        self.failed.remove(&handle);
        self.pending.remove(&handle);
        self.completed.insert(handle)
    }

    fn retry_evicted(&mut self, handle: ImageHandle) {
        self.completed.remove(&handle);
    }
}

fn image_sizes_for_layout(
    decoded: &trust::layout2::ImageSizes,
    failed: &HashSet<ImageHandle>,
    sources: &[String],
) -> trust::layout2::ImageSizes {
    let mut sizes = decoded.clone();
    for source in sources {
        let handle = ImageHandle::for_source(source);
        if !failed.contains(&handle) {
            // HTML Rendering: while the UA expects an image to become
            // available, it remains a replaced element. CSS 2.2 §10.3.2's
            // no-intrinsic default object size is 300×150 CSS px. This avoids
            // laying a gallery's accessibility text out as page content while
            // its image requests are merely pending. A failed request is
            // deliberately omitted so the canonical alt-text path takes over.
            sizes.entry(source.clone()).or_insert(PENDING_IMAGE_SIZE);
        }
    }
    sizes
}

/// HTML lazy-image scheduling with a two-viewport look-ahead. Eager resources
/// keep their existing immediate path; lazy resources enter the queue shortly
/// before they can be painted. The returned visible set is also the only set
/// allowed to retry after decoded-cache eviction, preventing an off-screen
/// gallery from cycling forever through the bounded LRU.
fn scheduled_page_images(
    page: &PageLayoutCache,
    scroll: CssPoint,
    viewport: CssSize,
) -> (Vec<trust::render::ImageRequest>, HashSet<ImageHandle>) {
    let band = CssRect::new(
        scroll.x - viewport.width,
        scroll.y - viewport.height * 2.0,
        viewport.width * 3.0,
        viewport.height * 5.0,
    );
    let mut visible = HashSet::new();
    collect_visible_image_handles(&page.layout.paint.primitives, band, &mut visible);
    // Fixed-position image commands are viewport-relative. They are cheap and
    // necessarily near the user whenever their layer is active.
    for command in &page.layout.paint.fixed_primitives {
        if let trust::render::DisplayCommand::Image { handle, .. } = command {
            visible.insert(*handle);
        }
    }
    let requests = page
        .layout
        .paint
        .image_requests
        .iter()
        .filter(|request| {
            !page.document.lazy_image_handles.contains(&request.handle)
                || visible.contains(&request.handle)
        })
        .cloned()
        .collect();
    (requests, visible)
}

fn collect_visible_image_handles(
    commands: &[trust::render::DisplayCommand],
    band: CssRect,
    visible: &mut HashSet<ImageHandle>,
) {
    let mut transform = trust::render::Affine2d::IDENTITY;
    let mut stack = Vec::new();
    for command in commands {
        match command {
            trust::render::DisplayCommand::PushTransform(next) => {
                stack.push(transform);
                transform = transform.then(*next);
            }
            trust::render::DisplayCommand::PopTransform => {
                transform = stack.pop().unwrap_or(trust::render::Affine2d::IDENTITY);
            }
            trust::render::DisplayCommand::Image { rect, handle, .. } => {
                let corners = [
                    CssPoint::new(rect.x, rect.y),
                    CssPoint::new(rect.x + rect.width, rect.y),
                    CssPoint::new(rect.x, rect.y + rect.height),
                    CssPoint::new(rect.x + rect.width, rect.y + rect.height),
                ]
                .map(|point| transform.map_point(point));
                let left = corners
                    .iter()
                    .map(|point| point.x)
                    .fold(f32::INFINITY, f32::min);
                let right = corners
                    .iter()
                    .map(|point| point.x)
                    .fold(f32::NEG_INFINITY, f32::max);
                let top = corners
                    .iter()
                    .map(|point| point.y)
                    .fold(f32::INFINITY, f32::min);
                let bottom = corners
                    .iter()
                    .map(|point| point.y)
                    .fold(f32::NEG_INFINITY, f32::max);
                if left < band.x + band.width
                    && right > band.x
                    && top < band.y + band.height
                    && bottom > band.y
                {
                    visible.insert(*handle);
                }
            }
            _ => {}
        }
    }
}

struct TerminalSession {
    view: trust::terminal_view::TerminalView,
    handle: trust::telnet::Handle,
    cols: u16,
    rows: u16,
    connected: bool,
    remote_echo: bool,
    linemode_active: bool,
    linemode_edit: bool,
    line_editor: TextEditor,
}

impl TerminalSession {
    fn char_mode(&self) -> bool {
        self.connected && (self.remote_echo || (self.linemode_active && !self.linemode_edit))
    }
}

enum DesktopRenderer {
    Cpu(Box<VelloCpuRenderer>),
    Hybrid(Box<VelloHybridRenderer>),
}

impl DesktopRenderer {
    fn kind(&self) -> RendererKind {
        match self {
            Self::Cpu(_) => RendererKind::Cpu,
            Self::Hybrid(renderer) => renderer.kind(),
        }
    }
}

struct DesktopApp {
    browser: BrowserController,
    renderer: Option<DesktopRenderer>,
    renderer_preference: RendererPreference,
    surface: Option<Surface<OwnedDisplayHandle, Arc<Window>>>,
    context: Option<Context<OwnedDisplayHandle>>,
    window: Option<Arc<Window>>,
    accessibility: Option<AccessAdapter>,
    metrics: ViewportMetrics,
    scene: Option<Scene>,
    pointer: CssPoint,
    cursor_icon: CursorIcon,
    modifiers: ModifiersState,
    focus: FocusTarget,
    /// The command-line URL starts only after the native window reports its
    /// real CSS viewport and device scale. Responsive scripts must not observe
    /// the constructor's provisional 1x environment during initial parsing.
    initial_navigation: Option<String>,
    initial_address_focus_pending: bool,
    address: TextEditor,
    find: TextEditor,
    console: TextEditor,
    form_editor: Option<TextEditor>,
    composing: bool,
    page_layout: Option<PageLayoutCache>,
    protocol_page: Option<ProtocolPageCache>,
    runtime: Handle,
    event_proxy: EventLoopProxy<DesktopEvent>,
    image_store: ImageStore,
    image_sizes: trust::layout2::ImageSizes,
    image_loads: ImageLoadScheduler,
    image_tasks: HashMap<ImageHandle, tokio::task::JoinHandle<()>>,
    image_flush_scheduled: bool,
    decoded_images_pending_layout: bool,
    hovered_actor: Option<usize>,
    link_preview: String,
    selection: Option<TextSelection>,
    selecting: bool,
    find_matches: Vec<TextSelection>,
    find_index: usize,
    clipboard: Option<arboard::Clipboard>,
    exit_requested: bool,
    terminal: Option<TerminalSession>,
    keyboard_target: Option<PageHit>,
    pressed_hit: Option<PageHit>,
    pressed_control: Option<ControlId>,
    /// winit/Softbuffer may deliver platform exposure/redraw notifications
    /// after a present. Only browser/UI invalidation is allowed to consume CPU
    /// and build a new frame; this latch prevents an idle presentation loop.
    redraw_pending: bool,
}

impl DesktopApp {
    fn new(
        browser: BrowserController,
        runtime: Handle,
        event_proxy: EventLoopProxy<DesktopEvent>,
        renderer_preference: RendererPreference,
        initial_navigation: Option<String>,
    ) -> Self {
        let style = chrome_text_style();
        Self {
            browser,
            renderer: None,
            renderer_preference,
            surface: None,
            context: None,
            window: None,
            accessibility: None,
            metrics: ViewportMetrics::from_physical(
                PhysicalSize::new(960, 640),
                ScaleFactor::default(),
            ),
            scene: None,
            pointer: CssPoint::default(),
            cursor_icon: CursorIcon::Default,
            modifiers: ModifiersState::empty(),
            focus: FocusTarget::Page,
            initial_navigation,
            initial_address_focus_pending: true,
            address: TextEditor::new("", &style, 600.0, false),
            find: TextEditor::new("", &style, 500.0, false),
            console: TextEditor::new("", &style, 700.0, false),
            form_editor: None,
            composing: false,
            page_layout: None,
            protocol_page: None,
            runtime,
            event_proxy,
            image_store: ImageStore::default(),
            image_sizes: trust::layout2::ImageSizes::new(),
            image_loads: ImageLoadScheduler::default(),
            image_tasks: HashMap::new(),
            image_flush_scheduled: false,
            decoded_images_pending_layout: false,
            hovered_actor: None,
            link_preview: String::new(),
            selection: None,
            selecting: false,
            find_matches: Vec::new(),
            find_index: 0,
            clipboard: arboard::Clipboard::new().ok(),
            exit_requested: false,
            terminal: None,
            keyboard_target: None,
            pressed_hit: None,
            pressed_control: None,
            redraw_pending: false,
        }
    }

    fn request_page_images(
        &mut self,
        generation: u64,
        page: &url::Url,
        requests: &[trust::render::ImageRequest],
        visible_retries: &HashSet<ImageHandle>,
    ) {
        self.sync_image_document(generation, page);
        if self.image_loads.retired {
            return;
        }
        let image_epoch = self.image_loads.epoch();
        for request in requests {
            if let Some(image) = self.image_store.get(request.handle) {
                if self.image_loads.mark_cached(request.handle)
                    && self.image_sizes.get(&request.source) != Some(&(image.width, image.height))
                {
                    self.image_sizes
                        .insert(request.source.clone(), (image.width, image.height));
                    self.decoded_images_pending_layout = true;
                }
                continue;
            }
            if self.image_loads.completed.contains(&request.handle) {
                if !visible_retries.contains(&request.handle) {
                    continue;
                }
                self.image_loads.retry_evicted(request.handle);
            }
            self.image_loads.enqueue(request.clone());
        }
        self.pump_image_loads(generation, image_epoch, page);
        if self.decoded_images_pending_layout {
            self.flush_images_when_settled(generation, image_epoch);
        }
    }

    fn sync_image_document(&mut self, generation: u64, page: &url::Url) {
        if !self.image_loads.reset(generation, page) {
            return;
        }
        // Decoded pixels belong to one foreground document. Compressed HTTP
        // cache entries remain reusable, but retaining the previous page's
        // full decoded gallery would pin its high-water after navigation.
        self.image_store.clear();
        self.image_sizes.clear();
        for (_, task) in self.image_tasks.drain() {
            task.abort();
        }
        self.image_flush_scheduled = false;
        self.decoded_images_pending_layout = false;
    }

    /// Stop all unfinished work owned by the displayed document while keeping
    /// its completed layout and decoded pixels available as a frozen snapshot.
    /// HTML §7.5.11's abort-a-document algorithm cancels document fetches and
    /// discards their queued tasks; the epoch makes already-queued native
    /// completion events equally inert.
    fn retire_page_loading(&mut self) {
        if !self.image_loads.retire() {
            return;
        }
        for (_, task) in self.image_tasks.drain() {
            task.abort();
        }
        self.image_flush_scheduled = false;
        self.decoded_images_pending_layout = false;
    }

    fn pump_image_loads(&mut self, generation: u64, image_epoch: u64, page: &url::Url) {
        let slots = IMAGE_FETCH_CONCURRENCY.saturating_sub(self.image_tasks.len());
        for request in self.image_loads.take_ready(slots) {
            let proxy = self.event_proxy.clone();
            let page = page.clone();
            let source = request.source.clone();
            let handle = request.handle;
            let task = self.runtime.spawn(async move {
                let result = match trust::http::fetch_graphical_image(&page, &source).await {
                    Ok(bytes) => {
                        tokio::task::spawn_blocking(move || trust::img::decode_graphical(&bytes))
                            .await
                            .map_err(|error| format!("image decode task failed: {error}"))
                            .and_then(|result| result)
                    }
                    Err(error) => Err(error),
                };
                let _ = proxy.send_event(DesktopEvent::ImageLoaded {
                    generation,
                    image_epoch,
                    handle,
                    source,
                    result,
                });
            });
            self.image_tasks.insert(handle, task);
        }
    }

    fn schedule_image_flush(&mut self, generation: u64, image_epoch: u64) {
        if self.image_flush_scheduled {
            return;
        }
        self.image_flush_scheduled = true;
        let proxy = self.event_proxy.clone();
        self.runtime.spawn(async move {
            tokio::time::sleep(IMAGE_PROGRESS_RELAYOUT_DELAY).await;
            let _ = proxy.send_event(DesktopEvent::ImagesReady {
                generation,
                image_epoch,
            });
        });
    }

    fn flush_images_when_settled(&mut self, generation: u64, image_epoch: u64) {
        if self.image_loads.pending.is_empty() {
            // Do not make a fast gallery wait for the progressive-load timer.
            // A previously armed timer is harmless: after this event drains the
            // dirty flag, the delayed event becomes a no-op.
            let _ = self.event_proxy.send_event(DesktopEvent::ImagesReady {
                generation,
                image_epoch,
            });
        } else {
            self.schedule_image_flush(generation, image_epoch);
        }
    }

    fn request_redraw(&mut self) {
        self.redraw_pending = true;
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    fn dispatch(&mut self, action: UserAction) {
        let leaves_terminal = matches!(
            &action,
            UserAction::Back
                | UserAction::Forward
                | UserAction::Navigate(_)
                | UserAction::Activate(
                    Link::Gopher(_) | Link::Gemini(_) | Link::Http(_) | Link::OneShot(_)
                )
        );
        let leaves_keyboard_target = matches!(
            &action,
            UserAction::Back
                | UserAction::Forward
                | UserAction::Reload
                | UserAction::Navigate(_)
                | UserAction::Activate(_)
        );
        if leaves_terminal && let Some(session) = self.terminal.take() {
            let _ = session
                .handle
                .commands
                .try_send(trust::telnet::Command::Close);
        }
        if leaves_keyboard_target {
            self.keyboard_target = None;
        }
        let outcome = self.browser.handle_action(action);
        if outcome.loading_retired {
            self.retire_page_loading();
        }
        if outcome.invalidated {
            self.request_redraw();
        }
    }

    fn navigate(&mut self, address: String) {
        if let Some((host, port, tls)) = parse_telnet_target(&address) {
            self.start_terminal(host, port, tls, address);
        } else {
            if let Some(session) = self.terminal.take() {
                let _ = session
                    .handle
                    .commands
                    .try_send(trust::telnet::Command::Close);
            }
            self.dispatch(UserAction::Navigate(address));
        }
    }

    fn start_terminal(&mut self, host: String, port: u16, tls: bool, address: String) {
        if let Some(session) = self.terminal.take() {
            let _ = session
                .handle
                .commands
                .try_send(trust::telnet::Command::Close);
        }
        let view = trust::terminal_view::TerminalView::new(24, 80);
        let _runtime = self.runtime.enter();
        let (handle, mut events) = trust::telnet::connect(host, port, (80, 24), tls);
        drop(_runtime);
        let proxy = self.event_proxy.clone();
        self.runtime.spawn(async move {
            while let Some(event) = events.recv().await {
                if proxy.send_event(DesktopEvent::Telnet(event)).is_err() {
                    break;
                }
            }
        });
        self.browser.open_external_session(address.clone());
        self.retire_page_loading();
        self.image_store.clear();
        self.image_sizes.clear();
        self.page_layout = None;
        self.protocol_page = None;
        self.terminal = Some(TerminalSession {
            view,
            handle,
            cols: 80,
            rows: 24,
            connected: false,
            remote_echo: false,
            linemode_active: false,
            linemode_edit: false,
            line_editor: TextEditor::new("", &terminal_text_style(), 700.0, false),
        });
        self.set_focus(FocusTarget::Page);
    }

    fn update_metrics(&mut self, scale_factor: Option<f64>) {
        let Some(window) = &self.window else { return };
        let size = window.inner_size();
        let scale = ScaleFactor::new(scale_factor.unwrap_or_else(|| window.scale_factor()));
        self.metrics =
            ViewportMetrics::from_physical(PhysicalSize::new(size.width, size.height), scale);
        self.dispatch(UserAction::DevicePixelRatio(
            self.metrics.scale_factor.get() as f32,
        ));
        self.dispatch(UserAction::Resize(self.browser_viewport()));
    }

    fn browser_viewport(&self) -> CssSize {
        let overlay = if matches!(self.focus, FocusTarget::Find | FocusTarget::Console) {
            38.0
        } else {
            0.0
        };
        CssSize::new(
            self.metrics.css.width,
            (self.metrics.css.height - 82.0 - overlay).max(0.0),
        )
    }

    fn editor_visual(editor: &mut TextEditor, password: bool) -> EditorVisual {
        let (selection, caret, _) = editor.geometry();
        EditorVisual {
            text: if password {
                "•".repeat(editor.text().chars().count())
            } else {
                editor.raw_text().to_string()
            },
            selection: selection
                .into_iter()
                .map(|rect| CssRect::new(rect.x, rect.y, rect.width, rect.height))
                .collect(),
            caret: caret.map(|rect| CssRect::new(rect.x, rect.y, rect.width, rect.height)),
        }
    }

    fn chrome_model(&mut self, snapshot: &trust::core::BrowserSnapshot) -> ChromeModel {
        if self.focus != FocusTarget::Address && self.address.text() != snapshot.address {
            self.address.set_text(&snapshot.address);
        }
        let address = Self::editor_visual(&mut self.address, false);
        let find =
            (self.focus == FocusTarget::Find).then(|| Self::editor_visual(&mut self.find, false));
        let console = (self.focus == FocusTarget::Console)
            .then(|| Self::editor_visual(&mut self.console, false));
        ChromeModel {
            address,
            address_focused: self.focus == FocusTarget::Address,
            title: self
                .page_layout
                .as_ref()
                .map(|page| page.title.clone())
                .unwrap_or_default(),
            status: snapshot.status.clone(),
            link_preview: self.link_preview.clone(),
            find,
            find_count: (!self.find_matches.is_empty())
                .then_some((self.find_index + 1, self.find_matches.len())),
            console,
        }
    }

    fn ensure_page_layout(&mut self, viewport: CssSize) {
        let device_pixel_ratio = self.metrics.scale_factor.get() as f32;
        let generation = self.browser.document_generation();
        let revision = self.browser.snapshot().page_revision;
        let current_http_url = self.browser.current_page().and_then(|page| {
            let FetchedDocument::Http(response) = &page.document else {
                return None;
            };
            Some(&response.url)
        });
        let current = self.page_layout.as_ref().is_some_and(|cache| {
            cache.generation == generation
                && cache.revision == revision
                && cache.viewport == viewport
                && cache.device_pixel_ratio == device_pixel_ratio
                && current_http_url == Some(&cache.document.base)
        });
        if current {
            return;
        }
        let seed = self
            .page_layout
            .as_ref()
            .filter(|cache| cache.generation == generation)
            .map(|cache| cache.document.forms.clone());
        let document = self.browser.current_page().and_then(|page| {
            let FetchedDocument::Http(response) = &page.document else {
                return None;
            };
            if let Some(html) = page.rendered_html() {
                trust::http::graphical_document_from_html_for_environment(
                    response,
                    html,
                    seed.as_deref(),
                    trust::layout2::Viewport::new(viewport.width, viewport.height),
                    device_pixel_ratio,
                )
            } else {
                trust::http::graphical_document_for_environment(
                    response,
                    trust::layout2::Viewport::new(viewport.width, viewport.height),
                    device_pixel_ratio,
                )
            }
        });
        let layout_image_sizes = if let Some(document) = &document {
            self.sync_image_document(generation, &document.base);
            // Image discovery is a parse result, not a layout result. Start
            // network/decode work before the synchronous CSS layout pass so a
            // gallery's thumbnails overlap grid/text layout instead of
            // waiting behind it on the UI thread. Layout still uses only
            // already-decoded intrinsic sizes and receives one bounded reflow
            // when this batch settles.
            let requests = document
                .eager_image_urls
                .iter()
                .map(|source| trust::render::ImageRequest {
                    handle: ImageHandle::for_source(source),
                    source: source.clone(),
                })
                .collect::<Vec<_>>();
            self.request_page_images(generation, &document.base, &requests, &HashSet::new());
            image_sizes_for_layout(
                &self.image_sizes,
                &self.image_loads.failed,
                &document.image_urls,
            )
        } else {
            trust::layout2::ImageSizes::new()
        };
        self.page_layout = document.map(|mut document| {
            for node in document
                .dom
                .descendants(trust::dom::DOCUMENT)
                .collect::<Vec<_>>()
            {
                let Some(actor) = document
                    .dom
                    .attr(node, "data-trust-node")
                    .and_then(|value| value.parse::<usize>().ok())
                else {
                    continue;
                };
                if let Some(offset) = self.browser.interaction().nested_scroll.get(&actor) {
                    document.dom.set_scroll_pos(
                        node,
                        f64::from(offset.y),
                        f64::from(offset.x),
                        false,
                    );
                }
            }
            let layout = trust::layout2::lay_out_graphical(
                &document.dom,
                &document.base,
                trust::layout2::Viewport::new(viewport.width, viewport.height),
                &document.forms,
                &document.controls,
                &layout_image_sizes,
            );
            let title = trust::accessibility::document_title(&document.dom);
            PageLayoutCache {
                generation,
                revision,
                viewport,
                device_pixel_ratio,
                document,
                layout,
                title,
            }
        });
        if self.page_layout.is_none() {
            let fresh = self
                .protocol_page
                .as_ref()
                .is_some_and(|cache| cache.generation == generation && cache.viewport == viewport);
            if !fresh {
                let carried_selection = self
                    .protocol_page
                    .as_ref()
                    .filter(|cache| cache.generation == generation)
                    .and_then(|cache| cache.selected);
                self.protocol_page = self.browser.current_page().and_then(|page| {
                    let document = trust::render::documents::document(page)?;
                    let gopherus = matches!(document.url, Link::Gopher(_) | Link::Gemini(_));
                    let mut selected = if gopherus {
                        carried_selection.filter(|&line| {
                            document
                                .lines
                                .get(line)
                                .is_some_and(|line| line.link.is_some())
                        })
                    } else {
                        None
                    };
                    let mut layout = trust::render::documents::paint_doc_selected(
                        &document,
                        viewport.width,
                        selected,
                    );
                    // Match the TUI invariant: a fresh page highlights its
                    // first visible link, not an off-screen future target.
                    if gopherus && selected.is_none() {
                        selected = gopherus_visible_links(&layout.lines, 0.0, viewport.height)
                            .first()
                            .copied();
                        if selected.is_some() {
                            layout = trust::render::documents::paint_doc_selected(
                                &document,
                                viewport.width,
                                selected,
                            );
                        }
                    }
                    Some(ProtocolPageCache {
                        generation,
                        viewport,
                        document,
                        layout,
                        selected,
                    })
                });
                self.link_preview = self
                    .protocol_page
                    .as_ref()
                    .and_then(|cache| cache.selected.map(|line| (&cache.document, line)))
                    .and_then(|(document, line)| document.lines.get(line))
                    .and_then(|line| line.link.as_ref())
                    .map(ToString::to_string)
                    .unwrap_or_default();
            }
        } else {
            self.protocol_page = None;
        }
    }

    fn relayout_cached_page(&mut self) {
        let Some(document) = self.page_layout.as_ref().map(|cache| &cache.document) else {
            return;
        };
        let layout_image_sizes = image_sizes_for_layout(
            &self.image_sizes,
            &self.image_loads.failed,
            &document.image_urls,
        );
        let Some(cache) = &mut self.page_layout else {
            return;
        };
        cache.layout = trust::layout2::lay_out_graphical(
            &cache.document.dom,
            &cache.document.base,
            trust::layout2::Viewport::new(cache.viewport.width, cache.viewport.height),
            &cache.document.forms,
            &cache.document.controls,
            &layout_image_sizes,
        );
    }

    fn initialize_renderer(&mut self) -> Result<(), String> {
        if self.renderer.is_some() {
            return Ok(());
        }
        let window = self
            .window
            .as_ref()
            .cloned()
            .ok_or_else(|| String::from("desktop window is unavailable"))?;
        match self.renderer_preference {
            RendererPreference::Cpu => {
                self.renderer = Some(DesktopRenderer::Cpu(Box::new(VelloCpuRenderer::new())));
                self.ensure_software_surface()?;
            }
            RendererPreference::Hybrid | RendererPreference::Auto => {
                match self
                    .runtime
                    .block_on(VelloHybridRenderer::new_window(window))
                {
                    Ok(renderer) => {
                        eprintln!(
                            "trust-desktop: renderer=hybrid adapter={}",
                            renderer.adapter_name()
                        );
                        self.renderer = Some(DesktopRenderer::Hybrid(Box::new(renderer)));
                    }
                    Err(error) if self.renderer_preference == RendererPreference::Auto => {
                        eprintln!("trust-desktop: Hybrid unavailable ({error}); using Vello CPU");
                        self.renderer =
                            Some(DesktopRenderer::Cpu(Box::new(VelloCpuRenderer::new())));
                        self.ensure_software_surface()?;
                    }
                    Err(error) => return Err(error),
                }
            }
        }
        Ok(())
    }

    fn ensure_software_surface(&mut self) -> Result<(), String> {
        if self.surface.is_some() {
            return Ok(());
        }
        let context = self
            .context
            .as_ref()
            .ok_or_else(|| String::from("software display context is unavailable"))?;
        let window = self
            .window
            .as_ref()
            .cloned()
            .ok_or_else(|| String::from("desktop window is unavailable"))?;
        self.surface = Some(Surface::new(context, window).map_err(|error| error.to_string())?);
        Ok(())
    }

    fn present_scene(&mut self, scene: &Scene) -> Result<(), String> {
        let hybrid_result = match self.renderer.as_mut() {
            Some(DesktopRenderer::Hybrid(renderer)) => Some(renderer.present(scene)),
            Some(DesktopRenderer::Cpu(_)) => None,
            None => return Err(String::from("desktop renderer is not initialized")),
        };
        if let Some(result) = hybrid_result {
            match result {
                Ok(PresentOutcome::Presented | PresentOutcome::Skipped) => return Ok(()),
                Err(error) => {
                    // A malformed resource, device loss, or evolving Hybrid
                    // implementation must not tear down the browser. Drop all
                    // GPU-owned state and repaint this same display list using
                    // the independent CPU reference backend.
                    eprintln!("trust-desktop: Hybrid failed ({error}); switching to Vello CPU");
                    self.renderer = Some(DesktopRenderer::Cpu(Box::new(VelloCpuRenderer::new())));
                    self.ensure_software_surface()?;
                }
            }
        }

        self.ensure_software_surface()?;
        let (renderer, surface) = (&mut self.renderer, &mut self.surface);
        let Some(DesktopRenderer::Cpu(renderer)) = renderer else {
            return Err(String::from("software fallback renderer is unavailable"));
        };
        let frame = renderer.render(scene)?;
        let surface = surface
            .as_mut()
            .ok_or_else(|| String::from("desktop software surface is not available"))?;
        let width = NonZeroU32::new(frame.size.width)
            .ok_or_else(|| String::from("zero-width desktop surface"))?;
        let height = NonZeroU32::new(frame.size.height)
            .ok_or_else(|| String::from("zero-height desktop surface"))?;
        surface
            .resize(width, height)
            .map_err(|error| error.to_string())?;
        let mut buffer = surface.buffer_mut().map_err(|error| error.to_string())?;
        if buffer.len() != frame.pixels.len() {
            return Err(format!(
                "presentation buffer has {} pixels, renderer produced {}",
                buffer.len(),
                frame.pixels.len()
            ));
        }
        buffer.copy_from_slice(frame.pixels);
        buffer.present().map_err(|error| error.to_string())
    }

    fn draw(&mut self) -> Result<(), String> {
        if self.metrics.physical.is_empty() {
            return Ok(());
        }
        let trace = std::env::var_os("TRUST_DESKTOP_TRACE").is_some();
        let frame_started = std::time::Instant::now();
        let snapshot = self.browser.snapshot();
        let initial_chrome = self.chrome_model(&snapshot);
        let initial_scene = desktop_chrome(self.metrics, &snapshot, &initial_chrome);
        let page_viewport = CssSize::new(
            initial_scene.content_viewport.width,
            initial_scene.content_viewport.height,
        );
        if self.terminal.is_none() {
            self.ensure_page_layout(page_viewport);
        }
        if trace {
            eprintln!("desktop: layout + chrome {:?}", frame_started.elapsed());
        }
        // Layout supplies the final document title, so rebuild the cheap chrome
        // scene before painting the page rather than displaying it one frame late.
        let chrome = self.chrome_model(&snapshot);
        let mut scene = desktop_chrome(self.metrics, &snapshot, &chrome);
        scene.image_store = self.image_store.clone();

        let generation = self.browser.document_generation();
        if let Some(page) = &self.page_layout {
            let base = page.document.base.clone();
            let (requests, visible_retries) =
                scheduled_page_images(page, self.browser.interaction().scroll, page_viewport);
            self.request_page_images(generation, &base, &requests, &visible_retries);
        }
        if let Some(terminal) = &mut self.terminal {
            let line_mode = !terminal.char_mode();
            let terminal_viewport = CssSize::new(
                page_viewport.width,
                (page_viewport.height - if line_mode { 32.0 } else { 0.0 }).max(1.0),
            );
            let (cols, rows) = terminal.view.size_for_viewport(terminal_viewport);
            if (cols, rows) != (terminal.cols, terminal.rows) {
                terminal.cols = cols;
                terminal.rows = rows;
                terminal.view.resize(cols, rows);
                let _ = terminal
                    .handle
                    .commands
                    .try_send(trust::telnet::Command::Resize { cols, rows });
            }
            scene.append_page(&terminal.view.paint(), CssPoint::default());
            if line_mode {
                let input_rect = CssRect::new(
                    scene.content_viewport.x + 8.0,
                    scene.content_viewport.y + scene.content_viewport.height - 30.0,
                    (scene.content_viewport.width - 16.0).max(1.0),
                    26.0,
                );
                terminal
                    .line_editor
                    .set_width((input_rect.width - 12.0).max(1.0));
                let visual = Self::editor_visual(&mut terminal.line_editor, false);
                scene.primitives.push(DisplayCommand::FillRect {
                    rect: input_rect,
                    color: PaintColor::Rgba(20, 25, 33, 255),
                });
                paint_text_editor(
                    &mut scene.primitives,
                    &visual,
                    input_rect,
                    PaintColor::Rgba(222, 232, 242, 255),
                );
                scene.primitives.push(DisplayCommand::Stroke {
                    shape: PaintShape::Rect(input_rect),
                    brush: PaintBrush::Solid(PaintColor::Accent),
                    style: StrokeStyle::solid(1.0),
                });
            }
        } else if let Some(page) = &self.page_layout {
            scene.append_page(&page.layout.paint, self.browser.interaction().scroll);
        } else if let Some(page) = &self.protocol_page {
            scene.append_page(&page.layout.paint, self.browser.interaction().scroll);
        }

        let query = self.find.text();
        self.find_matches = scene.find_text(&query);
        if !self.find_matches.is_empty() {
            self.find_index = self.find_index.min(self.find_matches.len() - 1);
            for (index, found) in self.find_matches.iter().copied().enumerate() {
                let color = if index == self.find_index {
                    PaintColor::Rgba(255, 166, 32, 125)
                } else {
                    PaintColor::Rgba(255, 226, 64, 80)
                };
                for rect in scene.selection_rects(found) {
                    scene
                        .primitives
                        .push(DisplayCommand::FillRect { rect, color });
                }
            }
        }
        if let Some(selection) = self.selection {
            for rect in scene.selection_rects(selection) {
                scene.primitives.push(DisplayCommand::FillRect {
                    rect,
                    color: PaintColor::Rgba(72, 128, 240, 105),
                });
            }
        }
        self.paint_focused_form(&mut scene);
        self.paint_keyboard_focus(&mut scene);

        if trace {
            eprintln!(
                "desktop: scene {} commands {:?}",
                scene.primitives.len(),
                frame_started.elapsed()
            );
        }

        self.present_scene(&scene)?;
        if trace {
            let renderer = self
                .renderer
                .as_ref()
                .map(|renderer| renderer.kind().name())
                .unwrap_or("uninitialized");
            eprintln!("desktop: raster ({renderer}) {:?}", frame_started.elapsed());
        }
        if let Some(window) = &self.window {
            let title = if chrome.title.is_empty() {
                "TRust Desktop".to_string()
            } else {
                format!("{} — TRust", chrome.title)
            };
            window.set_title(&title);
        }
        self.scene = Some(scene);
        self.update_accessibility(false);
        if trace {
            eprintln!("desktop: frame complete {:?}", frame_started.elapsed());
        }
        Ok(())
    }

    fn paint_focused_form(&mut self, scene: &mut Scene) {
        let FocusTarget::Form { form, field } = self.focus else {
            return;
        };
        let Some(cache) = &self.page_layout else {
            return;
        };
        let Some((&node, _)) = cache
            .document
            .controls
            .iter()
            .find(|(_, indices)| **indices == (form, field))
        else {
            return;
        };
        let Some(bounds) = cache.layout.boxes.get(&node) else {
            return;
        };
        let rect = CssRect::new(
            scene.content_viewport.x + bounds.left as f32 - self.browser.interaction().scroll.x,
            scene.content_viewport.y + bounds.top as f32 - self.browser.interaction().scroll.y,
            bounds.width as f32,
            bounds.height.max(26.0) as f32,
        );
        if let Some(editor) = &mut self.form_editor {
            let password = cache
                .document
                .forms
                .get(form)
                .and_then(|form| form.fields.get(field))
                .is_some_and(|field| field.kind == FieldKind::Password);
            let visual = Self::editor_visual(editor, password);
            scene.primitives.push(DisplayCommand::FillRect {
                rect,
                color: PaintColor::Rgba(250, 252, 255, 255),
            });
            paint_text_editor(
                &mut scene.primitives,
                &visual,
                rect,
                PaintColor::Rgba(25, 31, 39, 255),
            );
        }
        scene.primitives.push(DisplayCommand::Stroke {
            shape: PaintShape::Rect(rect),
            brush: PaintBrush::Solid(PaintColor::Accent),
            style: StrokeStyle::solid(2.0),
        });
    }

    fn paint_keyboard_focus(&self, scene: &mut Scene) {
        let Some(target) = &self.keyboard_target else {
            return;
        };
        let rect = scene
            .interactive_hits()
            .into_iter()
            .find(|candidate| same_page_target(candidate, target))
            .map(|candidate| candidate.rect)
            .unwrap_or(target.rect);
        scene.primitives.push(DisplayCommand::Stroke {
            shape: PaintShape::Rect(rect),
            brush: PaintBrush::Solid(PaintColor::Accent),
            style: StrokeStyle::solid(2.0),
        });
    }

    fn set_focus(&mut self, focus: FocusTarget) {
        let old_viewport = self.browser_viewport();
        self.focus = focus;
        if focus != FocusTarget::Page {
            self.keyboard_target = None;
        }
        let editable = match focus {
            FocusTarget::Address | FocusTarget::Find | FocusTarget::Console => true,
            FocusTarget::Form { form, field } => self
                .page_layout
                .as_ref()
                .and_then(|page| page.document.forms.get(form))
                .and_then(|form| form.fields.get(field))
                .is_some_and(|field| {
                    matches!(
                        field.kind,
                        FieldKind::Text | FieldKind::Password | FieldKind::Textarea
                    )
                }),
            FocusTarget::Page => self
                .terminal
                .as_ref()
                .is_some_and(|terminal| !terminal.char_mode()),
        };
        if let Some(window) = &self.window {
            window.set_ime_allowed(editable);
        }
        let new_viewport = self.browser_viewport();
        if new_viewport != old_viewport {
            self.dispatch(UserAction::Resize(new_viewport));
        }
        self.composing = false;
        self.request_redraw();
    }

    fn focus_address_and_select_all(&mut self) {
        select_address_text(&mut self.address);
        self.set_focus(FocusTarget::Address);
    }

    fn apply_initial_address_focus(&mut self) {
        if !take_initial_address_focus(&mut self.initial_address_focus_pending) {
            return;
        }
        let address = self.browser.snapshot().address;
        if self.address.text() != address {
            self.address.set_text(&address);
        }
        self.focus_address_and_select_all();
    }

    fn activate_control(&mut self, control: ControlId) {
        match control {
            ControlId::Back => self.dispatch(UserAction::Back),
            ControlId::Forward => self.dispatch(UserAction::Forward),
            ControlId::Reload => self.dispatch(UserAction::Reload),
            ControlId::Stop => self.dispatch(UserAction::Stop),
            ControlId::Address => self.focus_address_and_select_all(),
            ControlId::Find => self.focus_chrome_editor(FocusTarget::Find, control),
            ControlId::Console => self.focus_chrome_editor(FocusTarget::Console, control),
            ControlId::FindPrevious => self.advance_find(false),
            ControlId::FindNext => self.advance_find(true),
            ControlId::FindClose => {
                self.find.set_text("");
                self.set_focus(FocusTarget::Page);
            }
        }
    }

    fn focus_chrome_editor(&mut self, focus: FocusTarget, control: ControlId) {
        let rect = self.scene.as_ref().and_then(|scene| {
            scene
                .controls
                .iter()
                .find(|region| region.id == control)
                .map(|region| region.rect)
        });
        self.set_focus(focus);
        if let Some(rect) = rect {
            let point = self.pointer;
            if let Some(editor) = self.active_editor_mut() {
                editor.move_to_point(
                    (point.x - rect.x - 6.0).max(0.0),
                    (point.y - rect.y - 5.0).max(0.0),
                    false,
                );
            }
        }
    }

    fn advance_find(&mut self, forward: bool) {
        if self.find_matches.is_empty() {
            return;
        }
        self.find_index = if forward {
            (self.find_index + 1) % self.find_matches.len()
        } else {
            (self.find_index + self.find_matches.len() - 1) % self.find_matches.len()
        };
        if let Some(scene) = &self.scene
            && let Some(rect) = scene
                .selection_rects(self.find_matches[self.find_index])
                .first()
        {
            let y =
                (self.browser.interaction().scroll.y + rect.y - scene.content_viewport.y - 24.0)
                    .max(0.0);
            self.dispatch(UserAction::SetViewportScroll(CssPoint::new(
                self.browser.interaction().scroll.x,
                y,
            )));
        }
        self.request_redraw();
    }

    fn active_editor_mut(&mut self) -> Option<&mut TextEditor> {
        match self.focus {
            FocusTarget::Address => Some(&mut self.address),
            FocusTarget::Find => Some(&mut self.find),
            FocusTarget::Console => Some(&mut self.console),
            FocusTarget::Form { .. } => self.form_editor.as_mut(),
            FocusTarget::Page => self
                .terminal
                .as_mut()
                .filter(|terminal| !terminal.char_mode())
                .map(|terminal| &mut terminal.line_editor),
        }
    }

    fn finish_text_edit(&mut self) {
        let FocusTarget::Form { form, field } = self.focus else {
            return;
        };
        let Some(value) = self.form_editor.as_ref().map(TextEditor::text) else {
            return;
        };
        let Some(cache) = &mut self.page_layout else {
            return;
        };
        let Some(control) = cache
            .document
            .forms
            .get_mut(form)
            .and_then(|form| form.fields.get_mut(field))
        else {
            return;
        };
        if control.value == value {
            return;
        }
        control.value = value.clone();
        let actor = control.live_node;
        let _ = control;
        self.dispatch(UserAction::SetFormValue {
            actor,
            value,
            checked: None,
        });
        self.relayout_cached_page();
    }

    fn copy(&mut self, cut: bool) {
        let selected = self
            .active_editor_mut()
            .and_then(|editor| editor.selected_text().map(str::to_string))
            .or_else(|| {
                let scene = self.scene.as_ref()?;
                self.selection
                    .map(|selection| scene.selected_text(selection))
            });
        if let Some(text) = selected.filter(|text| !text.is_empty())
            && let Some(clipboard) = &mut self.clipboard
        {
            let _ = clipboard.set_text(text);
            if cut && let Some(editor) = self.active_editor_mut() {
                editor.delete_selection();
                self.finish_text_edit();
            }
        }
    }

    fn paste(&mut self) {
        let text = self
            .clipboard
            .as_mut()
            .and_then(|clipboard| clipboard.get_text().ok());
        if let Some(text) = text
            && let Some(editor) = self.active_editor_mut()
        {
            editor.replace_selection(&text);
            self.finish_text_edit();
            self.request_redraw();
        }
    }

    fn handle_gopherus_key(&mut self, input: &KeyInput) -> bool {
        if input.state != KeyState::Pressed
            || input.modifiers.control
            || input.modifiers.meta
            || input.modifiers.alt
        {
            return false;
        }
        let Some(cache) = &self.protocol_page else {
            return false;
        };
        if !matches!(cache.document.url, Link::Gopher(_) | Link::Gemini(_)) {
            return false;
        }

        match &input.key {
            Key::ArrowLeft => {
                self.dispatch(UserAction::Back);
                true
            }
            Key::ArrowRight | Key::Enter => {
                let link = cache
                    .selected
                    .and_then(|line| cache.document.lines.get(line))
                    .and_then(|line| line.link.clone());
                if let Some(link) = link {
                    self.activate_link(link);
                }
                true
            }
            Key::ArrowUp | Key::ArrowDown => {
                let direction = if input.key == Key::ArrowDown { 1 } else { -1 };
                let position = gopherus_arrow(
                    &cache.layout.lines,
                    cache.selected,
                    self.browser.interaction().scroll.y,
                    cache.viewport.height,
                    cache.layout.paint.height,
                    direction,
                );
                self.apply_gopherus_position(position);
                true
            }
            Key::PageUp | Key::PageDown | Key::Home | Key::End => {
                let current = self.browser.interaction().scroll.y;
                let max_scroll = (cache.layout.paint.height - cache.viewport.height).max(0.0);
                let (target, direction) = match &input.key {
                    Key::PageUp => ((current - cache.viewport.height * 0.9).max(0.0), -1),
                    Key::PageDown => ((current + cache.viewport.height * 0.9).min(max_scroll), 1),
                    Key::Home => (0.0, -1),
                    Key::End => (max_scroll, 1),
                    _ => unreachable!(),
                };
                let selected = gopherus_retarget(
                    &cache.layout.lines,
                    cache.selected,
                    target,
                    cache.viewport.height,
                    direction,
                );
                self.apply_gopherus_position(GopherusPosition {
                    selected,
                    scroll: target,
                });
                true
            }
            _ => false,
        }
    }

    fn apply_gopherus_position(&mut self, position: GopherusPosition) {
        let old_scroll = self.browser.interaction().scroll;
        let (selection_changed, preview) = {
            let Some(cache) = &mut self.protocol_page else {
                return;
            };
            let changed = cache.selected != position.selected;
            cache.selected = position.selected;
            if changed {
                cache.layout = trust::render::documents::paint_doc_selected(
                    &cache.document,
                    cache.viewport.width,
                    cache.selected,
                );
            }
            let preview = cache
                .selected
                .and_then(|line| cache.document.lines.get(line))
                .and_then(|line| line.link.as_ref())
                .map(ToString::to_string)
                .unwrap_or_default();
            (changed, preview)
        };
        self.keyboard_target = None;
        self.link_preview = preview;
        if (old_scroll.y - position.scroll).abs() > f32::EPSILON {
            self.dispatch(UserAction::SetViewportScroll(CssPoint::new(
                old_scroll.x,
                position.scroll,
            )));
        }
        if selection_changed {
            self.request_redraw();
        }
    }

    fn handle_keyboard(&mut self, event: winit::event::KeyEvent) {
        let pressed = event.state == ElementState::Pressed;
        let command = self.modifiers.control_key() || self.modifiers.super_key();
        if pressed
            && command
            && let WinitKey::Character(character) = &event.logical_key
        {
            if character.eq_ignore_ascii_case("l") {
                self.address.set_text(&self.browser.snapshot().address);
                self.focus_address_and_select_all();
                return;
            }
            if character.eq_ignore_ascii_case("f") {
                self.find.select_all();
                self.set_focus(FocusTarget::Find);
                return;
            }
            if character.eq_ignore_ascii_case("r") {
                self.dispatch(UserAction::Reload);
                return;
            }
            if character.eq_ignore_ascii_case("c") {
                self.copy(false);
                return;
            }
            if character.eq_ignore_ascii_case("x") {
                self.copy(true);
                return;
            }
            if character.eq_ignore_ascii_case("v") {
                self.paste();
                return;
            }
        }
        if pressed {
            match (&event.logical_key, self.modifiers.alt_key()) {
                (WinitKey::Named(NamedKey::ArrowLeft), true) => {
                    self.dispatch(UserAction::Back);
                    return;
                }
                (WinitKey::Named(NamedKey::ArrowRight), true) => {
                    self.dispatch(UserAction::Forward);
                    return;
                }
                _ => {}
            }
        }

        let input = KeyInput {
            key: translate_key(&event.logical_key),
            state: if pressed {
                KeyState::Pressed
            } else {
                KeyState::Released
            },
            modifiers: translate_modifiers(self.modifiers),
            repeat: event.repeat,
            composing: self.composing,
        };
        if self.focus == FocusTarget::Page
            && self.terminal.is_none()
            && self.handle_gopherus_key(&input)
        {
            return;
        }
        if pressed {
            let focus = self.focus;
            match (focus, &input.key) {
                (FocusTarget::Address, Key::Enter) => {
                    let address = self.address.text();
                    if !address.trim().is_empty() {
                        self.navigate(address);
                    }
                    self.set_focus(FocusTarget::Page);
                    return;
                }
                (FocusTarget::Find, Key::Enter) => {
                    self.advance_find(!input.modifiers.shift);
                    return;
                }
                (FocusTarget::Console, Key::Enter) => {
                    self.execute_console();
                    return;
                }
                (FocusTarget::Form { form, field }, Key::Enter) => {
                    let kind = self
                        .page_layout
                        .as_ref()
                        .and_then(|page| page.document.forms.get(form))
                        .and_then(|form| form.fields.get(field))
                        .map(|field| field.kind.clone());
                    match kind {
                        Some(FieldKind::Textarea) => {}
                        Some(FieldKind::Text | FieldKind::Password) => {
                            self.finish_text_edit();
                            let submitter = self
                                .page_layout
                                .as_ref()
                                .and_then(|page| page.document.forms.get(form))
                                .and_then(|form| {
                                    form.fields
                                        .iter()
                                        .position(|field| field.kind == FieldKind::Submit)
                                });
                            self.submit_form(form, submitter);
                            return;
                        }
                        Some(_) => {
                            self.activate_form_control(form, field);
                            return;
                        }
                        None => {}
                    }
                }
                (FocusTarget::Form { form, field }, Key::Character(text)) if text == " " => {
                    let editable = self
                        .page_layout
                        .as_ref()
                        .and_then(|page| page.document.forms.get(form))
                        .and_then(|form| form.fields.get(field))
                        .is_some_and(|field| {
                            matches!(
                                field.kind,
                                FieldKind::Text | FieldKind::Password | FieldKind::Textarea
                            )
                        });
                    if !editable {
                        self.activate_form_control(form, field);
                        return;
                    }
                }
                (FocusTarget::Page, Key::Enter) if self.terminal.is_none() => {
                    if let Some(target) = self.keyboard_target.clone() {
                        if let Some(link) = target.link {
                            self.activate_link(link);
                        } else if let Some(actor) = target.actor {
                            self.dispatch(UserAction::Activate(Link::JsClick {
                                node: actor,
                                href: String::new(),
                            }));
                        }
                        return;
                    }
                }
                (_, Key::Tab) if self.terminal.is_none() => {
                    self.focus_next(input.modifiers.shift);
                    return;
                }
                (_, Key::Escape) if self.focus != FocusTarget::Page => {
                    self.finish_text_edit();
                    self.set_focus(FocusTarget::Page);
                    return;
                }
                (_, Key::Escape) if self.terminal.is_none() => {
                    self.dispatch(UserAction::Stop);
                    return;
                }
                (_, Key::Escape)
                    if self
                        .terminal
                        .as_ref()
                        .is_some_and(|terminal| !terminal.char_mode()) =>
                {
                    self.set_focus(FocusTarget::Console);
                    return;
                }
                // ESC is meaningful terminal input; leave it for the VT
                // keyboard encoder below.
                (_, Key::Escape) => {}
                _ => {}
            }
        }
        let consumed = self
            .active_editor_mut()
            .is_some_and(|editor| editor.handle_key(&input));
        if consumed {
            self.finish_text_edit();
            self.request_redraw();
            return;
        }
        if self.focus == FocusTarget::Page
            && let Some(terminal) = &mut self.terminal
        {
            if terminal.char_mode() {
                if let Some(bytes) = trust::terminal_view::TerminalView::encode_key(&input) {
                    let _ = terminal
                        .handle
                        .commands
                        .try_send(trust::telnet::Command::Send(bytes));
                    return;
                }
            } else if pressed && input.key == Key::Enter {
                let mut bytes = terminal.line_editor.text().into_bytes();
                bytes.extend_from_slice(b"\r\n");
                terminal.line_editor.set_text("");
                let _ = terminal
                    .handle
                    .commands
                    .try_send(trust::telnet::Command::Send(bytes));
                self.request_redraw();
                return;
            }
        }
        if pressed
            && !self.composing
            && !command
            && !self.modifiers.alt_key()
            && let Some(text) = &event.text
        {
            let text: String = text
                .chars()
                .filter(|character| !character.is_control())
                .collect();
            if !text.is_empty()
                && let Some(editor) = self.active_editor_mut()
            {
                editor.replace_selection(&text);
                self.finish_text_edit();
                self.request_redraw();
                return;
            }
        }
        self.dispatch(UserAction::Key(input));
    }

    fn handle_ime(&mut self, event: Ime) {
        let action = match event {
            Ime::Enabled => {
                self.composing = false;
                ImeAction::Enabled
            }
            Ime::Preedit(text, cursor) => {
                self.composing = !text.is_empty();
                ImeAction::Preedit { text, cursor }
            }
            Ime::Commit(text) => {
                self.composing = false;
                ImeAction::Commit(text)
            }
            Ime::Disabled => {
                self.composing = false;
                ImeAction::Disabled
            }
        };
        if let Some(editor) = self.active_editor_mut() {
            editor.handle_ime(&action);
            self.finish_text_edit();
        } else if let ImeAction::Commit(text) = &action
            && let Some(terminal) = &self.terminal
        {
            let _ = terminal
                .handle
                .commands
                .try_send(trust::telnet::Command::Send(text.as_bytes().to_vec()));
        }
        self.dispatch(UserAction::Ime(action));
        self.update_ime_cursor_area();
        self.request_redraw();
    }

    fn update_ime_cursor_area(&mut self) {
        let focus = self.focus;
        let Some(editor) = self.active_editor_mut() else {
            return;
        };
        let (_, caret, ime) = editor.geometry();
        let area = caret.unwrap_or(ime);
        let origin = match focus {
            FocusTarget::Address => CssPoint::new(282.0, 16.0),
            FocusTarget::Find | FocusTarget::Console => CssPoint::new(16.0, 90.0),
            FocusTarget::Form { form, field } => self
                .page_layout
                .as_ref()
                .and_then(|page| {
                    page.document.controls.iter().find_map(|(node, indices)| {
                        (*indices == (form, field))
                            .then(|| page.layout.boxes.get(node))
                            .flatten()
                    })
                })
                .and_then(|bounds| {
                    self.scene.as_ref().map(|scene| {
                        CssPoint::new(
                            scene.content_viewport.x + bounds.left as f32
                                - self.browser.interaction().scroll.x,
                            scene.content_viewport.y + bounds.top as f32
                                - self.browser.interaction().scroll.y,
                        )
                    })
                })
                .unwrap_or(self.pointer),
            FocusTarget::Page => self
                .scene
                .as_ref()
                .map(|scene| {
                    CssPoint::new(
                        scene.content_viewport.x + 8.0,
                        scene.content_viewport.y + scene.content_viewport.height - 30.0,
                    )
                })
                .unwrap_or_default(),
        };
        if let Some(window) = &self.window {
            window.set_ime_cursor_area(
                LogicalPosition::new(f64::from(origin.x + area.x), f64::from(origin.y + area.y)),
                LogicalSize::new(
                    f64::from(area.width.max(1.0)),
                    f64::from(area.height.max(1.0)),
                ),
            );
        }
    }

    fn execute_console(&mut self) {
        let command = self.console.text();
        self.console.set_text("");
        let command = command.trim();
        let (verb, argument) = command.split_once(' ').unwrap_or((command, ""));
        match verb.to_ascii_lowercase().as_str() {
            "q" | "quit" | "exit" => self.exit_requested = true,
            "back" => self.dispatch(UserAction::Back),
            "forward" => self.dispatch(UserAction::Forward),
            "reload" => self.dispatch(UserAction::Reload),
            "stop" => self.dispatch(UserAction::Stop),
            "open" if !argument.trim().is_empty() => {
                self.navigate(argument.trim().to_string())
            }
            "find" => {
                self.find.set_text(argument);
                self.set_focus(FocusTarget::Find);
                return;
            }
            "set" if argument.eq_ignore_ascii_case("encoding cp437") => {
                if let Some(terminal) = &mut self.terminal {
                    terminal.view.encoding = trust::terminal_view::Encoding::Cp437;
                    self.browser.set_status("Terminal encoding: CP437");
                }
            }
            "set" if argument.eq_ignore_ascii_case("encoding utf8") => {
                if let Some(terminal) = &mut self.terminal {
                    terminal.view.encoding = trust::terminal_view::Encoding::Utf8;
                    self.browser.set_status("Terminal encoding: UTF-8");
                }
            }
            "help" | "?" => self.browser.set_status(
                "Commands: open URL, back, forward, reload, stop, find TEXT, set encoding cp437|utf8, quit",
            ),
            "" => {}
            _ if command.contains('.') || command.contains("://") => {
                self.navigate(command.to_string())
            }
            _ => self.browser.set_status(format!("Unknown command: {verb}")),
        }
        self.set_focus(FocusTarget::Page);
    }

    fn focus_next(&mut self, reverse: bool) {
        let Some(scene) = &self.scene else {
            self.set_focus(FocusTarget::Address);
            return;
        };
        let mut controls = scene.interactive_hits();
        controls.sort_by(|a, b| {
            a.rect
                .y
                .total_cmp(&b.rect.y)
                .then(a.rect.x.total_cmp(&b.rect.x))
        });
        controls.dedup_by(|left, right| same_page_target(left, right));
        if reverse {
            controls.reverse();
        }
        if controls.is_empty() {
            self.set_focus(FocusTarget::Address);
            return;
        }
        let current = self
            .keyboard_target
            .as_ref()
            .and_then(|current| {
                controls
                    .iter()
                    .position(|candidate| same_page_target(candidate, current))
            })
            .or_else(|| {
                let FocusTarget::Form { form, field } = self.focus else {
                    return None;
                };
                controls.iter().position(|candidate| {
                    matches!(candidate.link, Some(Link::Form { form: candidate_form, field: candidate_field }) if (candidate_form, candidate_field) == (form, field))
                })
            })
            .map_or(0, |index| index + 1);
        let target = controls[current % controls.len()].clone();
        if let Some(Link::Form { form, field }) = target.link {
            self.keyboard_target = None;
            self.focus_form(form, field);
            return;
        }
        self.finish_text_edit();
        self.set_focus(FocusTarget::Page);
        self.scroll_page_target_into_view(target.node, target.rect);
        self.link_preview = target
            .link
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_default();
        self.keyboard_target = Some(target);
        self.request_redraw();
    }

    fn scroll_page_target_into_view(&mut self, node: usize, rect: CssRect) {
        let Some(scene) = &self.scene else { return };
        let nested = scene
            .page_scroll_containers
            .iter()
            .rev()
            .find(|container| {
                if !self.page_layout.as_ref().is_some_and(|page| {
                    page.layout.boxes.contains_key(&node)
                        && page
                            .document
                            .dom
                            .is_host_including_inclusive_ancestor(container.node, node)
                }) {
                    return false;
                }
                let x = rect.x - container.viewport.x + container.offset.x;
                let y = rect.y - container.viewport.y + container.offset.y;
                x + rect.width > 0.0
                    && y + rect.height > 0.0
                    && x < container.content.width
                    && y < container.content.height
            })
            .cloned();
        if let Some(container) = &nested {
            let mut top = container.offset.y;
            let mut left = container.offset.x;
            if rect.y < container.viewport.y {
                top -= container.viewport.y - rect.y;
            } else if rect.y + rect.height > container.viewport.y + container.viewport.height {
                top += rect.y + rect.height - container.viewport.y - container.viewport.height;
            }
            if rect.x < container.viewport.x {
                left -= container.viewport.x - rect.x;
            } else if rect.x + rect.width > container.viewport.x + container.viewport.width {
                left += rect.x + rect.width - container.viewport.x - container.viewport.width;
            }
            top = top.clamp(
                0.0,
                (container.content.height - container.viewport.height).max(0.0),
            );
            left = left.clamp(
                0.0,
                (container.content.width - container.viewport.width).max(0.0),
            );
            if (top - container.offset.y).abs() > f32::EPSILON
                || (left - container.offset.x).abs() > f32::EPSILON
            {
                if let Some(page) = &mut self.page_layout {
                    page.document.dom.set_scroll_pos(
                        container.node,
                        f64::from(top),
                        f64::from(left),
                        false,
                    );
                }
                self.dispatch(UserAction::SetNestedScroll {
                    actor: container.actor,
                    top,
                    left,
                });
                self.relayout_cached_page();
            }
        }
        let rect = nested.map_or(rect, |container| container.viewport);
        let Some(scene) = &self.scene else { return };
        let viewport = scene.content_viewport;
        let current = self.browser.interaction().scroll;
        let y = if rect.y < viewport.y {
            current.y - (viewport.y - rect.y)
        } else if rect.y + rect.height > viewport.y + viewport.height {
            current.y + (rect.y + rect.height - viewport.y - viewport.height)
        } else {
            current.y
        };
        if (y - current.y).abs() > f32::EPSILON {
            self.dispatch(UserAction::SetViewportScroll(CssPoint::new(
                current.x,
                y.max(0.0),
            )));
        }
    }

    fn focus_form(&mut self, form: usize, field: usize) {
        let Some(control) = self
            .page_layout
            .as_ref()
            .and_then(|page| page.document.forms.get(form))
            .and_then(|form| form.fields.get(field))
            .cloned()
        else {
            return;
        };
        self.form_editor = match control.kind {
            FieldKind::Text | FieldKind::Password | FieldKind::Textarea => Some(TextEditor::new(
                &control.value,
                &TextStyle::default(),
                360.0,
                control.kind == FieldKind::Textarea,
            )),
            _ => None,
        };
        self.set_focus(FocusTarget::Form { form, field });
        self.scroll_control_into_view(form, field);
    }

    fn activate_form_control(&mut self, form: usize, field: usize) {
        let Some(control) = self
            .page_layout
            .as_ref()
            .and_then(|page| page.document.forms.get(form))
            .and_then(|form| form.fields.get(field))
            .cloned()
        else {
            return;
        };
        match control.kind {
            FieldKind::Text | FieldKind::Password | FieldKind::Textarea => {
                self.focus_form(form, field);
                self.position_form_caret(form, field);
            }
            FieldKind::Checkbox => self.toggle_check(form, field, !control.checked),
            FieldKind::Radio => self.toggle_radio(form, field),
            FieldKind::Select(options) => {
                if !options.is_empty() {
                    let index = options
                        .iter()
                        .position(|(_, value)| *value == control.value)
                        .map_or(0, |index| (index + 1) % options.len());
                    self.set_field_value(form, field, options[index].1.clone(), None);
                }
            }
            FieldKind::Submit => self.submit_form(form, Some(field)),
            FieldKind::Button => {
                if let Some(actor) = control.live_node {
                    self.dispatch(UserAction::Activate(Link::JsClick {
                        node: actor,
                        href: String::new(),
                    }));
                }
            }
            FieldKind::Reset => self.reset_form(form),
            FieldKind::Hidden => {}
        }
    }

    fn position_form_caret(&mut self, form: usize, field: usize) {
        let origin = self.page_layout.as_ref().and_then(|page| {
            page.document.controls.iter().find_map(|(node, indices)| {
                if *indices != (form, field) {
                    return None;
                }
                let bounds = page.layout.boxes.get(node)?;
                let scene = self.scene.as_ref()?;
                Some(CssPoint::new(
                    scene.content_viewport.x + bounds.left as f32
                        - self.browser.interaction().scroll.x,
                    scene.content_viewport.y + bounds.top as f32
                        - self.browser.interaction().scroll.y,
                ))
            })
        });
        if let Some(origin) = origin {
            let point = self.pointer;
            if let Some(editor) = &mut self.form_editor {
                editor.move_to_point(
                    (point.x - origin.x - 6.0).max(0.0),
                    (point.y - origin.y - 5.0).max(0.0),
                    false,
                );
            }
        }
    }

    fn scroll_control_into_view(&mut self, form: usize, field: usize) {
        let Some(page) = &self.page_layout else {
            return;
        };
        let Some(rect) = page.document.controls.iter().find_map(|(node, indices)| {
            (*indices == (form, field))
                .then(|| page.layout.boxes.get(node))
                .flatten()
        }) else {
            return;
        };
        let current = self.browser.interaction().scroll;
        let mut y = current.y;
        if (rect.top as f32) < current.y {
            y = rect.top as f32;
        } else if (rect.top + rect.height) as f32 > current.y + page.viewport.height {
            y = ((rect.top + rect.height) as f32 - page.viewport.height).max(0.0);
        }
        if y != current.y {
            self.dispatch(UserAction::SetViewportScroll(CssPoint::new(current.x, y)));
        }
    }

    fn reset_form(&mut self, form: usize) {
        if let Some(actor) = self
            .page_layout
            .as_ref()
            .and_then(|page| page.document.forms.get(form))
            .and_then(|form| {
                form.fields
                    .iter()
                    .find(|field| field.kind == FieldKind::Reset)
            })
            .and_then(|field| field.live_node)
        {
            self.dispatch(UserAction::Activate(Link::JsClick {
                node: actor,
                href: String::new(),
            }));
            return;
        }
        let Some(page) = &mut self.page_layout else {
            return;
        };
        let (defaults, _) =
            trust::http::extract_forms_arena(&page.document.dom, &page.document.base, None);
        let Some(default) = defaults.get(form).cloned() else {
            return;
        };
        let old = std::mem::replace(&mut page.document.forms[form], default.clone());
        for (before, after) in old.fields.iter().zip(&default.fields) {
            if before.value != after.value || before.checked != after.checked {
                self.dispatch(UserAction::SetFormValue {
                    actor: after.live_node,
                    value: after.value.clone(),
                    checked: matches!(after.kind, FieldKind::Checkbox | FieldKind::Radio)
                        .then_some(after.checked),
                });
            }
        }
        self.relayout_cached_page();
    }

    fn toggle_check(&mut self, form: usize, field: usize, checked: bool) {
        let value = self
            .page_layout
            .as_ref()
            .and_then(|page| page.document.forms.get(form))
            .and_then(|form| form.fields.get(field))
            .map(|field| field.value.clone())
            .unwrap_or_default();
        self.set_field_value(form, field, value, Some(checked));
    }

    fn toggle_radio(&mut self, form: usize, field: usize) {
        let Some(page) = &mut self.page_layout else {
            return;
        };
        let Some(target) = page
            .document
            .forms
            .get(form)
            .and_then(|form| form.fields.get(field))
            .cloned()
        else {
            return;
        };
        if let Some(fields) = page
            .document
            .forms
            .get_mut(form)
            .map(|form| &mut form.fields)
        {
            for candidate in fields.iter_mut() {
                if candidate.kind == FieldKind::Radio && candidate.name == target.name {
                    candidate.checked = false;
                }
            }
        }
        self.set_field_value(form, field, target.value, Some(true));
    }

    fn set_field_value(&mut self, form: usize, field: usize, value: String, checked: Option<bool>) {
        let Some(page) = &mut self.page_layout else {
            return;
        };
        let Some(control) = page
            .document
            .forms
            .get_mut(form)
            .and_then(|form| form.fields.get_mut(field))
        else {
            return;
        };
        control.value = value.clone();
        if let Some(checked) = checked {
            control.checked = checked;
        }
        let actor = control.live_node;
        self.dispatch(UserAction::SetFormValue {
            actor,
            value,
            checked,
        });
        self.relayout_cached_page();
    }

    fn submit_form(&mut self, form: usize, submitter: Option<usize>) {
        let Some(form) = self
            .page_layout
            .as_ref()
            .and_then(|page| page.document.forms.get(form))
            .cloned()
        else {
            return;
        };
        self.dispatch(UserAction::SubmitForm { form, submitter });
        self.set_focus(FocusTarget::Page);
    }

    fn activate_link(&mut self, link: Link) {
        match link {
            Link::Form { form, field } => self.activate_form_control(form, field),
            Link::Media(url) => {
                let referrer = self
                    .browser
                    .current_page()
                    .and_then(|page| match page.target() {
                        Link::Http(url) => Some(url),
                        _ => None,
                    });
                match trust::media::launch_mpv(url.as_str(), referrer) {
                    Ok(()) => self.browser.set_status(format!("▶ mpv {url}")),
                    Err(error) => self.browser.set_status(error),
                }
            }
            Link::Http(url) if self.same_document_fragment(&url) => {
                self.scroll_static_fragment(url.fragment().unwrap_or(""));
            }
            Link::External(target) if parse_telnet_target(&target).is_some() => {
                self.navigate(target)
            }
            Link::Telnet { host, port, tls } => {
                let address = format!("{}://{host}:{port}", if tls { "telnets" } else { "telnet" });
                self.start_terminal(host, port, tls, address);
            }
            link => self.dispatch(UserAction::Activate(link)),
        }
    }

    fn same_document_fragment(&self, target: &url::Url) -> bool {
        let Some(current) = self
            .browser
            .current_page()
            .and_then(|page| match page.target() {
                Link::Http(url) => Some(url),
                _ => None,
            })
        else {
            return false;
        };
        if target.fragment().is_none() {
            return false;
        }
        let mut left = current.clone();
        let mut right = target.clone();
        left.set_fragment(None);
        right.set_fragment(None);
        left == right
    }

    fn scroll_static_fragment(&mut self, fragment: &str) {
        let Some(page) = &self.page_layout else {
            return;
        };
        let y = if fragment.is_empty() {
            Some(0.0)
        } else {
            page.document
                .dom
                .descendants(trust::dom::DOCUMENT)
                .find(|node| {
                    page.document.dom.attr(*node, "id") == Some(fragment)
                        || (page.document.dom.tag_name(*node) == Some("a")
                            && page.document.dom.attr(*node, "name") == Some(fragment))
                })
                .and_then(|node| page.layout.boxes.get(&node))
                .map(|rect| rect.top as f32)
        };
        if let Some(y) = y {
            self.dispatch(UserAction::SetViewportScroll(CssPoint::new(
                0.0,
                y.max(0.0),
            )));
        }
    }

    fn pointer_moved(&mut self, point: CssPoint) {
        self.pointer = point;
        self.dispatch(UserAction::PointerMove(point));
        let mut visual_changed = false;
        if self.selecting
            && let Some(position) = self
                .scene
                .as_ref()
                .and_then(|scene| scene.text_position_at(point))
            && let Some(selection) = &mut self.selection
        {
            selection.focus = position;
            visual_changed = true;
        }
        let hit = self
            .scene
            .as_ref()
            .and_then(|scene| scene.page_hit_at(point));
        let actor = hit.as_ref().and_then(|hit| hit.actor).or_else(|| {
            hit.as_ref().and_then(|hit| match &hit.link {
                Some(Link::JsClick { node, .. }) => Some(*node),
                _ => None,
            })
        });
        if actor != self.hovered_actor {
            self.hovered_actor = actor;
            visual_changed = true;
            let viewport = self.scene.as_ref().map_or(point, |scene| {
                CssPoint::new(
                    point.x - scene.content_viewport.x,
                    point.y - scene.content_viewport.y,
                )
            });
            self.dispatch(UserAction::PageHover {
                actor,
                position: viewport,
            });
        }
        let link_preview = hit
            .as_ref()
            .and_then(|hit| hit.link.as_ref())
            .map(ToString::to_string)
            .unwrap_or_default();
        if link_preview != self.link_preview {
            self.link_preview = link_preview;
            visual_changed = true;
        }
        let cursor = match hit.as_ref().and_then(|hit| hit.link.as_ref()) {
            Some(Link::Form { form, field })
                if self
                    .page_layout
                    .as_ref()
                    .and_then(|page| page.document.forms.get(*form))
                    .and_then(|form| form.fields.get(*field))
                    .is_some_and(|field| {
                        matches!(
                            field.kind,
                            FieldKind::Text | FieldKind::Password | FieldKind::Textarea
                        )
                    }) =>
            {
                CursorIcon::Text
            }
            Some(_) => CursorIcon::Pointer,
            None if self
                .scene
                .as_ref()
                .and_then(|scene| scene.text_position_at(point))
                .is_some() =>
            {
                CursorIcon::Text
            }
            _ => CursorIcon::Default,
        };
        if cursor != self.cursor_icon {
            self.cursor_icon = cursor;
            if let Some(window) = &self.window {
                window.set_cursor(cursor);
            }
        }
        // Plain motion over the same semantic target only updates the core's
        // pointer coordinates and platform cursor. JS/DOM mutations wake us
        // through BrowserWake; there is no reason to rebuild and rasterize an
        // identical full page at the mouse-reporting rate.
        if visual_changed {
            self.request_redraw();
        }
    }

    fn handle_pointer_button(&mut self, state: ElementState, button: MouseButton) {
        let button = translate_button(button);
        self.dispatch(UserAction::PointerButton {
            position: self.pointer,
            button,
            state: if state == ElementState::Pressed {
                ButtonState::Pressed
            } else {
                ButtonState::Released
            },
        });
        if state == ElementState::Released {
            self.selecting = false;
            match button {
                PointerButton::Back => self.dispatch(UserAction::Back),
                PointerButton::Forward => self.dispatch(UserAction::Forward),
                PointerButton::Primary => {
                    let control = self
                        .scene
                        .as_ref()
                        .and_then(|scene| scene.control_at(self.pointer));
                    if let Some(control) = control
                        && self.pressed_control.take() == Some(control)
                    {
                        self.activate_control(control);
                        self.pressed_hit = None;
                        return;
                    }
                    let released = self
                        .scene
                        .as_ref()
                        .and_then(|scene| scene.page_hit_at(self.pointer))
                        .filter(|hit| hit.link.is_some() || hit.actor.is_some());
                    if let (Some(pressed), Some(released)) = (self.pressed_hit.take(), released)
                        && same_page_target(&pressed, &released)
                    {
                        if let Some(link) = released.link {
                            self.activate_link(link);
                        } else if let Some(actor) = released.actor {
                            self.dispatch(UserAction::Activate(Link::JsClick {
                                node: actor,
                                href: String::new(),
                            }));
                        }
                    }
                }
                _ => {}
            }
            self.pressed_control = None;
            self.pressed_hit = None;
            return;
        }
        if button != PointerButton::Primary {
            return;
        }
        self.pressed_control = self
            .scene
            .as_ref()
            .and_then(|scene| scene.control_at(self.pointer));
        if self.pressed_control.is_some() {
            return;
        }
        self.pressed_hit = self
            .scene
            .as_ref()
            .and_then(|scene| scene.page_hit_at(self.pointer))
            .filter(|hit| hit.link.is_some() || hit.actor.is_some());
        if self.pressed_hit.is_some() {
            return;
        }
        if let Some(position) = self
            .scene
            .as_ref()
            .and_then(|scene| scene.text_position_at(self.pointer))
        {
            self.selection = Some(TextSelection {
                anchor: position,
                focus: position,
            });
            self.selecting = true;
        } else {
            self.selection = None;
        }
        self.keyboard_target = None;
        self.finish_text_edit();
        self.set_focus(FocusTarget::Page);
    }

    fn handle_scroll(&mut self, delta: MouseScrollDelta) {
        let (dx, dy) = match delta {
            MouseScrollDelta::LineDelta(x, y) => (-x * 40.0, -y * 40.0),
            MouseScrollDelta::PixelDelta(position) => {
                let scale = self.metrics.scale_factor.get();
                ((-position.x / scale) as f32, (-position.y / scale) as f32)
            }
        };
        if let Some(terminal) = &mut self.terminal {
            terminal.view.scroll((dy / 19.0).round() as i32);
            self.request_redraw();
            return;
        }
        if let Some(container) = self
            .scene
            .as_ref()
            .and_then(|scene| scene.scroll_container_at(self.pointer))
            .cloned()
        {
            let left = if container.horizontal {
                (container.offset.x + dx).clamp(
                    0.0,
                    (container.content.width - container.viewport.width).max(0.0),
                )
            } else {
                container.offset.x
            };
            let top = if container.vertical {
                (container.offset.y + dy).clamp(
                    0.0,
                    (container.content.height - container.viewport.height).max(0.0),
                )
            } else {
                container.offset.y
            };
            if let Some(cache) = &mut self.page_layout {
                cache.document.dom.set_scroll_pos(
                    container.node,
                    f64::from(top),
                    f64::from(left),
                    false,
                );
                if let Some(scroll) = cache
                    .layout
                    .paint
                    .scroll_containers
                    .iter_mut()
                    .find(|scroll| scroll.node == container.node)
                {
                    scroll.offset = CssPoint::new(left, top);
                }
            }
            self.dispatch(UserAction::SetNestedScroll {
                actor: container.actor,
                top,
                left,
            });
            // Scrolling changes composition transforms and sticky resolution,
            // not CSS box geometry. Updating the retained pixel scroll
            // metadata avoids a full DOM/style/layout pass per touchpad tick.
            self.request_redraw();
            return;
        }
        let Some(scene) = &self.scene else { return };
        let max_x = (scene.page_size.width - scene.content_viewport.width).max(0.0);
        let max_y = (scene.page_size.height - scene.content_viewport.height).max(0.0);
        let current = self.browser.interaction().scroll;
        self.dispatch(UserAction::SetViewportScroll(CssPoint::new(
            (current.x + dx).clamp(0.0, max_x),
            (current.y + dy).clamp(0.0, max_y),
        )));
    }

    fn scroll_to_fragment(&mut self) {
        let Some(fragment) = self.browser.take_fragment_request() else {
            return;
        };
        let Some(page) = &self.page_layout else {
            return;
        };
        let target = if fragment.is_empty() {
            Some(0.0)
        } else {
            page.document
                .dom
                .descendants(trust::dom::DOCUMENT)
                .find(|node| {
                    page.document.dom.attr(*node, "id") == Some(fragment.as_str())
                        || (page.document.dom.tag_name(*node) == Some("a")
                            && page.document.dom.attr(*node, "name") == Some(fragment.as_str()))
                })
                .and_then(|node| page.layout.boxes.get(&node))
                .map(|rect| rect.top as f32)
        };
        if let Some(y) = target {
            self.dispatch(UserAction::SetViewportScroll(CssPoint::new(
                0.0,
                y.max(0.0),
            )));
        }
    }

    fn update_accessibility(&mut self, initial: bool) {
        let viewport = self
            .scene
            .as_ref()
            .map(|scene| scene.content_viewport)
            .unwrap_or(CssRect::new(0.0, 82.0, self.metrics.css.width, 0.0));
        let scroll = self.browser.interaction().scroll;
        let snapshot = self.browser.snapshot();
        let update = build_accessibility_update(
            AccessibilityFrame {
                metrics: self.metrics,
                page: self.page_layout.as_ref(),
                focus: self.focus,
                snapshot: &snapshot,
                content_viewport: viewport,
                scroll,
                keyboard_node: self.keyboard_target.as_ref().map(|target| target.node),
            },
            initial,
        );
        let Some(adapter) = &mut self.accessibility else {
            return;
        };
        adapter.update_if_active(|| update);
    }

    fn handle_access_action(&mut self, request: ActionRequest) {
        match request.target_node {
            ACCESS_BACK if request.action == AccessAction::Click => self.dispatch(UserAction::Back),
            ACCESS_FORWARD if request.action == AccessAction::Click => {
                self.dispatch(UserAction::Forward)
            }
            ACCESS_RELOAD if request.action == AccessAction::Click => {
                self.dispatch(UserAction::Reload)
            }
            ACCESS_ADDRESS => match request.action {
                AccessAction::Focus => self.set_focus(FocusTarget::Address),
                AccessAction::Click => self.focus_address_and_select_all(),
                AccessAction::SetValue | AccessAction::ReplaceSelectedText => {
                    if let Some(ActionData::Value(value)) = request.data {
                        self.address.set_text(&value);
                        self.set_focus(FocusTarget::Address);
                    }
                }
                _ => {}
            },
            ACCESS_FIND if request.action == AccessAction::Click => {
                self.set_focus(FocusTarget::Find)
            }
            ACCESS_CONSOLE if request.action == AccessAction::Click => {
                self.set_focus(FocusTarget::Console)
            }
            AccessNodeId(id) if id >= PAGE_ACCESS_BASE + SemanticTree::DOM_BASE => {
                let node = (id - PAGE_ACCESS_BASE - SemanticTree::DOM_BASE) as usize;
                let control = self
                    .page_layout
                    .as_ref()
                    .and_then(|page| page.document.controls.get(&node).copied());
                if let Some((form, field)) = control {
                    if let Some(ActionData::Value(value)) = request.data {
                        self.set_field_value(form, field, value.into(), None);
                    } else if request.action == AccessAction::Click {
                        self.activate_form_control(form, field);
                    } else {
                        self.focus_form(form, field);
                    }
                } else {
                    let target = self.scene.as_ref().and_then(|scene| {
                        scene
                            .interactive_hits()
                            .into_iter()
                            .find(|target| target.node == node)
                    });
                    if let Some(target) = target {
                        if request.action == AccessAction::Click {
                            if let Some(link) = target.link {
                                self.activate_link(link);
                            } else if let Some(actor) = target.actor {
                                self.dispatch(UserAction::Activate(Link::JsClick {
                                    node: actor,
                                    href: String::new(),
                                }));
                            }
                        } else if matches!(
                            request.action,
                            AccessAction::Focus | AccessAction::ScrollIntoView
                        ) {
                            self.scroll_page_target_into_view(target.node, target.rect);
                            self.keyboard_target = Some(target);
                            self.set_focus(FocusTarget::Page);
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

impl ApplicationHandler<DesktopEvent> for DesktopApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        event_loop.set_control_flow(ControlFlow::Wait);
        if self.window.is_none() {
            let attributes = Window::default_attributes()
                .with_title("TRust Desktop")
                .with_visible(false)
                .with_inner_size(LogicalSize::new(960.0, 640.0))
                .with_min_inner_size(LogicalSize::new(480.0, 320.0));
            match event_loop.create_window(attributes) {
                Ok(window) => {
                    let window = Arc::new(window);
                    self.accessibility = Some(AccessAdapter::with_event_loop_proxy(
                        event_loop,
                        &window,
                        self.event_proxy.clone(),
                    ));
                    window.set_visible(true);
                    self.window = Some(window);
                }
                Err(error) => {
                    eprintln!("trust-desktop: could not create window: {error}");
                    event_loop.exit();
                    return;
                }
            }
        }
        if self.context.is_none() {
            match Context::new(event_loop.owned_display_handle()) {
                Ok(context) => self.context = Some(context),
                Err(error) => {
                    eprintln!("trust-desktop: could not create software context: {error}");
                    event_loop.exit();
                    return;
                }
            }
        }
        if self.renderer.is_none() {
            if let Err(error) = self.initialize_renderer() {
                eprintln!("trust-desktop: could not initialize renderer: {error}");
                event_loop.exit();
                return;
            }
        } else if let Some(DesktopRenderer::Hybrid(renderer)) = &mut self.renderer {
            let Some(window) = &self.window else { return };
            if let Err(error) = renderer.resume(Arc::clone(window)) {
                eprintln!(
                    "trust-desktop: could not resume Hybrid ({error}); switching to Vello CPU"
                );
                self.renderer = Some(DesktopRenderer::Cpu(Box::new(VelloCpuRenderer::new())));
                if let Err(error) = self.ensure_software_surface() {
                    eprintln!("trust-desktop: CPU fallback unavailable: {error}");
                    event_loop.exit();
                    return;
                }
            }
        } else if let Err(error) = self.ensure_software_surface() {
            eprintln!("trust-desktop: could not resume software surface: {error}");
            event_loop.exit();
            return;
        }
        self.update_metrics(None);
        if let Some(address) = self.initial_navigation.take() {
            self.navigate(address);
        }
        let outcome = self.browser.process_async_events();
        if outcome.loading_retired {
            self.retire_page_loading();
        }
        self.apply_initial_address_focus();
        self.request_redraw();
    }

    fn suspended(&mut self, _event_loop: &ActiveEventLoop) {
        if let Some(DesktopRenderer::Hybrid(renderer)) = &mut self.renderer {
            renderer.suspend();
        }
        self.surface = None;
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: DesktopEvent) {
        match event {
            DesktopEvent::BrowserWake => {
                let outcome = self.browser.process_async_events();
                if outcome.loading_retired {
                    self.retire_page_loading();
                }
                if std::env::var_os("TRUST_DESKTOP_TRACE").is_some() {
                    eprintln!(
                        "desktop: browser wake changed={} retired={}",
                        outcome.invalidated, outcome.loading_retired
                    );
                }
                if outcome.invalidated {
                    self.scroll_to_fragment();
                    self.request_redraw();
                }
            }
            DesktopEvent::ImageLoaded {
                generation,
                image_epoch,
                handle,
                source,
                result,
            } => {
                if generation != self.browser.document_generation()
                    || !self.image_loads.accepts(generation, image_epoch)
                {
                    return;
                }
                let success = result.is_ok();
                if !self.image_loads.finish(handle, success) {
                    return;
                }
                self.image_tasks.remove(&handle);
                match result {
                    Ok(image) => {
                        self.image_sizes.insert(source, (image.width, image.height));
                        self.image_store.insert(handle, image);
                        self.decoded_images_pending_layout = true;
                    }
                    Err(error) => {
                        if std::env::var_os("TRUST_DESKTOP_TRACE").is_some() {
                            eprintln!("desktop: image {source} failed: {error}");
                        }
                        // A pending image was represented by the CSS default
                        // object size. Relayout once it becomes broken so the
                        // HTML alt-text rendering path replaces that box.
                        self.decoded_images_pending_layout = true;
                    }
                }
                if let Some(page) = self
                    .page_layout
                    .as_ref()
                    .map(|page| page.document.base.clone())
                {
                    self.pump_image_loads(generation, image_epoch, &page);
                }
                self.flush_images_when_settled(generation, image_epoch);
            }
            DesktopEvent::ImagesReady {
                generation,
                image_epoch,
            } => {
                if std::env::var_os("TRUST_DESKTOP_TRACE").is_some() {
                    eprintln!(
                        "desktop: images-ready dirty={} pending={}",
                        self.decoded_images_pending_layout,
                        self.image_loads.pending.len()
                    );
                }
                if generation != self.browser.document_generation()
                    || !self.image_loads.accepts(generation, image_epoch)
                {
                    return;
                }
                self.image_flush_scheduled = false;
                if !self.decoded_images_pending_layout {
                    return;
                }
                self.decoded_images_pending_layout = false;
                self.browser.send_image_sizes(&self.image_sizes);
                self.relayout_cached_page();
                self.request_redraw();
            }
            DesktopEvent::Access(event) => match event.window_event {
                accesskit_winit::WindowEvent::InitialTreeRequested => {
                    self.update_accessibility(true)
                }
                accesskit_winit::WindowEvent::ActionRequested(request) => {
                    self.handle_access_action(request)
                }
                accesskit_winit::WindowEvent::AccessibilityDeactivated => {}
            },
            DesktopEvent::Telnet(event) => {
                match event {
                    trust::telnet::Event::Connected { peer, tls } => {
                        if let Some(terminal) = &mut self.terminal {
                            terminal.connected = true;
                        }
                        self.browser.set_status(format!(
                            "Connected to {peer}{}",
                            if tls { " over TLS" } else { "" }
                        ));
                    }
                    trust::telnet::Event::Data(data) => {
                        if let Some(terminal) = &mut self.terminal {
                            for reply in terminal.view.process(&data) {
                                let _ = terminal
                                    .handle
                                    .commands
                                    .try_send(trust::telnet::Command::Send(reply));
                            }
                        }
                    }
                    trust::telnet::Event::LineMode { active, edit } => {
                        if let Some(terminal) = &mut self.terminal {
                            terminal.linemode_active = active;
                            terminal.linemode_edit = edit;
                        }
                        self.browser.set_status(format!(
                            "Terminal input: {}",
                            if active && edit {
                                "line mode"
                            } else {
                                "character mode"
                            }
                        ));
                    }
                    trust::telnet::Event::Closed(reason) => {
                        if let Some(terminal) = &mut self.terminal {
                            terminal.connected = false;
                            terminal.remote_echo = false;
                            terminal.linemode_active = false;
                        }
                        self.browser.set_status(reason.map_or_else(
                            || String::from("Connection closed by foreign host."),
                            |error| format!("Terminal connection closed: {error}"),
                        ));
                    }
                    trust::telnet::Event::Negotiation { command, option } => {
                        if option == libmudtelnet::telnet::op_option::ECHO
                            && let Some(terminal) = &mut self.terminal
                        {
                            terminal.remote_echo =
                                command == libmudtelnet::telnet::op_command::WILL;
                        }
                    }
                }
                if self.focus == FocusTarget::Page {
                    self.set_focus(FocusTarget::Page);
                }
                self.request_redraw();
            }
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        if self
            .window
            .as_ref()
            .is_none_or(|window| window.id() != window_id)
        {
            return;
        }
        if let (Some(adapter), Some(window)) = (&mut self.accessibility, &self.window) {
            adapter.process_event(window, &event);
        }
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Destroyed => {
                self.surface = None;
                self.renderer = None;
                self.window = None;
                event_loop.exit();
            }
            WindowEvent::Resized(_) => {
                if std::env::var_os("TRUST_DESKTOP_TRACE").is_some() {
                    eprintln!("desktop: window resized");
                }
                self.update_metrics(None);
                self.request_redraw();
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                self.update_metrics(Some(scale_factor));
                self.request_redraw();
            }
            WindowEvent::RedrawRequested => {
                if !consume_pending_redraw(&mut self.redraw_pending) {
                    return;
                }
                if let Err(error) = self.draw() {
                    eprintln!("trust-desktop: draw failed: {error}");
                }
            }
            WindowEvent::Focused(focused) => {
                if !focused {
                    self.composing = false;
                    self.selecting = false;
                }
                self.dispatch(UserAction::Focus(focused));
            }
            WindowEvent::ModifiersChanged(modifiers) => self.modifiers = modifiers.state(),
            WindowEvent::KeyboardInput {
                event,
                is_synthetic: false,
                ..
            } => {
                self.handle_keyboard(event);
                if self.exit_requested {
                    event_loop.exit();
                }
            }
            WindowEvent::Ime(event) => self.handle_ime(event),
            WindowEvent::CursorMoved { position, .. } => {
                self.pointer_moved(self.metrics.physical_to_css(position.x, position.y));
            }
            WindowEvent::CursorLeft { .. } => {
                self.hovered_actor = None;
                self.link_preview.clear();
                self.dispatch(UserAction::PageHover {
                    actor: None,
                    position: CssPoint::default(),
                });
            }
            WindowEvent::MouseInput { state, button, .. } => {
                self.handle_pointer_button(state, button)
            }
            WindowEvent::MouseWheel { delta, .. } => self.handle_scroll(delta),
            WindowEvent::Touch(touch) => {
                let point = self
                    .metrics
                    .physical_to_css(touch.location.x, touch.location.y);
                self.pointer_moved(point);
                match touch.phase {
                    TouchPhase::Started => {
                        self.handle_pointer_button(ElementState::Pressed, MouseButton::Left)
                    }
                    TouchPhase::Ended | TouchPhase::Cancelled => {
                        self.handle_pointer_button(ElementState::Released, MouseButton::Left)
                    }
                    TouchPhase::Moved => {}
                }
            }
            _ => {}
        }
    }
}

struct AccessibilityFrame<'a> {
    metrics: ViewportMetrics,
    page: Option<&'a PageLayoutCache>,
    focus: FocusTarget,
    snapshot: &'a trust::core::BrowserSnapshot,
    content_viewport: CssRect,
    scroll: CssPoint,
    keyboard_node: Option<usize>,
}

fn build_accessibility_update(frame: AccessibilityFrame<'_>, initial: bool) -> TreeUpdate {
    let AccessibilityFrame {
        metrics,
        page,
        focus,
        snapshot,
        content_viewport,
        scroll,
        keyboard_node,
    } = frame;
    let mut nodes = Vec::new();
    let mut root = AccessNode::new(AccessRole::Window);
    root.set_label("TRust Desktop");
    root.set_bounds(AccessRect::new(
        0.0,
        0.0,
        f64::from(metrics.css.width),
        f64::from(metrics.css.height),
    ));
    root.set_transform(
        Affine::translate(AccessVec2::ZERO) * Affine::scale(metrics.scale_factor.get()),
    );
    let mut children = vec![
        ACCESS_BACK,
        ACCESS_FORWARD,
        ACCESS_RELOAD,
        ACCESS_ADDRESS,
        ACCESS_FIND,
        ACCESS_CONSOLE,
    ];
    let buttons = [
        (ACCESS_BACK, "Back", snapshot.can_go_back),
        (ACCESS_FORWARD, "Forward", snapshot.can_go_forward),
        (
            ACCESS_RELOAD,
            if snapshot.loading { "Stop" } else { "Reload" },
            true,
        ),
        (ACCESS_FIND, "Find in page", true),
        (ACCESS_CONSOLE, "TRust command console", true),
    ];
    for (id, label, enabled) in buttons {
        let mut node = AccessNode::new(AccessRole::Button);
        node.set_label(label);
        let index = match id {
            ACCESS_BACK => 0,
            ACCESS_FORWARD => 1,
            ACCESS_RELOAD => 2,
            ACCESS_CONSOLE => 4,
            ACCESS_FIND => 5,
            _ => 0,
        };
        let left = 8.0 + f64::from(index) * 44.0;
        node.set_bounds(AccessRect::new(left, 8.0, left + 36.0, 44.0));
        if enabled {
            node.add_action(AccessAction::Click);
            node.add_action(AccessAction::Focus);
        } else {
            node.set_disabled();
        }
        nodes.push((id, node));
    }
    let mut address = AccessNode::new(AccessRole::TextInput);
    address.set_label("Address");
    address.set_value(snapshot.address.as_str());
    let address_left = 8.0 + 6.0 * 44.0;
    address.set_bounds(AccessRect::new(
        address_left,
        8.0,
        f64::from(metrics.css.width) - 8.0,
        44.0,
    ));
    address.add_action(AccessAction::Focus);
    address.add_action(AccessAction::SetValue);
    address.add_action(AccessAction::ReplaceSelectedText);
    nodes.push((ACCESS_ADDRESS, address));

    if let Some(page) = page {
        let focused = match focus {
            FocusTarget::Form { form, field } => page
                .document
                .controls
                .iter()
                .find_map(|(node, indices)| (*indices == (form, field)).then_some(*node)),
            _ => None,
        };
        let semantic = SemanticTree::for_document(
            &page.document.dom,
            &page.layout.boxes,
            &page.document.forms,
            &page.document.controls,
            focused,
        );
        children.push(AccessNodeId(PAGE_ACCESS_BASE + semantic.root));
        for semantic_node in semantic.nodes {
            let id = AccessNodeId(PAGE_ACCESS_BASE + semantic_node.id);
            let mut node = AccessNode::new(access_role(semantic_node.role));
            if !semantic_node.name.is_empty() {
                node.set_label(semantic_node.name.as_str());
            }
            if let Some(value) = semantic_node.value {
                node.set_value(value);
            }
            node.set_bounds(AccessRect::new(
                f64::from(content_viewport.x + semantic_node.bounds.x - scroll.x),
                f64::from(content_viewport.y + semantic_node.bounds.y - scroll.y),
                f64::from(
                    content_viewport.x + semantic_node.bounds.x + semantic_node.bounds.width
                        - scroll.x,
                ),
                f64::from(
                    content_viewport.y + semantic_node.bounds.y + semantic_node.bounds.height
                        - scroll.y,
                ),
            ));
            node.set_children(
                semantic_node
                    .children
                    .into_iter()
                    .map(|child| AccessNodeId(PAGE_ACCESS_BASE + child))
                    .collect::<Vec<_>>(),
            );
            for action in semantic_node.actions {
                node.add_action(match action {
                    trust::accessibility::Action::Focus => AccessAction::Focus,
                    trust::accessibility::Action::Activate => AccessAction::Click,
                    trust::accessibility::Action::SetValue => AccessAction::SetValue,
                    trust::accessibility::Action::SetSelection => AccessAction::SetTextSelection,
                    trust::accessibility::Action::ScrollIntoView => AccessAction::ScrollIntoView,
                });
            }
            if let Some(checked) = semantic_node.checked {
                node.set_toggled(if checked {
                    accesskit::Toggled::True
                } else {
                    accesskit::Toggled::False
                });
            }
            nodes.push((id, node));
        }
    }
    root.set_children(children);
    nodes.push((ACCESS_ROOT, root));
    let focus = match focus {
        FocusTarget::Address => ACCESS_ADDRESS,
        FocusTarget::Find => ACCESS_FIND,
        FocusTarget::Console => ACCESS_CONSOLE,
        FocusTarget::Form { form, field } => page
            .and_then(|page| {
                page.document
                    .controls
                    .iter()
                    .find_map(|(node, indices)| (*indices == (form, field)).then_some(*node))
            })
            .map(|node| AccessNodeId(PAGE_ACCESS_BASE + SemanticTree::DOM_BASE + node as u64))
            .unwrap_or(ACCESS_ROOT),
        FocusTarget::Page => keyboard_node
            .filter(|node| page.is_some_and(|page| page.layout.boxes.contains_key(node)))
            .map(|node| AccessNodeId(PAGE_ACCESS_BASE + SemanticTree::DOM_BASE + node as u64))
            .unwrap_or(ACCESS_ROOT),
    };
    TreeUpdate {
        nodes,
        tree: initial.then(|| Tree::new(ACCESS_ROOT)),
        tree_id: TreeId::ROOT,
        focus,
    }
}

fn same_page_target(left: &PageHit, right: &PageHit) -> bool {
    match (left.actor, right.actor) {
        (Some(left), Some(right)) => left == right,
        (Some(_), None) | (None, Some(_)) => false,
        (None, None) => {
            left.node == right.node
                || (left.link == right.link
                    && (left.rect.x - right.rect.x).abs() < 1.0
                    && (left.rect.y - right.rect.y).abs() < 1.0)
        }
    }
}

fn access_role(role: SemanticRole) -> AccessRole {
    match role {
        SemanticRole::Document => AccessRole::RootWebArea,
        SemanticRole::Generic => AccessRole::GenericContainer,
        SemanticRole::Heading => AccessRole::Heading,
        SemanticRole::Paragraph => AccessRole::Paragraph,
        SemanticRole::Link => AccessRole::Link,
        SemanticRole::Button => AccessRole::Button,
        SemanticRole::TextInput => AccessRole::TextInput,
        SemanticRole::PasswordInput => AccessRole::PasswordInput,
        SemanticRole::Textarea => AccessRole::MultilineTextInput,
        SemanticRole::Checkbox => AccessRole::CheckBox,
        SemanticRole::Radio => AccessRole::RadioButton,
        SemanticRole::Select => AccessRole::ComboBox,
        SemanticRole::Image => AccessRole::Image,
        SemanticRole::List => AccessRole::List,
        SemanticRole::ListItem => AccessRole::ListItem,
        SemanticRole::Table => AccessRole::Table,
        SemanticRole::Row => AccessRole::Row,
        SemanticRole::Cell => AccessRole::Cell,
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct GopherusPosition {
    selected: Option<usize>,
    scroll: f32,
}

fn gopherus_arrow(
    lines: &[trust::render::documents::ProtocolLine],
    selected: Option<usize>,
    scroll: f32,
    viewport_height: f32,
    page_height: f32,
    direction: i32,
) -> GopherusPosition {
    debug_assert!(direction == -1 || direction == 1);
    let max_scroll = (page_height - viewport_height).max(0.0);

    // Gopherus first hands the highlight directly to an adjacent link.
    if let Some(current) = selected {
        let next = current as i64 + i64::from(direction);
        if next >= 0
            && lines
                .get(next as usize)
                .is_some_and(|line| line.link.is_some())
        {
            let next = next as usize;
            let line = &lines[next];
            let center = scroll + viewport_height / 2.0;
            let line_center = line.rect.y + line.rect.height / 2.0;
            let scroll = if direction > 0 && line_center > center {
                (scroll + line.rect.height.max(1.0)).min(max_scroll)
            } else if direction < 0 && line_center < center {
                (scroll - line.rect.height.max(1.0)).max(0.0)
            } else {
                scroll
            };
            return GopherusPosition {
                selected: Some(next),
                scroll,
            };
        }
    }

    let step = lines
        .iter()
        .find(|line| {
            line.rect.y + line.rect.height > scroll
                && line.rect.y < scroll + viewport_height.max(1.0)
        })
        .map_or(trust::theme::TERMINAL_FONT_SIZE_CSS_PX * 1.2, |line| {
            line.rect.height.max(1.0)
        });
    let target = (scroll + direction as f32 * step).clamp(0.0, max_scroll);
    let selected = if (target - scroll).abs() > f32::EPSILON {
        gopherus_retarget(lines, selected, target, viewport_height, direction)
    } else {
        gopherus_walk(lines, selected, scroll, viewport_height, direction)
    };
    GopherusPosition {
        selected,
        scroll: target,
    }
}

fn gopherus_retarget(
    lines: &[trust::render::documents::ProtocolLine],
    selected: Option<usize>,
    scroll: f32,
    viewport_height: f32,
    direction: i32,
) -> Option<usize> {
    let links = gopherus_visible_links(lines, scroll, viewport_height);
    let (&first, rest) = links.split_first()?;
    let last = *rest.last().unwrap_or(&first);
    let center = scroll + viewport_height / 2.0;
    let distance = |line: usize| {
        let rect = lines[line].rect;
        (rect.y + rect.height / 2.0 - center).abs()
    };
    match selected {
        Some(current)
            if lines.get(current).is_some_and(|line| {
                line.rect.y + line.rect.height > scroll && line.rect.y < scroll + viewport_height
            }) =>
        {
            let next = if direction > 0 {
                links.iter().copied().find(|&line| line > current)
            } else {
                links.iter().rev().copied().find(|&line| line < current)
            };
            next.filter(|&line| distance(line) < distance(current))
                .or(Some(current))
        }
        _ => Some(if direction > 0 { first } else { last }),
    }
}

fn gopherus_walk(
    lines: &[trust::render::documents::ProtocolLine],
    selected: Option<usize>,
    scroll: f32,
    viewport_height: f32,
    direction: i32,
) -> Option<usize> {
    let links = gopherus_visible_links(lines, scroll, viewport_height);
    match (selected, links.as_slice()) {
        (_, []) => None,
        (None, links) => Some(if direction > 0 {
            links[0]
        } else {
            *links.last().unwrap()
        }),
        (Some(current), links) if direction > 0 => links
            .iter()
            .copied()
            .find(|&line| line > current)
            .or(Some(current)),
        (Some(current), links) => links
            .iter()
            .rev()
            .copied()
            .find(|&line| line < current)
            .or(Some(current)),
    }
}

fn gopherus_visible_links(
    lines: &[trust::render::documents::ProtocolLine],
    scroll: f32,
    viewport_height: f32,
) -> Vec<usize> {
    lines
        .iter()
        .enumerate()
        .filter(|(_, line)| {
            line.link.is_some()
                && line.rect.y + line.rect.height > scroll
                && line.rect.y < scroll + viewport_height
        })
        .map(|(index, _)| index)
        .collect()
}

fn take_initial_address_focus(pending: &mut bool) -> bool {
    std::mem::take(pending)
}

fn select_address_text(editor: &mut TextEditor) {
    editor.select_all();
}

fn chrome_text_style() -> TextStyle {
    TextStyle {
        size: 15.0,
        ..TextStyle::default()
    }
}

fn terminal_text_style() -> TextStyle {
    TextStyle {
        family: String::from(trust::theme::TERMINAL_FONT_FAMILY),
        size: trust::theme::TERMINAL_FONT_SIZE_CSS_PX,
        weight: trust::theme::TERMINAL_FONT_WEIGHT,
        ..TextStyle::default()
    }
}

fn parse_telnet_target(address: &str) -> Option<(String, u16, bool)> {
    let address = address.trim();
    let tls = address.starts_with("telnets://");
    if !tls && !address.starts_with("telnet://") {
        return None;
    }
    let parsed = url::Url::parse(address).ok()?;
    let host = parsed.host_str()?.to_string();
    let port = parsed.port().unwrap_or(if tls { 992 } else { 23 });
    Some((host, port, tls))
}

fn translate_modifiers(modifiers: ModifiersState) -> Modifiers {
    Modifiers {
        shift: modifiers.shift_key(),
        control: modifiers.control_key(),
        alt: modifiers.alt_key(),
        meta: modifiers.super_key(),
    }
}

fn translate_button(button: MouseButton) -> PointerButton {
    match button {
        MouseButton::Left => PointerButton::Primary,
        MouseButton::Middle => PointerButton::Auxiliary,
        MouseButton::Right => PointerButton::Secondary,
        MouseButton::Back => PointerButton::Back,
        MouseButton::Forward => PointerButton::Forward,
        MouseButton::Other(button) => PointerButton::Other(button),
    }
}

fn translate_key(key: &WinitKey) -> Key {
    match key {
        WinitKey::Character(character) => Key::Character(character.to_string()),
        WinitKey::Named(NamedKey::Enter) => Key::Enter,
        WinitKey::Named(NamedKey::Escape) => Key::Escape,
        WinitKey::Named(NamedKey::Backspace) => Key::Backspace,
        WinitKey::Named(NamedKey::Delete) => Key::Delete,
        WinitKey::Named(NamedKey::Tab) => Key::Tab,
        WinitKey::Named(NamedKey::ArrowLeft) => Key::ArrowLeft,
        WinitKey::Named(NamedKey::ArrowRight) => Key::ArrowRight,
        WinitKey::Named(NamedKey::ArrowUp) => Key::ArrowUp,
        WinitKey::Named(NamedKey::ArrowDown) => Key::ArrowDown,
        WinitKey::Named(NamedKey::Home) => Key::Home,
        WinitKey::Named(NamedKey::End) => Key::End,
        WinitKey::Named(NamedKey::PageUp) => Key::PageUp,
        WinitKey::Named(NamedKey::PageDown) => Key::PageDown,
        other => Key::Other(format!("{other:?}")),
    }
}

fn consume_pending_redraw(pending: &mut bool) -> bool {
    std::mem::take(pending)
}

#[derive(Debug, Default, PartialEq, Eq)]
struct DesktopOptions {
    renderer: RendererPreference,
    address: Option<String>,
    help: bool,
}

fn parse_desktop_args(args: impl IntoIterator<Item = String>) -> Result<DesktopOptions, String> {
    let mut options = DesktopOptions::default();
    let mut args = args.into_iter();
    while let Some(argument) = args.next() {
        if argument == "-h" || argument == "--help" {
            options.help = true;
        } else if let Some(renderer) = argument.strip_prefix("--renderer=") {
            options.renderer = renderer.parse()?;
        } else if argument == "--renderer" {
            let renderer = args
                .next()
                .ok_or_else(|| String::from("--renderer requires cpu, hybrid, or auto"))?;
            options.renderer = renderer.parse()?;
        } else if argument.starts_with('-') {
            return Err(format!("unknown option {argument:?}"));
        } else if options.address.replace(argument).is_some() {
            return Err(String::from(
                "trust-desktop accepts at most one initial URL",
            ));
        }
    }
    Ok(options)
}

fn main() -> Result<(), Box<dyn Error>> {
    let options = parse_desktop_args(std::env::args().skip(1))
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
    if options.help {
        println!("Usage: trust-desktop [--renderer=auto|cpu|hybrid] [URL]");
        return Ok(());
    }
    let event_loop = EventLoop::<DesktopEvent>::with_user_event().build()?;
    let proxy = event_loop.create_proxy();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("trust-desktop-net")
        .build()?;
    let browser = BrowserController::new(
        runtime.handle().clone(),
        move || {
            let _ = proxy.send_event(DesktopEvent::BrowserWake);
        },
        CssSize::new(960.0, 558.0),
    );
    let mut app = DesktopApp::new(
        browser,
        runtime.handle().clone(),
        event_loop.create_proxy(),
        options.renderer,
        options.address,
    );
    event_loop.run_app(&mut app)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn protocol_line(y: f32, linked: bool) -> trust::render::documents::ProtocolLine {
        trust::render::documents::ProtocolLine {
            rect: CssRect::new(0.0, y, 100.0, 20.0),
            link: linked.then(|| Link::External(format!("test:{y}"))),
        }
    }

    #[test]
    fn desktop_cli_selects_renderer_without_stealing_initial_url() {
        assert_eq!(
            parse_desktop_args([
                String::from("--renderer=hybrid"),
                String::from("https://example.test/")
            ])
            .unwrap(),
            DesktopOptions {
                renderer: RendererPreference::Hybrid,
                address: Some(String::from("https://example.test/")),
                help: false,
            }
        );
        assert_eq!(
            parse_desktop_args([String::from("--renderer"), String::from("cpu")])
                .unwrap()
                .renderer,
            RendererPreference::Cpu
        );
        assert!(parse_desktop_args([String::from("--renderer=nope")]).is_err());
    }

    #[test]
    fn platform_redraw_without_trust_invalidation_is_ignored() {
        let mut pending = false;
        assert!(!consume_pending_redraw(&mut pending));
        pending = true;
        assert!(consume_pending_redraw(&mut pending));
        assert!(!consume_pending_redraw(&mut pending));
    }

    #[test]
    fn startup_address_focus_is_applied_exactly_once() {
        let mut pending = true;
        assert!(take_initial_address_focus(&mut pending));
        assert!(!take_initial_address_focus(&mut pending));
    }

    #[test]
    fn address_activation_selects_the_entire_unicode_value() {
        let mut editor = TextEditor::new(
            "gemini://例え.example/場所",
            &chrome_text_style(),
            600.0,
            false,
        );
        editor.select_byte_range(2, 2);
        select_address_text(&mut editor);
        assert_eq!(editor.selection(), 0..editor.raw_text().len());
    }

    #[test]
    fn gopherus_arrows_transition_adjacent_links_before_scrolling() {
        let lines = vec![
            protocol_line(0.0, true),
            protocol_line(20.0, true),
            protocol_line(40.0, false),
            protocol_line(60.0, true),
        ];
        assert_eq!(
            gopherus_arrow(&lines, Some(0), 0.0, 60.0, 100.0, 1),
            GopherusPosition {
                selected: Some(1),
                scroll: 0.0,
            }
        );
    }

    #[test]
    fn gopherus_scroll_retargets_to_the_nearest_link_without_skipping() {
        let lines = vec![
            protocol_line(0.0, true),
            protocol_line(20.0, false),
            protocol_line(40.0, true),
            protocol_line(60.0, true),
            protocol_line(80.0, false),
        ];
        assert_eq!(
            gopherus_arrow(&lines, Some(0), 0.0, 60.0, 100.0, 1),
            GopherusPosition {
                selected: Some(2),
                scroll: 20.0,
            }
        );
        // At a pinned document edge, arrows walk the visible links instead.
        assert_eq!(
            gopherus_arrow(&lines[..3], Some(0), 0.0, 60.0, 60.0, 1).selected,
            Some(2)
        );
    }

    fn image_request(index: usize) -> trust::render::ImageRequest {
        let source = format!("https://images.example/{index}.png");
        trust::render::ImageRequest {
            handle: ImageHandle::for_source(&source),
            source,
        }
    }

    #[test]
    fn desktop_image_scheduler_bounds_deduplicates_and_resets_requests() {
        let page = url::Url::parse("https://example.com/gallery").unwrap();
        let mut scheduler = ImageLoadScheduler::default();
        assert!(scheduler.reset(7, &page));
        let first_epoch = scheduler.epoch();
        for index in 0..20 {
            scheduler.enqueue(image_request(index));
            scheduler.enqueue(image_request(index));
        }

        let first = scheduler.take_ready(IMAGE_FETCH_CONCURRENCY);
        assert_eq!(first.len(), IMAGE_FETCH_CONCURRENCY);
        assert_eq!(scheduler.pending.len(), 20, "queued and active are pending");
        assert!(scheduler.finish(first[0].handle, true));
        assert!(!scheduler.finish(first[0].handle, true));
        assert_eq!(scheduler.take_ready(1).len(), 1);
        scheduler.enqueue(first[0].clone());
        assert_eq!(
            scheduler.pending.len(),
            19,
            "available images are not restarted"
        );

        let next = url::Url::parse("https://example.com/next").unwrap();
        assert!(scheduler.reset(7, &next));
        assert_ne!(scheduler.epoch(), first_epoch);
        assert!(scheduler.pending.is_empty());
        assert!(scheduler.failed.is_empty());
        assert!(scheduler.completed.is_empty());
    }

    #[test]
    fn retiring_a_document_stops_requests_without_erasing_available_images() {
        let page = url::Url::parse("https://example.com/gallery").unwrap();
        let mut scheduler = ImageLoadScheduler::default();
        assert!(scheduler.reset(7, &page));
        let available = image_request(1);
        let pending = image_request(2);
        scheduler.mark_cached(available.handle);
        scheduler.enqueue(pending.clone());
        let active_epoch = scheduler.epoch();

        assert!(scheduler.retire());
        assert_ne!(scheduler.epoch(), active_epoch);
        assert!(scheduler.pending.is_empty());
        assert!(scheduler.completed.contains(&available.handle));
        scheduler.enqueue(pending);
        assert!(scheduler.take_ready(1).is_empty());
        assert!(
            !scheduler.reset(7, &page),
            "painting the frozen document must not reactivate its scheduler"
        );

        let next = url::Url::parse("https://example.com/next").unwrap();
        assert!(scheduler.reset(8, &next));
        scheduler.enqueue(image_request(3));
        assert_eq!(scheduler.take_ready(1).len(), 1);
    }

    #[test]
    fn cached_image_enters_available_state_only_once() {
        let page = url::Url::parse("https://example.com/").unwrap();
        let mut scheduler = ImageLoadScheduler::default();
        scheduler.reset(1, &page);
        let handle = image_request(1).handle;
        assert!(scheduler.mark_cached(handle));
        assert!(!scheduler.mark_cached(handle));
        scheduler.enqueue(image_request(1));
        assert!(scheduler.take_ready(1).is_empty());
    }

    #[test]
    fn pending_images_keep_replaced_geometry_until_available_or_broken() {
        let pending = image_request(1);
        let available = image_request(2);
        let broken = image_request(3);
        let mut decoded = trust::layout2::ImageSizes::new();
        decoded.insert(available.source.clone(), (640, 480));
        let failed = HashSet::from([broken.handle]);
        let sources = vec![
            pending.source.clone(),
            available.source.clone(),
            broken.source.clone(),
        ];

        let sizes = image_sizes_for_layout(&decoded, &failed, &sources);

        assert_eq!(sizes.get(&pending.source), Some(&PENDING_IMAGE_SIZE));
        assert_eq!(sizes.get(&available.source), Some(&(640, 480)));
        assert_eq!(sizes.get(&broken.source), None);
    }

    #[test]
    fn lazy_visibility_applies_nested_transforms_and_skips_far_images() {
        let near = image_request(1).handle;
        let far = image_request(2).handle;
        let commands = vec![
            trust::render::DisplayCommand::PushTransform(trust::render::Affine2d::translate(
                0.0, -800.0,
            )),
            trust::render::DisplayCommand::Image {
                rect: CssRect::new(0.0, 900.0, 100.0, 100.0),
                handle: near,
                source_rect: None,
                fit: trust::render::ImageFit::Contain,
                sampling: trust::render::ImageSampling::Smooth,
                clip: None,
                node: 1,
                link: None,
            },
            trust::render::DisplayCommand::PopTransform,
            trust::render::DisplayCommand::Image {
                rect: CssRect::new(0.0, 5_000.0, 100.0, 100.0),
                handle: far,
                source_rect: None,
                fit: trust::render::ImageFit::Contain,
                sampling: trust::render::ImageSampling::Smooth,
                clip: None,
                node: 2,
                link: None,
            },
        ];
        let mut visible = HashSet::new();
        collect_visible_image_handles(
            &commands,
            CssRect::new(0.0, 0.0, 800.0, 1_800.0),
            &mut visible,
        );
        assert!(visible.contains(&near));
        assert!(!visible.contains(&far));
    }
}
