//! Application state and the main event loop.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use url::Url;

use crossterm::event::{
    Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use futures::StreamExt;
use libmudtelnet::telnet::{op_command, op_option};
use ratatui::DefaultTerminal;
use tokio::sync::mpsc;
use tui_term::vt100;

use ratatui_image::picker::{Picker, ProtocolType};
use ratatui_image::protocol::Protocol;
use ratatui_image::sliced::SlicedProtocol;

use crate::cp437;
use crate::doc::{Doc, Link};
use crate::gemini::{self, GeminiUrl};
use crate::gopher::{self, GopherUrl};
use crate::http;
use crate::img;
use crate::oneshot;
use crate::telnet;
use crate::tls;
use crate::ui;

const RESIZE_WRAP_DEBOUNCE: Duration = Duration::from_millis(200);

/// What the input field feeds, mirroring GNU telnet's two states: lines go
/// to the remote host, or to the `telnet>` command prompt reached with Ctrl-].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Session,
    Command,
    /// Query entry for a gopher type-7 search, a gemini 1x input, or an
    /// HTML form field (the target lives in `search_target`).
    Search,
    /// In-page find (Ctrl-F) over the open browser doc. The query lives in
    /// `input`; matches/highlights live in `find`.
    Find,
}

/// Manual override for the line/char decision (GNU telnet's `mode` command).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum InputMode {
    Character,
    Line,
}

/// How inbound bytes are interpreted before terminal emulation.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    Utf8,
    Cp437,
}

/// Counts BEL requests from the emulator so the app can ring the real
/// terminal's bell.
#[derive(Default)]
pub struct VtCallbacks {
    bells: usize,
}

impl vt100::Callbacks for VtCallbacks {
    fn audible_bell(&mut self, _: &mut vt100::Screen) {
        self.bells += 1;
    }
}

type Vt = vt100::Parser<VtCallbacks>;

fn new_vt(rows: u16, cols: u16) -> Vt {
    vt100::Parser::new_with_callbacks(rows, cols, SCROLLBACK_LINES, VtCallbacks::default())
}

const HISTORY_CAP: usize = 500;

/// Lines of session scrollback kept in memory.
const SCROLLBACK_LINES: usize = 10_000;

/// In-memory entry history for the input field (Up/Down recall). Never
/// persisted — it lives and dies with the process.
#[derive(Default)]
struct History {
    entries: Vec<String>,
    /// Index into `entries` while browsing; None when editing a fresh line.
    nav: Option<usize>,
    /// The unfinished line stashed when browsing starts, restored when the
    /// user arrows back past the newest entry.
    draft: String,
}

impl History {
    fn push(&mut self, line: &str) {
        self.nav = None;
        if line.is_empty() || self.entries.last().is_some_and(|last| last == line) {
            return;
        }
        if self.entries.len() == HISTORY_CAP {
            self.entries.remove(0);
        }
        self.entries.push(line.to_string());
    }

    /// Step to an older entry, stashing the in-progress line first.
    fn up(&mut self, current: &str) -> Option<String> {
        let i = match self.nav {
            None if !self.entries.is_empty() => {
                self.draft = current.to_string();
                self.entries.len() - 1
            }
            Some(i) if i > 0 => i - 1,
            _ => return None,
        };
        self.nav = Some(i);
        Some(self.entries[i].clone())
    }

    /// Step to a newer entry, or back to the stashed draft past the end.
    fn down(&mut self) -> Option<String> {
        match self.nav {
            Some(i) if i + 1 < self.entries.len() => {
                self.nav = Some(i + 1);
                Some(self.entries[i + 1].clone())
            }
            Some(_) => {
                self.nav = None;
                Some(std::mem::take(&mut self.draft))
            }
            None => None,
        }
    }

    /// Editing a recalled entry detaches it from the browse position.
    fn detach(&mut self) {
        self.nav = None;
    }
}

/// Terminal queries a server may send to probe the "terminal" — which is
/// our embedded emulator, so we must answer where a real terminal would.
/// BBS ANSI detection hinges on these.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Probe {
    /// `ESC[6n` — Device Status Report asking for a Cursor Position Report.
    CursorPosition,
    /// `ESC[5n` — Device Status Report asking if the terminal is OK.
    Status,
    /// `ESC[c` / `ESC[0c` — Primary Device Attributes.
    DeviceAttributes,
}

/// Scans the inbound byte stream for terminal queries. Keeps its state
/// across calls so probes split between TCP segments still match.
#[derive(Default)]
struct ProbeDetector {
    state: ProbeState,
}

#[derive(Default, Clone, PartialEq, Eq)]
enum ProbeState {
    #[default]
    Ground,
    Escape,
    /// Inside a CSI sequence, accumulating parameter bytes.
    Csi(Vec<u8>),
}

impl ProbeDetector {
    fn feed(&mut self, data: &[u8]) -> Vec<Probe> {
        let mut found = Vec::new();
        for &byte in data {
            self.state = match std::mem::take(&mut self.state) {
                ProbeState::Ground => match byte {
                    0x1b => ProbeState::Escape,
                    _ => ProbeState::Ground,
                },
                ProbeState::Escape => match byte {
                    b'[' => ProbeState::Csi(Vec::new()),
                    0x1b => ProbeState::Escape,
                    _ => ProbeState::Ground,
                },
                ProbeState::Csi(mut params) => match byte {
                    // Parameter bytes; cap length so binary noise can't grow it.
                    0x30..=0x3f if params.len() < 8 => {
                        params.push(byte);
                        ProbeState::Csi(params)
                    }
                    // Final byte ends the sequence.
                    0x40..=0x7e => {
                        match (byte, params.as_slice()) {
                            (b'n', b"6") => found.push(Probe::CursorPosition),
                            (b'n', b"5" | b"") => found.push(Probe::Status),
                            (b'c', b"" | b"0") => found.push(Probe::DeviceAttributes),
                            _ => {}
                        }
                        ProbeState::Ground
                    }
                    0x1b => ProbeState::Escape,
                    _ => ProbeState::Ground,
                },
            };
        }
        found
    }
}

/// The gopherus-style browser state (shared by gopher and gemini): a
/// scrolling viewport with a link cursor constrained to it, and an
/// in-RAM back history.
pub struct BrowserView {
    pub doc: Doc,
    /// The selected link's line index (gopher/gemini line model). Always
    /// a visible link when one is on screen; None while no link is in the
    /// viewport.
    pub selected: Option<usize>,
    /// The selected item, as `(row, item)`, on an HTTP laid-out doc. The
    /// HTTP path uses this instead of `selected`; the two are never both
    /// active (a doc is either laid out or not).
    pub sel_item: Option<(usize, usize)>,
    /// First visible line/row.
    pub scroll: usize,
    history: Vec<(Doc, ViewPos, usize)>,
}

/// An open `<select>` dropdown overlay. It anchors to the field's document
/// position and lists the options for ↑↓/click selection — the real-browser
/// dropdown, replacing the old Enter-cycles-one-option behavior.
pub struct SelectMenu {
    /// The `(form, field)` the chosen value is written back to.
    pub form: usize,
    pub field: usize,
    /// `(label, value)` options, in document order.
    pub options: Vec<(String, String)>,
    /// The highlighted option (committed on Enter/click).
    pub highlight: usize,
    /// First option row shown — kept in sync with `highlight` by the
    /// renderer, and read by the mouse hit-test.
    pub scroll: usize,
    /// The field's document row/column, to place the popup beside it.
    pub anchor_row: usize,
    pub anchor_col: u16,
}

/// In-page find state (Ctrl-F). The query text itself lives in `App.input`
/// (reusing the line editor); this holds the located matches and which one
/// is active. Browser-only — telnet sessions never enter find.
#[derive(Default)]
pub struct FindState {
    /// The lowercased query `matches` was computed for, so a keystroke that
    /// doesn't change the query (a cursor move) skips the rescan and keeps
    /// the active match put.
    last_query: String,
    /// Every match, in document order.
    pub matches: Vec<FindMatch>,
    /// Index into `matches` of the active match — the one scrolled to and
    /// drawn reversed. None when there are no matches.
    pub current: Option<usize>,
}

/// One find match: a char range within a doc line (gopher/gemini line model)
/// or within a row's item (HTTP 2D layout model).
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct FindMatch {
    /// Row index (HTTP) or line index (gopher/gemini) the match sits on.
    pub line: usize,
    /// Item index within the row for the HTTP model; None for the line model.
    pub item: Option<usize>,
    /// Char offsets of the match within the item/line text.
    pub start: usize,
    pub end: usize,
}

/// A saved selection across history pops — whichever model the doc used.
#[derive(Clone, Copy, Default)]
struct ViewPos {
    selected: Option<usize>,
    sel_item: Option<(usize, usize)>,
    /// The page had a live JS engine when we navigated away. Going back
    /// to it revives the page (re-runs JS) instead of restoring a frozen
    /// snapshot whose script links/forms are dead.
    was_live: bool,
}

/// What a background fetch produced, by protocol.
enum Payload {
    Gopher(Vec<u8>),
    Gemini(gemini::Response),
    Http(http::Response),
    OneShot(Vec<u8>),
}

/// Result of a background fetch.
struct FetchMsg {
    target: Link,
    result: Result<Payload, String>,
}

/// A fetched image shown full-panel over the browser. The raw bytes
/// stay in RAM (like everything else) so panel resizes and protocol
/// switches can re-encode without refetching; the Arc keeps those
/// re-encode round-trips from copying megabytes.
pub struct ImageView {
    /// Where the image came from (title bar).
    pub url: Link,
    raw: std::sync::Arc<[u8]>,
    /// "W×H mime", for the status bar.
    pub info: String,
    /// Encoded for the panel size in `encoded_for`.
    pub protocol: Protocol,
    /// Panel size the protocol was encoded for; a mismatch with the
    /// current size triggers a background re-encode.
    encoded_for: (u16, u16),
}

/// Result of a background image decode + encode.
struct ImgMsg {
    url: Link,
    raw: std::sync::Arc<[u8]>,
    size: (u16, u16),
    result: Result<(Protocol, String), String>,
}

/// A page image decoded once and kept in RAM: the raw bytes (for the
/// stateless encode), its pixel size, and the cell box chosen decode-first
/// from the terminal's font aspect. Layout reads `cell`; the renderer
/// encodes `raw` to a `Protocol` for that box.
#[derive(Clone)]
struct DecodedImage {
    raw: std::sync::Arc<[u8]>,
    cell: (u16, u16),
}

/// One page image finished the parallel fetch+decode pipeline.
struct ImgLoadMsg {
    url: String,
    decoded: Option<DecodedImage>,
}

/// Cache/dedup key for an encoded inline image: URL, cell box `(w, h)`, and
/// whether it was `object-fit: cover`-cropped (the same image+box can render
/// Fit or Crop). The key is SCROLL-INDEPENDENT: each box encodes ONCE to a
/// `SlicedProtocol` that the renderer (`ui::render_inline_images`) clips to any
/// vertical slice at draw time, so scrolling a tall image never re-encodes it.
#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) struct EncKey {
    pub(crate) url: String,
    pub(crate) w: u16,
    pub(crate) h: u16,
    pub(crate) crop: bool,
}

impl EncKey {
    /// The cache key for an image item's box. Shared by the encode pass and the
    /// renderer so they key (and so find) the same encoded protocol.
    pub(crate) fn for_item(url: &str, item: &crate::layout::Item) -> Self {
        EncKey {
            url: url.to_string(),
            w: item.width,
            h: item.height,
            crop: item.crop,
        }
    }
}

/// One inline-image box finished encoding to a sliced terminal protocol.
struct EncMsg {
    key: EncKey,
    protocol: Option<SlicedProtocol>,
}

/// Cap a decoded image's natural cell box (never upscales; preserves
/// aspect). Layout clamps width further to the content width.
const IMG_MAX_CELLS: (f32, f32) = (80.0, 24.0);

