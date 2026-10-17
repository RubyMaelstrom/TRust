//! The browser's windows. Each native window is one `DesktopWindow` with its
//! own tabs, and every tab in every window is one top-level traversable of
//! the user agent's top-level traversable set (HTML
//! #top-level-traversable-set). Ctrl+N creates a fresh top-level traversable
//! in a new window as Ctrl+T does in a new tab (HTML #nav-traversal-ui).
//! Windows share the process: its network runtime, page engines, caches and
//! site storage, so a window costs what a tab does plus its native surface
//! and renderer.
//!
//! `DesktopApp` owns the windows: it routes the event loop's events to
//! them, opens new ones (Ctrl+N, or a later launch of the browser that
//! `single_instance` handed over), quits only once every window's pages
//! allowed it, and ends the process when the last window closes.

use super::*;
use winit::event_loop::AsyncRequestSerial;
use winit::window::WindowAttributes;

/// How long Ctrl+N waits for the window manager's activation token before
/// opening its window without one.
const ACTIVATION_TOKEN_PATIENCE: Duration = Duration::from_millis(500);

/// A window's identity for the life of the process; never reused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct WindowKey(u64);

/// The event loop's events.
#[derive(Debug)]
pub(super) enum AppEvent {
    /// An event for one window.
    Window(WindowKey, DesktopEvent),
    /// The shared services (site storage notices) have news for the pages.
    ServicesWake,
    /// A video element's player showed a new frame (`media::player`).
    MediaFrame,
    /// An assistive technology's request to the window it names.
    Access(AccessEvent),
    /// A later launch of the browser handed its window to this process.
    Launch(NewWindow),
}

impl From<AccessEvent> for AppEvent {
    fn from(event: AccessEvent) -> Self {
        Self::Access(event)
    }
}

/// A window's handle on the event loop: the events it sends reach it.
#[derive(Clone)]
pub(super) struct WindowProxy {
    window: WindowKey,
    proxy: EventLoopProxy<AppEvent>,
}

impl WindowProxy {
    /// Send `event` to this window; false once the event loop has ended.
    pub(super) fn send_event(&self, event: DesktopEvent) -> bool {
        self.proxy
            .send_event(AppEvent::Window(self.window, event))
            .is_ok()
    }

    pub(super) fn window(&self) -> WindowKey {
        self.window
    }

    /// The event loop's own proxy, for services that address windows
    /// themselves (AccessKit names the window in its events).
    pub(super) fn event_loop(&self) -> &EventLoopProxy<AppEvent> {
        &self.proxy
    }

    /// A proxy for a window outside `DesktopApp` (tests).
    #[cfg(test)]
    pub(super) fn for_test(proxy: EventLoopProxy<AppEvent>) -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1 << 32);
        Self {
            window: WindowKey(NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)),
            proxy,
        }
    }
}

/// What a new window opens with.
#[derive(Debug, Default)]
pub(super) struct NewWindow {
    /// What its tab loads (an address or a local file's URL); without one it
    /// opens COMMAND for an address, as a new tab does.
    pub(super) address: Option<String>,
    /// The activation token of the user action that asked for it
    /// (xdg-activation-v1, X11 startup notification), with which the window
    /// manager gives the new window focus.
    pub(super) activation_token: Option<String>,
    /// The size of the window it was opened from, in logical pixels.
    pub(super) size: Option<(f64, f64)>,
}

/// What a window asks of the application, taken after each of its events.
pub(super) enum AppRequest {
    NewWindow(NewWindow),
    /// Quit the browser (Ctrl+Q): every window's pages are asked first.
    Quit,
}

pub(super) struct DesktopApp {
    runtime: Handle,
    proxy: EventLoopProxy<AppEvent>,
    renderer_preference: RendererPreference,
    windows: Vec<DesktopWindow>,
    next_window: u64,
    /// Quitting the browser: it ends once every window's pages allowed it,
    /// and a page that keeps the user keeps every window.
    quitting: bool,
    /// The event loop runs: a new window opens its native window at once.
    resumed: bool,
    /// Windows waiting for the event loop to open their native windows.
    opening: Vec<WindowKey>,
    /// The GPU device the windows' Hybrid renderers share.
    gpu: std::rc::Rc<std::cell::RefCell<Option<trust::render::vello_hybrid::HybridDevice>>>,
}

