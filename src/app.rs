//! Application state and the main event loop.

use std::collections::HashSet;

use crossterm::event::{
    Event as TermEvent, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind,
};
use futures::StreamExt;
use libmudtelnet::telnet::{op_command, op_option};
use ratatui::DefaultTerminal;
use tokio::sync::mpsc;
use tui_term::vt100;

use crate::cp437;
use crate::gopher::{self, GopherDoc, GopherUrl};
use crate::telnet;
use crate::ui;

/// What the input field feeds, mirroring GNU telnet's two states: lines go
/// to the remote host, or to the `telnet>` command prompt reached with Ctrl-].
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Session,
    Command,
    /// Query entry for a gopher type-7 search item.
    Search,
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

/// The gopherus-style browser state: a scrolling viewport with a link
/// cursor constrained to it, and an in-RAM back history.
pub struct GopherView {
    pub doc: GopherDoc,
    /// The selected link's line index. Always a visible link when one is
    /// on screen; None while no link is in the viewport.
    pub selected: Option<usize>,
    /// First visible line.
    pub scroll: usize,
    history: Vec<(GopherDoc, Option<usize>, usize)>,
}

/// Result of a background gopher fetch.
struct FetchMsg {
    url: GopherUrl,
    result: Result<Vec<u8>, String>,
}