thread_local! {
    /// Set TRUE on exactly one thread: the `#[tokio::main]` driver thread
    /// that owns the live terminal and polls the run loop (set in `main`,
    /// right after `ratatui::init`). The global panic hook reads it as an
    /// ALLOWLIST: it restores the terminal (`ratatui::restore()`) ONLY for a
    /// panic on the owner thread — a genuine render/run-loop panic, where we
    /// want the alt screen torn down so the backtrace is readable.
    ///
    /// EVERY other thread — tokio workers (the fetch + image-load tasks),
    /// the blocking image decode/encode pool, the `trust-*` JS workers — is
    /// already sandboxed by `catch_unwind`/tokio upstream, so its panic costs
    /// one operation and is swallowed. Restoring the terminal from one of
    /// THOSE threads was the bug: it drops the alt screen and disables raw
    /// mode (via `disable_raw_mode`) out from under the still-running run
    /// loop — the page keeps working, but the render is corrupt and the mouse
    /// SGR stream leaks as text. An allowlist (own-thread-only) makes that
    /// impossible regardless of which background op panics, where a denylist
    /// (skip the threads we happened to think of) leaked any we missed.
    ///
    /// Safe because the `#[tokio::main]` `block_on` root future — our run
    /// loop — never migrates off its thread, even multi-threaded with
    /// spawned tasks (verified). So the owner flag set at run-loop entry
    /// stays valid for every draw.
    pub(crate) static TERMINAL_OWNER: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub struct App {
    pub mode: Mode,
    /// Terminal emulation of the remote byte stream, rendered by tui-term.
    pub vt: Vt,
    /// Inbound byte interpretation (`set encoding cp437` for BBS art).
    pub encoding: Encoding,
    /// Run page JavaScript (budgeted) on the web. ON by default (her
    /// call, 2026-06-12 — the engine only spins up for pages that
    /// actually carry scripts); `set js off` opts out.
    js_enabled: bool,
    /// Session-lifetime web storage for page JS (RAM-only, origin-keyed).
    web_storage: crate::js::WebStorage,
    /// The living page behind the current browser doc, if its JS left
    /// anything to interact with. ONE live engine, ever.
    live_page: Option<crate::js::PageHandle>,
    page_rx: Option<mpsc::Receiver<crate::js::PageEvt>>,
    /// A page-script dispatch is in flight (drives the loading heart).
    page_busy: bool,
    /// Static form submit to perform if the live page does not prevent
    /// the submitted form's default action.
    pending_live_submit: Option<(usize, usize)>,
    /// Cumulative page-JS errors, for the status badge.
    page_js_errors: usize,
    /// The next fetched document replaces the current one instead of
    /// pushing history (`reload`).
    replace_nav: bool,
    /// GNU telnet's `crlf` toggle: Enter sends CR LF when true, CR NUL
    /// when false (char mode only; line mode always sends CR LF).
    crlf: bool,
    /// Bell count already forwarded to the real terminal.
    bells_seen: usize,
    /// Options the server has enabled on its side (WILL ...).
    remote_opts: HashSet<u8>,
    /// Options the server has enabled on our side (DO ...).
    local_opts: HashSet<u8>,
    /// LINEMODE (RFC 1184) is in effect on this connection (we WILL'd it).
    linemode_active: bool,
    /// LINEMODE EDIT bit: true = the server wants local line editing, false
    /// = character-at-a-time. Only meaningful while `linemode_active`.
    linemode_edit: bool,
    /// The local-echo entry field at the bottom of the screen.
    pub input: String,
    /// Cursor position in `input`, counted in chars.
    pub cursor: usize,
    /// Selection anchor (char index) while Shift+movement extends a
    /// selection in the input field; None when nothing is selected.
    pub select_anchor: Option<usize>,
    /// Animation frame for the fetch-in-flight pulse, advanced by the
    /// run loop's ticker while a fetch is pending.
    pub spinner: usize,
    /// Last drawn browser/session content rect, in terminal coordinates.
    pub(crate) last_content_area: ratatui::layout::Rect,
    /// Screen row of the bottom status line as of the last draw, so a click
    /// on it (or the top address bar) can open the command console.
    pub(crate) last_status_row: u16,
    pub host: Option<String>,
    pub port: u16,
    /// Port explicitly given at startup (CLI), if any. `None` means the
    /// startup target had no port → it opens as web (https, falling back to
    /// http), the way a bare host typed in the command console does.
    pub(crate) start_port: Option<u16>,
    pub connected: bool,
    /// Whether the live connection is wrapped in TLS.
    pub tls: bool,
    /// Connection state / last result, shown in the status bar.
    pub status: String,
    /// Inner size of the session widget as of the last draw (cols, rows).
    pub last_inner: (u16, u16),
    /// `mode character` / `mode line` override; None means follow ECHO.
    mode_override: Option<InputMode>,
    /// Up/Down recall for lines sent to the remote host.
    session_history: History,
    /// Up/Down recall for `trust>` commands, kept separate so MUD spam
    /// doesn't bury `open`/`mode` invocations.
    command_history: History,
    /// Watches inbound data for terminal queries we must answer.
    probes: ProbeDetector,
    /// When Some, the browser (gopher/gemini) replaces the terminal panel.
    pub browser: Option<BrowserView>,
    /// When Some, the image viewer sits over the browser (or terminal).
    pub viewer: Option<ImageView>,
    /// Terminal graphics: font size + protocol, queried at startup.
    pub picker: Picker,
    /// What the startup query found, restored by `set image auto`.
    auto_protocol: ProtocolType,
    /// In-flight fetch, if any.
    fetch_rx: Option<mpsc::Receiver<FetchMsg>>,
    /// Handle to the in-flight fetch task, so Esc can abort it.
    fetch_task: Option<tokio::task::JoinHandle<()>>,
    /// In-flight image decode/encode, if any.
    img_rx: Option<mpsc::Receiver<ImgMsg>>,
    /// Decoded page images (inline `<img>`), keyed by absolute URL.
    /// RAM-only, session-lifetime; survives re-layout/resize so a resize
    /// never refetches. Built by the parallel pipeline.
    image_cache: HashMap<String, DecodedImage>,
    /// In-flight parallel page-image fetch+decode batch.
    imgs_rx: Option<mpsc::Receiver<ImgLoadMsg>>,
    /// Encoded inline-image protocols, keyed by `(url, cell_w, cell_h, crop)`.
    /// Each is a `SlicedProtocol` (encoded once for the whole box; the renderer
    /// clips it to any vertical slice), so scroll never re-encodes. Bounded to
    /// the on-screen set by `sync_image_encodes` (entries scrolled away evict).
    pub(crate) image_protocols: HashMap<EncKey, SlicedProtocol>,
    /// Boxes currently being encoded (one async encode per key in flight).
    /// Non-empty drives the loading pulse (the channel is persistent so it
    /// can't gate the pulse itself).
    image_encoding: HashSet<EncKey>,
    /// Persistent channel for finished inline-image encodes.
    enc_tx: mpsc::Sender<EncMsg>,
    enc_rx: mpsc::Receiver<EncMsg>,
    /// The gopher type-7 item, gemini 1x URL, or form field awaiting
    /// input (pub so the UI can label the prompt accordingly).
    pub(crate) search_target: Option<Link>,
    /// A capsule that answered status 60: the prompt is asking for a
    /// name to mint a client identity under.
    pub(crate) cert_for: Option<GeminiUrl>,
    /// An open `<select>` dropdown, modal over the browser: it captures
    /// keys/mouse until the user picks an option or cancels.
    pub(crate) select_menu: Option<SelectMenu>,
    /// In-page find (Ctrl-F) state while `mode == Mode::Find`; None otherwise.
    pub(crate) find: Option<FindState>,
    /// The popup's last drawn screen rect (set by the renderer) so a mouse
    /// click can hit-test against the option rows.
    pub(crate) last_select_rect: Option<ratatui::layout::Rect>,
    /// A fetch just failed (or came back empty): the status bar shows
    /// the message even while a link is selected, until the next key.
    pub(crate) notice: bool,
    conn: Option<telnet::Handle>,
    events: Option<mpsc::Receiver<telnet::Event>>,
    quit: bool,
}

impl App {
    pub fn new(host: Option<String>, port: u16) -> Self {
        // Like GNU telnet: no host argument drops you at the command prompt.
        let mode = match host {
            Some(_) => Mode::Session,
            None => Mode::Command,
        };
        let (enc_tx, enc_rx) = mpsc::channel(64);
        Self {
            mode,
            // In memory only, like the entry histories.
            vt: new_vt(24, 80),
            encoding: Encoding::Utf8,
            js_enabled: true,
            web_storage: Default::default(),
            live_page: None,
            page_rx: None,
            page_busy: false,
            pending_live_submit: None,
            page_js_errors: 0,
            replace_nav: false,
            crlf: false,
            bells_seen: 0,
            remote_opts: HashSet::new(),
            local_opts: HashSet::new(),
            linemode_active: false,
            linemode_edit: false,
            input: String::new(),
            cursor: 0,
            select_anchor: None,
            spinner: 0,
            last_content_area: ratatui::layout::Rect::new(0, 0, 80, 24),
            last_status_row: 23,
            host,
            port,
            start_port: None,
            connected: false,
            tls: false,
            status: String::from("No connection. Ctrl-] for commands."),
            last_inner: (80, 24),
            mode_override: None,
            session_history: History::default(),
            command_history: History::default(),
            probes: ProbeDetector::default(),
            browser: None,
            viewer: None,
            // Tests and pre-query startup get the universal fallback;
            // main installs the queried picker via set_picker.
            picker: Picker::halfblocks(),
            auto_protocol: ProtocolType::Halfblocks,
            fetch_rx: None,
            fetch_task: None,
            img_rx: None,
            image_cache: HashMap::new(),
            imgs_rx: None,
            image_protocols: HashMap::new(),
            image_encoding: HashSet::new(),
            enc_tx,
            enc_rx,
            search_target: None,
            cert_for: None,
            select_menu: None,
            find: None,
            last_select_rect: None,
            notice: false,
            conn: None,
            events: None,
            quit: false,
        }
    }

    /// Install the terminal-queried graphics picker (once, at startup).
    pub fn set_picker(&mut self, picker: Picker) {
        self.auto_protocol = picker.protocol_type();
        self.picker = picker;
    }

    /// A fetch or image encode is in flight (drives the loading pulse).
    pub fn loading(&self) -> bool {
        self.fetch_rx.is_some()
            || self.img_rx.is_some()
            || self.imgs_rx.is_some()
            || !self.image_encoding.is_empty()
            || self.page_busy
    }

    pub async fn run(mut self, mut terminal: DefaultTerminal) -> std::io::Result<()> {
        // (Terminal ownership for the panic hook is claimed in `main`, on this
        // same `block_on` thread — see `TERMINAL_OWNER`.)
        // Terminal input runs on its own reader thread feeding an mpsc channel
        // (the same shape as the telnet/fetch/image channels), NOT crossterm's
        // async `EventStream`. The reason is the coalescing drain below: a burst
        // of scroll/key events must be applied with ONE redraw, which means
        // peeking for already-queued events. The async stream can't be peeked
        // safely — its `poll_next` registers a single wake-task keyed on the
        // first waker to poll it and won't re-register while that task is live,
        // so polling it with a throwaway waker (`now_or_never`) strands the real
        // `select!` waker and deadlocks input. A channel drains with `try_recv`
        // with no such hazard. (crossterm's own `EventStream` uses an identical
        // background-reader thread internally; we just own the channel.)
        let (input_tx, mut input_rx) = mpsc::unbounded_channel::<TermEvent>();
        std::thread::Builder::new()
            .name("trust-input".into())
            .spawn(move || {
                // Blocking reads off the (already mouse-captured, query-drained)
                // stdin; a closed receiver (app quit) or a read error ends it.
                while let Ok(event) = crossterm::event::read() {
                    if input_tx.send(event).is_err() {
                        break;
                    }
                }
            })
            .expect("spawn terminal input reader");
        let mut pending_wrap_target: Option<(usize, bool)> = None;
        let mut wrap_sleep: Option<std::pin::Pin<Box<tokio::time::Sleep>>> = None;
        // Tracks the `<select>` dropdown across frames. A sixel image is one
        // escape sequence anchored at its top-left cell; a popup covering the
        // middle of an image leaves that anchor cell unchanged, so ratatui's
        // cell diff never re-emits the sequence when the popup closes. Force a
        // full repaint on close (only when images are present) so the pixels
        // behind the dropdown come back.
        let mut menu_was_open = false;
        // The run loop only redraws on events; this ticker animates the
        // loading pulse while a fetch is pending (and is disabled, via
        // the select guard, the rest of the time).
        let mut tick = tokio::time::interval(std::time::Duration::from_millis(120));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // Draw once before connecting so the first NAWS negotiation reports
        // the widget's real size instead of the 80x24 default.
        terminal.draw(|frame| ui::draw(frame, &mut self))?;
        self.sync_vt_size().await;
        if let Some(host) = self.host.clone() {
            self.dispatch_open(&host, self.start_port);
        }

        while !self.quit {
            let menu_open = self.select_menu.is_some();
            if menu_was_open && !menu_open && !self.image_protocols.is_empty() {
                terminal.clear()?;
            }
            menu_was_open = menu_open;
            terminal.draw(|frame| ui::draw(frame, &mut self))?;
            self.sync_vt_size().await;
            match self.pending_browser_wrap_target() {
                Some(target) if pending_wrap_target != Some(target) => {
                    pending_wrap_target = Some(target);
                    wrap_sleep = Some(Box::pin(tokio::time::sleep(RESIZE_WRAP_DEBOUNCE)));
                }
                Some(_) => {}
                None => {
                    pending_wrap_target = None;
                    wrap_sleep = None;
                }
            }
            self.sync_viewer_size();
            self.sync_image_encodes();

            tokio::select! {
                event = input_rx.recv() => match event {
                    Some(event) => self.on_terminal_event(event).await,
                    None => break, // the input reader thread ended
                },
                event = recv_opt(&mut self.events) => match event {
                    Some(event) => self.on_telnet_event(event).await,
                    None => self.events = None,
                },
                msg = recv_opt(&mut self.fetch_rx) => match msg {
                    Some(msg) => self.on_fetch(msg),
                    None => self.fetch_rx = None,
                },
                msg = recv_opt(&mut self.img_rx) => match msg {
                    Some(msg) => self.on_img(msg),
                    None => self.img_rx = None,
                },
                msg = recv_opt(&mut self.imgs_rx) => match msg {
                    Some(msg) => self.on_img_load(msg),
                    None => self.imgs_rx = None,
                },
                Some(msg) = self.enc_rx.recv() => self.on_enc(msg),
                evt = recv_opt(&mut self.page_rx) => match evt {
                    Some(evt) => self.on_page_evt(evt),
                    None => {
                        // The actor is gone; the last render stands.
                        self.drop_live_page();
                    }
                },
                _ = async {
                    if let Some(sleep) = wrap_sleep.as_mut() {
                        sleep.as_mut().await;
                    }
                }, if wrap_sleep.is_some() => {
                    pending_wrap_target = None;
                    wrap_sleep = None;
                    self.sync_browser_wrap();
                },
                _ = tick.tick(), if self.loading() => {
                    self.spinner = self.spinner.wrapping_add(1);
                },
            }

            // Coalesce input bursts into one redraw. A held arrow/wheel or key
            // autorepeat queues many scroll/key events; the `select!` above
            // takes ONE per loop and then redraws, and on an image-heavy HTTP
            // page each redraw emits a large sixel payload — so the backlog
            // drains slower than it fills and the view "slideshows" while input
            // lags behind. Drain every terminal event that is ALREADY queued and
            // apply it now, so the whole burst collapses into the single redraw
            // at the top of the next iteration. Self-adapting: `try_recv` finds
            // nothing when idle (idle stays idle, no busy-poll) and cheap
            // text/vt100 redraws never build a backlog, so only the slow sixel
            // pages actually coalesce. The cap guarantees we return to a draw
            // even under a continuous flood (e.g. mouse-move spam during a drag).
            let mut drained = 0;
            while !self.quit && drained < 512 {
                match input_rx.try_recv() {
                    Ok(ev) => {
                        self.on_terminal_event(ev).await;
                        drained += 1;
                    }
                    Err(_) => break, // Empty (nothing queued) or Disconnected
                }
            }
        }
        Ok(())
    }

    /// Resize the emulated screen (and renegotiate NAWS) when the widget's
    /// inner area changed during the last draw.
    async fn sync_vt_size(&mut self) {
        let (cols, rows) = self.last_inner;
        let (cur_rows, cur_cols) = self.vt.screen().size();
        if (cur_cols, cur_rows) != (cols, rows) && cols > 0 && rows > 0 {
            self.vt.screen_mut().set_size(rows, cols);
            if let Some(conn) = &self.conn {
                let _ = conn
                    .commands
                    .send(telnet::Command::Resize { cols, rows })
                    .await;
            }
        }
    }

    fn on_mouse_event(&mut self, mouse: MouseEvent) {
        // A left-click on the top address bar or the bottom status line opens
        // the command console — a discoverable alternative to Tab/Ctrl-], in
        // EVERY mode and view (handled before the viewer/dropdown/browser
        // grabs below, since the chrome rows never overlap them).
        if self.is_chrome_click(&mouse) {
            self.open_command();
            return;
        }
        if self.viewer.is_some() {
            return; // nothing to scroll in the image viewer
        }
        // An open <select> dropdown grabs the mouse: wheel scrolls the
        // options, a click inside picks one, a click outside cancels.
        if self.select_menu.is_some() {
            self.select_menu_mouse(mouse);
            return;
        }
        // 3 lines per wheel click, matching terminal convention.
        match (mouse.kind, self.browser.is_some()) {
            (MouseEventKind::ScrollUp, false) => self.scroll_by(3),
            (MouseEventKind::ScrollDown, false) => self.scroll_by(-3),
            (MouseEventKind::ScrollUp, true) => self.browser_scroll(-3, true),
            (MouseEventKind::ScrollDown, true) => self.browser_scroll(3, true),
            (MouseEventKind::Down(MouseButton::Mouse4), true) if self.mode == Mode::Session => {
                self.browser_back();
            }
            (MouseEventKind::Moved, true) if self.mode == Mode::Session => {
                let on_link = self.browser_mouse_hover(mouse.column, mouse.row);
                // Hovering onto a link supersedes a sticky `notice` (the
                // mpv-launch confirmation, a fetch error) so the bar shows the
                // link preview again instead of pinning the old message.
                self.notice = self.notice && !on_link;
            }
            (MouseEventKind::Down(MouseButton::Left), true)
                if self.mode == Mode::Session
                    && self.browser_mouse_hover(mouse.column, mouse.row) =>
            {
                self.browser_follow();
            }
            _ => {}
        }
    }

    /// A left-click on the top address bar (the bordered title row) or the
    /// bottom status line. Active in every mode (the chrome is always drawn).
    fn is_chrome_click(&self, mouse: &MouseEvent) -> bool {
        if !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            return false;
        }
        let title_row = self.last_content_area.y.saturating_sub(1);
        mouse.row == title_row || mouse.row == self.last_status_row
    }

    /// Open the command console (the same state Tab/Ctrl-] enters).
    fn open_command(&mut self) {
        self.mode = Mode::Command;
        self.cert_for = None;
        self.select_menu = None;
    }

    /// Mouse hover/click target in the browser, dispatching by layout model:
    /// HTTP laid-out docs use the 2D item hit-test, gopher/gemini the
    /// line-based one. Returns whether an interactive target was hit.
    fn browser_mouse_hover(&mut self, col: u16, row: u16) -> bool {
        match self.browser.as_ref() {
            Some(g) if g.doc.laid_out() => self.http_mouse_hover(col, row),
            Some(_) => self.gopher_mouse_hover(col, row),
            None => false,
        }
    }

    /// gopher/gemini hover: select the link line under the cursor (sticky —
    /// hovering off a link leaves the highlight where it is, matching the
    /// gopherus keyboard/scroll model). Returns whether a link was hit.
    fn gopher_mouse_hover(&mut self, col: u16, row: u16) -> bool {
        let Some(idx) = self.gopher_hit_test(col, row) else {
            return false;
        };
        if let Some(g) = &mut self.browser {
            g.selected = Some(idx);
        }
        true
    }

    /// The gopher/gemini document line under the cursor, if it carries a link.
    fn gopher_hit_test(&self, col: u16, row: u16) -> Option<usize> {
        if !self.mouse_in_content_area(col, row) {
            return None;
        }
        let g = self.browser.as_ref()?;
        if g.doc.laid_out() {
            return None;
        }
        let local_row = row.saturating_sub(self.last_content_area.y) as usize;
        let line_idx = g.scroll + local_row;
        g.doc
            .lines
            .get(line_idx)
            .filter(|l| l.link.is_some())
            .map(|_| line_idx)
    }

    async fn on_terminal_event(&mut self, event: TermEvent) {
        let key = match event {
            TermEvent::Key(key) => key,
            TermEvent::Mouse(mouse) => {
                self.on_mouse_event(mouse);
                return;
            }
            _ => return,
        };
        if key.kind == KeyEventKind::Release {
            return;
        }

        // Ctrl-] — GNU telnet's escape character — toggles command mode.
        // Legacy terminal input delivers it as raw byte 0x1D, which
        // crossterm reports as Ctrl-5; the kitty protocol reports Ctrl-].
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char(']') | KeyCode::Char('5'))
        {
            // Ctrl-] from find dismisses the find box (clearing its query)
            // on the way to the command console.
            if self.mode == Mode::Find {
                self.input.clear();
                self.cursor = 0;
            }
            self.find = None;
            self.mode = match self.mode {
                Mode::Session | Mode::Search | Mode::Find => Mode::Command,
                Mode::Command => Mode::Session,
            };
            self.cert_for = None;
            self.select_menu = None;
            return;
        }

        // Tab opens the command console — a friendlier alias for Ctrl-].
        // In character-at-a-time sessions Tab is a real keystroke the remote
        // needs (menus/forms), so it's only intercepted when keys aren't
        // already going straight to the remote.
        if key.code == KeyCode::Tab
            && key.modifiers.is_empty()
            && !(self.mode == Mode::Session && self.char_mode())
        {
            if self.mode == Mode::Find {
                self.input.clear();
                self.cursor = 0;
            }
            self.find = None;
            self.mode = match self.mode {
                Mode::Session | Mode::Search | Mode::Find => Mode::Command,
                Mode::Command => Mode::Session,
            };
            self.cert_for = None;
            self.select_menu = None;
            return;
        }

        // The image viewer sits over the browser: any of the "back"
        // keys close it, returning to the page (or terminal) beneath.
        if self.mode == Mode::Session && self.viewer.is_some() {
            self.notice = false;
            if matches!(
                key.code,
                KeyCode::Esc | KeyCode::Left | KeyCode::Backspace | KeyCode::Char('q')
            ) {
                self.viewer = None;
                self.status = String::from("Image closed.");
            }
            return;
        }

        // An open <select> dropdown is modal over the browser: it captures
        // keys until the user picks an option or cancels.
        if self.mode == Mode::Session && self.select_menu.is_some() {
            self.select_menu_nav(key);
            return;
        }

        // Ctrl-F opens in-page find over a browser doc. In a telnet session
        // (no browser) it falls through to the remote, so full-screen apps
        // keep their Ctrl-F (the char-mode invariant is untouched).
        if self.mode == Mode::Session
            && self.browser.is_some()
            && key.code == KeyCode::Char('f')
            && key.modifiers.contains(KeyModifiers::CONTROL)
        {
            self.open_find();
            return;
        }

        // Find mode owns navigation keys (next/prev/close); text-editing keys
        // fall through to the shared line editor below, after which the
        // query is re-scanned (a no-op when it didn't change).
        if self.mode == Mode::Find {
            let shift = key.modifiers.contains(KeyModifiers::SHIFT);
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            match key.code {
                KeyCode::Esc => {
                    self.close_find();
                    return;
                }
                KeyCode::Enter => {
                    if shift {
                        self.find_prev();
                    } else {
                        self.find_next();
                    }
                    return;
                }
                // Ctrl-F / Ctrl-G cycle matches; Up/Down do too, so prev works
                // even where the terminal can't deliver Shift-Enter.
                KeyCode::Char('f' | 'g') if ctrl => {
                    if shift {
                        self.find_prev();
                    } else {
                        self.find_next();
                    }
                    return;
                }
                KeyCode::Up => {
                    self.find_prev();
                    return;
                }
                KeyCode::Down => {
                    self.find_next();
                    return;
                }
                _ => {}
            }
        }

        // The browser captures session-mode keys while open. HTTP laid-out
        // docs use the 2D item model (Enter follows, Backspace backs, arrows
        // move the selection laterally and vertically); gopher/gemini keep
        // the gopherus line model.
        if self.mode == Mode::Session && self.browser.is_some() {
            if self.browser.as_ref().is_some_and(|g| g.doc.laid_out()) {
                self.http_nav(key);
            } else {
                self.browser_nav(key);
            }
            return;
        }

        // Character-at-a-time mode: every keystroke goes straight to the
        // remote end, which does the echoing.
        if self.mode == Mode::Session && self.char_mode() {
            if let Some(bytes) = encode_key(key, self.crlf) {
                self.send_bytes(bytes).await;
            }
            return;
        }

        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        match key.code {
            KeyCode::Esc if matches!(self.mode, Mode::Command | Mode::Search) => {
                self.mode = Mode::Session;
                self.search_target = None;
                self.cert_for = None;
                self.select_anchor = None;
            }
            // Esc in a line-mode session opens command mode, like
            // Ctrl-]. (Char-mode sessions never reach here — their Esc
            // goes to the remote, where full-screen apps need it.)
            KeyCode::Esc => {
                self.mode = Mode::Command;
                self.select_anchor = None;
            }
            KeyCode::Enter => {
                let line = std::mem::take(&mut self.input);
                self.cursor = 0;
                self.select_anchor = None;
                self.active_history().push(&line);
                match self.mode {
                    Mode::Session => self.send_line(&line).await,
                    Mode::Command => {
                        self.mode = Mode::Session;
                        self.execute_command(&line).await;
                    }
                    Mode::Search => {
                        self.mode = Mode::Session;
                        self.run_search(&line);
                    }
                    // Find handles Enter (next/prev) in its own block above.
                    Mode::Find => {}
                }
            }
            // In session mode, control chords bypass the input field and go
            // straight to the remote end (Ctrl-C, Ctrl-D, Ctrl-Z, ...).
            KeyCode::Char(c)
                if self.mode == Mode::Session && key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                let upper = c.to_ascii_uppercase();
                if ('@'..='_').contains(&upper) {
                    self.send_bytes(vec![upper as u8 & 0x1f]).await;
                }
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                // Typing over a selection replaces it.
                self.delete_selection();
                self.input.insert(self.byte_cursor(), c);
                self.cursor += 1;
                self.active_history().detach();
            }
            KeyCode::Backspace | KeyCode::Delete if self.selection().is_some() => {
                self.delete_selection();
            }
            KeyCode::Backspace if self.cursor > 0 => {
                self.cursor -= 1;
                self.input.remove(self.byte_cursor());
                self.active_history().detach();
            }
            KeyCode::Delete if self.cursor < self.input.chars().count() => {
                self.input.remove(self.byte_cursor());
                self.active_history().detach();
            }
            KeyCode::Up => {
                let current = self.input.clone();
                if let Some(text) = self.active_history().up(&current) {
                    self.cursor = text.chars().count();
                    self.input = text;
                    self.select_anchor = None;
                }
            }
            KeyCode::Down => {
                if let Some(text) = self.active_history().down() {
                    self.cursor = text.chars().count();
                    self.input = text;
                    self.select_anchor = None;
                }
            }
            KeyCode::Left => self.move_cursor(self.cursor.saturating_sub(1), shift),
            KeyCode::Right => {
                self.move_cursor((self.cursor + 1).min(self.input.chars().count()), shift)
            }
            KeyCode::Home => self.move_cursor(0, shift),
            KeyCode::End => self.move_cursor(self.input.chars().count(), shift),
            _ => {}
        }

        // A text edit in the find box re-scans the doc (no-op if the query
        // didn't actually change, e.g. cursor moves).
        if self.mode == Mode::Find {
            self.recompute_find();
        }
    }

    /// Replace the emulator with a fresh one, sized to the current widget.
    /// A dead session can leave a scroll region (DECSTBM), origin mode, or
    /// the alternate screen active; without a reset those confine the next
    /// session's output — e.g. every line overwriting itself on the bottom
    /// row inside a stale one-line scroll region.
    fn reset_screen(&mut self) {
        let (cols, rows) = self.last_inner;
        self.vt = new_vt(rows.max(1), cols.max(1));
        self.probes = ProbeDetector::default();
        self.bells_seen = 0;
    }

    /// Scroll the session view through vt100's scrollback buffer.
    /// Positive deltas scroll toward older output.
    fn scroll_by(&mut self, delta: i32) {
        let cur = self.vt.screen().scrollback() as i64;
        let new = (cur + i64::from(delta)).max(0) as usize;
        // set_scrollback clamps to the rows actually buffered.
        self.vt.screen_mut().set_scrollback(new);
    }

    /// The history matching what the input field currently feeds.
    fn active_history(&mut self) -> &mut History {
        match self.mode {
            Mode::Session => &mut self.session_history,
            // Find never pushes/recalls history (its Up/Down navigate matches),
            // but the shared editor calls `detach()` while typing — harmless.
            Mode::Command | Mode::Search | Mode::Find => &mut self.command_history,
        }
    }

    /// Open in-page find over the current browser doc (Ctrl-F).
    fn open_find(&mut self) {
        self.input.clear();
        self.cursor = 0;
        self.select_anchor = None;
        self.mode = Mode::Find;
        self.find = Some(FindState::default());
        self.status =
            String::from("Find: type to search · Enter/↓ next · Shift-Enter/↑ prev · Esc close");
    }

    /// Close find, dropping matches/highlights and returning to the browser.
    fn close_find(&mut self) {
        self.find = None;
        self.input.clear();
        self.cursor = 0;
        self.select_anchor = None;
        self.mode = Mode::Session;
        self.status = String::from("Find closed.");
    }

    /// Re-scan the doc for the current query. A no-op when the query is
    /// unchanged (so cursor moves keep the active match put); otherwise it
    /// rebuilds matches, picks the nearest one at/after the current scroll,
    /// and scrolls to it.
    fn recompute_find(&mut self) {
        let query = self.input.to_ascii_lowercase();
        if self.find.as_ref().is_some_and(|f| f.last_query == query) {
            return;
        }
        let mut matches = Vec::new();
        if let Some(g) = self.browser.as_ref()
            && !query.is_empty()
        {
            if g.doc.laid_out() {
                for (r, row) in g.doc.rows.iter().enumerate() {
                    for (i, item) in row.items.iter().enumerate() {
                        push_text_matches(&item.text, &query, r, Some(i), &mut matches);
                    }
                }
            } else {
                for (l, line) in g.doc.lines.iter().enumerate() {
                    push_text_matches(&line.text, &query, l, None, &mut matches);
                }
            }
        }
        // Jump to the first match at or after the current scroll, else the top.
        let scroll = self.browser.as_ref().map_or(0, |g| g.scroll);
        let current = (!matches.is_empty())
            .then(|| matches.iter().position(|m| m.line >= scroll).unwrap_or(0));
        if let Some(f) = self.find.as_mut() {
            f.last_query = query;
            f.matches = matches;
            f.current = current;
        }
        self.scroll_to_current_match();
        self.update_find_status();
    }

    /// Advance the active match (wrapping), scrolling it into view.
    fn find_next(&mut self) {
        if let Some(f) = self.find.as_mut()
            && !f.matches.is_empty()
        {
            let n = f.matches.len();
            f.current = Some(f.current.map_or(0, |c| (c + 1) % n));
        }
        self.scroll_to_current_match();
        self.update_find_status();
    }

    /// Retreat the active match (wrapping), scrolling it into view.
    fn find_prev(&mut self) {
        if let Some(f) = self.find.as_mut()
            && !f.matches.is_empty()
        {
            let n = f.matches.len();
            f.current = Some(f.current.map_or(n - 1, |c| (c + n - 1) % n));
        }
        self.scroll_to_current_match();
        self.update_find_status();
    }

    /// Centre the viewport on the active match's line/row.
    fn scroll_to_current_match(&mut self) {
        let line = {
            let Some(f) = self.find.as_ref() else { return };
            let Some(ci) = f.current else { return };
            match f.matches.get(ci) {
                Some(m) => m.line,
                None => return,
            }
        };
        let height = self.last_inner.1 as usize;
        if let Some(g) = self.browser.as_mut() {
            let max_scroll = g.doc.extent().saturating_sub(height.max(1));
            g.scroll = line.saturating_sub(height / 2).min(max_scroll);
        }
    }

    /// Refresh the status line with the find query and match counter.
    fn update_find_status(&mut self) {
        let Some(f) = self.find.as_ref() else { return };
        self.status = if self.input.is_empty() {
            String::from("Find: type to search · Esc close")
        } else if f.matches.is_empty() {
            format!("Find \"{}\" · no matches", self.input)
        } else {
            let n = f.current.map_or(0, |c| c + 1);
            format!("Find \"{}\" · {}/{}", self.input, n, f.matches.len())
        };
    }

    /// True when keystrokes should bypass the input field, GNU telnet
    /// style: the server echoes (WILL ECHO), or the user forced it.
    pub fn char_mode(&self) -> bool {
        self.connected
            && match self.mode_override {
                Some(InputMode::Character) => true,
                Some(InputMode::Line) => false,
                // ECHO dominates (it covers password prompts even under
                // LINEMODE); otherwise an active LINEMODE with EDIT clear
                // also means character-at-a-time.
                None => {
                    self.remote_opts.contains(&op_option::ECHO)
                        || (self.linemode_active && !self.linemode_edit)
                }
            }
    }

    /// Byte offset of the char cursor into `input`.
    fn byte_cursor(&self) -> usize {
        self.byte_at(self.cursor)
    }

    /// Byte offset of a char index into `input`.
    fn byte_at(&self, char_idx: usize) -> usize {
        self.input
            .char_indices()
            .nth(char_idx)
            .map_or(self.input.len(), |(i, _)| i)
    }

    /// The selected char range (lo..hi), if a non-empty selection exists.
    pub fn selection(&self) -> Option<(usize, usize)> {
        let anchor = self.select_anchor?;
        if anchor == self.cursor {
            return None;
        }
        Some((anchor.min(self.cursor), anchor.max(self.cursor)))
    }

    /// Remove the selected text, parking the cursor where it started.
    /// Returns whether anything was deleted.
    fn delete_selection(&mut self) -> bool {
        let Some((lo, hi)) = self.selection() else {
            self.select_anchor = None;
            return false;
        };
        let range = self.byte_at(lo)..self.byte_at(hi);
        self.input.replace_range(range, "");
        self.cursor = lo;
        self.select_anchor = None;
        self.active_history().detach();
        true
    }

    /// Move the cursor for an arrow/Home/End press: Shift extends the
    /// selection from the current position, plain movement clears it.
    fn move_cursor(&mut self, to: usize, shift: bool) {
        if shift {
            self.select_anchor.get_or_insert(self.cursor);
        } else {
            self.select_anchor = None;
        }
        self.cursor = to;
    }

    /// Send one entered line to the remote host.
    async fn send_line(&mut self, line: &str) {
        if self.conn.is_none() {
            self.status = String::from("No connection. Ctrl-] then `open <host>`.");
            return;
        }
        // TODO: GNU telnet maps end-of-line per the crlf/binary toggles
        // (CR LF vs CR NUL); we always send CR LF for now.
        let mut bytes = line.as_bytes().to_vec();
        bytes.extend_from_slice(b"\r\n");
        self.send_bytes(bytes).await;
    }

    async fn send_bytes(&mut self, bytes: Vec<u8>) {
        // Sending anything snaps the view back to the live screen.
        self.vt.screen_mut().set_scrollback(0);
        if let Some(conn) = &self.conn {
            let _ = conn.commands.send(telnet::Command::Send(bytes)).await;
        }
    }

    /// Under active LINEMODE, tell the server about a `mode` change so the
    /// negotiated state stays in sync (RFC 1184 MODE request). A no-op when
    /// LINEMODE isn't active — the kludge ECHO/SGA path needs no MODE; the
    /// server's ACK updates `linemode_edit`, so `mode auto` then follows it.
    async fn request_linemode(&self, edit: bool) {
        if self.linemode_active
            && let Some(conn) = &self.conn
        {
            let _ = conn
                .commands
                .send(telnet::Command::LineModeRequest { edit })
                .await;
        }
    }

    async fn execute_command(&mut self, line: &str) {
        let mut parts = line.split_whitespace();
        match parts.next() {
            None => {}
            Some("quit" | "q" | "exit") => self.quit = true,
            Some("reload") => self.reload(),
            Some("close" | "c") => match self.conn.take() {
                Some(conn) => {
                    let _ = conn.commands.send(telnet::Command::Close).await;
                }
                None => self.status = String::from("No connection to close."),
            },
            Some("open" | "o") => match parts.next() {
                Some(host) => {
                    let port = match parts.next() {
                        Some(p) => match parse_port(p) {
                            Some(p) => Some(p),
                            None => {
                                self.status = format!("bad port or service name: {p}");
                                return;
                            }
                        },
                        None => None,
                    };
                    self.dispatch_open(host, port);
                }
                None => self.status = String::from("usage: open <host> [port]"),
            },
            Some("post") => match parts.next().map(str::to_string) {
                Some(target) => match http::parse_url(&target) {
                    Some(url) => {
                        let body = parts.collect::<Vec<_>>().join(" ");
                        self.start_post(url, body, None);
                    }
                    None => self.status = String::from("post needs an http(s):// URL"),
                },
                None => self.status = String::from("usage: post <url> [body]"),
            },
            Some("mode" | "m") => match parts.next() {
                Some("character" | "char") => {
                    self.mode_override = Some(InputMode::Character);
                    self.request_linemode(false).await;
                    self.status = String::from("Input mode forced to character-at-a-time.");
                }
                Some("line") => {
                    self.mode_override = Some(InputMode::Line);
                    self.request_linemode(true).await;
                    self.status = String::from("Input mode forced to line-by-line.");
                }
                Some("auto") => {
                    self.mode_override = None;
                    self.status = String::from("Input mode follows ECHO negotiation.");
                }
                _ => self.status = String::from("usage: mode character|line|auto"),
            },
            Some("send") => match parts.next() {
                // A literal Ctrl-] (0x1D), otherwise swallowed as our escape.
                Some("escape") => {
                    self.send_bytes(vec![0x1d]).await;
                    self.status = String::from("Sent escape character.");
                }
                Some(name) => match iac_code(name) {
                    Some(code) => {
                        if let Some(conn) = &self.conn {
                            let _ = conn.commands.send(telnet::Command::SendIac(code)).await;
                            self.status = format!("Sent IAC {}.", name.to_uppercase());
                        } else {
                            self.status = String::from("No connection.");
                        }
                    }
                    None => self.status = String::from(SEND_USAGE),
                },
                None => self.status = String::from(SEND_USAGE),
            },
            Some("set") => match (parts.next(), parts.next()) {
                (Some("encoding"), Some("cp437")) => {
                    self.encoding = Encoding::Cp437;
                    self.status = String::from("Encoding set to CP437 (BBS art mode).");
                }
                (Some("encoding"), Some("utf8" | "utf-8")) => {
                    self.encoding = Encoding::Utf8;
                    self.status = String::from("Encoding set to UTF-8.");
                }
                (Some("image"), Some(proto)) => self.set_image_protocol(proto),
                (Some("cookies"), Some("on")) => {
                    http::set_cookies_enabled(true);
                    self.status = String::from(
                        "Cookies on: RAM-only, exact-host only, sent with matching requests.",
                    );
                }
                (Some("cookies"), Some("off")) => {
                    http::set_cookies_enabled(false);
                    self.status = String::from("Cookies off: no cookie capture, reads, or sends.");
                }
                (Some("js"), Some("on")) => {
                    self.js_enabled = true;
                    self.status = String::from(
                        "JavaScript on: pages run scripts, fetch/XHR allowed (budgeted, capped).",
                    );
                }
                (Some("js"), Some("off")) => {
                    self.js_enabled = false;
                    self.status = String::from("JavaScript off (on is the default).");
                }
                (Some("borders"), Some("on")) => {
                    crate::layout::set_borders_enabled(true);
                    self.relayout_browser();
                    self.status =
                        String::from("Borders on: CSS borders render as box-drawing chrome.");
                }
                (Some("borders"), Some("off")) => {
                    crate::layout::set_borders_enabled(false);
                    self.relayout_browser();
                    self.status = String::from("Borders off (the default): borders aren't drawn.");
                }
                _ => {
                    self.status = String::from(
                        "usage: set encoding cp437|utf8 · set image <protocol>|auto · set js on|off · set cookies on|off · set borders on|off",
                    )
                }
            },
            Some("toggle" | "t") => match parts.next() {
                Some("crlf") => {
                    self.crlf = !self.crlf;
                    self.status = format!(
                        "Enter now sends {}.",
                        if self.crlf { "CR LF" } else { "CR NUL" }
                    );
                }
                _ => self.status = String::from("usage: toggle crlf"),
            },
            Some("status" | "st") => self.show_status(),
            Some("finger" | "f") => match parts.next() {
                Some(target) => {
                    let (user, host) = match target.rsplit_once('@') {
                        Some((user, host)) => (user, host),
                        None => ("", target),
                    };
                    if host.is_empty() {
                        self.status = String::from("usage: finger [user]@<host>");
                        return;
                    }
                    let (host, port) = split_host_port(host);
                    self.start_fetch(Link::OneShot(oneshot::OneShotUrl {
                        scheme: oneshot::Scheme::Finger,
                        host: host.to_string(),
                        port: port.unwrap_or(79),
                        query: user.to_string(),
                    }));
                }
                None => self.status = String::from("usage: finger [user]@<host>"),
            },
            Some("whois") => match parts.next() {
                Some(query) => {
                    let (host, port) =
                        split_host_port(parts.next().unwrap_or(oneshot::WHOIS_DEFAULT));
                    self.start_fetch(Link::OneShot(oneshot::OneShotUrl {
                        scheme: oneshot::Scheme::Whois,
                        host: host.to_string(),
                        port: port.unwrap_or(43),
                        query: query.to_string(),
                    }));
                }
                None => self.status = String::from("usage: whois <domain> [server]"),
            },
            Some("dict" | "define") => match parts.next() {
                Some(word) => {
                    let (host, port) =
                        split_host_port(parts.next().unwrap_or(oneshot::DICT_DEFAULT));
                    self.start_fetch(Link::OneShot(oneshot::OneShotUrl {
                        scheme: oneshot::Scheme::Dict,
                        host: host.to_string(),
                        port: port.unwrap_or(2628),
                        query: word.to_string(),
                    }));
                }
                None => self.status = String::from("usage: dict <word> [server]"),
            },
            // A bare URL — or a bare hostname/IP — opens directly, as if
            // `open` had been typed (the address-bar habit). A schemeless
            // host with no port becomes https (falling back to http).
            Some(target) if target.contains("://") || looks_like_host(target) => {
                let port = parts.next().and_then(parse_port);
                self.dispatch_open(target, port);
            }
            // TODO for GNU telnet parity: full set/unset, display,
            // logout, z (suspend), ! (shell escape).
            Some(other) => {
                self.status = format!(
                    "unknown command: {other} (open/close/mode/send/set/toggle/finger/whois/dict/status/quit — or just type a URL)"
                )
            }
        }
    }

    /// `set image <protocol>`: force the graphics protocol, or `auto`
    /// to restore what the startup query found. An open viewer
    /// re-encodes under the new protocol.
    fn set_image_protocol(&mut self, proto: &str) {
        let chosen = match proto {
            "sixel" => ProtocolType::Sixel,
            "halfblocks" | "blocks" => ProtocolType::Halfblocks,
            "kitty" => ProtocolType::Kitty,
            "iterm2" => ProtocolType::Iterm2,
            "auto" => self.auto_protocol,
            _ => {
                self.status = String::from("usage: set image sixel|halfblocks|kitty|iterm2|auto");
                return;
            }
        };
        self.picker.set_protocol_type(chosen);
        self.status = format!(
            "Image protocol: {}{}.",
            format!("{chosen:?}").to_lowercase(),
            if proto == "auto" { " (queried)" } else { "" }
        );
        if let Some(v) = &mut self.viewer {
            v.encoded_for = (0, 0); // force the next sync to re-encode
        }
    }

    /// GNU telnet's `status` command: print connection state into the
    /// session feed, the way GNU telnet prints to the terminal.
    fn show_status(&mut self) {
        let connection = match (&self.host, self.connected) {
            (Some(host), true) if self.tls => {
                format!("Connected to {host}:{} over TLS (TOFU-pinned).", self.port)
            }
            (Some(host), true) => format!("Connected to {host}:{}.", self.port),
            (Some(host), false) => format!("Not connected (last host {host}:{}).", self.port),
            _ => String::from("No connection."),
        };
        let mode = match (self.mode_override, self.char_mode()) {
            (Some(InputMode::Character), _) => "character (forced)",
            (Some(InputMode::Line), _) => "line (forced)",
            (None, true) if self.linemode_active => "character (LINEMODE)",
            (None, false) if self.linemode_active => "line (LINEMODE)",
            (None, true) => "character (negotiated)",
            (None, false) => "line (negotiated)",
        };
        let report = format!(
            "\r\n\x1b[36m--- TRUST STATUS ---\x1b[0m\r\n\
             {connection}\r\n\
             Escape character: Ctrl-]\r\n\
             Input mode: {mode}\r\n\
             Enter sends: {eol}\r\n\
             Encoding: {enc}\r\n\
             JavaScript: {js}\r\n\
             Cookies: {cookies}\r\n\
             Remote options (WILL): {remote}\r\n\
             Local options (DO): {local}\r\n\
             \x1b[36m--------------------\x1b[0m\r\n",
            eol = if self.crlf { "CR LF" } else { "CR NUL" },
            enc = match self.encoding {
                Encoding::Utf8 => "UTF-8",
                Encoding::Cp437 => "CP437",
            },
            js = if self.js_enabled { "on" } else { "off" },
            cookies = if http::cookies_enabled() {
                "on (RAM-only, exact-host)"
            } else {
                "off"
            },
            remote = option_names(&self.remote_opts),
            local = option_names(&self.local_opts),
        );
        self.vt.process(report.as_bytes());
    }

    /// Route an open target to the right protocol: gopher:// and
    /// gemini:// URLs (or ports 70/1965) get the browser, everything
    /// else is telnet (TLS for telnets:// / port 992).
    /// Open a target. An explicit scheme always wins. With no scheme, a bare
    /// host with NO port opens as the WEB (https→http). An EXPLICIT port picks
    /// the protocol: 80/443→web, 70→gopher, 79→finger, 1965→gemini,
    /// 992→telnets, and ANY OTHER port→telnet (the web lives on its standard
    /// ports; an odd port is a MUD/BBS, so `host:2222` is telnet, not a doomed
    /// HTTP GET). `port` is the explicitly-supplied port, if any; a `host:port`
    /// in the target wins over it.
    fn dispatch_open(&mut self, target: &str, port: Option<u16>) {
        if let Some(url) = GopherUrl::parse(target) {
            self.start_fetch(Link::Gopher(url));
        } else if let Some(url) = GeminiUrl::parse(target) {
            self.start_fetch(Link::Gemini(url));
        } else if let Some(url) = http::parse_url(target) {
            self.start_fetch(Link::Http(url));
        } else if let Some(url) = oneshot::OneShotUrl::parse(target) {
            self.start_fetch(Link::OneShot(url));
        } else if let Some(rest) = target.strip_prefix("telnet://") {
            let (host, url_port) = split_host_port(rest.trim_end_matches('/'));
            self.open(host.to_string(), url_port.or(port).unwrap_or(23), false);
        } else if let Some(rest) = target.strip_prefix("telnets://") {
            let (host, url_port) = split_host_port(rest.trim_end_matches('/'));
            self.open(host.to_string(), url_port.or(port).unwrap_or(992), true);
        } else {
            // No scheme: a trailing `:port` on the host wins over the argument.
            // No port → the web (https→http). An explicit port picks the
            // protocol by well-known number; ANY OTHER port is telnet, because
            // the web lives on its standard ports and an odd port is a MUD/BBS.
            let (host, embedded) = split_host_port(target);
            match embedded.or(port) {
                None => self.start_web(host, None),
                Some(80) => self.start_web(host, Some(80)),
                Some(443) => self.start_web(host, Some(443)),
                Some(70) => self.start_fetch(Link::Gopher(GopherUrl {
                    host: host.to_string(),
                    port: 70,
                    item_type: '1',
                    selector: String::new(),
                })),
                Some(1965) => self.start_fetch(Link::Gemini(GeminiUrl {
                    host: host.to_string(),
                    port: 1965,
                    path: String::from("/"),
                })),
                // Finger with an empty query: who is logged in.
                Some(79) => self.start_fetch(Link::OneShot(oneshot::OneShotUrl {
                    scheme: oneshot::Scheme::Finger,
                    host: host.to_string(),
                    port: 79,
                    query: String::new(),
                })),
                Some(992) => self.open(host.to_string(), 992, true),
                // Any other explicit port: plain telnet (MUD/BBS).
                Some(p) => self.open(host.to_string(), p, false),
            }
        }
    }

    /// Open a bare host as the web. With no port it's https with an http
    /// fallback (a bare hostname typed without a scheme — see
    /// `http::fetch_web_default`); port 443 is https, 80 or any other port is
    /// plain http on that port.
    fn start_web(&mut self, host: &str, port: Option<u16>) {
        let (raw, fallback) = match port {
            None => (format!("https://{host}/"), true),
            Some(443) => (format!("https://{host}/"), false),
            Some(80) => (format!("http://{host}/"), false),
            Some(p) => (format!("http://{host}:{p}/"), false),
        };
        match http::parse_url(&raw) {
            Some(url) => self.start_fetch_opts(Link::Http(url), fallback, None),
            None => {
                self.status = format!("bad host: {host}");
                self.notice = true;
            }
        }
    }

    /// `reload`: re-fetch what is on screen (image viewer first, else
    /// the browser document), replacing it in place — history untouched,
    /// scroll kept. The way to re-render after `set js on`.
    fn reload(&mut self) {
        let target = match (&self.viewer, &self.browser) {
            (Some(v), _) => v.url.clone(),
            (None, Some(g)) => g.doc.url.clone(),
            (None, None) => {
                self.status = String::from("Nothing to reload.");
                return;
            }
        };
        self.replace_nav = true;
        self.start_fetch(target);
    }

    /// Fetch a document in the background; the result arrives in the
    /// select loop as a FetchMsg.
    fn start_fetch(&mut self, target: Link) {
        self.start_fetch_opts(target, false, None);
    }

    /// The `Referer` source for a navigation that originates from the page on
    /// screen (a followed link, a form submit, a live-page navigation): the
    /// current document's URL when it's a web page. None for any other source
    /// — a typed URL, the CLI, history back — which carry no referrer, and for
    /// a non-web current page (gopher/gemini have no referer concept).
    fn http_referrer(&self) -> Option<url::Url> {
        match self.browser.as_ref().map(|g| &g.doc.url) {
            Some(Link::Http(u)) => Some(u.clone()),
            _ => None,
        }
    }

    /// Follow `target` as if a link on the current page was clicked: a web
    /// target carries the page's Referer (browser default policy); a foreign
    /// scheme falls back to the plain address-bar dispatch (no referrer).
    fn navigate_from_page(&mut self, target: &str) {
        match http::parse_url(target) {
            Some(url) => {
                let referrer = self.http_referrer();
                self.start_fetch_opts(Link::Http(url), false, referrer);
            }
            None => self.dispatch_open(target, None),
        }
    }

    /// Fetch a document in the background. With `fallback_http`, an https web
    /// target that fails to connect is retried over plain http — for a bare
    /// host opened without a scheme (see `http::fetch_web_default`). `referrer`
    /// is the page a link/form was followed from, when policy says to send one.
    fn start_fetch_opts(&mut self, target: Link, fallback_http: bool, referrer: Option<url::Url>) {
        let (tx, rx) = mpsc::channel(1);
        self.fetch_rx = Some(rx);
        self.status = format!("Fetching {target} ...");
        let f = self.picker.font_size();
        let viewport = self.last_inner;
        let cell_px = (f.width, f.height);
        let storage = self.web_storage.clone();
        let js_on = self.js_enabled;
        let task = tokio::spawn(async move {
            let result = match &target {
                Link::Gopher(url) => gopher::fetch(url).await.map(Payload::Gopher),
                Link::Gemini(url) => gemini::fetch(url).await.map(Payload::Gemini),
                Link::Http(url) => {
                    let fetched = if fallback_http {
                        http::fetch_web_default(url).await
                    } else {
                        let mut req = http::Request::get(url.clone());
                        if let Some(page) = &referrer {
                            http::set_referrer(&mut req, page);
                        }
                        http::fetch(&req).await
                    };
                    match fetched {
                        // JS on: full transform. JS off: still bake the page's
                        // CSS so it lays out per its own stylesheets.
                        Ok(response) => Ok(Payload::Http(if js_on {
                            http::execute_js(response, viewport, cell_px, storage).await
                        } else {
                            http::css_only(response, viewport, cell_px).await
                        })),
                        Err(err) => Err(err),
                    }
                }
                Link::OneShot(url) => oneshot::fetch(url).await.map(Payload::OneShot),
                Link::External(url) => Err(format!("cannot fetch foreign scheme: {url}")),
                Link::Form { .. } => Err(String::from("form controls are not fetchable")),
                Link::JsClick { .. } => {
                    Err(String::from("page-script links need their living page"))
                }
                Link::CarouselScroll(_) => Err(String::from(
                    "carousel controls scroll in place, not fetched",
                )),
            };
            let _ = tx.send(FetchMsg { target, result }).await;
        });
        self.fetch_task = Some(task);
    }

    /// POST a form-encoded body to a web URL (her use case; the UX can
    /// grow content-type options once the target application is known).
    fn start_post(&mut self, url: url::Url, body: String, referrer: Option<url::Url>) {
        let (tx, rx) = mpsc::channel(1);
        self.fetch_rx = Some(rx);
        self.status = format!("POSTing to {url} ...");
        let f = self.picker.font_size();
        let viewport = self.last_inner;
        let cell_px = (f.width, f.height);
        let storage = self.web_storage.clone();
        let js_on = self.js_enabled;
        let task = tokio::spawn(async move {
            let mut request = http::Request {
                method: String::from("POST"),
                url: url.clone(),
                body: Some((
                    String::from("application/x-www-form-urlencoded"),
                    body.into_bytes(),
                )),
                headers: Vec::new(),
            };
            if let Some(page) = &referrer {
                http::set_referrer(&mut request, page);
            }
            let result = match http::fetch(&request).await {
                Ok(response) => Ok(Payload::Http(if js_on {
                    http::execute_js(response, viewport, cell_px, storage).await
                } else {
                    http::css_only(response, viewport, cell_px).await
                })),
                Err(err) => Err(err),
            };
            let _ = tx
                .send(FetchMsg {
                    target: Link::Http(url),
                    result,
                })
                .await;
        });
        self.fetch_task = Some(task);
    }

    /// Decode and scale-to-fit encode an image off the UI thread; the
    /// viewer opens (or refreshes) when the ImgMsg comes back.
    fn open_image(&mut self, url: Link, raw: impl Into<std::sync::Arc<[u8]>>) {
        // The viewer always replaces; a pending reload flag is spent.
        self.replace_nav = false;
        let raw = raw.into();
        let (tx, rx) = mpsc::channel(1);
        self.img_rx = Some(rx);
        self.status = format!("Rendering {url} ...");
        let picker = self.picker.clone();
        let size = self.last_inner;
        tokio::task::spawn_blocking(move || {
            let result = img::decode(&raw).and_then(|(image, mime)| {
                let info = format!("{}×{} {mime}", image.width(), image.height());
                let panel = ratatui::layout::Size::new(size.0, size.1);
                // The full-screen image viewer fits (contains) the panel.
                img::encode(&picker, image, panel, false).map(|protocol| (protocol, info))
            });
            let _ = tx.blocking_send(ImgMsg {
                url,
                raw,
                size,
                result,
            });
        });
    }

    fn on_img(&mut self, msg: ImgMsg) {
        self.img_rx = None;
        match msg.result {
            Ok((protocol, info)) => {
                self.status = format!("{} — {info}", msg.url);
                self.viewer = Some(ImageView {
                    url: msg.url,
                    raw: msg.raw,
                    info,
                    protocol,
                    encoded_for: msg.size,
                });
            }
            Err(err) => {
                self.status = format!("{} — {err}", msg.url);
                self.notice = true;
            }
        }
    }

    /// Re-encode the viewed image when the panel size (or the protocol,
    /// which zeroes `encoded_for`) changed. One encode in flight at a
    /// time; the old rendering stays up until the new one lands.
    fn sync_viewer_size(&mut self) {
        if self.img_rx.is_some() {
            return;
        }
        let Some(v) = &self.viewer else { return };
        if v.encoded_for == self.last_inner {
            return;
        }
        self.open_image(v.url.clone(), v.raw.clone());
    }

    /// The URL→cell-box map the layout pass reads, built from the decode
    /// cache (only decoded images get a real box; the rest stay alt text).
    fn image_sizes(&self) -> crate::layout::ImageSizes {
        self.image_cache
            .iter()
            .map(|(u, d)| (u.clone(), d.cell))
            .collect()
    }

    /// Start the parallel fetch+decode of every page image not already in
    /// the cache. Fetches overlap (pooled, `buffer_unordered`) and each
    /// decode runs on a blocking task — no serial wall, results stream
    /// back over `imgs_rx` and re-layout as they land.
    fn start_image_loads(&mut self, page: Url, urls: Vec<String>) {
        let todo: Vec<String> = urls
            .into_iter()
            .filter(|u| !self.image_cache.contains_key(u))
            .collect();
        if todo.is_empty() {
            return;
        }
        let font = self.picker.font_size();
        let (tx, rx) = mpsc::channel(todo.len().max(1));
        self.imgs_rx = Some(rx);
        tokio::spawn(async move {
            futures::stream::iter(todo.into_iter().map(|url| {
                let tx = tx.clone();
                let page = page.clone();
                async move {
                    let decoded = load_one_image(&page, &url, font).await;
                    let _ = tx.send(ImgLoadMsg { url, decoded }).await;
                }
            }))
            .buffer_unordered(IMG_FETCH_CONCURRENCY)
            .for_each(|_| async {})
            .await;
        });
    }

    /// One image finished decoding: cache it and re-flow the page so its
    /// box (and the rows beneath it) appear in place of the alt text.
    fn on_img_load(&mut self, msg: ImgLoadMsg) {
        let Some(decoded) = msg.decoded else {
            return; // fetch/decode failed: the alt text stands
        };
        self.image_cache.insert(msg.url, decoded);
        self.relayout_browser();
    }

    /// Re-lay-out the current HTTP doc with the decoded-image sizes,
    /// preserving the selected item and scroll (same as a resize re-flow).
    fn relayout_browser(&mut self) {
        let width = (self.last_inner.0 as usize).max(10);
        let height = self.last_inner.1.max(1) as usize;
        let images = self.image_sizes();
        let Some(g) = &mut self.browser else { return };
        let Link::Http(url) = g.doc.url.clone() else {
            return;
        };
        if g.doc.raw.is_empty() {
            return;
        }
        let item_target = g
            .sel_item
            .and_then(|(r, i)| g.doc.rows.get(r).and_then(|row| row.items.get(i)).cloned());
        let meta = g.doc.meta.clone().unwrap_or_default();
        let forms = std::mem::take(&mut g.doc.forms);
        let raw = std::mem::take(&mut g.doc.raw);
        g.doc = http::parse_seeded(&url, &meta, &raw, width, Some(&forms), &images);
        g.sel_item = item_target.and_then(|t| Self::find_item_like(&g.doc.rows, &t));
        let max_scroll = g.doc.rows.len().saturating_sub(height);
        g.scroll = g.scroll.min(max_scroll);
    }

    fn on_enc(&mut self, msg: EncMsg) {
        self.image_encoding.remove(&msg.key);
        if let Some(protocol) = msg.protocol {
            self.image_protocols.insert(msg.key, protocol);
        }
    }

    /// Encode every inline image whose box reaches the current viewport but
    /// isn't yet encoded. One `SlicedProtocol` per box, on a blocking task
    /// (parallel), keyed by `(url, w, h, crop)` so scroll — which never changes
    /// the box — never re-encodes (the renderer slices the cached protocol).
    /// Also EVICTS protocols whose boxes have scrolled out of range, so the
    /// cache stays bounded to the on-screen set instead of growing for the
    /// whole session. Called each loop tick before the draw, like
    /// `sync_browser_wrap`.
    fn sync_image_encodes(&mut self) {
        let vh = self.last_inner.1;
        let Some(g) = &self.browser else { return };
        if !g.doc.laid_out() {
            return;
        }
        let end = (g.scroll + vh as usize).min(g.doc.rows.len());
        // Start the scan above the viewport top: a tall image whose top row is
        // already scrolled off the top still reaches down into view (the
        // renderer draws its lower slice).
        let start = g.scroll.saturating_sub(crate::layout::MAX_IMAGE_LOOKBACK);
        let mut live: HashSet<EncKey> = HashSet::new();
        for (off, row) in g.doc.rows[start..end].iter().enumerate() {
            let doc_row = start + off;
            for item in &row.items {
                let Some(url) = &item.image else { continue };
                // Apply the carousel scroll/clip exactly as the renderer does
                // (`visible_col`): a strip image scrolled into the band is live,
                // one scrolled out of the band isn't.
                if crate::layout::visible_col(&g.doc.carousels, doc_row, item).is_none() {
                    continue;
                }
                live.insert(EncKey::for_item(url, item));
            }
        }
        // Bound the cache: drop protocols for boxes no longer in range.
        self.image_protocols.retain(|k, _| live.contains(k));
        let wanted: Vec<EncKey> = live
            .into_iter()
            .filter(|k| !self.image_protocols.contains_key(k) && !self.image_encoding.contains(k))
            .collect();
        for key in wanted {
            self.request_image_encode(key);
        }
    }

    /// Spawn one blocking encode of a decoded image to a sliced terminal
    /// protocol for the given cell box; the result lands over `enc_rx`.
    fn request_image_encode(&mut self, key: EncKey) {
        let Some(decoded) = self.image_cache.get(&key.url) else {
            return;
        };
        let raw = decoded.raw.clone();
        let picker = self.picker.clone();
        let tx = self.enc_tx.clone();
        self.image_encoding.insert(key.clone());
        tokio::task::spawn_blocking(move || {
            // Runs on a tokio BLOCKING thread. Sandbox it: a decode/encode
            // panic (a malformed image, or a ratatui-image sixel edge case
            // on an odd box) must fail this ONE image to alt text, never
            // unwind the worker. The terminal is safe regardless — only the
            // run-loop thread restores it (see TERMINAL_OWNER).
            let box_size = ratatui::layout::Size::new(key.w, key.h);
            let protocol = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                crate::img::decode(&raw).ok().and_then(|(image, _)| {
                    crate::img::encode_sliced(&picker, image, box_size, key.crop).ok()
                })
            }))
            .ok()
            .flatten();
            let _ = tx.blocking_send(EncMsg { key, protocol });
        });
    }

    fn on_fetch(&mut self, msg: FetchMsg) {
        self.fetch_rx = None;
        self.fetch_task = None;
        self.notice = false;
        let failed = msg.result.is_err();
        if failed {
            // A failed reload must not make the NEXT navigation replace.
            self.replace_nav = false;
        }
        let width = (self.last_inner.0 as usize).max(10);
        match (msg.result, msg.target) {
            (Ok(Payload::Gopher(raw)), Link::Gopher(url)) => {
                // Image item types go to the viewer, not the document
                // parser ('I' any image, 'g' GIF, 'p' PNG).
                if matches!(url.item_type, 'I' | 'g' | 'p') {
                    self.open_image(Link::Gopher(url), raw);
                    return;
                }
                let cp437 = self.encoding == Encoding::Cp437;
                let doc = gopher::parse(&url, raw, cp437, width);
                self.status = format!("{url} — {} lines", doc.lines.len());
                self.navigate_to(doc);
            }
            (Ok(Payload::OneShot(raw)), Link::OneShot(url)) => {
                let doc = oneshot::parse(&url, raw, width);
                self.status = format!("{url} — {} lines", doc.lines.len());
                self.navigate_to(doc);
            }
            (Ok(Payload::Gemini(response)), _) => self.on_gemini_response(response, width),
            (Ok(Payload::Http(response)), _) => self.on_http_response(response, width),
            (Err(err), target) => {
                self.status = format!("{target} — {err}");
                self.notice = true;
            }
            _ => {}
        }
    }

    /// Act on a gemini response by status class. 3x redirects were
    /// already followed inside the fetch task.
    fn on_gemini_response(&mut self, response: gemini::Response, width: usize) {
        match response.status {
            20..=29 => {
                if response.meta.starts_with("image/") {
                    self.open_image(Link::Gemini(response.url), response.body);
                    return;
                }
                let doc = gemini::parse(&response.url, &response.meta, &response.body, width);
                let media = if response.meta.is_empty() {
                    "text/gemini"
                } else {
                    response.meta.as_str()
                };
                let id = if response.identity { " · ID" } else { "" };
                self.status = format!("{} — {media}{id}", response.url);
                self.navigate_to(doc);
            }
            // 1x: the server wants input; reuse the search prompt.
            // (11 is "sensitive input" — we don't mask the field yet.)
            10..=19 => {
                self.status = if response.meta.is_empty() {
                    String::from("Input requested.")
                } else {
                    response.meta.clone()
                };
                self.search_target = Some(Link::Gemini(response.url));
                self.mode = Mode::Search;
                self.input.clear();
                self.cursor = 0;
                self.select_anchor = None;
            }
            // 60 with nothing on file: offer to mint an identity right
            // there. Everything else in the 6x class (61/62, or a 60
            // even though we presented one) shows the server's words,
            // plus which file spoke for us.
            60..=69 => {
                if response.status == 60 && !response.identity {
                    self.input = std::env::var("USER").unwrap_or_default();
                    self.cursor = self.input.chars().count();
                    self.select_anchor = None;
                    self.cert_for = Some(response.url.clone());
                    self.mode = Mode::Search;
                    self.status = format!(
                        "{} requests an identity — Enter mints a certificate with that name.",
                        response.url
                    );
                } else {
                    let sent = tls::identity_path(&response.url.host)
                        .filter(|_| response.identity)
                        .map(|p| format!(" (sent {})", p.display()))
                        .unwrap_or_default();
                    self.status = format!(
                        "{}: {} {}{sent}",
                        response.url, response.status, response.meta
                    );
                    self.notice = true;
                }
            }
            status => {
                self.status = format!("{}: {} {}", response.url, status, response.meta);
                self.notice = true;
            }
        }
    }

    /// Render an http response. Non-2xx pages still render when the
    /// server sent a usable body (error pages are content); the status
    /// code always lands in the status bar.
    fn on_http_response(&mut self, mut response: http::Response, width: usize) {
        let live = response.live.take();
        // A bot-mitigation interstitial (AWS WAF / Cloudflare): the request
        // got a JS proof-of-work challenge, not the real page, so anything we
        // render is an empty shell. Don't navigate into the blank shell —
        // keep the prior page and say plainly why nothing loaded.
        if let Some(label) = response.challenge.take() {
            drop(live);
            self.status = format!(
                "{}: bot wall ({label}) — the real page is gated behind a challenge TRust can't pass.",
                response.url
            );
            self.notice = true;
            return;
        }
        let media = response
            .content_type
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        // A followed link the server declares as audio/video plays in mpv,
        // not the page view — we never download media bodies (read_response
        // skips them, so what's here is empty). This is the general,
        // content-type-driven catch-all behind the `is_playable_media_url`
        // extension fast-path: an extensionless or oddly-named media stream
        // still plays on click. Keep the page we came from rather than
        // navigating into an empty doc.
        if media.starts_with("audio/") || media.starts_with("video/") {
            drop(live);
            self.launch_mpv(response.url.to_string());
            return;
        }
        if response.body.is_empty() {
            self.status = format!(
                "{}: HTTP {} (empty response)",
                response.url, response.status
            );
            self.notice = true;
            return;
        }
        // Images open the viewer. The sniff fallback covers servers
        // that serve pixels as octet-stream (or with no type at all).
        if media.starts_with("image/")
            || (media == "application/octet-stream" && img::sniff(&response.body).is_some())
        {
            self.open_image(Link::Http(response.url), response.body);
            return;
        }
        let doc = crate::http::parse(
            &response.url,
            &response.content_type,
            &response.body,
            width,
            &self.image_sizes(),
        );
        let image_urls = doc.image_urls.clone();
        let page = response.url.clone();
        // JS visibility: a clean run gets a quiet badge; script errors
        // get a count (the page still rendered — no notice).
        let js_note = match &response.js {
            Some(o) if !o.errors.is_empty() => format!(" · JS:{}!", o.errors.len()),
            Some(o) if o.modules_skipped > 0 => String::from(" · JS (modules skipped)"),
            Some(_) => String::from(" · JS"),
            None => String::new(),
        };
        self.status = if response.status == 200 {
            format!("{} — {media}{js_note}", response.url)
        } else {
            format!(
                "{} — HTTP {} ({media}){js_note}",
                response.url, response.status
            )
        };
        self.navigate_to(doc);
        // navigate_to dropped the previous living page; install this one.
        if let Some(live) = live {
            self.live_page = Some(live.handle);
            self.page_rx = Some(live.events);
            self.page_js_errors = response.js.as_ref().map_or(0, |o| o.errors.len());
        }
        // Kick off the parallel image pipeline; decoded images re-flow in.
        self.start_image_loads(page, image_urls);
    }

    fn drop_live_page(&mut self) {
        self.live_page = None;
        self.page_rx = None;
        self.page_busy = false;
        self.pending_live_submit = None;
    }

    /// Esc in the browser: stop any in-flight load and kill the page's JS
    /// engine, leaving whatever has rendered frozen on screen. It never
    /// closes the browser or drops to the telnet terminal; when nothing is
    /// loading and no engine is alive it's a no-op — you stay on the page.
    /// (A Boa run already executing on a worker thread finishes its current
    /// budget window in the background — there's no mid-run cancel into the
    /// engine — but the UI stops waiting at once and the resident page actor
    /// is dropped, which ends it.)
    fn stop_loading(&mut self) {
        let was_active = self.fetch_rx.is_some()
            || self.imgs_rx.is_some()
            || self.live_page.is_some()
            || self.page_busy;
        if let Some(task) = self.fetch_task.take() {
            task.abort();
        }
        self.fetch_rx = None;
        self.imgs_rx = None;
        self.drop_live_page();
        if was_active {
            self.notice = true;
            self.status = String::from("Stopped — load cancelled, page scripts killed.");
        }
    }

    /// Enter on a `Link::JsClick`: send the click to the living page.
    fn dispatch_click(&mut self, node: usize) {
        let Some(handle) = &self.live_page else {
            // The page's engine is gone — it froze (navigated away) or died
            // during load (e.g. a runaway script tripped the iteration
            // limit). Progressive-enhancement fallback: if this clickable is
            // an anchor with a real href, just navigate it, the way a browser
            // falls back to the `<a href>` when JS isn't available. Pure
            // JS-only controls (no href) genuinely need a reload.
            if let Some(url) = self.selected_web_url() {
                self.navigate_from_page(&url);
            } else {
                self.status = String::from("This page's scripts are no longer running (reload?).");
                self.notice = true;
            }
            return;
        };
        match handle.cmds.try_send(crate::js::PageCmd::Click(node)) {
            Ok(()) => {
                self.page_busy = true;
                self.status = String::from("· click dispatched to page script");
            }
            Err(_) => {
                self.status = String::from("Page script is busy; try again.");
                self.notice = true;
            }
        }
    }

    fn dispatch_live_form_set(
        &mut self,
        node: usize,
        value: String,
        checked: Option<bool>,
        status: String,
    ) -> bool {
        let Some(handle) = &self.live_page else {
            return false;
        };
        match handle.cmds.try_send(crate::js::PageCmd::SetValue {
            node,
            value,
            checked,
        }) {
            Ok(()) => {
                self.page_busy = true;
                self.status = status;
                true
            }
            Err(_) => {
                self.status = String::from("Page script is busy; try again.");
                self.notice = true;
                true
            }
        }
    }

    fn dispatch_live_submit(
        &mut self,
        form_index: usize,
        field_index: usize,
        form_node: usize,
        submitter_node: Option<usize>,
    ) -> bool {
        let Some(handle) = &self.live_page else {
            return false;
        };
        match handle.cmds.try_send(crate::js::PageCmd::Submit {
            form: form_node,
            submitter: submitter_node,
        }) {
            Ok(()) => {
                self.page_busy = true;
                self.pending_live_submit = Some((form_index, field_index));
                self.status = String::from("· submit dispatched to page script");
                true
            }
            Err(_) => {
                self.status = String::from("Page script is busy; try again.");
                self.notice = true;
                true
            }
        }
    }

    /// A living page spoke. Updates are COALESCED: when several renders
    /// queued up, only the newest is parsed (parsing is the cost, not
    /// drawing) — the redraw-economy requirement.
    fn on_page_evt(&mut self, evt: crate::js::PageEvt) {
        use crate::js::PageEvt;
        self.page_busy = false;
        let mut latest_update: Option<(String, crate::js::Outcome)> = None;
        let mut trouble: Vec<String> = Vec::new();
        let mut navigate: Option<String> = None;
        let mut submit_default = false;
        // A click-triggered native submit carries its form/submitter arena
        // nodes (the app didn't pre-record them the way the Submit path does).
        let mut submit_nodes: Option<(usize, usize)> = None;
        let pending_submit = self.pending_live_submit.take();
        let mut pending = Some(evt);
        loop {
            match pending {
                Some(PageEvt::Updated { html, outcome } | PageEvt::Static { html, outcome }) => {
                    latest_update = Some((html, outcome));
                }
                Some(PageEvt::Trouble(errors)) => trouble.extend(errors),
                Some(PageEvt::Settled) => {}
                Some(PageEvt::SubmitDefault) => submit_default = true,
                Some(PageEvt::SubmitForm { form, submitter }) => {
                    submit_nodes = Some((form, submitter));
                }
                Some(PageEvt::Navigate(url)) => navigate = Some(url),
                None => break,
            }
            pending = self.page_rx.as_mut().and_then(|rx| rx.try_recv().ok());
        }

        if let Some((html, outcome)) = latest_update {
            self.page_js_errors += outcome.errors.len();
            self.replace_live_doc(html.into_bytes());
            self.status = if self.page_js_errors > 0 {
                format!("page updated · JS:{}!", self.page_js_errors)
            } else {
                String::from("page updated · JS")
            };
        }
        if !trouble.is_empty() {
            self.page_js_errors += trouble.len();
            self.status = format!("page JS: {} (JS:{}!)", trouble[0], self.page_js_errors);
            self.notice = true;
        }
        if submit_default && let Some((form, field)) = pending_submit {
            self.submit_form_static(form, field);
        }
        if let Some((form_node, submitter_node)) = submit_nodes
            && let Some((form, field)) = self.form_indices_for_nodes(form_node, submitter_node)
        {
            self.submit_form_static(form, field);
        }
        if let Some(url) = navigate {
            // An un-prevented click on a live anchor: a real navigation, so it
            // carries the page's Referer like any followed link.
            self.navigate_from_page(&url);
        }
    }

    /// Swap the living page's fresh render into the browser doc:
    /// history untouched, scroll kept, selection re-found by TARGET
    /// (line indices shift under a mutating page; the gopherus
    /// navigation model must not jumble).
    fn replace_live_doc(&mut self, raw: Vec<u8>) {
        let width = (self.last_inner.0 as usize).max(10);
        let height = self.last_inner.1.max(1) as usize;
        let images = self.image_sizes();
        let Some(g) = &mut self.browser else { return };
        let Link::Http(url) = g.doc.url.clone() else {
            return;
        };
        // Remember the selected item by its arena node (and link) so the
        // selection survives the DOM mutating under it.
        let selected_target = g
            .sel_item
            .and_then(|(r, i)| g.doc.rows.get(r).and_then(|row| row.items.get(i)).cloned());
        // Was that selection ON-SCREEN before this update? Only then may we
        // re-center the viewport onto it (the update pushed a visible selection
        // off-screen). If the user had scrolled it out of view, their scroll
        // position is sacred: now that the engine runs at rest, an AUTONOMOUS
        // re-render (a timer/animation tick) arrives here repeatedly, and must
        // never drag the viewport back to an off-screen selection every frame
        // while the user reads elsewhere. (`browser_scroll` deliberately leaves
        // the selection put when the wheel scrolls a laid-out doc.)
        let sel_was_visible = g
            .sel_item
            .is_some_and(|(r, _)| r >= g.scroll && r < g.scroll + height);
        let doc = http::parse_seeded(&url, "text/html; charset=utf-8", &raw, width, None, &images);
        g.doc = doc;
        g.sel_item = selected_target
            .and_then(|target| Self::find_item_like(&g.doc.rows, &target))
            // Lost it? Fall back to the first interactive item in view.
            .or_else(|| Self::http_first_visible_item(g, height));
        let max_scroll = g.doc.rows.len().saturating_sub(height);
        g.scroll = match g.sel_item {
            // Keep the selection visible if an update pushed it off-screen —
            // but only if it was visible to begin with (see `sel_was_visible`).
            Some((r, _)) if sel_was_visible && (r < g.scroll || r >= g.scroll + height) => {
                r.saturating_sub(height / 2).min(max_scroll)
            }
            _ => g.scroll.min(max_scroll),
        };
        // A live update can introduce images that weren't in the prior
        // render: archive.org's collection tiles (and any SPA's lazily
        // mounted thumbnails) are fetched AFTER first paint and filled in
        // during settle, arriving here — not on the initial-load path that
        // kicks off the parallel image pipeline. Without this the tiles'
        // <img>s never decode and stay alt text. `start_image_loads` skips
        // anything already cached, so a shell image still in flight (not
        // yet cached) is merely re-fetched by the new batch, never lost,
        // and the per-batch channel still closes when done (idle-CPU gate).
        let image_urls = g.doc.image_urls.clone();
        self.start_image_loads(url, image_urls);
    }

    /// Find the `(row, item)` matching `target` in fresh rows: same arena
    /// node wins (a control that moved), else same link target.
    fn find_item_like(
        rows: &[crate::layout::Row],
        target: &crate::layout::Item,
    ) -> Option<(usize, usize)> {
        for (r, row) in rows.iter().enumerate() {
            for (i, it) in row.items.iter().enumerate() {
                if !it.is_interactive() {
                    continue;
                }
                let same = (target.node != crate::layout::NO_NODE && it.node == target.node)
                    || (it.link.is_some() && it.link == target.link);
                if same {
                    return Some((r, i));
                }
            }
        }
        None
    }

    /// Show a fetched document, pushing the current one onto the back
    /// history (RAM-only, dropped when the view closes).
    fn navigate_to(&mut self, doc: Doc) {
        // A new page replaces any image that was being viewed, and ends
        // whatever living page came before it (freeze: its last render
        // is already the doc going into history).
        self.viewer = None;
        // Capture before dropping: a doc going into history that had a
        // living engine should be revived (not restored static) on back.
        let was_live = self.live_page.is_some();
        self.drop_live_page();
        let replace = std::mem::take(&mut self.replace_nav);
        match &mut self.browser {
            Some(g) if replace => {
                // Reload: swap the document in place, keep history and
                // ride the scroll (clamped to the fresh content).
                g.doc = doc;
                g.selected = None;
                g.sel_item = None;
                g.scroll = g.scroll.min(g.doc.extent().saturating_sub(1));
            }
            Some(g) => {
                let old = std::mem::replace(&mut g.doc, doc);
                let pos = ViewPos {
                    selected: g.selected,
                    sel_item: g.sel_item,
                    was_live,
                };
                g.history.push((old, pos, g.scroll));
                g.selected = None;
                g.sel_item = None;
                g.scroll = 0;
            }
            None => {
                self.browser = Some(BrowserView {
                    doc,
                    selected: None,
                    sel_item: None,
                    scroll: 0,
                    history: Vec::new(),
                });
            }
        }
        // A fresh page selects its first interactive target.
        let height = self.last_inner.1.max(1) as usize;
        if let Some(g) = &mut self.browser {
            if g.doc.laid_out() {
                g.sel_item = Self::http_first_visible_item(g, height);
            } else {
                g.selected = Self::browser_visible_links(g, height).first().copied();
            }
        }
    }

    /// gopherus keys: Up/Down scroll the page (the highlight rides the
    /// visible links), Right follows, Left goes back, Esc closes.
    fn browser_nav(&mut self, key: KeyEvent) {
        self.notice = false;
        let page = i64::from(self.last_inner.1.max(2)) - 1;
        match key.code {
            KeyCode::Up => self.browser_arrow(-1),
            KeyCode::Down => self.browser_arrow(1),
            KeyCode::PageUp => self.browser_scroll(-page, false),
            KeyCode::PageDown => self.browser_scroll(page, false),
            KeyCode::Home => self.browser_scroll(i64::MIN / 2, false),
            KeyCode::End => self.browser_scroll(i64::MAX / 2, false),
            KeyCode::Right | KeyCode::Enter => self.browser_follow(),
            KeyCode::Left => self.browser_back(),
            KeyCode::Char('v' | 'V') => self.open_in_mpv(),
            KeyCode::Esc => self.stop_loading(),
            _ => {}
        }
    }

    fn http_mouse_hover(&mut self, col: u16, row: u16) -> bool {
        let Some(target) = self.http_hit_test(col, row) else {
            if self.mouse_in_content_area(col, row)
                && self.browser.as_ref().is_some_and(|g| g.doc.laid_out())
                && let Some(g) = &mut self.browser
            {
                g.sel_item = None;
            }
            return false;
        };
        if let Some(g) = &mut self.browser {
            g.sel_item = Some(target);
        }
        true
    }

    fn mouse_in_content_area(&self, col: u16, row: u16) -> bool {
        let a = self.last_content_area;
        col >= a.x
            && col < a.x.saturating_add(a.width)
            && row >= a.y
            && row < a.y.saturating_add(a.height)
    }

    fn http_hit_test(&self, col: u16, row: u16) -> Option<(usize, usize)> {
        if !self.mouse_in_content_area(col, row) {
            return None;
        }
        let g = self.browser.as_ref()?;
        if !g.doc.laid_out() {
            return None;
        }
        let local_row = row.saturating_sub(self.last_content_area.y) as usize;
        let doc_row = g.scroll + local_row;
        if doc_row >= g.doc.rows.len() {
            return None;
        }
        let local_col = col.saturating_sub(self.last_content_area.x);
        (g.scroll..=doc_row).rev().find_map(|r| {
            let row_offset = doc_row.saturating_sub(r);
            let row = g.doc.rows.get(r)?;
            // Use the SAME on-screen placement the renderer draws (carousel
            // clip + gap-fill + overlap-append), so a click lands on the item
            // actually under the cursor — not the raw `item.col`, which diverges
            // when items overlap (an overlay drawn after the content it covers).
            crate::layout::visual_columns(row, &g.doc.carousels, r)
                .into_iter()
                .find_map(|(i, start)| {
                    let item = &row.items[i];
                    let end = start.saturating_add(item.width);
                    let covers_row = row_offset < item.height.max(1) as usize;
                    (item.is_interactive() && covers_row && local_col >= start && local_col < end)
                        .then_some((r, i))
                })
        })
    }

    /// HTTP 2D navigation: Enter follows, Backspace goes back, Up/Down
    /// move the selection to the nearest interactive item in an adjacent
    /// row, Left/Right step between items (spilling to adjacent rows),
    /// Esc closes. The arrows are free here because nav lives on
    /// Enter/Backspace — the HTTP-only layout model.
    fn http_nav(&mut self, key: KeyEvent) {
        self.notice = false;
        let page = i64::from(self.last_inner.1.max(2)) - 1;
        match key.code {
            KeyCode::Up => self.http_move(-1, false),
            KeyCode::Down => self.http_move(1, false),
            // In a carousel, ←/→ scroll the strip a card at a time; elsewhere
            // they move the selection laterally.
            KeyCode::Left if self.scroll_selected_carousel(-1) => {}
            KeyCode::Right if self.scroll_selected_carousel(1) => {}
            KeyCode::Left => self.http_move(-1, true),
            KeyCode::Right => self.http_move(1, true),
            KeyCode::PageUp => self.http_scroll(-page),
            KeyCode::PageDown => self.http_scroll(page),
            KeyCode::Home => self.http_scroll(i64::MIN / 2),
            KeyCode::End => self.http_scroll(i64::MAX / 2),
            KeyCode::Enter => self.browser_follow(),
            KeyCode::Backspace => self.browser_back(),
            KeyCode::Char('v' | 'V') => self.open_in_mpv(),
            KeyCode::Esc => self.stop_loading(),
            _ => {}
        }
    }

    /// If the selection sits in a horizontal carousel, scroll it one card
    /// (`dir` ±1) and re-anchor the selection to the first card now visible.
    /// Returns whether a carousel handled the key.
    fn scroll_selected_carousel(&mut self, dir: i32) -> bool {
        let Some(g) = self.browser.as_mut() else {
            return false;
        };
        let Some((row, _)) = g.sel_item else {
            return false;
        };
        let Some(idx) = g.doc.carousels.iter().position(|c| c.contains_row(row)) else {
            return false;
        };
        g.doc.carousels[idx].scroll_cards(dir);
        // Re-anchor onto the first interactive card now in the band, so the
        // highlight stays visible and Enter follows something on screen.
        let c = &g.doc.carousels[idx];
        let (start, end, left) = (c.start, c.end, c.left);
        let target = (start..end).find_map(|r| {
            g.doc
                .rows
                .get(r)?
                .items
                .iter()
                .enumerate()
                .find_map(|(i, it)| {
                    (it.link.is_some() && it.col >= left && c.shows(it.col, it.width))
                        .then_some((r, i))
                })
        });
        if let Some(sel) = target {
            g.sel_item = Some(sel);
        }
        true
    }

    /// If the activated item is a generated carousel scroll control (the
    /// `‹`/`›` glyphs), page the nearest carousel in its direction instead of
    /// navigating. The selection stays on the control so repeated Enter/clicks
    /// keep paging, like a real scroll button. Returns whether it handled it.
    fn activate_carousel_control(&mut self) -> bool {
        let Some(g) = self.browser.as_mut() else {
            return false;
        };
        let Some((row, idx)) = g.sel_item else {
            return false;
        };
        let dir = match g.doc.rows.get(row).and_then(|r| r.items.get(idx)) {
            Some(it) => match it.link {
                Some(crate::doc::Link::CarouselScroll(d)) => i32::from(d),
                _ => return false,
            },
            None => return false,
        };
        // The control sits just above its band; page the nearest carousel
        // whose band starts at or below the control's row.
        let Some(c) = g
            .doc
            .carousels
            .iter_mut()
            .filter(|c| c.end > row)
            .min_by_key(|c| c.start.abs_diff(row))
        else {
            return false;
        };
        c.scroll_page(dir);
        true
    }

    /// Interactive item indices on a row (followable links; form controls
    /// fold in later).
    fn row_interactives(row: &crate::layout::Row) -> Vec<usize> {
        Self::row_interactives_excluding(row, crate::layout::NO_NODE)
    }

    /// Interactive item indices on a row, excluding pieces of `skip` (a
    /// link that wrapped onto this row) so navigation steps link-to-link
    /// rather than through one link's own wrapped fragments.
    fn row_interactives_excluding(row: &crate::layout::Row, skip: usize) -> Vec<usize> {
        row.items
            .iter()
            .enumerate()
            .filter(|(_, it)| {
                it.link.is_some() && (skip == crate::layout::NO_NODE || it.node != skip)
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// The first interactive item in the viewport, `(row, item)`.
    fn http_first_visible_item(g: &BrowserView, height: usize) -> Option<(usize, usize)> {
        let end = (g.scroll + height).min(g.doc.rows.len());
        (g.scroll..end).find_map(|r| {
            Self::row_interactives(&g.doc.rows[r])
                .first()
                .map(|&i| (r, i))
        })
    }

    /// Move the item selection. `horizontal` steps within/between rows in
    /// document order; otherwise it jumps to the column-nearest item in an
    /// adjacent row. The page scrolls to keep the new selection visible.
    fn http_move(&mut self, dir: i64, horizontal: bool) {
        let height = self.last_inner.1.max(1) as usize;
        let Some(g) = &mut self.browser else { return };
        let rows = &g.doc.rows;
        if rows.is_empty() {
            return;
        }
        // No selection yet: take the first/last interactive item on screen.
        let Some((cr, ci)) = g.sel_item else {
            g.sel_item = Self::http_first_visible_item(g, height);
            self.http_keep_visible();
            return;
        };

        let target = if horizontal {
            Self::http_step_horizontal(rows, cr, ci, dir)
        } else {
            Self::http_step_vertical(rows, cr, ci, dir)
        };
        if let Some(next) = target {
            g.sel_item = Some(next);
        }
        self.http_keep_visible();
    }

    /// Next interactive item in document order from `(cr, ci)`, scanning
    /// items on the current row first, then spilling into later/earlier
    /// rows.
    fn http_step_horizontal(
        rows: &[crate::layout::Row],
        cr: usize,
        ci: usize,
        dir: i64,
    ) -> Option<(usize, usize)> {
        let cur_node = rows[cr]
            .items
            .get(ci)
            .map_or(crate::layout::NO_NODE, |it| it.node);
        let here = Self::row_interactives_excluding(&rows[cr], cur_node);
        if dir > 0 {
            if let Some(&i) = here.iter().find(|&&i| i > ci) {
                return Some((cr, i));
            }
            ((cr + 1)..rows.len()).find_map(|r| {
                Self::row_interactives_excluding(&rows[r], cur_node)
                    .first()
                    .map(|&i| (r, i))
            })
        } else {
            if let Some(&i) = here.iter().rev().find(|&&i| i < ci) {
                return Some((cr, i));
            }
            (0..cr).rev().find_map(|r| {
                Self::row_interactives_excluding(&rows[r], cur_node)
                    .last()
                    .map(|&i| (r, i))
            })
        }
    }

    /// The interactive item in the next row (in `dir`) whose column is
    /// nearest the current item's column.
    fn http_step_vertical(
        rows: &[crate::layout::Row],
        cr: usize,
        ci: usize,
        dir: i64,
    ) -> Option<(usize, usize)> {
        let cur_col = rows[cr].items.get(ci).map_or(0, |it| it.col);
        let cur_node = rows[cr]
            .items
            .get(ci)
            .map_or(crate::layout::NO_NODE, |it| it.node);
        let candidates: Box<dyn Iterator<Item = usize>> = if dir > 0 {
            Box::new((cr + 1)..rows.len())
        } else {
            Box::new((0..cr).rev())
        };
        for r in candidates {
            let inter = Self::row_interactives_excluding(&rows[r], cur_node);
            if let Some(&best) = inter
                .iter()
                .min_by_key(|&&i| (i32::from(rows[r].items[i].col) - i32::from(cur_col)).abs())
            {
                return Some((r, best));
            }
        }
        None
    }

    /// Scroll the HTTP viewport by `delta` rows, dropping any selection
    /// that scrolls out of view onto the nearest visible interactive item.
    fn http_scroll(&mut self, delta: i64) {
        let height = self.last_inner.1.max(1) as usize;
        let Some(g) = &mut self.browser else { return };
        let max_scroll = g.doc.rows.len().saturating_sub(height);
        let target = (g.scroll as i64)
            .saturating_add(delta)
            .clamp(0, max_scroll as i64);
        g.scroll = target as usize;
        // Re-aim the selection into the viewport if it scrolled away.
        if let Some((r, _)) = g.sel_item
            && (r < g.scroll || r >= g.scroll + height)
        {
            g.sel_item = Self::http_first_visible_item(g, height);
        } else if g.sel_item.is_none() {
            g.sel_item = Self::http_first_visible_item(g, height);
        }
    }

    /// Scroll just enough that the selected item's row is on screen,
    /// keeping it roughly centered when it would otherwise be clipped.
    fn http_keep_visible(&mut self) {
        let height = self.last_inner.1.max(1) as usize;
        let Some(g) = &mut self.browser else { return };
        let Some((r, _)) = g.sel_item else { return };
        let max_scroll = g.doc.rows.len().saturating_sub(height);
        if r < g.scroll {
            g.scroll = r.min(max_scroll);
        } else if r >= g.scroll + height {
            g.scroll = r.saturating_sub(height / 2).min(max_scroll);
        }
    }

    /// One Up/Down press, gopherus-style. If the adjacent line is also a
    /// link, the highlight steps onto it (the page scrolls along once the
    /// selection has reached the center of the screen, so it tends to stay
    /// there except near the document's ends). Otherwise the page scrolls
    /// under the sticky selection, and `browser_retarget` decides when a
    /// new link takes the highlight. With the page pinned at either end,
    /// the highlight walks between the visible links instead.
    fn browser_arrow(&mut self, dir: i64) {
        let height = self.last_inner.1.max(1) as i64;
        let center_row = (height - 1) / 2;
        let Some(g) = &mut self.browser else { return };
        let len = g.doc.lines.len() as i64;
        let max_scroll = (len - height).max(0);

        // Adjacent link: the highlight transitions instead of sticking.
        if let Some(sel) = g.selected {
            let next = sel as i64 + dir;
            if (0..len).contains(&next) && g.doc.lines[next as usize].link.is_some() {
                g.selected = Some(next as usize);
                let row = next - g.scroll as i64;
                if dir > 0 && row > center_row && (g.scroll as i64) < max_scroll {
                    g.scroll += 1;
                } else if dir < 0 && row < center_row && g.scroll > 0 {
                    g.scroll -= 1;
                }
                return;
            }
        }

        let can_scroll = if dir > 0 {
            (g.scroll as i64) < max_scroll
        } else {
            g.scroll > 0
        };
        if can_scroll {
            g.scroll = (g.scroll as i64 + dir) as usize;
            self.browser_retarget(dir);
        } else {
            self.browser_walk(dir);
        }
    }

    /// Scroll the viewport by `delta` lines (wheel, page keys, jumps) and
    /// re-aim the highlight. When the viewport can't move and
    /// `walk_at_edge` is set, step the highlight instead.
    fn browser_scroll(&mut self, delta: i64, walk_at_edge: bool) {
        let height = self.last_inner.1.max(1) as i64;
        let Some(g) = &mut self.browser else { return };
        // HTTP laid-out docs index by `rows` (their `lines` is empty), and
        // use the 2D item selection, not the gopherus line highlight. The
        // wheel is the one pure viewport scroll: move `scroll` line-by-line,
        // clamp, and leave the selection where it is.
        if g.doc.laid_out() {
            let max_scroll = (g.doc.rows.len() as i64 - height).max(0);
            g.scroll = (g.scroll as i64).saturating_add(delta).clamp(0, max_scroll) as usize;
            return;
        }
        let len = g.doc.lines.len() as i64;
        let max_scroll = (len - height).max(0);
        let target = (g.scroll as i64).saturating_add(delta).clamp(0, max_scroll);
        let moved = target != g.scroll as i64;
        g.scroll = target as usize;
        let dir = if delta >= 0 { 1 } else { -1 };
        if moved {
            self.browser_retarget(dir);
        } else if walk_at_edge && delta != 0 {
            self.browser_walk(dir);
        }
    }

    fn pending_browser_wrap_target(&self) -> Option<(usize, bool)> {
        let width = (self.last_inner.0 as usize).max(10);
        let cp437 = self.encoding == Encoding::Cp437;
        let g = self.browser.as_ref()?;
        if g.doc.raw.is_empty() || (g.doc.wrapped_to == width && g.doc.cp437 == cp437) {
            return None;
        }
        Some((width, cp437))
    }

    /// Re-wrap (and re-decode) the current document when the panel width
    /// or the encoding changed since it was parsed — including documents
    /// restored from history at an older width. The selection is carried
    /// over by its position in the document's link order.
    fn sync_browser_wrap(&mut self) {
        let width = (self.last_inner.0 as usize).max(10);
        let cp437 = self.encoding == Encoding::Cp437;
        let height = self.last_inner.1.max(1) as usize;
        let images = self.image_sizes();
        let Some(g) = &mut self.browser else { return };
        if g.doc.raw.is_empty() || (g.doc.wrapped_to == width && g.doc.cp437 == cp437) {
            return;
        }
        let link_ordinal = g.selected.map(|sel| {
            g.doc.lines[..=sel]
                .iter()
                .filter(|l| l.link.is_some())
                .count()
        });
        // For laid-out docs, carry the selection over by its item identity.
        let item_target = g
            .sel_item
            .and_then(|(r, i)| g.doc.rows.get(r).and_then(|row| row.items.get(i)).cloned());
        let raw = std::mem::take(&mut g.doc.raw);
        g.doc = match g.doc.url.clone() {
            Link::Gopher(url) => gopher::parse(&url, raw, cp437, width),
            Link::Gemini(url) => {
                let meta = g.doc.meta.clone().unwrap_or_default();
                gemini::parse(&url, &meta, &raw, width)
            }
            Link::Http(url) => {
                let meta = g.doc.meta.clone().unwrap_or_default();
                // Seed so typed-in form values survive the re-parse.
                let forms = std::mem::take(&mut g.doc.forms);
                http::parse_seeded(&url, &meta, &raw, width, Some(&forms), &images)
            }
            Link::OneShot(url) => oneshot::parse(&url, raw, width),
            Link::Form { .. } | Link::JsClick { .. } | Link::External(_) => return,
            Link::CarouselScroll(_) => return,
        };
        if g.doc.laid_out() {
            // Re-flow at the new width re-laid the rows; re-anchor the
            // selected item by identity.
            g.sel_item = item_target.and_then(|t| Self::find_item_like(&g.doc.rows, &t));
            let max_scroll = g.doc.rows.len().saturating_sub(height);
            g.scroll = match g.sel_item {
                Some((r, _)) => r.saturating_sub(height / 2).min(max_scroll),
                None => g.scroll.min(max_scroll),
            };
            return;
        }
        g.selected = link_ordinal.and_then(|n| {
            g.doc
                .lines
                .iter()
                .enumerate()
                .filter(|(_, l)| l.link.is_some())
                .map(|(i, _)| i)
                .nth(n - 1)
        });
        // Keep the selection on screen, roughly centered.
        let max_scroll = g.doc.lines.len().saturating_sub(height);
        g.scroll = match g.selected {
            Some(sel) => sel.saturating_sub(height / 2).min(max_scroll),
            None => g.scroll.min(max_scroll),
        };
    }

    /// Indices of the links currently in the viewport.
    fn browser_visible_links(g: &BrowserView, height: usize) -> Vec<usize> {
        let visible = g.scroll..(g.scroll + height).min(g.doc.lines.len());
        g.doc.lines[visible.clone()]
            .iter()
            .enumerate()
            .filter(|(_, l)| l.link.is_some())
            .map(|(off, _)| visible.start + off)
            .collect()
    }

    /// After the viewport moved: the selection is sticky, but hands the
    /// highlight to the *next* link in the travel direction (nearest in
    /// document order — never skipping over links) whenever that handoff
    /// moves the highlight closer to the center of the screen. Stepping
    /// nearest-first means the highlight walks inward from the page edges
    /// and then sticks near the center. A selection that scrolled off
    /// screen is replaced by the nearest visible link it left behind;
    /// none visible means none highlighted.
    fn browser_retarget(&mut self, dir: i64) {
        let height = self.last_inner.1.max(1) as usize;
        let Some(g) = &mut self.browser else { return };
        let links = Self::browser_visible_links(g, height);
        if links.is_empty() {
            g.selected = None;
            return;
        }
        let center = g.scroll as i64 + (height as i64 - 1) / 2;
        let dist = |i: usize| (i as i64 - center).abs();
        let visible = g.scroll..g.scroll + height;
        match g.selected {
            Some(cur) if visible.contains(&cur) => {
                let next = if dir > 0 {
                    links.iter().copied().find(|&i| i > cur)
                } else {
                    links.iter().rev().copied().find(|&i| i < cur)
                };
                if let Some(next) = next
                    && dist(next) < dist(cur)
                {
                    g.selected = Some(next);
                }
            }
            _ => {
                // The old selection left through one edge (or none existed):
                // take the nearest link from that side of the viewport.
                g.selected = Some(if dir > 0 {
                    links[0]
                } else {
                    *links.last().unwrap()
                });
            }
        }
    }

    /// Step the highlight between the visible links (used when the page
    /// is pinned at the top or bottom of the document).
    fn browser_walk(&mut self, dir: i64) {
        let height = self.last_inner.1.max(1) as usize;
        let Some(g) = &mut self.browser else { return };
        let links = Self::browser_visible_links(g, height);
        g.selected = match (g.selected, links.as_slice()) {
            (_, []) => None,
            (None, links) => Some(if dir > 0 {
                links[0]
            } else {
                *links.last().unwrap()
            }),
            (Some(cur), links) => Some(if dir > 0 {
                links.iter().copied().find(|&i| i > cur).unwrap_or(cur)
            } else {
                links
                    .iter()
                    .rev()
                    .copied()
                    .find(|&i| i < cur)
                    .unwrap_or(cur)
            }),
        };
    }

    fn browser_follow(&mut self) {
        // A carousel's own prev/next button scrolls the strip rather than
        // navigating — its JS click can't reach our parse-time layout, so we
        // page the band ourselves (the CSS `::scroll-button` behavior).
        if self.activate_carousel_control() {
            return;
        }
        // Auto-route recognized video links (YouTube and its various
        // formats) straight to mpv, in EVERY view — people post these on
        // gopher and gemini too, and following one should play it, not try
        // to render YouTube. Manual `v` covers any other web link.
        if let Some(url) = self.selected_web_url()
            && is_youtube_video_url(&url)
        {
            self.launch_mpv(url);
            return;
        }
        // A direct link to media mpv can play (a video/audio file, or the
        // source behind a `<video>`/`<audio>` representation) opens in mpv too
        // — in every view, automatic for everything mpv routinely plays.
        if let Some(url) = self.selected_web_url()
            && is_playable_media_url(&url)
        {
            self.launch_mpv(url);
            return;
        }
        let Some(link) = self.selected_link() else {
            return;
        };
        match link {
            Link::Gopher(url) => match url.item_type {
                '0' | '1' | 'I' | 'g' | 'p' => self.start_fetch(Link::Gopher(url)),
                '7' => {
                    self.search_target = Some(Link::Gopher(url));
                    self.mode = Mode::Search;
                    self.input.clear();
                    self.cursor = 0;
                    self.select_anchor = None;
                }
                other => self.status = format!("item type '{other}' not supported yet"),
            },
            Link::Gemini(url) => self.start_fetch(Link::Gemini(url)),
            Link::Http(url) => {
                let referrer = self.http_referrer();
                self.start_fetch_opts(Link::Http(url), false, referrer);
            }
            Link::OneShot(url) => self.start_fetch(Link::OneShot(url)),
            Link::Form { form, field } => self.form_interact(form, field),
            Link::JsClick { node, .. } => self.dispatch_click(node),
            Link::External(target) => {
                self.status = format!("external link: {target}");
            }
            // Handled by `activate_carousel_control` before this match.
            Link::CarouselScroll(_) => {}
        }
    }

    /// `v`: hand the selected link's URL to mpv, which plays direct video
    /// files and — via yt-dlp — YouTube, Vimeo, and hundreds of others.
    /// TRust shows the page; mpv plays the stream. (TRust's first
    /// external-process delegation; nothing persists.) No-op with a notice
    /// when nothing web-shaped is selected or mpv isn't on PATH.
    fn open_in_mpv(&mut self) {
        let Some(url) = self.selected_web_url() else {
            self.status = String::from("Select a web link first (v opens it in mpv).");
            self.notice = true;
            return;
        };
        self.launch_mpv(url);
    }

    /// Spawn mpv on a URL, detached. Shared by the `v` key, the automatic
    /// YouTube routing, and the `<video>`/`<audio>` → mpv path.
    fn launch_mpv(&mut self, url: String) {
        // notice so the confirmation/error shows over the selected-link
        // hint until the next keypress.
        self.notice = true;
        let mut cmd = std::process::Command::new("mpv");
        // Many media hosts hotlink-protect their files with a Referer check —
        // erome's mp4s 403 without one. Hand mpv the current page as referrer
        // so a direct media URL plays (harmless for yt-dlp-handled links).
        if let Some(Link::Http(page)) = self.browser.as_ref().map(|g| &g.doc.url) {
            cmd.arg(format!("--referrer={page}"));
        }
        match cmd
            .arg(&url)
            // Detach from our tty so mpv can't fight ratatui for the screen;
            // it opens its own window. We don't wait on it.
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(_) => self.status = format!("▶ mpv {url}"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                self.status = String::from("mpv not found on PATH (can't play video).");
            }
            Err(e) => self.status = format!("mpv failed to launch: {e}"),
        }
    }

    /// The active selection's link, whichever model the doc uses (HTTP
    /// item selection or the gopher/gemini line selection).
    pub(crate) fn selected_link(&self) -> Option<Link> {
        let g = self.browser.as_ref()?;
        if g.doc.laid_out() {
            let (r, i) = g.sel_item?;
            g.doc.rows.get(r)?.items.get(i)?.link.clone()
        } else {
            g.selected
                .and_then(|i| g.doc.lines.get(i))
                .and_then(|l| l.link.clone())
        }
    }

    /// The selected link as an http(s) URL string, for handing to an
    /// external program. Relative JS-page hrefs resolve against the page;
    /// foreign schemes (mailto:, …) and non-link selections return None.
    fn selected_web_url(&self) -> Option<String> {
        let g = self.browser.as_ref()?;
        let link = self.selected_link()?;
        let raw: String = match &link {
            Link::Http(url) => url.to_string(),
            Link::JsClick { href, .. } if !href.is_empty() => href.clone(),
            Link::External(s) => s.clone(),
            _ => return None,
        };
        if let Ok(u) = url::Url::parse(&raw) {
            return matches!(u.scheme(), "http" | "https").then(|| u.to_string());
        }
        // Relative href on a living JS page: resolve against the page URL.
        if let Link::Http(base) = &g.doc.url
            && let Ok(u) = base.join(&raw)
            && matches!(u.scheme(), "http" | "https")
        {
            return Some(u.to_string());
        }
        None
    }

    /// Enter on a form control: edit, toggle, cycle, or submit per kind.
    fn form_interact(&mut self, form: usize, field: usize) {
        use crate::doc::FieldKind;
        let Some((kind, name, value, checked, live_node)) = self
            .browser
            .as_ref()
            .and_then(|g| g.doc.forms.get(form))
            .and_then(|f| f.fields.get(field))
            .map(|f| {
                (
                    f.kind.clone(),
                    f.name.clone(),
                    f.value.clone(),
                    f.checked,
                    f.live_node,
                )
            })
        else {
            return;
        };
        match kind {
            FieldKind::Text | FieldKind::Password | FieldKind::Textarea => {
                self.input = value;
                self.cursor = self.input.chars().count();
                self.select_anchor = None;
                self.search_target = Some(Link::Form { form, field });
                self.mode = Mode::Search;
                self.status = format!("Editing {name} — Enter sets, Esc cancels.");
            }
            FieldKind::Checkbox => {
                if let Some(node) = live_node
                    && self.dispatch_live_form_set(
                        node,
                        String::new(),
                        Some(!checked),
                        format!("· {name} changed by page script"),
                    )
                {
                    return;
                }
                if let Some(g) = &mut self.browser {
                    let f = &mut g.doc.forms[form].fields[field];
                    f.checked = !f.checked;
                }
                self.refresh_forms();
            }
            FieldKind::Radio => {
                if let Some(node) = live_node
                    && self.dispatch_live_form_set(
                        node,
                        value,
                        Some(true),
                        format!("· {name} changed by page script"),
                    )
                {
                    return;
                }
                if let Some(g) = &mut self.browser {
                    for (i, f) in g.doc.forms[form].fields.iter_mut().enumerate() {
                        if f.kind == FieldKind::Radio && f.name == name {
                            f.checked = i == field;
                        }
                    }
                }
                self.refresh_forms();
            }
            FieldKind::Select(options) => self.open_select_menu(form, field, options, &value),
            FieldKind::Submit => self.submit_form(form, field),
            FieldKind::Hidden => {}
        }
    }

    /// Open the dropdown overlay for a `<select>`, highlighting its current
    /// value and anchoring to the field's position on the page.
    fn open_select_menu(
        &mut self,
        form: usize,
        field: usize,
        options: Vec<(String, String)>,
        value: &str,
    ) {
        if options.is_empty() {
            return;
        }
        let highlight = options.iter().position(|(_, v)| v == value).unwrap_or(0);
        let (anchor_row, anchor_col) = self.browser.as_ref().map_or((0, 0), |g| match g.sel_item {
            Some((r, i)) => (
                r,
                g.doc
                    .rows
                    .get(r)
                    .and_then(|row| row.items.get(i))
                    .map_or(0, |it| it.col),
            ),
            None => (g.scroll, 0),
        });
        self.select_menu = Some(SelectMenu {
            form,
            field,
            options,
            highlight,
            scroll: 0,
            anchor_row,
            anchor_col,
        });
        self.status = String::from("Select — ↑↓ choose · Enter set · Esc cancel");
    }

    /// Keys while a `<select>` dropdown is open.
    fn select_menu_nav(&mut self, key: KeyEvent) {
        let Some(menu) = self.select_menu.as_mut() else {
            return;
        };
        let last = menu.options.len().saturating_sub(1);
        match key.code {
            KeyCode::Up => menu.highlight = menu.highlight.saturating_sub(1),
            KeyCode::Down => menu.highlight = (menu.highlight + 1).min(last),
            KeyCode::Home => menu.highlight = 0,
            KeyCode::End => menu.highlight = last,
            KeyCode::PageUp => menu.highlight = menu.highlight.saturating_sub(10),
            KeyCode::PageDown => menu.highlight = (menu.highlight + 10).min(last),
            KeyCode::Enter => self.commit_select_highlight(),
            KeyCode::Esc => {
                self.select_menu = None;
                self.status = String::from("Select cancelled.");
            }
            _ => {}
        }
    }

    /// The dropdown option index under a screen point, if it lands on the
    /// open menu's option rows (the rect interior, minus its border).
    fn select_option_at(&self, col: u16, row: u16) -> Option<usize> {
        let menu = self.select_menu.as_ref()?;
        let r = self.last_select_rect?;
        let inside = col > r.x
            && col < r.right().saturating_sub(1)
            && row > r.y
            && row < r.bottom().saturating_sub(1);
        let idx = menu.scroll + (row.saturating_sub(r.y + 1)) as usize;
        (inside && idx < menu.options.len()).then_some(idx)
    }

    /// Mouse while a `<select>` dropdown is open: wheel and hover move the
    /// highlight, a click picks the option under the cursor (or cancels if
    /// outside the popup).
    fn select_menu_mouse(&mut self, mouse: MouseEvent) {
        let Some(last) = self
            .select_menu
            .as_ref()
            .map(|m| m.options.len().saturating_sub(1))
        else {
            return;
        };
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                if let Some(m) = self.select_menu.as_mut() {
                    m.highlight = m.highlight.saturating_sub(1);
                }
            }
            MouseEventKind::ScrollDown => {
                if let Some(m) = self.select_menu.as_mut() {
                    m.highlight = (m.highlight + 1).min(last);
                }
            }
            MouseEventKind::Moved | MouseEventKind::Drag(MouseButton::Left) => {
                // Hover follows the cursor (no commit).
                if let Some(idx) = self.select_option_at(mouse.column, mouse.row)
                    && let Some(m) = self.select_menu.as_mut()
                {
                    m.highlight = idx;
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                match self.select_option_at(mouse.column, mouse.row) {
                    Some(idx) => {
                        if let Some(m) = self.select_menu.as_mut() {
                            m.highlight = idx;
                        }
                        self.commit_select_highlight();
                    }
                    None => {
                        self.select_menu = None;
                        self.status = String::from("Select cancelled.");
                    }
                }
            }
            _ => {}
        }
    }

    /// Commit the highlighted option and close the dropdown.
    fn commit_select_highlight(&mut self) {
        let Some(menu) = self.select_menu.take() else {
            return;
        };
        let value = menu.options[menu.highlight].1.clone();
        self.commit_select(menu.form, menu.field, value);
    }

    /// Write a chosen `<select>` value back: a living page sees a real
    /// `change`; otherwise the static field updates and re-renders.
    fn commit_select(&mut self, form: usize, field: usize, value: String) {
        let (name, live_node) = self
            .browser
            .as_ref()
            .and_then(|g| g.doc.forms.get(form))
            .and_then(|f| f.fields.get(field))
            .map_or((String::new(), None), |f| (f.name.clone(), f.live_node));
        if let Some(node) = live_node
            && self.dispatch_live_form_set(
                node,
                value.clone(),
                None,
                format!("· {name} changed by page script"),
            )
        {
            return;
        }
        if let Some(g) = &mut self.browser {
            g.doc.forms[form].fields[field].value = value;
        }
        self.refresh_forms();
    }

    /// Map a click-triggered submit's `(form, submitter)` arena nodes to its
    /// doc-model `(form_index, field_index)`. The submitter falls back to the
    /// form's Submit control (then field 0) when it isn't itself a tracked
    /// field, so the native GET/POST still encodes a submit button.
    fn form_indices_for_nodes(
        &self,
        form_node: usize,
        submitter_node: usize,
    ) -> Option<(usize, usize)> {
        use crate::doc::FieldKind;
        let g = self.browser.as_ref()?;
        let form_index = g
            .doc
            .forms
            .iter()
            .position(|f| f.live_node == Some(form_node))?;
        let form = &g.doc.forms[form_index];
        let field = form
            .fields
            .iter()
            .position(|f| f.live_node == Some(submitter_node))
            .or_else(|| form.fields.iter().position(|f| f.kind == FieldKind::Submit))
            .unwrap_or(0);
        Some((form_index, field))
    }

    /// Fire a form: a living page sees a real submit event first. If page
    /// JS does not preventDefault(), the static HTTP submit proceeds.
    fn submit_form(&mut self, form: usize, pressed: usize) {
        if let Some((form_node, submitter_node)) = self.browser.as_ref().and_then(|g| {
            let form_doc = g.doc.forms.get(form)?;
            Some((form_doc.live_node?, form_doc.fields.get(pressed)?.live_node))
        }) && self.dispatch_live_submit(form, pressed, form_node, submitter_node)
        {
            return;
        }
        self.submit_form_static(form, pressed);
    }

    /// Fire a static form: GET serializes into the action's query string,
    /// POST goes form-urlencoded through the existing post plumbing.
    fn submit_form_static(&mut self, form: usize, pressed: usize) {
        use crate::doc::FormMethod;
        let Some(form) = self.browser.as_ref().and_then(|g| g.doc.forms.get(form)) else {
            return;
        };
        let query = form.encode(pressed);
        let action = form.action.clone();
        let referrer = self.http_referrer();
        match form.method {
            FormMethod::Get => {
                let mut url = action;
                url.set_query((!query.is_empty()).then_some(query.as_str()));
                self.start_fetch_opts(Link::Http(url), false, referrer);
            }
            FormMethod::Post => self.start_post(action, query, referrer),
        }
    }

    /// Re-render the page after a form value changed: force the wrap
    /// sync's re-parse, which seeds the fresh parse from live form state.
    fn refresh_forms(&mut self) {
        if let Some(g) = &mut self.browser {
            g.doc.wrapped_to = 0;
        }
        self.sync_browser_wrap();
    }

    fn browser_back(&mut self) {
        let js = self.js_enabled;
        let Some(g) = &mut self.browser else { return };
        match g.history.pop() {
            Some((doc, pos, scroll)) => {
                g.doc = doc;
                g.selected = pos.selected;
                g.sel_item = pos.sel_item;
                g.scroll = scroll;
                // Revive a page that was interactive when we left it:
                // re-run its JS so links/forms work again, rather than
                // restoring a frozen snapshot with dead script links. The
                // frozen doc shows meanwhile; the reload replaces it in
                // place (history already popped). Needs JS on + http(s).
                let revive = pos.was_live && js;
                let url = g.doc.url.clone();
                // Whatever living page was foreground froze when we left.
                self.drop_live_page();
                if revive && let Link::Http(u) = url {
                    self.replace_nav = true;
                    self.start_fetch(Link::Http(u));
                    // After start_fetch (which sets its own "Fetching"
                    // status) so the user sees why we're reloading.
                    self.status = String::from("Reviving page scripts …");
                }
            }
            None => self.status = String::from("History empty (Esc returns to terminal)."),
        }
    }

    /// Run a query the page asked for: gopher type-7 search (RFC 1436:
    /// selector TAB query) or a gemini 1x input (percent-encoded ?query).
    fn run_search(&mut self, query: &str) {
        // A pending status-60 prompt: mint the identity, then retry
        // the page that asked for it.
        if let Some(url) = self.cert_for.take() {
            let name = query.trim();
            let name = if name.is_empty() { "anonymous" } else { name };
            match tls::create_identity(&url.host, name) {
                Ok(path) => {
                    self.status = format!("Identity '{name}' saved to {}.", path.display());
                    self.start_fetch(Link::Gemini(url));
                }
                Err(err) => {
                    self.status = err;
                    self.notice = true;
                }
            }
            return;
        }
        match self.search_target.take() {
            Some(Link::Gopher(base)) => {
                let url = GopherUrl {
                    selector: format!("{}\t{}", base.selector, query),
                    ..base
                };
                self.start_fetch(Link::Gopher(url));
            }
            Some(Link::Gemini(base)) => {
                let path = base.path.split('?').next().unwrap_or("/").to_string();
                let url = GeminiUrl {
                    path: format!("{path}?{}", gemini::encode_query(query)),
                    ..base
                };
                self.start_fetch(Link::Gemini(url));
            }
            // A form field edit: living pages receive input/change in
            // the DOM; static pages store the value in Doc.forms.
            Some(Link::Form { form, field }) => {
                let target = self.browser.as_ref().and_then(|g| {
                    let f = g.doc.forms.get(form)?.fields.get(field)?;
                    Some((f.name.clone(), f.live_node))
                });
                if let Some((name, Some(node))) = target
                    && self.dispatch_live_form_set(
                        node,
                        query.to_string(),
                        None,
                        if name.is_empty() {
                            String::from("· field changed by page script")
                        } else {
                            format!("· {name} changed by page script")
                        },
                    )
                {
                    return;
                }
                if let Some(g) = &mut self.browser
                    && let Some(f) = g
                        .doc
                        .forms
                        .get_mut(form)
                        .and_then(|f| f.fields.get_mut(field))
                {
                    f.value = query.to_string();
                    self.status = if f.name.is_empty() {
                        String::from("Field set.")
                    } else {
                        format!("{} set.", f.name)
                    };
                }
                self.refresh_forms();
            }
            _ => {}
        }
    }

    fn open(&mut self, host: String, port: u16, use_tls: bool) {
        // A fresh telnet session takes the screen; a browser or image
        // viewer left open would hide the connection behind it.
        self.browser = None;
        self.viewer = None;
        self.drop_live_page();
        self.reset_screen();
        let (handle, events) = telnet::connect(host.clone(), port, self.last_inner, use_tls);
        self.conn = Some(handle);
        self.events = Some(events);
        self.connected = false;
        self.tls = false;
        self.remote_opts.clear();
        self.local_opts.clear();
        self.linemode_active = false;
        self.linemode_edit = false;
        let padlock = if use_tls { " (TLS)" } else { "" };
        self.status = format!("Trying {host}:{port}{padlock}...");
        self.host = Some(host);
        self.port = port;
    }

    async fn on_telnet_event(&mut self, event: telnet::Event) {
        match event {
            telnet::Event::Connected { peer, tls } => {
                self.connected = true;
                self.tls = tls;
                let padlock = if tls { " over TLS" } else { "" };
                self.status = format!("Connected to {peer}{padlock}. Escape character is Ctrl-].");
            }
            telnet::Event::Data(data) => {
                // Answer terminal probes directly, not via send_bytes, so a
                // background query doesn't yank the scrollback view to live.
                for reply in self.on_data(&data) {
                    if let Some(conn) = &self.conn {
                        let _ = conn.commands.send(telnet::Command::Send(reply)).await;
                    }
                }
                if self.take_bell() {
                    ring_terminal_bell();
                }
            }
            telnet::Event::Negotiation { command, option } => match command {
                op_command::WILL => {
                    self.remote_opts.insert(option);
                }
                op_command::WONT => {
                    self.remote_opts.remove(&option);
                }
                op_command::DO => {
                    self.local_opts.insert(option);
                }
                op_command::DONT => {
                    self.local_opts.remove(&option);
                }
                _ => {}
            },
            telnet::Event::LineMode { active, edit } => {
                self.linemode_active = active;
                self.linemode_edit = edit;
            }
            telnet::Event::Closed(reason) => {
                self.connected = false;
                self.tls = false;
                self.remote_opts.clear();
                self.local_opts.clear();
                self.linemode_active = false;
                self.linemode_edit = false;
                self.conn = None;
                self.events = None;
                self.status = match reason {
                    Some(err) => format!("Connection failed: {err}"),
                    None => String::from("Connection closed by foreign host."),
                };
            }
        }
    }

    /// Process inbound application data through the emulator and build the
    /// replies a real terminal would send for any probes found in it. The
    /// data is processed first so a Cursor Position Report reflects e.g.
    /// the `ESC[255;255H` a BBS sends just before `ESC[6n` to size us.
    fn on_data(&mut self, data: &[u8]) -> Vec<Vec<u8>> {
        match self.encoding {
            Encoding::Utf8 => self.vt.process(data),
            Encoding::Cp437 => self.vt.process(&cp437::decode(data)),
        }
        // The detector sees the raw bytes; probe sequences are pure ASCII,
        // which both encodings pass through unchanged.
        self.probes
            .feed(data)
            .into_iter()
            .map(|probe| match probe {
                Probe::CursorPosition => {
                    let (row, col) = self.vt.screen().cursor_position();
                    format!("\x1b[{};{}R", row + 1, col + 1).into_bytes()
                }
                Probe::Status => b"\x1b[0n".to_vec(),
                // VT100 with Advanced Video Option.
                Probe::DeviceAttributes => b"\x1b[?1;2c".to_vec(),
            })
            .collect()
    }

    /// True once per BEL the emulator has seen since the last call.
    fn take_bell(&mut self) -> bool {
        let bells = self.vt.callbacks().bells;
        let ring = bells > self.bells_seen;
        self.bells_seen = bells;
        ring
    }
}