impl DesktopApp {
    pub(super) fn new(
        runtime: Handle,
        proxy: EventLoopProxy<AppEvent>,
        renderer_preference: RendererPreference,
        first: NewWindow,
    ) -> Self {
        let mut app = Self {
            runtime,
            proxy,
            renderer_preference,
            windows: Vec::new(),
            next_window: 0,
            quitting: false,
            resumed: false,
            opening: Vec::new(),
            gpu: Default::default(),
        };
        app.add_window(first);
        app
    }

    fn add_window(&mut self, request: NewWindow) -> WindowKey {
        self.next_window += 1;
        let proxy = WindowProxy {
            window: WindowKey(self.next_window),
            proxy: self.proxy.clone(),
        };
        let mut window = DesktopWindow::new(
            self.runtime.clone(),
            proxy,
            self.renderer_preference,
            request.address,
        );
        window.activation_token = request.activation_token;
        window.initial_size = request.size;
        window.shared_gpu = std::rc::Rc::clone(&self.gpu);
        let key = window.event_proxy.window();
        self.windows.push(window);
        key
    }

    /// Open a window for `request`; the event loop opens its native window
    /// (`settle`). A window asked for while the browser quits keeps it
    /// open: the user wants it.
    fn open_window(&mut self, request: NewWindow) {
        if self.quitting {
            self.cancel_quit();
        }
        let key = self.add_window(request);
        self.opening.push(key);
    }

    fn window_mut(&mut self, key: WindowKey) -> Option<&mut DesktopWindow> {
        self.windows
            .iter_mut()
            .find(|window| window.event_proxy.window() == key)
    }

    fn native_window_mut(&mut self, id: WindowId) -> Option<&mut DesktopWindow> {
        self.windows
            .iter_mut()
            .find(|window| window.window.as_ref().is_some_and(|w| w.id() == id))
    }

    /// Ctrl+Q: ask every window's pages whether the browser may quit.
    fn request_quit(&mut self) {
        self.quitting = true;
        for window in &mut self.windows {
            window.request_quit(true);
        }
    }

    fn cancel_quit(&mut self) {
        self.quitting = false;
        for window in &mut self.windows {
            if window.quitting_browser() {
                window.cancel_quit();
            }
        }
    }

    /// While the browser quits: once every window's pages allowed it, every
    /// window closes together; a window that stopped quitting (one of its
    /// pages kept the user) stops the others.
    fn service_quit(&mut self) {
        if !self.quitting {
            return;
        }
        if self
            .windows
            .iter()
            .any(|window| !window.closed && !window.quitting_browser())
        {
            self.cancel_quit();
        } else if self.windows.iter().all(DesktopWindow::quit_allowed) {
            for window in &mut self.windows {
                window.close_now();
            }
        }
    }

    /// After each event: carry out what the windows asked, open the new
    /// windows' native windows, drop the windows that closed, and end the
    /// process with the last.
    fn settle(&mut self, event_loop: &ActiveEventLoop) {
        self.service();
        if self.resumed {
            for key in std::mem::take(&mut self.opening) {
                if let Some(window) = self.window_mut(key) {
                    window.resumed(event_loop);
                }
            }
            // A window that could not open closed.
            self.service();
        }
        if self.windows.is_empty() {
            event_loop.exit();
        }
    }