pub struct App {
    pub mode: Mode,
    /// Terminal emulation of the remote byte stream, rendered by tui-term.
    pub vt: Vt,
    /// Inbound byte interpretation (`set encoding cp437` for BBS art).
    pub encoding: Encoding,
    /// GNU telnet's `crlf` toggle: Enter sends CR LF when true, CR NUL
    /// when false (char mode only; line mode always sends CR LF).
    crlf: bool,
    /// Bell count already forwarded to the real terminal.
    bells_seen: usize,
    /// Options the server has enabled on its side (WILL ...).
    remote_opts: HashSet<u8>,
    /// Options the server has enabled on our side (DO ...).
    local_opts: HashSet<u8>,
    /// The local-echo entry field at the bottom of the screen.
    pub input: String,
    /// Cursor position in `input`, counted in chars.
    pub cursor: usize,
    pub host: Option<String>,
    pub port: u16,
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
    /// When Some, the gopher browser replaces the terminal panel.
    pub gopher: Option<GopherView>,
    /// In-flight gopher fetch, if any.
    fetch_rx: Option<mpsc::Receiver<FetchMsg>>,
    /// The type-7 item awaiting a query from Mode::Search.
    search_target: Option<GopherUrl>,
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
        Self {
            mode,
            // In memory only, like the entry histories.
            vt: new_vt(24, 80),
            encoding: Encoding::Utf8,
            crlf: false,
            bells_seen: 0,
            remote_opts: HashSet::new(),
            local_opts: HashSet::new(),
            input: String::new(),
            cursor: 0,
            host,
            port,
            connected: false,
            tls: false,
            status: String::from("No connection. Ctrl-] for commands."),
            last_inner: (80, 24),
            mode_override: None,
            session_history: History::default(),
            command_history: History::default(),
            probes: ProbeDetector::default(),
            gopher: None,
            fetch_rx: None,
            search_target: None,
            conn: None,
            events: None,
            quit: false,
        }
    }

    pub async fn run(mut self, mut terminal: DefaultTerminal) -> std::io::Result<()> {
        let mut input = EventStream::new();
        // Draw once before connecting so the first NAWS negotiation reports
        // the widget's real size instead of the 80x24 default.
        terminal.draw(|frame| ui::draw(frame, &mut self))?;
        self.sync_vt_size().await;
        if let Some(host) = self.host.clone() {
            self.dispatch_open(&host, self.port);
        }

        while !self.quit {
            terminal.draw(|frame| ui::draw(frame, &mut self))?;
            self.sync_vt_size().await;
            self.sync_gopher_wrap();

            tokio::select! {
                event = input.next() => match event {
                    Some(Ok(event)) => self.on_terminal_event(event).await,
                    Some(Err(err)) => return Err(err),
                    None => break,
                },
                event = recv_opt(&mut self.events) => match event {
                    Some(event) => self.on_telnet_event(event).await,
                    None => self.events = None,
                },
                msg = recv_opt(&mut self.fetch_rx) => match msg {
                    Some(msg) => self.on_fetch(msg),
                    None => self.fetch_rx = None,
                },
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

    async fn on_terminal_event(&mut self, event: TermEvent) {
        let key = match event {
            TermEvent::Key(key) => key,
            TermEvent::Mouse(mouse) => {
                // 3 lines per wheel click, matching terminal convention.
                match (mouse.kind, self.gopher.is_some()) {
                    (MouseEventKind::ScrollUp, false) => self.scroll_by(3),
                    (MouseEventKind::ScrollDown, false) => self.scroll_by(-3),
                    (MouseEventKind::ScrollUp, true) => self.gopher_scroll(-3, true),
                    (MouseEventKind::ScrollDown, true) => self.gopher_scroll(3, true),
                    _ => {}
                }
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
            self.mode = match self.mode {
                Mode::Session | Mode::Search => Mode::Command,
                Mode::Command => Mode::Session,
            };
            return;
        }

        // The gopher browser captures session-mode keys while open.
        if self.mode == Mode::Session && self.gopher.is_some() {
            self.gopher_nav(key);
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

        match key.code {
            KeyCode::Esc if matches!(self.mode, Mode::Command | Mode::Search) => {
                self.mode = Mode::Session;
                self.search_target = None;
            }
            KeyCode::Enter => {
                let line = std::mem::take(&mut self.input);
                self.cursor = 0;
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
                self.input.insert(self.byte_cursor(), c);
                self.cursor += 1;
                self.active_history().detach();
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
                }
            }
            KeyCode::Down => {
                if let Some(text) = self.active_history().down() {
                    self.cursor = text.chars().count();
                    self.input = text;
                }
            }
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => {
                self.cursor = (self.cursor + 1).min(self.input.chars().count());
            }
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.input.chars().count(),
            _ => {}
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
            Mode::Command | Mode::Search => &mut self.command_history,
        }
    }

    /// True when keystrokes should bypass the input field, GNU telnet
    /// style: the server echoes (WILL ECHO), or the user forced it.
    pub fn char_mode(&self) -> bool {
        self.connected
            && match self.mode_override {
                Some(InputMode::Character) => true,
                Some(InputMode::Line) => false,
                None => self.remote_opts.contains(&op_option::ECHO),
            }
    }

    /// Byte offset of the char cursor into `input`.
    fn byte_cursor(&self) -> usize {
        self.input
            .char_indices()
            .nth(self.cursor)
            .map_or(self.input.len(), |(i, _)| i)
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

    async fn execute_command(&mut self, line: &str) {
        let mut parts = line.split_whitespace();
        match parts.next() {
            None => {}
            Some("quit" | "q" | "exit") => self.quit = true,
            Some("close" | "c") => match self.conn.take() {
                Some(conn) => {
                    let _ = conn.commands.send(telnet::Command::Close).await;
                }
                None => self.status = String::from("No connection to close."),
            },
            Some("open" | "o") => match parts.next() {
                Some(host) => {
                    let port = match parts.next() {
                        Some(p) => match p.parse() {
                            Ok(p) => p,
                            Err(_) => {
                                self.status = format!("bad port number: {p}");
                                return;
                            }
                        },
                        None => 23,
                    };
                    self.dispatch_open(host, port);
                }
                None => self.status = String::from("usage: open <host> [port]"),
            },
            Some("mode" | "m") => match parts.next() {
                Some("character" | "char") => {
                    self.mode_override = Some(InputMode::Character);
                    self.status = String::from("Input mode forced to character-at-a-time.");
                }
                Some("line") => {
                    self.mode_override = Some(InputMode::Line);
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
                _ => self.status = String::from("usage: set encoding cp437|utf8"),
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
            // TODO for GNU telnet parity: full set/unset, display,
            // logout, z (suspend), ! (shell escape).
            Some(other) => {
                self.status = format!(
                    "unknown command: {other} (open/close/mode/send/set/toggle/status/quit)"
                )
            }
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
             Remote options (WILL): {remote}\r\n\
             Local options (DO): {local}\r\n\
             \x1b[36m--------------------\x1b[0m\r\n",
            eol = if self.crlf { "CR LF" } else { "CR NUL" },
            enc = match self.encoding {
                Encoding::Utf8 => "UTF-8",
                Encoding::Cp437 => "CP437",
            },
            remote = option_names(&self.remote_opts),
            local = option_names(&self.local_opts),
        );
        self.vt.process(report.as_bytes());
    }

    /// Route an open target to the right protocol: gopher:// URLs and
    /// port 70 get the browser, everything else is telnet.
    fn dispatch_open(&mut self, target: &str, port: u16) {
        if let Some(url) = GopherUrl::parse(target) {
            self.start_fetch(url);
        } else if let Some(host) = target.strip_prefix("telnet://") {
            self.open(host.trim_end_matches('/').to_string(), port, false);
        } else if let Some(host) = target.strip_prefix("telnets://") {
            let host = host.trim_end_matches('/').to_string();
            let port = if port == 23 { 992 } else { port };
            self.open(host, port, true);
        } else if port == 70 {
            self.start_fetch(GopherUrl {
                host: target.to_string(),
                port,
                item_type: '1',
                selector: String::new(),
            });
        } else {
            // telnets convention: port 992 is telnet over TLS.
            self.open(target.to_string(), port, port == 992);
        }
    }

    /// Fetch a gopher item in the background; the result arrives in the
    /// select loop as a FetchMsg.
    fn start_fetch(&mut self, url: GopherUrl) {
        let (tx, rx) = mpsc::channel(1);
        self.fetch_rx = Some(rx);
        self.status = format!("Fetching {url} ...");
        tokio::spawn(async move {
            let result = gopher::fetch(&url).await;
            let _ = tx.send(FetchMsg { url, result }).await;
        });
    }

    fn on_fetch(&mut self, msg: FetchMsg) {
        self.fetch_rx = None;
        match msg.result {
            Ok(raw) => {
                let cp437 = self.encoding == Encoding::Cp437;
                let width = (self.last_inner.0 as usize).max(10);
                let doc = gopher::parse(&msg.url, raw, cp437, width);
                self.status = format!("{} — {} lines", msg.url, doc.lines.len());
                self.navigate_to(doc);
            }
            Err(err) => self.status = format!("gopher: {} — {err}", msg.url),
        }
    }

    /// Show a fetched document, pushing the current one onto the back
    /// history (RAM-only, dropped when the view closes).
    fn navigate_to(&mut self, doc: GopherDoc) {
        match &mut self.gopher {
            Some(g) => {
                let old = std::mem::replace(&mut g.doc, doc);
                g.history.push((old, g.selected, g.scroll));
                g.selected = None;
                g.scroll = 0;
            }
            None => {
                self.gopher = Some(GopherView {
                    doc,
                    selected: None,
                    scroll: 0,
                    history: Vec::new(),
                });
            }
        }
        // A fresh page selects its first visible link, gopherus-style.
        let height = self.last_inner.1.max(1) as usize;
        if let Some(g) = &mut self.gopher {
            g.selected = Self::gopher_visible_links(g, height).first().copied();
        }
    }

    /// gopherus keys: Up/Down scroll the page (the highlight rides the
    /// visible links), Right follows, Left goes back, Esc closes.
    fn gopher_nav(&mut self, key: KeyEvent) {
        let page = i64::from(self.last_inner.1.max(2)) - 1;
        match key.code {
            KeyCode::Up => self.gopher_arrow(-1),
            KeyCode::Down => self.gopher_arrow(1),
            KeyCode::PageUp => self.gopher_scroll(-page, false),
            KeyCode::PageDown => self.gopher_scroll(page, false),
            KeyCode::Home => self.gopher_scroll(i64::MIN / 2, false),
            KeyCode::End => self.gopher_scroll(i64::MAX / 2, false),
            KeyCode::Right | KeyCode::Enter => self.gopher_follow(),
            KeyCode::Left => self.gopher_back(),
            KeyCode::Esc => {
                self.gopher = None;
                self.status = String::from("Gopher view closed.");
            }
            _ => {}
        }
    }

    /// One Up/Down press, gopherus-style. If the adjacent line is also a
    /// link, the highlight steps onto it (the page scrolls along once the
    /// selection has reached the center of the screen, so it tends to stay
    /// there except near the document's ends). Otherwise the page scrolls
    /// under the sticky selection, and `gopher_retarget` decides when a
    /// new link takes the highlight. With the page pinned at either end,
    /// the highlight walks between the visible links instead.
    fn gopher_arrow(&mut self, dir: i64) {
        let height = self.last_inner.1.max(1) as i64;
        let center_row = (height - 1) / 2;
        let Some(g) = &mut self.gopher else { return };
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
            self.gopher_retarget(dir);
        } else {
            self.gopher_walk(dir);
        }
    }

    /// Scroll the viewport by `delta` lines (wheel, page keys, jumps) and
    /// re-aim the highlight. When the viewport can't move and
    /// `walk_at_edge` is set, step the highlight instead.
    fn gopher_scroll(&mut self, delta: i64, walk_at_edge: bool) {
        let height = self.last_inner.1.max(1) as i64;
        let Some(g) = &mut self.gopher else { return };
        let len = g.doc.lines.len() as i64;
        let max_scroll = (len - height).max(0);
        let target = (g.scroll as i64).saturating_add(delta).clamp(0, max_scroll);
        let moved = target != g.scroll as i64;
        g.scroll = target as usize;
        let dir = if delta >= 0 { 1 } else { -1 };
        if moved {
            self.gopher_retarget(dir);
        } else if walk_at_edge && delta != 0 {
            self.gopher_walk(dir);
        }
    }

    /// Re-wrap (and re-decode) the current document when the panel width
    /// or the encoding changed since it was parsed — including documents
    /// restored from history at an older width. The selection is carried
    /// over by its position in the document's link order.
    fn sync_gopher_wrap(&mut self) {
        let width = (self.last_inner.0 as usize).max(10);
        let cp437 = self.encoding == Encoding::Cp437;
        let height = self.last_inner.1.max(1) as usize;
        let Some(g) = &mut self.gopher else { return };
        if g.doc.raw.is_empty() || (g.doc.wrapped_to == width && g.doc.cp437 == cp437) {
            return;
        }
        let link_ordinal = g.selected.map(|sel| {
            g.doc.lines[..=sel]
                .iter()
                .filter(|l| l.link.is_some())
                .count()
        });
        let url = g.doc.url.clone();
        let raw = std::mem::take(&mut g.doc.raw);
        g.doc = gopher::parse(&url, raw, cp437, width);
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
    fn gopher_visible_links(g: &GopherView, height: usize) -> Vec<usize> {
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
    fn gopher_retarget(&mut self, dir: i64) {
        let height = self.last_inner.1.max(1) as usize;
        let Some(g) = &mut self.gopher else { return };
        let links = Self::gopher_visible_links(g, height);
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
    fn gopher_walk(&mut self, dir: i64) {
        let height = self.last_inner.1.max(1) as usize;
        let Some(g) = &mut self.gopher else { return };
        let links = Self::gopher_visible_links(g, height);
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

    fn gopher_follow(&mut self) {
        let Some(g) = &self.gopher else { return };
        let Some(link) = g
            .selected
            .and_then(|i| g.doc.lines.get(i))
            .and_then(|l| l.link.clone())
        else {
            return;
        };
        match link.item_type {
            '0' | '1' => self.start_fetch(link),
            '7' => {
                self.search_target = Some(link);
                self.mode = Mode::Search;
                self.input.clear();
                self.cursor = 0;
            }
            'h' => {
                // HTML items carry "URL:<target>" selectors by convention.
                let target = link.selector.strip_prefix("URL:").unwrap_or(&link.selector);
                self.status = format!("web link (use a browser): {target}");
            }
            other => self.status = format!("item type '{other}' not supported yet"),
        }
    }

    fn gopher_back(&mut self) {
        let Some(g) = &mut self.gopher else { return };
        match g.history.pop() {
            Some((doc, selected, scroll)) => {
                g.doc = doc;
                g.selected = selected;
                g.scroll = scroll;
            }
            None => self.status = String::from("History empty (Esc returns to terminal)."),
        }
    }

    /// Run a gopher type-7 search with the entered query (RFC 1436:
    /// selector and query separated by a tab).
    fn run_search(&mut self, query: &str) {
        if let Some(base) = self.search_target.take() {
            let url = GopherUrl {
                selector: format!("{}\t{}", base.selector, query),
                ..base
            };
            self.start_fetch(url);
        }
    }

    fn open(&mut self, host: String, port: u16, use_tls: bool) {
        self.reset_screen();
        let (handle, events) = telnet::connect(host.clone(), port, self.last_inner, use_tls);
        self.conn = Some(handle);
        self.events = Some(events);
        self.connected = false;
        self.tls = false;
        self.remote_opts.clear();
        self.local_opts.clear();
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
            telnet::Event::Closed(reason) => {
                self.connected = false;
                self.tls = false;
                self.remote_opts.clear();
                self.local_opts.clear();
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

/// Pass a BEL through to the real terminal.
fn ring_terminal_bell() {
    use std::io::Write;
    let mut out = std::io::stdout();
    let _ = out.write_all(b"\x07");
    let _ = out.flush();
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

    /// Build a gopher doc from a pattern: 'x' = link line, '.' = text.
    fn gopher_doc(pattern: &str) -> crate::gopher::GopherDoc {
        let url = crate::gopher::GopherUrl::parse("gopher://test.host").unwrap();
        let lines = pattern
            .chars()
            .enumerate()
            .map(|(i, c)| crate::gopher::DocLine {
                kind: if c == 'x' { '1' } else { 'i' },
                text: format!("line {i}"),
                link: (c == 'x').then(|| crate::gopher::GopherUrl {
                    host: String::from("test.host"),
                    port: 70,
                    item_type: '1',
                    selector: format!("/{i}"),
                }),
            })
            .collect();
        crate::gopher::GopherDoc {
            url,
            lines,
            raw: Vec::new(), // synthetic docs are never re-wrapped
            wrapped_to: 80,
            cp437: false,
        }
    }

    fn selected(app: &super::App) -> Option<usize> {
        app.gopher.as_ref().unwrap().selected
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
        app.gopher_arrow(1);
        assert_eq!(selected(&app), Some(1));
        // It scrolls off the top: highlight jumps to the next visible link.
        app.gopher_arrow(1);
        assert_eq!(selected(&app), Some(5));
        // Scrolling back up: 5 leaves through the bottom, 1 returns.
        app.gopher_arrow(-1);
        assert_eq!(selected(&app), Some(1));
    }

    #[test]
    fn gopherus_no_visible_link_means_no_highlight() {
        let mut app = super::App::new(None, 23);
        app.last_inner = (80, 3);
        app.navigate_to(gopher_doc("x......x"));

        assert_eq!(selected(&app), Some(0));
        // Scroll into the link-free middle: nothing highlighted.
        app.gopher_arrow(1);
        assert_eq!(selected(&app), None);
        app.gopher_scroll(3, true); // mouse wheel
        assert_eq!(selected(&app), None);
        // The next link enters the viewport and takes the highlight.
        app.gopher_arrow(1);
        assert_eq!(selected(&app), Some(7));
    }

    #[test]
    fn gopherus_walks_links_when_page_cannot_scroll() {
        let mut app = super::App::new(None, 23);
        app.last_inner = (80, 10); // taller than the document
        app.navigate_to(gopher_doc(".x.xx"));

        assert_eq!(selected(&app), Some(1));
        // The page is pinned, so Up/Down step between visible links.
        app.gopher_arrow(1);
        assert_eq!(selected(&app), Some(3));
        app.gopher_arrow(1); // line 4 is adjacent to 3: direct transition
        assert_eq!(selected(&app), Some(4));
        app.gopher_arrow(1);
        assert_eq!(selected(&app), Some(4), "stays on the last link");
        app.gopher_arrow(-1); // adjacent transition back
        assert_eq!(selected(&app), Some(3));
        app.gopher_arrow(-1);
        app.gopher_arrow(-1);
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
        app.gopher_arrow(1);
        let g = app.gopher.as_ref().unwrap();
        let lines_at_80 = g.doc.lines.len();
        assert_eq!(g.doc.lines[g.selected.unwrap()].text, "Link two");

        // Halving the width re-wraps the prose; the links move to new
        // line indices but the selection stays on "Link two".
        app.last_inner = (40, 10);
        app.sync_gopher_wrap();
        let g = app.gopher.as_ref().unwrap();
        assert!(g.doc.lines.len() > lines_at_80, "narrower wrap adds rows");
        assert!(g.doc.lines.iter().all(|l| l.text.chars().count() <= 40));
        assert_eq!(g.doc.lines[g.selected.unwrap()].text, "Link two");
    }

    #[test]
    fn gopherus_steps_to_nearest_link_never_skipping() {
        let mut app = super::App::new(None, 23);
        app.last_inner = (80, 9); // center row 4
        // Links at 1, 3, and 6: from the top, 6 sits nearest the center,
        // but Down must visit 3 first.
        app.navigate_to(gopher_doc(".x.x..x......."));

        assert_eq!(selected(&app), Some(1));
        app.gopher_arrow(1);
        assert_eq!(selected(&app), Some(3), "nearest link, not the center one");
        app.gopher_arrow(1);
        assert_eq!(selected(&app), Some(6));
        // No further links below: sticks near the center while scrolling.
        app.gopher_arrow(1);
        assert_eq!(selected(&app), Some(6));
        // Mirror going up: 3 is farther from the center than 6, so the
        // handoff back upward waits (sticky) rather than snapping.
        app.gopher_arrow(-1);
        assert_eq!(selected(&app), Some(6));
    }

    #[test]
    fn gopherus_adjacent_links_transition_and_hold_center() {
        let mut app = super::App::new(None, 23);
        app.last_inner = (80, 5); // center row 2
        app.navigate_to(gopher_doc("..xxx..."));

        let g = |app: &super::App| {
            let g = app.gopher.as_ref().unwrap();
            (g.selected, g.scroll)
        };
        // First link sits exactly on the center row.
        assert_eq!(g(&app), (Some(2), 0));
        // Adjacent links below: the highlight steps down and the page
        // scrolls along, holding the selection on the center row.
        app.gopher_arrow(1);
        assert_eq!(g(&app), (Some(3), 1));
        app.gopher_arrow(1);
        assert_eq!(g(&app), (Some(4), 2));
        // Next line is text: the selection sticks and the page scrolls.
        app.gopher_arrow(1);
        assert_eq!(g(&app), (Some(4), 3));
        // Page now pinned at the bottom, no link below: nothing changes.
        app.gopher_arrow(1);
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
        app.gopher_arrow(1);
        assert_eq!(selected(&app), Some(5));
        // Scrolling back up: link 1 is merely *as* close to the center,
        // not closer, so the selection stays put (sticky).
        app.gopher_arrow(-1);
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
}