/// Resolve a port argument: a number, or a well-known service name —
/// GNU telnet's getservbyname, in miniature.
pub(crate) fn parse_port(s: &str) -> Option<u16> {
    if let Ok(port) = s.parse() {
        return Some(port);
    }
    Some(match s {
        "echo" => 7,
        "daytime" => 13,
        "chargen" => 19,
        "ftp" => 21,
        "telnet" => 23,
        "smtp" | "mail" => 25,
        "whois" | "nicname" => 43,
        "domain" => 53,
        "gopher" => 70,
        "finger" => 79,
        "http" | "www" => 80,
        "pop3" => 110,
        "nntp" => 119,
        "imap" => 143,
        "https" => 443,
        "telnets" => 992,
        "gemini" => 1965,
        "dict" => 2628,
        "irc" => 6667,
        _ => return None,
    })
}

/// Whether a bare console token looks like a web host/address — a dotted
/// name (`example.com`, `192.168.0.1`), `host:port`, or `localhost` — so it
/// opens as if `open` had been typed. Conservative on purpose: real command
/// typos (no dot, no `localhost`) still fall through to the usage hint.
/// Tokens are whitespace-split, so they never contain spaces.
fn looks_like_host(s: &str) -> bool {
    let host = s.split(':').next().unwrap_or(s);
    host == "localhost" || (host.contains('.') && !host.starts_with('.') && !host.ends_with('.'))
}