    /// Carry out what the windows asked, keep a quit going, and drop the
    /// windows that closed.
    fn service(&mut self) {
        loop {
            let requests: Vec<AppRequest> = self
                .windows
                .iter_mut()
                .flat_map(|window| std::mem::take(&mut window.app_requests))
                .collect();
            if requests.is_empty() {
                break;
            }
            for request in requests {
                match request {
                    AppRequest::NewWindow(new) => self.open_window(new),
                    AppRequest::Quit => self.request_quit(),
                }
            }
        }
        self.service_quit();
        if self.windows.iter().any(|window| window.closed) {
            for window in self.windows.iter_mut().filter(|window| window.closed) {
                window.release_native_pointer_lock(false);
            }
            self.windows.retain(|window| !window.closed);
            if self.windows.is_empty() {
                self.quitting = false;
            }
        }
    }
}

#[cfg(test)]
impl DesktopApp {
    /// An application of `windows`, as a test configured them, with the
    /// event loop not yet running.
    pub(super) fn for_test(
        runtime: Handle,
        proxy: EventLoopProxy<AppEvent>,
        windows: Vec<DesktopWindow>,
    ) -> Self {
        Self {
            runtime,
            proxy,
            renderer_preference: RendererPreference::Cpu,
            windows,
            next_window: 0,
            quitting: false,
            resumed: false,
            opening: Vec::new(),
            gpu: Default::default(),
        }
    }

    pub(super) fn windows(&mut self) -> &mut [DesktopWindow] {
        &mut self.windows
    }

    pub(super) fn quitting(&self) -> bool {
        self.quitting
    }

    pub(super) fn serve(&mut self) {
        self.service();
    }
}

impl ApplicationHandler<AppEvent> for DesktopApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        self.resumed = true;
        self.opening.clear();
        for window in &mut self.windows {
            window.resumed(event_loop);
        }
        self.settle(event_loop);
    }

    fn suspended(&mut self, _event_loop: &ActiveEventLoop) {
        for window in &mut self.windows {
            window.suspended();
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let deadline = self
            .windows
            .iter_mut()
            .filter_map(DesktopWindow::about_to_wait)
            .min();
        self.settle(event_loop);
        event_loop.set_control_flow(match deadline {
            Some(at) => ControlFlow::WaitUntil(at),
            None => ControlFlow::Wait,
        });
    }

    fn device_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        _device: DeviceId,
        event: DeviceEvent,
    ) {
        // Raw pointer motion serves the window holding the pointer lock.
        for window in &mut self.windows {
            window.device_event(&event);
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: AppEvent) {
        match event {
            AppEvent::Window(key, event) => {
                if let Some(window) = self.window_mut(key) {
                    window.user_event(event);
                }
            }
            AppEvent::ServicesWake => {
                for window in &mut self.windows {
                    window.user_event(DesktopEvent::BrowserWake(None));
                }
            }
            AppEvent::MediaFrame => {
                MEDIA_FRAME_PENDING.store(false, std::sync::atomic::Ordering::Release);
                for window in &mut self.windows {
                    window.user_event(DesktopEvent::MediaFrame);
                }
            }
            AppEvent::Access(event) => {
                if let Some(window) = self.native_window_mut(event.window_id) {
                    window.user_event(DesktopEvent::Access(event));
                }
            }
            AppEvent::Launch(request) => self.open_window(request),
        }
        self.settle(event_loop);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        if let Some(window) = self.native_window_mut(id) {
            window.window_event(event_loop, event);
        }
        self.settle(event_loop);
    }
}

/// Free Unix desktops (Wayland, X11) hand focus to a new window with an
/// activation token; elsewhere a new window takes focus on its own.
#[cfg(all(unix, not(target_vendor = "apple"), not(target_os = "android")))]
mod activation {
    use super::*;
    use winit::platform::startup_notify::{
        WindowAttributesExtStartupNotify, WindowExtStartupNotify,
    };

    pub(super) fn request(window: &Window) -> Option<AsyncRequestSerial> {
        window.request_activation_token().ok()
    }

    pub(super) fn apply(attributes: WindowAttributes, token: String) -> WindowAttributes {
        attributes.with_activation_token(winit::window::ActivationToken::from_raw(token))
    }

    /// The token a launcher gave this process (`XDG_ACTIVATION_TOKEN` on
    /// Wayland, `DESKTOP_STARTUP_ID` on X11), removed so that the processes
    /// the browser starts do not inherit it (xdg-activation-v1: "The child
    /// process should unset the environment variable again right after
    /// reading it out").
    ///
    /// # Safety
    /// No other thread may read or write the environment meanwhile: call
    /// it first thing in `main`.
    pub(in super::super) unsafe fn take_from_environment() -> Option<String> {
        let wayland = std::env::var("XDG_ACTIVATION_TOKEN").ok();
        let x11 = std::env::var("DESKTOP_STARTUP_ID").ok();
        // SAFETY: the caller guarantees the process is single-threaded.
        unsafe {
            std::env::remove_var("XDG_ACTIVATION_TOKEN");
            std::env::remove_var("DESKTOP_STARTUP_ID");
        }
        wayland.or(x11).filter(|token| !token.is_empty())
    }
}

#[cfg(not(all(unix, not(target_vendor = "apple"), not(target_os = "android"))))]
mod activation {
    use super::*;

    pub(super) fn request(_window: &Window) -> Option<AsyncRequestSerial> {
        None
    }

    pub(super) fn apply(attributes: WindowAttributes, _token: String) -> WindowAttributes {
        attributes
    }

    /// # Safety
    /// Nothing to uphold where there are no activation tokens.
    pub(in super::super) unsafe fn take_from_environment() -> Option<String> {
        None
    }
}

pub(super) use activation::take_from_environment as take_activation_token;

impl DesktopWindow {
    /// Ctrl+N: a new window showing a fresh top-level traversable (HTML
    /// #nav-traversal-ui), the size of this one. Where the platform has
    /// activation tokens, this window asks for one first, so that the window
    /// manager focuses the new window as the user's action meant.
    pub(super) fn open_new_window(&mut self) {
        let Some(window) = &self.window else {
            self.app_requests
                .push(AppRequest::NewWindow(NewWindow::default()));
            return;
        };
        let size = window.inner_size().to_logical::<f64>(window.scale_factor());
        let size = Some((size.width, size.height));
        match activation::request(window) {
            Some(serial) => {
                self.pending_new_windows.push((serial, size));
                let proxy = self.event_proxy.clone();
                self.runtime.spawn(async move {
                    tokio::time::sleep(ACTIVATION_TOKEN_PATIENCE).await;
                    let _ = proxy.send_event(DesktopEvent::NewWindowDue(serial));
                });
            }
            None => self.app_requests.push(AppRequest::NewWindow(NewWindow {
                size,
                ..NewWindow::default()
            })),
        }
    }

    /// The window manager answered `open_new_window`'s request for a token.
    pub(super) fn activation_token_done(
        &mut self,
        serial: AsyncRequestSerial,
        token: winit::window::ActivationToken,
    ) {
        self.open_pending_new_window(serial, Some(token.into_raw()));
    }

    /// Open the window Ctrl+N asked for with `serial`, with the activation
    /// token if one came; nothing if it already opened.
    pub(super) fn open_pending_new_window(
        &mut self,
        serial: AsyncRequestSerial,
        activation_token: Option<String>,
    ) {
        let Some(index) = self
            .pending_new_windows
            .iter()
            .position(|(asked, _)| *asked == serial)
        else {
            return;
        };
        let (_, size) = self.pending_new_windows.remove(index);
        self.app_requests.push(AppRequest::NewWindow(NewWindow {
            address: None,
            activation_token,
            size,
        }));
    }

    /// The attributes of this window's native window.
    pub(super) fn native_window_attributes(&mut self) -> WindowAttributes {
        let (width, height) = self.initial_size.unwrap_or((960.0, 640.0));
        // The tab strip replaces the system title bar (`window_chrome`).
        let attributes = Window::default_attributes()
            .with_title("TRust")
            .with_decorations(false)
            .with_visible(false)
            .with_inner_size(LogicalSize::new(width, height))
            .with_min_inner_size(LogicalSize::new(480.0, 320.0));
        match self.activation_token.take() {
            Some(token) => activation::apply(attributes, token),
            None => attributes,
        }
    }
}