/// Split a trailing `:port` off a host string, the way users write
/// telnet targets (`isharmud.com:23`). Hosts with more than one colon
/// (raw IPv6 literals) are left whole.
fn split_host_port(s: &str) -> (&str, Option<u16>) {
    if let Some((host, port)) = s.rsplit_once(':')
        && !host.is_empty()
        && !host.contains(':')
        && let Ok(port) = port.parse::<u16>()
    {
        return (host, Some(port));
    }
    (s, None)
}

/// Pass a BEL through to the real terminal.
fn ring_terminal_bell() {
    use std::io::Write;
    let mut out = std::io::stdout();
    let _ = out.write_all(b"\x07");
    let _ = out.flush();
}

/// Recognize a YouTube video URL across its formats, for auto-routing to
/// mpv (yt-dlp resolves all of these). Covers `youtu.be/<id>`,
/// `youtube.com/watch`, and the `/shorts/ /embed/ /live/ /v/` paths, on
/// the bare, `www.`, `m.`, `music.`, and `-nocookie` hosts.
fn is_youtube_video_url(url: &str) -> bool {
    let Ok(u) = url::Url::parse(url) else {
        return false;
    };
    if !matches!(u.scheme(), "http" | "https") {
        return false;
    }
    let Some(host) = u.host_str() else {
        return false;
    };
    let host = host.to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    let path = u.path();
    match host {
        "youtu.be" => path.len() > 1,
        "youtube.com" | "m.youtube.com" | "music.youtube.com" | "youtube-nocookie.com" => {
            path == "/watch"
                || path.starts_with("/shorts/")
                || path.starts_with("/embed/")
                || path.starts_with("/live/")
                || path.starts_with("/v/")
        }
        _ => false,
    }
}

/// Whether a URL points at media mpv routinely plays directly — a video or
/// audio file, or a streaming manifest. Extension-based (direct media files,
/// which is what `<video>`/`<audio>` sources and direct media links almost
/// always are); following such a link auto-launches mpv, like YouTube does.
fn is_playable_media_url(url: &str) -> bool {
    let Ok(u) = url::Url::parse(url) else {
        return false;
    };
    if !matches!(u.scheme(), "http" | "https") {
        return false;
    }
    let path = u.path().to_ascii_lowercase();
    const EXTS: &[&str] = &[
        // video
        ".mp4", ".m4v", ".webm", ".mkv", ".mov", ".avi", ".flv", ".wmv", ".mpg", ".mpeg", ".ogv",
        ".ts", ".m2ts", ".3gp", ".ogm", // adaptive-streaming manifests mpv plays
        ".m3u8", ".mpd", // audio
        ".mp3", ".m4a", ".m4b", ".aac", ".ogg", ".oga", ".opus", ".flac", ".wav", ".wma", ".mka",
        ".weba", ".aiff", ".aif",
    ];
    EXTS.iter().any(|e| path.ends_with(e))
}

const SEND_USAGE: &str = "usage: send brk|ip|ao|ayt|ec|el|ga|nop|escape";

/// Map GNU telnet `send` argument names to IAC command codes (RFC 854).
fn iac_code(name: &str) -> Option<u8> {
    Some(match name {
        "brk" | "break" => 243,
        "ip" => 244,
        "ao" => 245,
        "ayt" => 246,
        "ec" => 247,
        "el" => 248,
        "ga" => 249,
        "nop" => 241,
        _ => return None,
    })
}

/// Human-readable names for negotiated options in `status` output.
/// Append every case-insensitive (ASCII-folded) occurrence of `query`
/// (already lowercased) in `text` as non-overlapping char-offset ranges.
/// Non-ASCII case is matched exactly — an honest limitation; find queries
/// are virtually always ASCII.
fn push_text_matches(
    text: &str,
    query: &str,
    line: usize,
    item: Option<usize>,
    out: &mut Vec<FindMatch>,
) {
    let lower: Vec<char> = text.chars().map(|c| c.to_ascii_lowercase()).collect();
    let q: Vec<char> = query.chars().collect();
    if q.is_empty() || q.len() > lower.len() {
        return;
    }
    let mut i = 0;
    while i + q.len() <= lower.len() {
        if lower[i..i + q.len()] == q[..] {
            out.push(FindMatch {
                line,
                item,
                start: i,
                end: i + q.len(),
            });
            i += q.len();
        } else {
            i += 1;
        }
    }
}

fn option_names(opts: &HashSet<u8>) -> String {
    if opts.is_empty() {
        return String::from("(none)");
    }
    let mut names: Vec<String> = opts
        .iter()
        .map(|&opt| match opt {
            op_option::BINARY => String::from("BINARY"),
            op_option::ECHO => String::from("ECHO"),
            op_option::SGA => String::from("SGA"),
            op_option::TTYPE => String::from("TTYPE"),
            op_option::NAWS => String::from("NAWS"),
            op_option::STATUS => String::from("STATUS"),
            op_option::TSPEED => String::from("TSPEED"),
            op_option::LFLOW => String::from("LFLOW"),
            op_option::LINEMODE => String::from("LINEMODE"),
            op_option::NEWENVIRON => String::from("NEW-ENVIRON"),
            other => other.to_string(),
        })
        .collect();
    names.sort();
    names.join(", ")
}

/// Receive from an optional channel, pending forever when there is none, so
/// the select loop works whether or not the channel exists.
async fn recv_opt<T>(rx: &mut Option<mpsc::Receiver<T>>) -> Option<T> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

/// How many page images fetch+decode at once.
const IMG_FETCH_CONCURRENCY: usize = 8;

/// Fetch one page image (pooled GET, SSRF-guarded against private
/// addresses) and decode it on a blocking task, returning its raw bytes
/// and decode-first cell box. `None` on any failure (the alt text stands).
async fn load_one_image(
    page: &Url,
    url: &str,
    font: ratatui_image::FontSize,
) -> Option<DecodedImage> {
    let parsed = http::parse_url(url)?;
    if !http::subresource_allowed(page, &parsed) {
        return None;
    }
    // Send a Referer like a browser does: many image/media CDNs (gelbooru
    // and most boorus, plenty of others) hotlink-protect and 302/403 a
    // refererless request to a placeholder instead of the file.
    let mut req = http::Request::get(parsed);
    http::set_referrer(&mut req, page);
    let resp = http::fetch(&req).await.ok()?;
    if resp.status != 200 || resp.body.is_empty() {
        return None;
    }
    let raw: std::sync::Arc<[u8]> = resp.body.into();
    let for_decode = raw.clone();
    let cell = tokio::task::spawn_blocking(move || {
        // Background blocking thread — sandbox the decode like the encode: a
        // bad image fails to None, never unwinds the worker. The terminal is
        // safe regardless (only the run loop restores it — TERMINAL_OWNER).
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            crate::img::decode(&for_decode)
                .ok()
                .map(|(image, _mime)| natural_cell_box(&image, font))
        }))
        .ok()
        .flatten()
    })
    .await
    .ok()??;
    Some(DecodedImage { raw, cell })
}

/// The cell box an image occupies: its natural size at the terminal font,
/// scaled down (never up) to fit `IMG_MAX_CELLS` while preserving aspect.
/// Layout clamps the width further to the content width (rescaling height).
fn natural_cell_box(image: &image::DynamicImage, font: ratatui_image::FontSize) -> (u16, u16) {
    let nat = ratatui_image::Resize::natural_size(image, font);
    let (cw, ch) = (nat.width.max(1) as f32, nat.height.max(1) as f32);
    let scale = (IMG_MAX_CELLS.0 / cw).min(IMG_MAX_CELLS.1 / ch).min(1.0);
    let w = (cw * scale).round().max(1.0) as u16;
    let h = (ch * scale).round().max(1.0) as u16;
    (w, h)
}

/// Translate a key event into the bytes a character-mode telnet client sends.
fn encode_key(key: KeyEvent, crlf: bool) -> Option<Vec<u8>> {
    let bytes = match key.code {
        KeyCode::Char(c) if key.modifiers.contains(KeyModifiers::CONTROL) => {
            // Ctrl-A..Ctrl-Z and friends map onto the C0 control range.
            let upper = c.to_ascii_uppercase();
            if ('@'..='_').contains(&upper) {
                vec![upper as u8 & 0x1f]
            } else {
                return None;
            }
        }
        KeyCode::Char(c) => {
            let mut buf = [0u8; 4];
            c.encode_utf8(&mut buf).as_bytes().to_vec()
        }
        // RFC 854: a bare CR is sent as CR NUL; GNU telnet's `crlf`
        // toggle switches Enter to CR LF instead.
        KeyCode::Enter if crlf => b"\r\n".to_vec(),
        KeyCode::Enter => b"\r\x00".to_vec(),
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => b"\x1b[A".to_vec(),
        KeyCode::Down => b"\x1b[B".to_vec(),
        KeyCode::Right => b"\x1b[C".to_vec(),
        KeyCode::Left => b"\x1b[D".to_vec(),
        KeyCode::Home => b"\x1b[H".to_vec(),
        KeyCode::End => b"\x1b[F".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
        _ => return None,
    };
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::{HISTORY_CAP, History};
    use crate::doc::Link;

    #[test]
    fn recalls_entries_and_restores_draft() {
        let mut h = History::default();
        h.push("north");
        h.push("kill rat");

        assert_eq!(h.up("unfinished"), Some("kill rat".into()));
        assert_eq!(h.up(""), Some("north".into()));
        assert_eq!(h.up(""), None); // already at the oldest
        assert_eq!(h.down(), Some("kill rat".into()));
        assert_eq!(h.down(), Some("unfinished".into())); // draft restored
        assert_eq!(h.down(), None); // already editing fresh
    }

    #[test]
    fn skips_empty_and_consecutive_duplicates() {
        let mut h = History::default();
        h.push("north");
        h.push("north");
        h.push("");
        h.push("south");
        h.push("north");
        assert_eq!(h.entries, ["north", "south", "north"]);
    }

    #[test]
    fn editing_detaches_from_browse_position() {
        let mut h = History::default();
        h.push("look");
        assert_eq!(h.up(""), Some("look".into()));
        h.detach();
        // Up now stashes the edited text as the new draft.
        assert_eq!(h.up("look around"), Some("look".into()));
        assert_eq!(h.down(), Some("look around".into()));
    }

    #[test]
    fn new_connection_gets_a_fresh_emulator() {
        let mut app = super::App::new(None, 23);

        // A server killed mid-redraw leaves a scroll region (rows 1-20,
        // e.g. a BBS with a status bar) with the cursor parked below it.
        app.vt.process(b"\x1b[1;20r\x1b[24;1H");

        // Reproduce the bug: below the region, LF can neither advance nor
        // scroll, so every line of the next session lands on row 24,
        // overwriting itself.
        app.vt.process(b"one\r\ntwo\r\nthree");
        let stale = app.vt.screen().contents();
        assert!(!stale.contains("one"), "scroll region should eat old lines");
        assert!(!stale.contains("two"), "scroll region should eat old lines");
        assert!(stale.contains("three"));

        // The fix: open() resets the emulator before connecting.
        app.reset_screen();
        app.vt.process(b"one\r\ntwo\r\nthree");
        let screen = app.vt.screen();
        assert_eq!(screen.cell(0, 0).unwrap().contents(), "o");
        assert_eq!(screen.cell(1, 0).unwrap().contents(), "t");
        assert_eq!(screen.cell(2, 0).unwrap().contents(), "t");
        let fresh = screen.contents();
        assert!(fresh.contains("one") && fresh.contains("two") && fresh.contains("three"));
    }

    #[test]
    fn answers_ansi_detection_probes() {
        let mut app = super::App::new(None, 23);

        // Synchronet-style ANSI detection: park the cursor far off-screen,
        // then ask where it actually ended up.
        let replies = app.on_data(b"\x1b[255;255H\x1b[6n");
        assert_eq!(replies, [b"\x1b[24;80R".to_vec()]);

        // Status report and device attributes.
        let replies = app.on_data(b"\x1b[5n\x1b[c");
        assert_eq!(replies, [b"\x1b[0n".to_vec(), b"\x1b[?1;2c".to_vec()]);

        // No probes in plain ANSI art: color codes must not trigger replies.
        assert!(app.on_data(b"\x1b[1;35mNIGHT CITY\x1b[0m\r\n").is_empty());
    }

    #[test]
    fn detects_probe_split_across_segments() {
        let mut det = super::ProbeDetector::default();
        assert!(det.feed(b"hello \x1b[").is_empty());
        assert!(det.feed(b"6").is_empty());
        assert_eq!(det.feed(b"n world"), [super::Probe::CursorPosition]);
    }

    #[test]
    fn enter_sends_cr_nul_unless_crlf_toggled() {
        use crossterm::event::{KeyCode, KeyEvent};
        let enter = KeyEvent::from(KeyCode::Enter);
        assert_eq!(super::encode_key(enter, false), Some(b"\r\x00".to_vec()));
        assert_eq!(super::encode_key(enter, true), Some(b"\r\n".to_vec()));
    }

    #[test]
    fn cp437_encoding_renders_bbs_art() {
        let mut app = super::App::new(None, 23);

        // Box-drawing bytes are invalid UTF-8: garbage without translation.
        app.on_data(b"\xC9\xCD\xBB");
        assert!(!app.vt.screen().contents().contains("╔═╗"));

        app.reset_screen();
        app.encoding = super::Encoding::Cp437;
        app.on_data(b"\xC9\xCD\xBB");
        assert!(app.vt.screen().contents().contains("╔═╗"));
    }

    #[test]
    fn bell_fires_once_per_bel() {
        let mut app = super::App::new(None, 23);
        assert!(!app.take_bell());
        app.on_data(b"ding\x07");
        assert!(app.take_bell());
        assert!(!app.take_bell());
        app.on_data(b"\x07\x07");
        assert!(app.take_bell());
    }

    #[tokio::test]
    async fn status_command_prints_into_the_feed() {
        let mut app = super::App::new(None, 23);
        app.execute_command("status").await;
        let contents = app.vt.screen().contents();
        assert!(contents.contains("TRUST STATUS"), "got: {contents}");
        assert!(contents.contains("No connection."));
        assert!(contents.contains("Enter sends: CR NUL"));
        assert!(contents.contains("Encoding: UTF-8"));
    }

    /// Build a browser doc from a pattern: 'x' = link line, '.' = text.
    fn gopher_doc(pattern: &str) -> crate::doc::Doc {
        use crate::doc::{Doc, DocLine, Kind, Link};
        let url = crate::gopher::GopherUrl::parse("gopher://test.host").unwrap();
        let lines = pattern
            .chars()
            .enumerate()
            .map(|(i, c)| DocLine {
                kind: if c == 'x' { Kind::Dir } else { Kind::Info },
                text: format!("line {i}"),
                link: (c == 'x').then(|| {
                    Link::Gopher(crate::gopher::GopherUrl {
                        host: String::from("test.host"),
                        port: 70,
                        item_type: '1',
                        selector: format!("/{i}"),
                    })
                }),
            })
            .collect();
        Doc {
            url: Link::Gopher(url),
            lines,
            raw: Vec::new(), // synthetic docs are never re-wrapped
            wrapped_to: 80,
            cp437: false,
            meta: None,
            forms: Vec::new(),
            rows: Vec::new(),
            image_urls: Vec::new(),
            carousels: Vec::new(),
        }
    }

    fn selected(app: &super::App) -> Option<usize> {
        app.browser.as_ref().unwrap().selected
    }

    #[tokio::test]
    async fn reload_replaces_in_place_without_history_growth() {
        let mut app = super::App::new(None, 23);
        app.navigate_to(gopher_doc("ix"));
        app.navigate_to(gopher_doc("xxi"));
        let depth = app.browser.as_ref().unwrap().history.len();
        assert_eq!(depth, 1);

        // A reload-flagged navigation swaps the doc, history untouched.
        app.replace_nav = true;
        app.navigate_to(gopher_doc("iii"));
        let g = app.browser.as_ref().unwrap();
        assert_eq!(g.history.len(), 1);
        assert_eq!(g.doc.lines.len(), 3);

        // The flag is one-shot: the next navigation pushes again.
        app.navigate_to(gopher_doc("x"));
        assert_eq!(app.browser.as_ref().unwrap().history.len(), 2);

        // Nothing on screen: reload is a polite no-op.
        app.browser = None;
        app.reload();
        assert_eq!(app.status, "Nothing to reload.");
    }

    #[test]
    fn gopherus_highlight_rides_the_viewport() {
        let mut app = super::App::new(None, 23);
        app.last_inner = (80, 4); // 4-line viewport
        //                 lines: 0    1    2  3  4  5    6  7
        app.navigate_to(gopher_doc(".x...x.."));

        // First visible link is selected on load.
        assert_eq!(selected(&app), Some(1));
        // Scroll down: the link is still on screen, highlight stays put.
        app.browser_arrow(1);
        assert_eq!(selected(&app), Some(1));
        // It scrolls off the top: highlight jumps to the next visible link.
        app.browser_arrow(1);
        assert_eq!(selected(&app), Some(5));
        // Scrolling back up: 5 leaves through the bottom, 1 returns.
        app.browser_arrow(-1);
        assert_eq!(selected(&app), Some(1));
    }

    #[test]
    fn gopherus_no_visible_link_means_no_highlight() {
        let mut app = super::App::new(None, 23);
        app.last_inner = (80, 3);
        app.navigate_to(gopher_doc("x......x"));

        assert_eq!(selected(&app), Some(0));
        // Scroll into the link-free middle: nothing highlighted.
        app.browser_arrow(1);
        assert_eq!(selected(&app), None);
        app.browser_scroll(3, true); // mouse wheel
        assert_eq!(selected(&app), None);
        // The next link enters the viewport and takes the highlight.
        app.browser_arrow(1);
        assert_eq!(selected(&app), Some(7));
    }

    #[test]
    fn gopherus_walks_links_when_page_cannot_scroll() {
        let mut app = super::App::new(None, 23);
        app.last_inner = (80, 10); // taller than the document
        app.navigate_to(gopher_doc(".x.xx"));

        assert_eq!(selected(&app), Some(1));
        // The page is pinned, so Up/Down step between visible links.
        app.browser_arrow(1);
        assert_eq!(selected(&app), Some(3));
        app.browser_arrow(1); // line 4 is adjacent to 3: direct transition
        assert_eq!(selected(&app), Some(4));
        app.browser_arrow(1);
        assert_eq!(selected(&app), Some(4), "stays on the last link");
        app.browser_arrow(-1); // adjacent transition back
        assert_eq!(selected(&app), Some(3));
        app.browser_arrow(-1);
        app.browser_arrow(-1);
        assert_eq!(selected(&app), Some(1), "stays on the first link");
    }

    #[test]
    fn rewrap_on_resize_preserves_the_selected_link() {
        let mut app = super::App::new(None, 23);
        app.last_inner = (80, 10);
        let url = crate::gopher::GopherUrl::parse("gopher://t.h").unwrap();
        let raw = format!(
            "i{}\t\terror.host\t1\r\n1Link one\t/1\tt.h\t70\r\n1Link two\t/2\tt.h\t70\r\n.\r\n",
            "prose ".repeat(30) // 180 chars of info text above the links
        )
        .into_bytes();
        app.navigate_to(crate::gopher::parse(&url, raw, false, 80));

        // Select the second link.
        app.browser_arrow(1);
        let g = app.browser.as_ref().unwrap();
        let lines_at_80 = g.doc.lines.len();
        assert_eq!(g.doc.lines[g.selected.unwrap()].text, "Link two");

        // Halving the width re-wraps the prose; the links move to new
        // line indices but the selection stays on "Link two".
        app.last_inner = (40, 10);
        app.sync_browser_wrap();
        let g = app.browser.as_ref().unwrap();
        assert!(g.doc.lines.len() > lines_at_80, "narrower wrap adds rows");
        assert!(g.doc.lines.iter().all(|l| l.text.chars().count() <= 40));
        assert_eq!(g.doc.lines[g.selected.unwrap()].text, "Link two");
    }

    #[test]
    fn v_routing_extracts_only_web_urls() {
        let mut app = super::App::new(None, 23);
        app.last_inner = (80, 24);
        let url = url::Url::parse("https://example.com/watch").unwrap();
        let html = b"<body><a href=\"https://youtu.be/dQw4w9WgXcQ\">vid</a>\
                     <a href=\"mailto:x@y.z\">mail</a></body>";
        app.navigate_to(crate::http::parse(
            &url,
            "text/html",
            html,
            80,
            &Default::default(),
        ));

        // A web link → its absolute URL, ready for mpv.
        select_item(&mut app, |it| {
            matches!(&it.link, Some(crate::doc::Link::Http(_)))
        });
        assert_eq!(
            app.selected_web_url().as_deref(),
            Some("https://youtu.be/dQw4w9WgXcQ")
        );

        // A foreign scheme (mailto:) is not something mpv should get.
        select_item(&mut app, |it| {
            matches!(&it.link, Some(crate::doc::Link::External(_)))
        });
        assert_eq!(app.selected_web_url(), None);
    }

    #[test]
    fn mouse_wheel_scrolls_an_http_laid_out_doc() {
        let mut app = super::App::new(None, 23);
        app.last_inner = (80, 10);
        let url = url::Url::parse("https://example.com/").unwrap();
        let body: String = (0..40).map(|i| format!("<p>row {i}</p>")).collect();
        let html = format!("<body>{body}</body>");
        app.navigate_to(crate::http::parse(
            &url,
            "text/html",
            html.as_bytes(),
            80,
            &Default::default(),
        ));
        assert!(app.browser.as_ref().unwrap().doc.laid_out());
        assert_eq!(app.browser.as_ref().unwrap().scroll, 0);
        // Wheel down moves the viewport (the bug: lines.len()==0 pinned it).
        app.browser_scroll(3, true);
        assert_eq!(
            app.browser.as_ref().unwrap().scroll,
            3,
            "wheel scrolled down"
        );
        app.browser_scroll(3, true);
        assert_eq!(app.browser.as_ref().unwrap().scroll, 6);
        // Wheel up returns; clamps at the top.
        app.browser_scroll(-9, true);
        assert_eq!(
            app.browser.as_ref().unwrap().scroll,
            0,
            "clamped at the top"
        );
    }

    fn mouse(
        kind: crossterm::event::MouseEventKind,
        col: u16,
        row: u16,
    ) -> crossterm::event::MouseEvent {
        crossterm::event::MouseEvent {
            kind,
            column: col,
            row,
            modifiers: crossterm::event::KeyModifiers::empty(),
        }
    }

    fn item_point(
        app: &super::App,
        pred: impl Fn(&crate::layout::Item) -> bool,
    ) -> (u16, u16, (usize, usize)) {
        let g = app.browser.as_ref().expect("a browser");
        for (r, row) in g.doc.rows.iter().enumerate() {
            for (i, item) in row.items.iter().enumerate() {
                if pred(item) {
                    return (
                        app.last_content_area.x + item.col,
                        app.last_content_area.y + r.saturating_sub(g.scroll) as u16,
                        (r, i),
                    );
                }
            }
        }
        panic!("target item not found")
    }

    #[test]
    fn http_mouse_hover_selects_clickable_item() {
        let mut app = super::App::new(None, 23);
        app.mode = super::Mode::Session;
        app.last_inner = (80, 10);
        app.last_content_area = ratatui::layout::Rect::new(2, 1, 80, 10);
        let url = url::Url::parse("https://example.com/").unwrap();
        let html = b"<body><p>plain <a href='/next'>next</a></p></body>";
        app.navigate_to(crate::http::parse(
            &url,
            "text/html",
            html,
            80,
            &Default::default(),
        ));
        app.browser.as_mut().unwrap().sel_item = None;
        let (x, y, target) = item_point(&app, |it| it.link.is_some());

        app.on_mouse_event(mouse(crossterm::event::MouseEventKind::Moved, x, y));

        assert_eq!(app.browser.as_ref().unwrap().sel_item, Some(target));
    }

    #[test]
    fn hovering_a_link_clears_a_sticky_notice_so_the_preview_shows() {
        let mut app = super::App::new(None, 23);
        app.mode = super::Mode::Session;
        app.last_inner = (80, 10);
        app.last_content_area = ratatui::layout::Rect::new(2, 1, 80, 10);
        let url = url::Url::parse("https://example.com/").unwrap();
        let html = b"<body><p>plain <a href='/next'>next</a></p></body>";
        app.navigate_to(crate::http::parse(
            &url,
            "text/html",
            html,
            80,
            &Default::default(),
        ));
        // A sticky message is up (the mpv-launch confirmation / a fetch error),
        // which the status bar pins over the selected-link preview.
        app.notice = true;
        let (x, y, _) = item_point(&app, |it| it.link.is_some());

        app.on_mouse_event(mouse(crossterm::event::MouseEventKind::Moved, x, y));

        assert!(
            !app.notice,
            "hovering a link releases the notice so the link preview shows"
        );
    }

    #[test]
    fn http_mouse_left_click_activates_link() {
        let mut app = super::App::new(None, 23);
        app.mode = super::Mode::Session;
        app.last_inner = (80, 10);
        app.last_content_area = ratatui::layout::Rect::new(3, 2, 80, 10);
        let url = url::Url::parse("https://example.com/").unwrap();
        let html = b"<body><p><a href='mailto:x@y.z'>mail</a></p></body>";
        app.navigate_to(crate::http::parse(
            &url,
            "text/html",
            html,
            80,
            &Default::default(),
        ));
        let (x, y, target) = item_point(&app, |it| matches!(it.link, Some(Link::External(_))));

        app.on_mouse_event(mouse(
            crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            x,
            y,
        ));

        assert_eq!(app.browser.as_ref().unwrap().sel_item, Some(target));
        assert_eq!(app.status, "external link: mailto:x@y.z");
    }

    #[test]
    fn http_mouse_left_click_activates_linked_image_on_bottom_row() {
        let mut app = super::App::new(None, 23);
        app.mode = super::Mode::Session;
        app.last_inner = (80, 10);
        app.last_content_area = ratatui::layout::Rect::new(3, 2, 80, 10);
        let url = url::Url::parse("https://example.com/").unwrap();
        let mut images = crate::layout::ImageSizes::new();
        images.insert("https://example.com/cat.png".to_owned(), (10, 4));
        let html = b"<body><a href='mailto:x@y.z'><img src='/cat.png' alt='cat'></a></body>";
        app.navigate_to(crate::http::parse(&url, "text/html", html, 80, &images));

        let g = app.browser.as_ref().unwrap();
        let (img_row, img_item) = g
            .doc
            .rows
            .iter()
            .enumerate()
            .find_map(|(r, row)| {
                row.items
                    .iter()
                    .enumerate()
                    .find(|(_, it)| it.image.is_some() && it.link.is_some())
                    .map(|(i, _)| (r, i))
            })
            .expect("linked image item");
        let img = &g.doc.rows[img_row].items[img_item];
        assert_eq!(img.height, 4, "test must cover a multi-row image box");
        let x = app.last_content_area.x + img.col + img.width - 1;
        let y = app.last_content_area.y + img_row as u16 + img.height - 1;

        app.on_mouse_event(mouse(
            crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            x,
            y,
        ));

        assert_eq!(
            app.browser.as_ref().unwrap().sel_item,
            Some((img_row, img_item))
        );
        assert_eq!(app.status, "external link: mailto:x@y.z");
    }

    #[test]
    fn http_mouse_left_click_activates_form_text_box() {
        let mut app = super::App::new(None, 23);
        app.mode = super::Mode::Session;
        app.last_inner = (80, 10);
        app.last_content_area = ratatui::layout::Rect::new(4, 2, 80, 10);
        let url = url::Url::parse("https://example.com/").unwrap();
        let html = b"<body><form><input name=q value=old></form></body>";
        app.navigate_to(crate::http::parse(
            &url,
            "text/html",
            html,
            80,
            &Default::default(),
        ));
        let (x, y, target) = item_point(&app, |it| matches!(it.link, Some(Link::Form { .. })));

        app.on_mouse_event(mouse(
            crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            x,
            y,
        ));

        assert_eq!(app.browser.as_ref().unwrap().sel_item, Some(target));
        assert_eq!(app.mode, super::Mode::Search);
        assert_eq!(app.input, "old");
        assert!(matches!(app.search_target, Some(Link::Form { .. })));
    }

    #[tokio::test]
    async fn gopher_mouse_hover_and_click_select_and_follow_links() {
        use crossterm::event::{MouseButton, MouseEventKind};
        let mut app = super::App::new(None, 23);
        app.mode = super::Mode::Session;
        app.last_inner = (80, 10);
        app.last_content_area = ratatui::layout::Rect::new(2, 1, 80, 10);
        // Lines: 0 info, 1 link, 2 info, 3 link, 4 info. Default = first link.
        app.navigate_to(gopher_doc(".x.x."));
        assert_eq!(selected(&app), Some(1));

        // Hovering the second link line (doc line 3 → screen row y+3) selects it.
        app.on_mouse_event(mouse(MouseEventKind::Moved, 4, 4));
        assert_eq!(selected(&app), Some(3));

        // Hovering a non-link line leaves the highlight where it is (sticky,
        // matching the gopherus model — unlike HTTP, which clears it).
        app.on_mouse_event(mouse(MouseEventKind::Moved, 4, 1));
        assert_eq!(selected(&app), Some(3));

        // A left-click on the first link line selects AND follows it.
        app.on_mouse_event(mouse(MouseEventKind::Down(MouseButton::Left), 4, 2));
        assert_eq!(selected(&app), Some(1));
        assert!(
            app.status.starts_with("Fetching gopher://test.host"),
            "got: {}",
            app.status
        );
    }

    #[test]
    fn clicking_the_chrome_bars_opens_the_command_console() {
        use crossterm::event::{MouseButton, MouseEventKind};
        let mut app = super::App::new(None, 23);
        app.mode = super::Mode::Session;
        app.last_inner = (80, 10);
        // Content at (2,1)→ title row is 0; status line at row 13.
        app.last_content_area = ratatui::layout::Rect::new(2, 1, 80, 10);
        app.last_status_row = 13;
        let click = |row| mouse(MouseEventKind::Down(MouseButton::Left), 5, row);

        // The top address bar (the bordered title row) opens the console.
        app.on_mouse_event(click(0));
        assert_eq!(app.mode, super::Mode::Command);

        // So does the bottom status line.
        app.mode = super::Mode::Session;
        app.on_mouse_event(click(13));
        assert_eq!(app.mode, super::Mode::Command);

        // A click in the content area does not (no link there → no-op).
        app.mode = super::Mode::Session;
        app.navigate_to(gopher_doc("..."));
        app.on_mouse_event(click(3));
        assert_eq!(app.mode, super::Mode::Session);
    }

    fn http_doc(path: &str) -> crate::doc::Doc {
        let url = url::Url::parse(&format!("https://example.com{path}")).unwrap();
        crate::http::parse(
            &url,
            "text/html",
            b"<body><p>hi</p></body>",
            80,
            &Default::default(),
        )
    }

    #[test]
    fn an_inline_image_does_not_shift_following_text_left() {
        // An inline image carries empty text but a real width box (its pixels
        // are overlaid in a second pass). The render pass must reserve those
        // columns so text after it stays at its laid-out column — else the row
        // collapses left UNDER the image (the header logo painting over the
        // nav links; an avatar over its post title).
        let mut images = crate::layout::ImageSizes::new();
        images.insert("https://example.com/logo.png".to_owned(), (6, 2));
        let url = url::Url::parse("https://example.com/").unwrap();
        let doc = crate::http::parse(
            &url,
            "text/html",
            br#"<body><span style="display:inline-flex"><img src="/logo.png" style="display:block"></span><a href="/f">Forums</a></body>"#,
            80,
            &images,
        );
        let g = super::BrowserView {
            doc,
            selected: None,
            sel_item: None,
            scroll: 0,
            history: vec![],
        };
        let lines = crate::ui::browser_rows(&g, 24, None);
        let rendered: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        let forums_item = g
            .doc
            .rows
            .iter()
            .flat_map(|r| &r.items)
            .find(|i| i.text.contains("Forums"))
            .expect("Forums laid out");
        let img = g
            .doc
            .rows
            .iter()
            .flat_map(|r| &r.items)
            .find(|i| i.image.is_some())
            .expect("image laid out");
        let forums_col = rendered.find("Forums").expect("Forums rendered");
        assert_eq!(
            forums_col, forums_item.col as usize,
            "rendered 'Forums' column matches the layout (not shifted under the image)"
        );
        assert!(
            forums_col >= (img.col + img.width) as usize,
            "'Forums' renders after the image box (col {forums_col}, image ends {})",
            img.col + img.width
        );
    }

    /// Build an App showing a browser doc parsed from `body` with `mime`.
    fn app_browsing(mime: &str, body: &str) -> super::App {
        let images = crate::layout::ImageSizes::new();
        let url = url::Url::parse("https://example.com/").unwrap();
        let doc = crate::http::parse(&url, mime, body.as_bytes(), 80, &images);
        let mut app = super::App::new(None, 23);
        app.mode = super::Mode::Session;
        app.browser = Some(super::BrowserView {
            doc,
            selected: None,
            sel_item: None,
            scroll: 0,
            history: vec![],
        });
        app
    }

    #[test]
    fn push_text_matches_is_case_insensitive_and_nonoverlapping() {
        let mut out = Vec::new();
        super::push_text_matches("Foo foo fOo bar", "foo", 7, None, &mut out);
        assert_eq!(out.len(), 3);
        assert_eq!((out[0].start, out[0].end), (0, 3));
        assert_eq!((out[1].start, out[1].end), (4, 7));
        assert_eq!((out[2].start, out[2].end), (8, 11));
        assert_eq!(out[0].line, 7);
        // Overlapping query advances past each hit (no double-count).
        let mut out = Vec::new();
        super::push_text_matches("aaaa", "aa", 0, None, &mut out);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn find_over_a_line_model_doc_navigates_and_wraps() {
        // text/plain → line model (rows empty).
        let mut app = app_browsing("text/plain", "alpha beta\ngamma beta delta\nbeta");
        assert!(!app.browser.as_ref().unwrap().doc.laid_out());

        app.open_find();
        assert_eq!(app.mode, super::Mode::Find);
        app.input = String::from("beta");
        app.cursor = 4;
        app.recompute_find();

        let f = app.find.as_ref().unwrap();
        assert_eq!(f.matches.len(), 3);
        assert!(f.matches.iter().all(|m| m.item.is_none()));
        assert_eq!(
            f.matches.iter().map(|m| m.line).collect::<Vec<_>>(),
            [0, 1, 2]
        );
        assert_eq!(f.current, Some(0));

        app.find_next();
        assert_eq!(app.find.as_ref().unwrap().current, Some(1));
        app.find_next();
        app.find_next();
        assert_eq!(app.find.as_ref().unwrap().current, Some(0), "wraps forward");
        app.find_prev();
        assert_eq!(
            app.find.as_ref().unwrap().current,
            Some(2),
            "wraps backward"
        );
    }

    #[test]
    fn find_over_an_http_doc_matches_within_items() {
        let mut app = app_browsing(
            "text/html",
            "<body><p>alpha beta</p><p>gamma beta</p></body>",
        );
        assert!(app.browser.as_ref().unwrap().doc.laid_out());

        app.open_find();
        app.input = String::from("beta");
        app.cursor = 4;
        app.recompute_find();

        let f = app.find.as_ref().unwrap();
        assert_eq!(f.matches.len(), 2);
        assert!(f.matches.iter().all(|m| m.item.is_some()));
        assert_eq!(f.current, Some(0));
    }

    #[test]
    fn find_scrolls_the_active_match_into_view() {
        let body: String = (0..100)
            .map(|i| {
                if i == 60 {
                    "needle".to_string()
                } else {
                    format!("line {i}")
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        let mut app = app_browsing("text/plain", &body);
        app.last_inner = (80, 24);

        app.open_find();
        app.input = String::from("needle");
        app.cursor = 6;
        app.recompute_find();

        let g = app.browser.as_ref().unwrap();
        // The match on line 60 is centred (60 - 24/2 = 48), within the viewport.
        assert!(
            g.scroll <= 60 && 60 < g.scroll + 24,
            "match visible (scroll {})",
            g.scroll
        );
        assert_eq!(g.scroll, 48);
    }

    #[tokio::test]
    async fn ctrl_f_opens_find_only_with_a_browser_and_esc_closes() {
        use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
        let ctrl_f = || Event::Key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL));

        // No browser: Ctrl-F is not intercepted (it would reach the remote).
        let mut bare = super::App::new(None, 23);
        bare.mode = super::Mode::Session;
        bare.on_terminal_event(ctrl_f()).await;
        assert_ne!(
            bare.mode,
            super::Mode::Find,
            "no find without a browser doc"
        );

        // With a browser: Ctrl-F opens find, typing finds, Esc closes.
        let mut app = app_browsing("text/plain", "find the beta here");
        app.on_terminal_event(ctrl_f()).await;
        assert_eq!(app.mode, super::Mode::Find);
        for c in "beta".chars() {
            app.on_terminal_event(Event::Key(KeyEvent::from(KeyCode::Char(c))))
                .await;
        }
        assert_eq!(app.find.as_ref().unwrap().matches.len(), 1);
        app.on_terminal_event(Event::Key(KeyEvent::from(KeyCode::Esc)))
            .await;
        assert_eq!(app.mode, super::Mode::Session);
        assert!(app.find.is_none(), "Esc clears find state");
    }

    #[test]
    fn mouse4_goes_back_in_browser_history() {
        let mut app = super::App::new(None, 23);
        app.mode = super::Mode::Session;
        app.browser = Some(super::BrowserView {
            doc: http_doc("/b"),
            selected: None,
            sel_item: None,
            scroll: 0,
            history: vec![(
                http_doc("/a"),
                super::ViewPos {
                    selected: None,
                    sel_item: None,
                    was_live: false,
                },
                7,
            )],
        });

        app.on_mouse_event(mouse(
            crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Mouse4),
            0,
            0,
        ));

        let g = app.browser.as_ref().unwrap();
        assert!(
            matches!(&g.doc.url, Link::Http(u) if u.path() == "/a"),
            "Mouse4 should navigate back to the previous page"
        );
        assert_eq!(g.scroll, 7);
    }

    #[tokio::test]
    async fn back_to_a_live_page_revives_it() {
        // A page that was interactive (had a JS engine) when we left it
        // is reloaded (re-run JS) on back, not restored as a dead snapshot.
        let mut app = super::App::new(None, 23);
        app.js_enabled = true;
        app.browser = Some(super::BrowserView {
            doc: http_doc("/b"),
            selected: None,
            sel_item: None,
            scroll: 0,
            history: vec![(
                http_doc("/a"),
                super::ViewPos {
                    selected: None,
                    sel_item: None,
                    was_live: true,
                },
                0,
            )],
        });

        app.browser_back();

        assert!(app.replace_nav, "revive reloads in place");
        assert!(app.status.contains("Reviving"), "status: {}", app.status);
        assert!(app.fetch_rx.is_some(), "a revive fetch was started");
        assert!(
            matches!(&app.browser.as_ref().unwrap().doc.url, Link::Http(u) if u.path() == "/a"),
            "the popped page shows (static) while reviving",
        );
    }

    #[tokio::test]
    async fn back_to_a_non_live_page_stays_static() {
        // A page that had no engine restores instantly, no reload.
        let mut app = super::App::new(None, 23);
        app.js_enabled = true;
        app.browser = Some(super::BrowserView {
            doc: http_doc("/b"),
            selected: None,
            sel_item: None,
            scroll: 0,
            history: vec![(
                http_doc("/a"),
                super::ViewPos {
                    selected: None,
                    sel_item: None,
                    was_live: false,
                },
                0,
            )],
        });

        app.browser_back();

        assert!(!app.replace_nav, "static back does not reload");
        assert!(app.fetch_rx.is_none(), "no fetch for a static back");
    }

    /// Point an HTTP laid-out doc's item selection at the first item that
    /// matches `pred`.
    fn select_item(app: &mut super::App, pred: impl Fn(&crate::layout::Item) -> bool) {
        let g = app.browser.as_mut().expect("a browser");
        for (r, row) in g.doc.rows.iter().enumerate() {
            if let Some(i) = row.items.iter().position(&pred) {
                g.sel_item = Some((r, i));
                return;
            }
        }
        panic!("no item matched the predicate");
    }

    #[test]
    fn youtube_url_recognizer_covers_the_formats() {
        use super::is_youtube_video_url as yt;
        // Auto-route these:
        assert!(yt("https://youtu.be/dQw4w9WgXcQ"));
        assert!(yt("https://www.youtube.com/watch?v=dQw4w9WgXcQ"));
        assert!(yt("https://youtube.com/watch?v=x&t=10s"));
        assert!(yt("https://m.youtube.com/watch?v=x"));
        assert!(yt("https://music.youtube.com/watch?v=x"));
        assert!(yt("https://www.youtube.com/shorts/abc123"));
        assert!(yt("https://www.youtube-nocookie.com/embed/abc"));
        assert!(yt("http://youtube.com/live/abc"));
        // Leave these to normal navigation:
        assert!(!yt("https://www.youtube.com/")); // channel/home, not a video
        assert!(!yt("https://www.youtube.com/feed/subscriptions"));
        assert!(!yt("https://youtu.be/")); // no id
        assert!(!yt("https://example.com/watch?v=x"));
        assert!(!yt("https://notyoutube.com/watch"));
        assert!(!yt("mailto:x@y.z"));
        assert!(!yt("not a url"));
    }

    #[test]
    fn playable_media_recognizer_covers_audio_and_video() {
        use super::is_playable_media_url as m;
        // Video, audio, and streaming manifests → mpv on follow:
        assert!(m("https://v1.erome.com/8335/BRbvsH39/UDODP8WH_720p.mp4"));
        assert!(m("https://cdn.example.com/clip.webm?token=abc"));
        assert!(m("https://example.com/a/b/movie.MKV")); // case-insensitive
        assert!(m("https://example.com/song.mp3"));
        assert!(m("https://example.com/voice.opus"));
        assert!(m("https://example.com/stream/index.m3u8"));
        assert!(m("https://example.com/manifest.mpd"));
        // An `.m4b` audiobook — the combined whole-book file archive.org links
        // — must play on click like any other audio (regression: it was the
        // one common audio extension missing from the list).
        assert!(m(
            "https://archive.org/download/blackcat0604_2605_librivox/BlackCatV6N4January1901_LibriVox.m4b"
        ));
        // Not media — normal navigation:
        assert!(!m("https://example.com/page.html"));
        assert!(!m("https://example.com/")); // no extension
        assert!(!m("https://example.com/image.png"));
        assert!(!m("mailto:x@y.z"));
        assert!(!m("not a url"));
    }

    #[test]
    fn gopherus_steps_to_nearest_link_never_skipping() {
        let mut app = super::App::new(None, 23);
        app.last_inner = (80, 9); // center row 4
        // Links at 1, 3, and 6: from the top, 6 sits nearest the center,
        // but Down must visit 3 first.
        app.navigate_to(gopher_doc(".x.x..x......."));

        assert_eq!(selected(&app), Some(1));
        app.browser_arrow(1);
        assert_eq!(selected(&app), Some(3), "nearest link, not the center one");
        app.browser_arrow(1);
        assert_eq!(selected(&app), Some(6));
        // No further links below: sticks near the center while scrolling.
        app.browser_arrow(1);
        assert_eq!(selected(&app), Some(6));
        // Mirror going up: 3 is farther from the center than 6, so the
        // handoff back upward waits (sticky) rather than snapping.
        app.browser_arrow(-1);
        assert_eq!(selected(&app), Some(6));
    }

    #[test]
    fn gopherus_adjacent_links_transition_and_hold_center() {
        let mut app = super::App::new(None, 23);
        app.last_inner = (80, 5); // center row 2
        app.navigate_to(gopher_doc("..xxx..."));

        let g = |app: &super::App| {
            let g = app.browser.as_ref().unwrap();
            (g.selected, g.scroll)
        };
        // First link sits exactly on the center row.
        assert_eq!(g(&app), (Some(2), 0));
        // Adjacent links below: the highlight steps down and the page
        // scrolls along, holding the selection on the center row.
        app.browser_arrow(1);
        assert_eq!(g(&app), (Some(3), 1));
        app.browser_arrow(1);
        assert_eq!(g(&app), (Some(4), 2));
        // Next line is text: the selection sticks and the page scrolls.
        app.browser_arrow(1);
        assert_eq!(g(&app), (Some(4), 3));
        // Page now pinned at the bottom, no link below: nothing changes.
        app.browser_arrow(1);
        assert_eq!(g(&app), (Some(4), 3));
    }

    #[test]
    fn gopherus_handoff_when_next_link_nears_center() {
        let mut app = super::App::new(None, 23);
        app.last_inner = (80, 7); // center row 3
        app.navigate_to(gopher_doc(".x...x...."));

        assert_eq!(selected(&app), Some(1));
        // One scroll: link 5 is now closer to the center than the stuck
        // link 1, so the highlight hands off downward.
        app.browser_arrow(1);
        assert_eq!(selected(&app), Some(5));
        // Scrolling back up: link 1 is merely *as* close to the center,
        // not closer, so the selection stays put (sticky).
        app.browser_arrow(-1);
        assert_eq!(selected(&app), Some(5));
    }

    #[test]
    fn capped_at_limit() {
        let mut h = History::default();
        for i in 0..HISTORY_CAP + 10 {
            h.push(&format!("line {i}"));
        }
        assert_eq!(h.entries.len(), HISTORY_CAP);
        assert_eq!(h.entries[0], "line 10");
    }

    /// A page shaped like rubymaelstrom.com/chat, in the browser.
    fn chat_app() -> super::App {
        let html = r#"
            <p>Talkie says hello.</p>
            <form method="POST" action="/chat">
              <input type="hidden" name="session" value="cafe123">
              <input type="text" name="msg" placeholder="Type a message...">
              <button type="submit">Send</button>
            </form>"#;
        let base = url::Url::parse("https://example.com/chat").unwrap();
        let mut app = super::App::new(None, 23);
        app.last_inner = (60, 10);
        app.navigate_to(crate::http::parse(
            &base,
            "text/html",
            html.as_bytes(),
            60,
            &Default::default(),
        ));
        app
    }

    #[test]
    fn form_field_edits_through_the_input_prompt() {
        let mut app = chat_app();
        // The msg field is a selectable Form item on the laid-out doc.
        select_item(&mut app, |it| {
            it.link == Some(Link::Form { form: 0, field: 1 })
        });

        // Enter on the field opens the input prompt (empty: no value yet).
        app.browser_follow();
        assert_eq!(app.mode, super::Mode::Search);
        assert_eq!(
            app.search_target,
            Some(Link::Form { form: 0, field: 1 }),
            "prompt is aimed at the field"
        );
        assert_eq!(app.input, "");

        // Submitting the prompt stores the value and re-renders the row.
        app.mode = super::Mode::Search; // as the Enter handler leaves it
        app.run_search("hello there");
        let g = app.browser.as_ref().unwrap();
        assert_eq!(g.doc.forms[0].fields[1].value, "hello there");
        assert!(
            g.doc
                .rows
                .iter()
                .flat_map(|r| &r.items)
                .any(|it| it.text.contains("[hello there]")),
            "widget item shows the value"
        );

        // Re-editing prefills the prompt with the current value (the
        // selection re-anchored to the field across the re-render).
        select_item(&mut app, |it| {
            it.link == Some(Link::Form { form: 0, field: 1 })
        });
        app.browser_follow();
        assert_eq!(app.input, "hello there");
        assert_eq!(app.cursor, "hello there".chars().count());
    }

    #[test]
    fn a_bot_challenge_surfaces_a_notice_without_navigating() {
        let mut app = super::App::new(None, 23);
        app.last_inner = (80, 10);
        let response = crate::http::Response {
            url: url::Url::parse("https://www.imdb.com/list/ls123/").unwrap(),
            status: 202,
            content_type: String::from("text/html"),
            // The challenge interstitial: an empty shell with no real content.
            body: b"<html><body><div id=\"challenge-container\"></div></body></html>".to_vec(),
            js: None,
            live: None,
            challenge: Some(String::from("AWS WAF (challenge)")),
        };
        app.on_http_response(response, 60);
        assert!(app.notice, "the wall is surfaced as a persistent notice");
        assert!(
            app.browser.is_none(),
            "we don't navigate into the blank challenge shell"
        );
        assert!(
            app.status.contains("bot wall") && app.status.contains("AWS WAF (challenge)"),
            "status explains the wall: {}",
            app.status
        );
    }

    async fn live_form_app(html: &str) -> super::App {
        let base = url::Url::parse("https://example.com/chat").unwrap();
        let mut app = super::App::new(None, 23);
        app.last_inner = (60, 10);
        let response = crate::http::Response {
            url: base,
            status: 200,
            content_type: String::from("text/html"),
            body: html.as_bytes().to_vec(),
            js: None,
            live: None,
            challenge: None,
        };
        let response =
            crate::http::execute_js(response, app.last_inner, (8, 16), Default::default()).await;
        app.on_http_response(response, 60);
        app
    }

    async fn drain_page_event(app: &mut super::App) {
        let evt = app
            .page_rx
            .as_mut()
            .expect("live page receiver")
            .recv()
            .await
            .expect("page event");
        app.on_page_evt(evt);
    }

    /// A live SPA's anchor with a click listener becomes a `JsClick` marker
    /// that normally routes through the page actor. If the engine is gone
    /// (it died during load — e.g. a runaway script tripped the iteration
    /// limit), following such a link must FALL BACK to navigating its real
    /// href (progressive enhancement, what a browser does when JS is
    /// unavailable) instead of dead-ending on "scripts are no longer
    /// running". archive.org/details is the live case: most of its links are
    /// listener-wrapped anchors with real `/search.php?...` hrefs.
    #[tokio::test]
    async fn dead_engine_click_falls_back_to_navigating_the_href() {
        let mut app = live_form_app(
            r#"<a href="/next" onclick="window.x=1">go</a><button onclick="void 0">b</button><script>window.y=1</script>"#,
        )
        .await;
        assert!(app.live_page.is_some(), "page started live");
        // Select the anchor — wrapped as a JsClick carrying href "/next".
        let sel = {
            let g = app.browser.as_ref().unwrap();
            g.doc.rows.iter().enumerate().find_map(|(r, row)| {
                row.items.iter().enumerate().find_map(|(i, it)| {
                    matches!(&it.link, Some(Link::JsClick { href, .. }) if href.contains("next"))
                        .then_some((r, i))
                })
            })
        }
        .expect("the listener-bound anchor became a JsClick with its href");
        app.browser.as_mut().unwrap().sel_item = Some(sel);
        // The engine dies (what the RuntimeLimit panic does to the actor).
        app.drop_live_page();
        assert!(app.live_page.is_none());
        // Following it now navigates the href instead of dead-ending.
        app.browser_follow();
        assert!(
            app.loading(),
            "dead-engine click kicked off a navigation (status: {})",
            app.status
        );
        assert!(
            app.status.contains("/next"),
            "navigated to the anchor's href: {}",
            app.status
        );
        assert!(
            !app.status.contains("no longer running"),
            "did not dead-end: {}",
            app.status
        );
    }

    #[tokio::test]
    async fn live_form_field_edits_through_the_page_actor() {
        let html = r#"
            <form method="POST" action="/chat">
              <input type="hidden" name="session" value="cafe123">
              <input id="msg" type="text" name="msg" placeholder="Type a message...">
              <button type="submit">Send</button>
            </form>
            <p id="out"></p>
            <script>
              const msg = document.getElementById('msg');
              const out = document.getElementById('out');
              const log = [];
              msg.addEventListener('input', () => log.push('input:' + msg.value));
              msg.addEventListener('change', () => { log.push('change:' + msg.value); out.textContent = log.join('|'); });
            </script>"#;
        let mut app = live_form_app(html).await;
        assert!(app.live_page.is_some(), "scripted form stays live");
        select_item(&mut app, |it| {
            it.link == Some(Link::Form { form: 0, field: 1 })
        });
        app.browser_follow();
        assert_eq!(app.mode, super::Mode::Search);
        app.run_search("hello live");
        assert!(app.page_busy, "edit dispatched to page actor");
        drain_page_event(&mut app).await;
        let g = app.browser.as_ref().unwrap();
        assert_eq!(g.doc.forms[0].fields[1].value, "hello live");
        assert!(
            g.doc
                .rows
                .iter()
                .flat_map(|r| &r.items)
                .any(|it| it.text.contains("input:hello live|change:hello live")),
            "input/change result rendered"
        );
    }

    #[tokio::test]
    async fn live_form_prevented_submit_updates_without_http_fetch() {
        let html = r#"
            <form method="POST" action="/chat">
              <input id="msg" type="text" name="msg" value="hi">
              <button type="submit">Send</button>
            </form>
            <p id="out"></p>
            <script>
              const form = document.querySelector('form');
              form.addEventListener('submit', (event) => {
                event.preventDefault();
                document.getElementById('out').textContent = 'handled:' + form.querySelector('input').value + ':' + event.submitter.textContent;
              });
            </script>"#;
        let mut app = live_form_app(html).await;
        let submit = app.browser.as_ref().unwrap().doc.forms[0]
            .fields
            .iter()
            .position(|f| f.kind == crate::doc::FieldKind::Submit)
            .expect("submit field");
        select_item(&mut app, move |it| {
            it.link
                == Some(Link::Form {
                    form: 0,
                    field: submit,
                })
        });
        app.browser_follow();
        assert!(app.page_busy, "submit dispatched to page actor");
        drain_page_event(&mut app).await;
        assert!(app.fetch_rx.is_none(), "preventDefault blocked HTTP submit");
        assert!(
            app.browser
                .as_ref()
                .unwrap()
                .doc
                .rows
                .iter()
                .flat_map(|r| &r.items)
                .any(|it| it.text.contains("handled:hi:Send")),
            "submit handler result rendered"
        );
    }

    /// Regression: a live update that introduces an image (mounted AFTER
    /// first paint) must start the image pipeline for it. archive.org's
    /// collection tiles are `services/img/<id>` raster thumbnails fetched
    /// post-DOMContentLoaded — they arrive in the FILLED render via the
    /// live channel, not on the initial-load path. Here a `setTimeout(0)`
    /// (fires during settle) mounts an `<img>`, so the shell has no image
    /// but the fill does; `replace_live_doc` must kick off the decode or
    /// the tile stays alt text forever.
    #[tokio::test]
    async fn live_update_loads_images_mounted_after_first_paint() {
        let html = r#"
            <button onclick="void 0">menu</button>
            <div id="grid"></div>
            <script>
              setTimeout(function () {
                var img = document.createElement('img');
                img.src = 'tile.png';
                img.alt = 'tile';
                document.getElementById('grid').appendChild(img);
              }, 0);
            </script>"#;
        let mut app = live_form_app(html).await;
        assert!(app.live_page.is_some(), "clickable page stays live");
        // The shell painted first, before the setTimeout fired — no image
        // yet, so the initial-load path started no batch.
        assert!(app.imgs_rx.is_none(), "shell carried no images");
        // Drain the filled render the settle (setTimeout) produced.
        drain_page_event(&mut app).await;
        let g = app.browser.as_ref().unwrap();
        assert!(
            g.doc.image_urls.iter().any(|u| u.ends_with("tile.png")),
            "filled render carries the mounted tile image: {:?}",
            g.doc.image_urls
        );
        assert!(
            app.imgs_rx.is_some(),
            "the live update kicked off the image pipeline for the new tile"
        );
    }

    /// Build a tall laid-out HTTP browser fixture: a `/top` link, then `filler`
    /// short paragraphs (so the doc far exceeds the 10-row viewport).
    fn tall_browser_app(filler: usize) -> (super::App, String) {
        let url = url::Url::parse("https://example.com/p").unwrap();
        let mut body = String::from(r#"<body><a href="/top">top link</a>"#);
        for i in 0..filler {
            body += &format!("<p>line {i}</p>");
        }
        body += "</body>";
        let doc = crate::http::parse(&url, "text/html", body.as_bytes(), 60, &Default::default());
        let mut app = super::App::new(None, 23);
        app.mode = super::Mode::Session;
        app.last_inner = (60, 10);
        app.browser = Some(super::BrowserView {
            doc,
            selected: None,
            sel_item: None,
            scroll: 0,
            history: Vec::new(),
        });
        (app, body)
    }

    #[tokio::test]
    async fn an_autonomous_rerender_keeps_a_scrolled_away_view() {
        // Now that the engine runs at rest, a timer/animation re-renders the
        // live doc on its own. Such an autonomous update must NOT drag the
        // viewport back to a selection the user scrolled away from — their
        // scroll is sacred (else a background timer makes the page unreadable,
        // snapping to the selection every frame).
        let (mut app, body) = tall_browser_app(50);
        let height = 10usize;
        {
            let g = app.browser.as_mut().unwrap();
            g.sel_item = super::App::http_first_visible_item(g, height);
            assert!(
                g.sel_item.is_some_and(|(r, _)| r < height),
                "the top link is selected and on-screen at load"
            );
            // The user wheel-scrolls far down; the selection is now off-screen.
            g.scroll = 30;
        }
        // A timer tick re-renders with unchanged content.
        app.replace_live_doc(body.into_bytes());
        assert_eq!(
            app.browser.as_ref().unwrap().scroll,
            30,
            "the user's scroll survived the autonomous re-render"
        );
    }

    #[tokio::test]
    async fn an_update_recenters_a_selection_it_pushed_off_screen() {
        // The complement: when the selection WAS visible and an update inserts
        // content above it (shoving it off-screen), the viewport follows so it
        // stays in view. Proves the narrowed re-center still fires when it should.
        let (mut app, _) = tall_browser_app(50);
        let height = 10usize;
        {
            let g = app.browser.as_mut().unwrap();
            g.sel_item = super::App::http_first_visible_item(g, height);
            // scroll stays 0 — the selected top link is visible.
        }
        assert!(
            app.browser
                .as_ref()
                .unwrap()
                .sel_item
                .is_some_and(|(r, _)| r < height)
        );
        // The update prepends 30 lines above the link, pushing it below the fold.
        let mut updated = String::from("<body>");
        for i in 0..30 {
            updated += &format!("<p>before {i}</p>");
        }
        updated += r#"<a href="/top">top link</a>"#;
        for i in 0..50 {
            updated += &format!("<p>after {i}</p>");
        }
        updated += "</body>";
        app.replace_live_doc(updated.into_bytes());
        let g = app.browser.as_ref().unwrap();
        assert!(
            g.scroll > 0,
            "the viewport followed the selection the update pushed off-screen (scroll={})",
            g.scroll
        );
        let (r, _) = g.sel_item.expect("selection re-found after the update");
        assert!(
            r >= g.scroll && r < g.scroll + height,
            "the re-found selection {r} is visible in [{}, {})",
            g.scroll,
            g.scroll + height
        );
    }

    #[tokio::test]
    async fn form_submits_via_the_button_item() {
        let mut app = chat_app();
        // Fill the message, then press the Send button item.
        select_item(&mut app, |it| {
            it.link == Some(Link::Form { form: 0, field: 1 })
        });
        app.browser_follow();
        app.mode = super::Mode::Search;
        app.run_search("hi talkie");
        // The submit control is the button (field 2 in the chat form).
        let submit = app.browser.as_ref().unwrap().doc.forms[0]
            .fields
            .iter()
            .position(|f| f.kind == crate::doc::FieldKind::Submit)
            .expect("a submit control");
        select_item(&mut app, move |it| {
            it.link
                == Some(Link::Form {
                    form: 0,
                    field: submit,
                })
        });
        app.browser_follow();
        // A POST fetch to the form action is now in flight (hidden +
        // typed fields encoded).
        assert!(app.loading(), "submit kicked off a fetch");
    }

    /// Render a grid of `object-fit:contain` tile images (archive.org's
    /// collection tiles) to a TestBackend and assert it doesn't panic. The
    /// abort on `/details/<id>` is in the render path (layout + JS are
    /// clean): the contain-fit change makes the (now shorter) covers fit the
    /// viewport, so render_inline_images actually draws them.
    #[tokio::test]
    async fn rendering_a_grid_of_contain_tile_images_does_not_panic() {
        use ratatui::{Terminal, backend::TestBackend};
        // A real raster "cover" (landscape, like a librivox cover).
        let cover = image::RgbImage::from_fn(140, 90, |x, y| {
            image::Rgb([(x % 256) as u8, (y % 256) as u8, 90])
        });
        let mut png = Vec::new();
        image::DynamicImage::ImageRgb8(cover)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let cell = super::natural_cell_box(
            &image::load_from_memory(&png).unwrap(),
            (8u16, 16u16).into(),
        );

        // A wrapping grid of tiles, each: <a> → contain cover + title.
        let mut tiles = String::new();
        for i in 0..18 {
            tiles.push_str(&format!(
                r#"<a href="/d{i}"><div style="display:flex;height:16rem;justify-content:center">
                   <img src="/c{i}.png" style="width:100%;height:100%;object-fit:contain"></div>
                   <h3>Tile {i}</h3></a>"#
            ));
        }
        let html =
            format!(r#"<body><div style="display:flex;flex-wrap:wrap">{tiles}</div></body>"#);
        let base = url::Url::parse("https://ex.com/").unwrap();
        let mut images = crate::layout::ImageSizes::new();
        for i in 0..18 {
            images.insert(format!("https://ex.com/c{i}.png"), cell);
        }

        let mut app = super::App::new(None, 23);
        // A few viewport sizes incl. a narrow single-column one (her shot).
        for (w, h) in [(90u16, 40u16), (44, 30), (38, 50)] {
            app.last_inner = (w.saturating_sub(2), h.saturating_sub(5));
            app.image_protocols.clear();
            let doc = crate::http::parse(
                &base,
                "text/html; charset=utf-8",
                html.as_bytes(),
                app.last_inner.0 as usize,
                &images,
            );
            app.navigate_to(doc);
            // Encode a protocol for every laid-out image item, the way
            // sync_image_encodes → on_enc would before a draw.
            let keys: Vec<super::EncKey> = app
                .browser
                .as_ref()
                .unwrap()
                .doc
                .rows
                .iter()
                .flat_map(|r| &r.items)
                .filter_map(|it| it.image.as_deref().map(|u| super::EncKey::for_item(u, it)))
                .collect();
            for key in keys {
                if app.image_protocols.contains_key(&key) {
                    continue;
                }
                let (image, _) = crate::img::decode(&png).unwrap();
                let size = ratatui::layout::Size::new(key.w, key.h);
                if let Ok(proto) = crate::img::encode_sliced(&app.picker, image, size, key.crop) {
                    app.image_protocols.insert(key, proto);
                }
            }
            let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
            term.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
            // Scroll through and redraw — encodes/areas shift as tiles enter.
            for _ in 0..6 {
                app.browser.as_mut().unwrap().scroll += 4;
                term.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
            }
        }
    }

    /// The core of the `SlicedProtocol` switch: a box encodes ONCE and every
    /// scroll position reuses it (the key is scroll-independent), so scrolling a
    /// tall image never re-encodes — and a box scrolled out of range is evicted
    /// so the cache stays bounded to the on-screen set.
    #[tokio::test]
    async fn scrolling_reuses_one_encode_and_evicts_when_out_of_range() {
        use ratatui::layout::Size;
        let png = crate::img::red_png();
        let (decoded, _) = crate::img::decode(&png).unwrap();
        let cell = super::natural_cell_box(&decoded, (8u16, 16u16).into());
        let base = url::Url::parse("https://ex.com/").unwrap();
        let mut images = crate::layout::ImageSizes::new();
        images.insert("https://ex.com/banner.png".to_string(), cell);
        // Image near the top, then a long column of text (> MAX_IMAGE_LOOKBACK
        // rows) so it can scroll entirely out of the encode scan range.
        let mut body = String::from(r#"<body><img src="/banner.png">"#);
        for i in 0..320 {
            body.push_str(&format!("<p>line {i}</p>"));
        }
        body.push_str("</body>");

        let mut app = super::App::new(None, 23);
        app.last_inner = (40, 12);
        let doc = crate::http::parse(
            &base,
            "text/html; charset=utf-8",
            body.as_bytes(),
            40,
            &images,
        );
        app.navigate_to(doc);
        // Seed the decoded cache + ONE box-keyed encode, as the pipeline would.
        app.image_cache.insert(
            "https://ex.com/banner.png".to_string(),
            super::DecodedImage {
                raw: png.clone().into(),
                cell,
            },
        );
        let key = app
            .browser
            .as_ref()
            .unwrap()
            .doc
            .rows
            .iter()
            .flat_map(|r| &r.items)
            .find_map(|it| it.image.as_deref().map(|u| super::EncKey::for_item(u, it)))
            .expect("an image item is laid out");
        let proto =
            crate::img::encode_sliced(&app.picker, decoded, Size::new(key.w, key.h), key.crop)
                .unwrap();
        app.image_protocols.insert(key.clone(), proto);

        // Scroll line-by-line through the image: the box key is scroll-invariant,
        // so it is ALREADY cached and sync requests NOTHING new (no per-line
        // re-encode) and never drops the still-visible encode.
        for scroll in 0..8usize {
            app.browser.as_mut().unwrap().scroll = scroll;
            app.sync_image_encodes();
            assert!(
                app.image_encoding.is_empty(),
                "scroll {scroll} spawned a re-encode"
            );
            assert!(
                app.image_protocols.contains_key(&key),
                "scroll {scroll} dropped the visible encode"
            );
        }

        // Scrolled far past the image (beyond the look-back window): it's evicted
        // so the protocol cache stays bounded to the on-screen set.
        let total = app.browser.as_ref().unwrap().doc.rows.len();
        let far = total.saturating_sub(12);
        assert!(
            far > crate::layout::MAX_IMAGE_LOOKBACK + key.h as usize,
            "page must be tall enough to scroll the image fully out of range"
        );
        app.browser.as_mut().unwrap().scroll = far;
        app.sync_image_encodes();
        assert!(
            !app.image_protocols.contains_key(&key),
            "an off-screen image's encode should be evicted (#3)"
        );
    }

    /// The image-encode sandbox contract: a panic on the blocking thread
    /// (a malformed image, a ratatui-image sixel edge case) is CAUGHT — it
    /// yields no protocol and the task completes normally, never unwinding
    /// the worker or aborting the process. And — the crux of the
    /// `/details/<id>` partial-crash bug — a background thread NEVER owns the
    /// terminal, so the panic hook leaves the live TUI untouched even when
    /// the contain-fit change lets many tile covers actually encode.
    #[tokio::test]
    async fn a_panicking_image_encode_is_contained() {
        // (The caught panic still logs a line via the default hook — that's
        // expected; the point is that it's caught, not fatal.)
        let result = tokio::task::spawn_blocking(|| {
            let protocol: Option<u8> =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Option<u8> {
                    panic!("simulated ratatui-image encode panic");
                }))
                .ok()
                .flatten();
            // A spawned/blocking thread is NOT the terminal owner, so the
            // global hook would not restore the screen for this panic.
            (protocol, super::TERMINAL_OWNER.with(|c| c.get()))
        })
        .await;
        let (protocol, owns_terminal) = result.expect("blocking task did not abort");
        assert_eq!(protocol, None, "the panic was caught → no protocol");
        assert!(
            !owns_terminal,
            "a background thread must never own the terminal (hook leaves the TUI alone)"
        );
    }

    /// Deliver a decoded+encoded image to the app, the way the
    /// blocking task does (halfblocks picker: deterministic, no tty).
    fn deliver_image(app: &mut super::App, url: Link, raw: Vec<u8>) {
        let (image, mime) = crate::img::decode(&raw).expect("test image decodes");
        let info = format!("{}×{} {mime}", image.width(), image.height());
        let size = ratatui::layout::Size::new(app.last_inner.0, app.last_inner.1);
        let protocol = crate::img::encode(&app.picker, image, size, false).unwrap();
        app.on_img(super::ImgMsg {
            url,
            raw: raw.into(),
            size: app.last_inner,
            result: Ok((protocol, info)),
        });
    }

    #[tokio::test]
    async fn image_viewer_opens_over_the_browser_and_closes_back() {
        use crossterm::event::{Event, KeyCode, KeyEvent};

        let mut app = chat_app();
        app.mode = super::Mode::Session; // keys go to the panels, not the prompt
        let url = Link::Http(url::Url::parse("https://example.com/cat.png").unwrap());
        deliver_image(&mut app, url.clone(), crate::img::red_png());

        let v = app.viewer.as_ref().expect("viewer open");
        assert_eq!(v.url, url);
        assert!(v.info.contains("4×4") && v.info.contains("image/png"));
        assert_eq!(v.encoded_for, app.last_inner);

        // Browser keys are captured by the viewer: Down must not move
        // the page selection underneath.
        let before = app.browser.as_ref().unwrap().selected;
        app.on_terminal_event(Event::Key(KeyEvent::from(KeyCode::Down)))
            .await;
        assert_eq!(app.browser.as_ref().unwrap().selected, before);
        assert!(app.viewer.is_some());

        // Esc closes the viewer; the page beneath is intact.
        app.on_terminal_event(Event::Key(KeyEvent::from(KeyCode::Esc)))
            .await;
        assert!(app.viewer.is_none());
        let g = app.browser.as_ref().expect("browser survived");
        assert!(
            g.doc
                .rows
                .iter()
                .flat_map(|r| &r.items)
                .any(|it| it.text.contains("Talkie"))
        );
    }

    #[test]
    fn failed_image_decode_reports_and_keeps_the_page() {
        let mut app = chat_app();
        app.on_img(super::ImgMsg {
            url: Link::Http(url::Url::parse("https://example.com/cat.png").unwrap()),
            raw: Vec::new().into(),
            size: app.last_inner,
            result: Err(String::from("unrecognized image format")),
        });
        assert!(app.viewer.is_none());
        assert!(app.notice, "failure must not hide behind the link hint");
        assert!(app.status.contains("unrecognized image format"));
    }

    #[tokio::test]
    async fn gopher_image_items_are_followable() {
        let mut app = super::App::new(None, 23);
        app.last_inner = (80, 10);
        let mut doc = gopher_doc("x");
        doc.lines[0].link = Some(Link::Gopher(crate::gopher::GopherUrl {
            host: String::from("test.host"),
            port: 70,
            item_type: 'p',
            selector: String::from("/cat.png"),
        }));
        app.navigate_to(doc);
        app.browser_follow();
        assert!(
            app.status.starts_with("Fetching"),
            "type p starts a fetch instead of 'not supported': {}",
            app.status
        );
    }

    #[tokio::test]
    async fn set_image_forces_protocol_and_reencodes_the_viewer() {
        use ratatui_image::picker::ProtocolType;

        let mut app = super::App::new(None, 23);
        app.last_inner = (40, 12);
        let url = Link::Http(url::Url::parse("https://example.com/cat.png").unwrap());
        deliver_image(&mut app, url, crate::img::red_png());

        app.execute_command("set image sixel").await;
        assert_eq!(app.picker.protocol_type(), ProtocolType::Sixel);
        assert_eq!(
            app.viewer.as_ref().unwrap().encoded_for,
            (0, 0),
            "open viewer is marked for re-encode"
        );
        app.execute_command("set image auto").await;
        assert_eq!(
            app.picker.protocol_type(),
            ProtocolType::Halfblocks,
            "auto restores the startup query result"
        );
        app.execute_command("set image vhs").await;
        assert!(app.status.starts_with("usage:"));
    }

    #[tokio::test]
    async fn shift_movement_selects_and_edits_replace() {
        use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
        let mut app = super::App::new(None, 23); // starts at the trust> prompt
        app.input = String::from("open bbs.example 23");
        app.cursor = app.input.chars().count();
        let key = |code, mods| Event::Key(KeyEvent::new(code, mods));

        // Shift+Left three times selects " 23"; Backspace removes it.
        for _ in 0..3 {
            app.on_terminal_event(key(KeyCode::Left, KeyModifiers::SHIFT))
                .await;
        }
        assert_eq!(app.selection(), Some((16, 19)));
        app.on_terminal_event(key(KeyCode::Backspace, KeyModifiers::NONE))
            .await;
        assert_eq!(app.input, "open bbs.example");
        assert_eq!(app.cursor, 16);
        assert_eq!(app.selection(), None);

        // Shift+Home selects everything; typing replaces the lot.
        app.on_terminal_event(key(KeyCode::Home, KeyModifiers::SHIFT))
            .await;
        assert_eq!(app.selection(), Some((0, 16)));
        app.on_terminal_event(key(KeyCode::Char('q'), KeyModifiers::NONE))
            .await;
        assert_eq!(app.input, "q");
        assert_eq!(app.cursor, 1);

        // A plain arrow clears any selection instead of extending it.
        app.on_terminal_event(key(KeyCode::Left, KeyModifiers::SHIFT))
            .await;
        assert!(app.selection().is_some());
        app.on_terminal_event(key(KeyCode::Right, KeyModifiers::NONE))
            .await;
        assert_eq!(app.selection(), None);
        assert_eq!(app.input, "q", "plain movement must not edit");
    }

    #[tokio::test]
    async fn opening_telnet_closes_the_browser() {
        let mut app = super::App::new(None, 23);
        app.last_inner = (80, 10);
        app.navigate_to(gopher_doc(".x."));
        deliver_image(
            &mut app,
            Link::Http(url::Url::parse("https://example.com/cat.png").unwrap()),
            crate::img::red_png(),
        );
        assert!(app.browser.is_some() && app.viewer.is_some());

        // `open <host> 23` for telnet must take the screen, not connect
        // invisibly behind the browser. (Port 23 still routes to telnet;
        // a bare host or other port would open the web instead.)
        app.execute_command("open 127.0.0.1 23").await;
        assert!(app.browser.is_none(), "browser closed");
        assert!(app.viewer.is_none(), "viewer closed");
    }

    #[test]
    fn splits_trailing_ports_but_not_ipv6() {
        use super::split_host_port;
        assert_eq!(
            split_host_port("isharmud.com:23"),
            ("isharmud.com", Some(23))
        );
        assert_eq!(split_host_port("bbs.example"), ("bbs.example", None));
        // Not a port: left attached to the host.
        assert_eq!(split_host_port("host:name"), ("host:name", None));
        // Raw IPv6 literals keep all their colons.
        assert_eq!(split_host_port("::1"), ("::1", None));
        assert_eq!(split_host_port("fe80::1:23"), ("fe80::1:23", None));
    }

    #[tokio::test]
    async fn esc_opens_command_mode_in_line_mode() {
        use crossterm::event::{Event, KeyCode, KeyEvent};
        let mut app = super::App::new(Some(String::from("h")), 23);
        assert_eq!(app.mode, super::Mode::Session);

        app.on_terminal_event(Event::Key(KeyEvent::from(KeyCode::Esc)))
            .await;
        assert_eq!(app.mode, super::Mode::Command);
        // And Esc backs out again.
        app.on_terminal_event(Event::Key(KeyEvent::from(KeyCode::Esc)))
            .await;
        assert_eq!(app.mode, super::Mode::Session);
    }

    #[test]
    fn char_mode_follows_linemode_edit_with_echo_dominant() {
        use super::{InputMode, op_option};

        let mut app = super::App::new(Some(String::from("h")), 23);
        app.connected = true;

        // No LINEMODE, no ECHO → line mode (local editing).
        assert!(!app.char_mode());

        // LINEMODE active with EDIT set stays line mode...
        app.linemode_active = true;
        app.linemode_edit = true;
        assert!(!app.char_mode());
        // ...and EDIT clear flips to character-at-a-time.
        app.linemode_edit = false;
        assert!(app.char_mode());

        // ECHO dominates even under LINEMODE EDIT (password prompts): the
        // server echoing forces character mode regardless of the EDIT bit.
        app.linemode_edit = true;
        app.remote_opts.insert(op_option::ECHO);
        assert!(app.char_mode());
        app.remote_opts.remove(&op_option::ECHO);

        // A manual override still wins over the negotiated state.
        app.linemode_edit = false; // negotiated character mode
        app.mode_override = Some(InputMode::Line);
        assert!(!app.char_mode());
        app.mode_override = Some(InputMode::Character);
        assert!(app.char_mode());
    }

    #[tokio::test]
    async fn tab_toggles_the_command_console() {
        use crossterm::event::{Event, KeyCode, KeyEvent};
        // No host → starts at the command prompt.
        let mut app = super::App::new(None, 23);
        assert_eq!(app.mode, super::Mode::Command);
        // Tab folds the console away (back to the session)...
        app.on_terminal_event(Event::Key(KeyEvent::from(KeyCode::Tab)))
            .await;
        assert_eq!(app.mode, super::Mode::Session);
        // ...and Tab opens it again.
        app.on_terminal_event(Event::Key(KeyEvent::from(KeyCode::Tab)))
            .await;
        assert_eq!(app.mode, super::Mode::Command);
    }

    #[tokio::test]
    async fn esc_stops_loading_and_keeps_the_browser_open() {
        use crossterm::event::{Event, KeyCode, KeyEvent};
        let mut app = super::App::new(None, 23);
        app.mode = super::Mode::Session; // browsing captures session-mode keys
        app.last_inner = (80, 10);
        app.navigate_to(gopher_doc(".x."));
        assert!(app.browser.is_some());
        // Simulate an in-flight fetch.
        let (_tx, rx) = tokio::sync::mpsc::channel(1);
        app.fetch_rx = Some(rx);
        assert!(app.loading());

        // Esc stops the load but does NOT close the browser or drop to the
        // telnet terminal (the new web-first behaviour).
        app.on_terminal_event(Event::Key(KeyEvent::from(KeyCode::Esc)))
            .await;
        assert!(!app.loading(), "the load was cancelled");
        assert!(app.browser.is_some(), "browser stays open on Esc");
        assert!(app.status.starts_with("Stopped"), "got: {}", app.status);
    }

    #[test]
    fn host_like_tokens_are_recognized() {
        use super::looks_like_host;
        assert!(looks_like_host("example.com"));
        assert!(looks_like_host("www.rubymaelstrom.com"));
        assert!(looks_like_host("192.168.0.1"));
        assert!(looks_like_host("localhost"));
        assert!(looks_like_host("localhost:8080"));
        assert!(looks_like_host("shop.example:1701"));
        // Bare words (command typos) are not host-like.
        assert!(!looks_like_host("reload"));
        assert!(!looks_like_host("quit"));
        assert!(!looks_like_host("status"));
    }

    #[tokio::test]
    async fn bare_urls_open_and_telnet_urls_split_ports() {
        let mut app = super::App::new(None, 23);

        // A bare URL at the trust> prompt behaves like `open <url>`.
        app.execute_command("gemini://gem.sdf.org").await;
        assert!(
            app.status.starts_with("Fetching gemini://gem.sdf.org"),
            "got: {}",
            app.status
        );

        // telnet:// URLs treat a trailing :port as the port.
        app.execute_command("telnet://isharmud.com:2323").await;
        assert_eq!(app.host.as_deref(), Some("isharmud.com"));
        assert_eq!(app.port, 2323);

        // telnets:// without a port keeps its 992 convention.
        app.execute_command("open telnets://secure.example").await;
        assert_eq!(app.port, 992);
        assert!(app.tls || !app.connected, "TLS path taken");

        // A bare host:port on a telnet port still opens telnet (host:port split).
        app.execute_command("open bbs.example:23").await;
        assert_eq!(app.host.as_deref(), Some("bbs.example"));
        assert_eq!(app.port, 23);

        // A bare host:port on ANY non-web port opens TELNET — the web lives on
        // its standard ports, an odd port is a MUD/BBS (the
        // flexiblesurvival.com:2222 fix; this used to fire an HTTP GET).
        app.execute_command("open shop.example:1701").await;
        assert_eq!(app.host.as_deref(), Some("shop.example"));
        assert_eq!(app.port, 1701);
        assert!(!app.tls, "plain telnet, not telnets");

        // ...but the standard web ports stay web even when given explicitly.
        app.execute_command("open shop.example:80").await;
        assert!(
            app.status.starts_with("Fetching http://shop.example/"),
            "got: {}",
            app.status
        );

        // A bare hostname with no port opens the web — https by default.
        app.execute_command("open www.rubymaelstrom.com").await;
        assert!(
            app.status
                .starts_with("Fetching https://www.rubymaelstrom.com/"),
            "got: {}",
            app.status
        );

        // The same hostname typed WITHOUT `open` (the address-bar habit)
        // opens too; a non-host typo still falls through to the usage hint.
        app.execute_command("duckduckgo.com").await;
        assert!(
            app.status.starts_with("Fetching https://duckduckgo.com/"),
            "got: {}",
            app.status
        );
        app.execute_command("notacommand").await;
        assert!(
            app.status.starts_with("unknown command"),
            "got: {}",
            app.status
        );
    }

    #[tokio::test]
    async fn oneshot_commands_build_the_right_queries() {
        let mut app = super::App::new(None, 23);

        app.execute_command("finger ruby@sdf.org").await;
        assert!(
            app.status.starts_with("Fetching finger://sdf.org/ruby"),
            "got: {}",
            app.status
        );
        app.execute_command("whois example.com").await;
        assert!(
            app.status
                .starts_with("Fetching whois://whois.iana.org/example.com"),
            "got: {}",
            app.status
        );
        app.execute_command("dict rain").await;
        assert!(
            app.status.starts_with("Fetching dict://dict.org/rain"),
            "got: {}",
            app.status
        );
        // URLs work through open and bare dispatch too.
        app.execute_command("dict://dict.org/d:neon").await;
        assert!(
            app.status.starts_with("Fetching dict://dict.org/neon"),
            "got: {}",
            app.status
        );
        app.execute_command("finger").await;
        assert!(app.status.starts_with("usage: finger"));
    }

    #[test]
    fn ports_resolve_by_number_or_service_name() {
        use super::parse_port;
        assert_eq!(parse_port("2323"), Some(2323));
        assert_eq!(parse_port("telnet"), Some(23));
        assert_eq!(parse_port("smtp"), Some(25));
        assert_eq!(parse_port("gemini"), Some(1965));
        assert_eq!(parse_port("warpgate"), None);
    }

    #[tokio::test]
    async fn status_60_prompts_for_and_mints_an_identity() {
        unsafe {
            std::env::set_var(
                "TRUST_IDENTITIES",
                std::env::temp_dir().join(format!("trust-test-ids-{}", std::process::id())),
            );
        }
        let mut app = super::App::new(None, 23);
        app.last_inner = (80, 10);
        let url = crate::gemini::GeminiUrl::parse("gemini://astro.test/app").unwrap();
        app.on_gemini_response(
            crate::gemini::Response {
                url: url.clone(),
                status: 60,
                meta: String::from("Certificate required"),
                body: Vec::new(),
                identity: false,
            },
            80,
        );

        // The amber prompt opens, prefilled with the system username.
        assert_eq!(app.mode, super::Mode::Search);
        assert_eq!(app.cert_for, Some(url));
        assert_eq!(app.input, std::env::var("USER").unwrap_or_default());

        // Naming it mints the file and retries the capsule.
        app.run_search("neon-ruby");
        let path = crate::tls::identity_path("astro.test").unwrap();
        let pem = std::fs::read_to_string(&path).expect("identity written");
        assert!(pem.contains("BEGIN CERTIFICATE"));
        assert!(app.cert_for.is_none());
        assert!(
            app.status.starts_with("Fetching gemini://astro.test"),
            "retries the page: {}",
            app.status
        );

        // A second 60 for the same host (cert now exists but the
        // server rejected it, say) must NOT offer to overwrite.
        app.run_search("x"); // no pending prompt: routes nowhere
        assert!(!app.status.contains("saved"), "{}", app.status);
    }

    #[test]
    fn form_selects_cycle_and_checkboxes_toggle() {
        use crossterm::event::{KeyCode, KeyEvent};
        let html = r#"
            <form action="/s">
              <select name="region">
                <option value="all" selected>Everywhere</option>
                <option value="us">United States</option>
              </select>
              <input type="checkbox" name="safe">
              <input type="submit" value="Go">
            </form>"#;
        let base = url::Url::parse("http://search.example/").unwrap();
        let mut app = super::App::new(None, 23);
        app.last_inner = (60, 10);
        app.navigate_to(crate::http::parse(
            &base,
            "text/html",
            html.as_bytes(),
            60,
            &Default::default(),
        ));

        let has_widget = |app: &super::App, needle: &str| {
            app.browser
                .as_ref()
                .unwrap()
                .doc
                .rows
                .iter()
                .flat_map(|r| &r.items)
                .any(|it| it.text.contains(needle))
        };

        // Activating a <select> opens the dropdown, highlighting the
        // current value; it doesn't change anything until a pick.
        app.form_interact(0, 0);
        let menu = app.select_menu.as_ref().expect("dropdown open");
        assert_eq!(menu.options.len(), 2);
        assert_eq!(menu.highlight, 0, "starts on the current value");
        assert_eq!(
            app.browser.as_ref().unwrap().doc.forms[0].fields[0].value,
            "all"
        );
        // Arrow down to "United States" and pick it.
        app.select_menu_nav(KeyEvent::from(KeyCode::Down));
        app.select_menu_nav(KeyEvent::from(KeyCode::Enter));
        assert!(app.select_menu.is_none(), "dropdown closes on pick");
        assert_eq!(
            app.browser.as_ref().unwrap().doc.forms[0].fields[0].value,
            "us"
        );
        assert!(has_widget(&app, "[United States ▾]"));

        // Esc cancels without changing the value.
        app.form_interact(0, 0);
        app.select_menu_nav(KeyEvent::from(KeyCode::Down));
        app.select_menu_nav(KeyEvent::from(KeyCode::Esc));
        assert!(app.select_menu.is_none());
        assert_eq!(
            app.browser.as_ref().unwrap().doc.forms[0].fields[0].value,
            "us"
        );

        app.form_interact(0, 1); // toggle the box
        assert!(app.browser.as_ref().unwrap().doc.forms[0].fields[1].checked);
        assert!(has_widget(&app, "[x] safe"));
        let g = app.browser.as_ref().unwrap();
        // The chosen select survived the toggle's re-render (seeding).
        assert_eq!(g.doc.forms[0].fields[0].value, "us");
    }

    #[test]
    fn select_menu_hover_moves_highlight() {
        use crossterm::event::{KeyModifiers, MouseEvent, MouseEventKind};
        let mut app = super::App::new(None, 23);
        app.select_menu = Some(super::SelectMenu {
            form: 0,
            field: 0,
            options: vec![
                ("A".into(), "a".into()),
                ("B".into(), "b".into()),
                ("C".into(), "c".into()),
            ],
            highlight: 0,
            scroll: 0,
            anchor_row: 0,
            anchor_col: 0,
        });
        // A 10×5 popup at (5,2): border on the edges, options on rows 3,4,5.
        app.last_select_rect = Some(ratatui::layout::Rect::new(5, 2, 10, 5));
        let moved = |col, row| MouseEvent {
            kind: MouseEventKind::Moved,
            column: col,
            row,
            modifiers: KeyModifiers::empty(),
        };
        // Hovering an option row moves the highlight to it.
        app.select_menu_mouse(moved(8, 4));
        assert_eq!(app.select_menu.as_ref().unwrap().highlight, 1);
        app.select_menu_mouse(moved(8, 5));
        assert_eq!(app.select_menu.as_ref().unwrap().highlight, 2);
        // Hovering off the popup leaves the highlight (and the menu) alone.
        app.select_menu_mouse(moved(8, 30));
        assert!(app.select_menu.is_some());
        assert_eq!(app.select_menu.as_ref().unwrap().highlight, 2);
    }
}
