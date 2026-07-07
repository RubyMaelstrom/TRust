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

/// How long the pointer (mouse cursor or keyboard selection) must REST on a
/// new target before the hover dispatch commits (`PageCmd::Hover`). The
/// target-change diff is the load-bearing throttle; this dwell just keeps a
/// fast sweep / arrow-key repeat from dispatching every intermediate stop.
const HOVER_DWELL: Duration = Duration::from_millis(100);

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
/// scrolling viewport with a link cursor constrained to it, and in-RAM
/// back/forward history.
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
    /// The selected item in the PINNED fixed layer, as `(fixed, row, item)`
    /// (`doc.fixed[fixed].rows[row].items[item]`). Mutually exclusive with
    /// `sel_item` — a hover/click is on either the scrolling doc or a pinned
    /// rail. `selected_link` reads it; the renderer highlights it.
    pub sel_fixed: Option<(usize, usize, usize)>,
    /// First visible line/row.
    pub scroll: usize,
    history: Vec<HistEntry>,
    /// Pages backed away from, nearest first at the top. Going back parks
    /// the current doc here; a NEW navigation truncates it (the standard
    /// browser model — you can't go forward to a branch you left).
    forward: Vec<HistEntry>,
}

/// One navigation-trail entry. The trail itself (URL + restore position) is
/// unbounded — an entry is a few hundred bytes — but the heavy parsed `doc`
/// is RETAINED only while the entry is adjacent to the shown page (the top
/// of its stack): one instant step in either direction, everything deeper
/// refetches on travel (the bfcache model, her strict-memory call). The one
/// exemption is a POST result, which can't be honestly refetched (a re-POST
/// double-submits), so its doc stays; its images still drop (`doc_image_keep`)
/// and are refetched on restore.
struct HistEntry {
    /// Where the page came from — the refetch target once `doc` is evicted.
    url: Link,
    pos: ViewPos,
    scroll: usize,
    /// The doc is a direct POST result (no PRG redirect): never evicted,
    /// never refetched — restoring shows the retained doc, and revive-on-back
    /// is skipped (a GET of the action URL would be a different page).
    post: bool,
    /// The parsed page, while this entry is adjacent (or `post`).
    doc: Option<Doc>,
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

/// Where a find match lives — every text surface the browser draws.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FindLoc {
    /// A doc line (gopher/gemini/oneshot line model).
    Line(usize),
    /// A laid-out doc row's item (HTTP 2D layout model).
    Item { row: usize, item: usize },
    /// Inside a scroll region's buffer: `(region index, buffer row, buffer
    /// item)`. Revealing it scrolls the region (`voffset`), not just the doc.
    Region {
        region: usize,
        brow: usize,
        bitem: usize,
    },
    /// On a PINNED fixed layer (`doc.fixed[layer].rows[row].items[item]`) —
    /// always on screen, so revealing it never scrolls.
    Fixed {
        layer: usize,
        row: usize,
        item: usize,
    },
}

/// One find match: a char range within the text at `loc`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct FindMatch {
    pub loc: FindLoc,
    /// Char offsets of the match within the item/line text.
    pub start: usize,
    pub end: usize,
}

impl FindMatch {
    /// The document row this match orders/scrolls by: region matches sort
    /// with their region's band (buffer order within it); fixed-layer
    /// matches are pinned chrome — they sort after the document (usize::MAX)
    /// and never drive a scroll.
    fn order_row(&self, regions: &[crate::layout::Region]) -> usize {
        match self.loc {
            FindLoc::Line(l) => l,
            FindLoc::Item { row, .. } => row,
            FindLoc::Region { region, .. } => regions.get(region).map_or(0, |rg| rg.start_row),
            FindLoc::Fixed { .. } => usize::MAX,
        }
    }
}

/// A saved selection across history pops — whichever model the doc used.
#[derive(Clone, Copy, Default)]
struct ViewPos {
    selected: Option<usize>,
    sel_item: Option<(usize, usize)>,
    /// The page had a live JS engine when we navigated away. Going back
    /// (or forward) to it revives the page (re-runs JS) instead of
    /// restoring a frozen snapshot whose script links/forms are dead.
    was_live: bool,
}

/// What a background fetch produced, by protocol.
enum Payload {
    Gopher(Vec<u8>),
    Gemini(gemini::Response),
    Http(http::Response),
    OneShot(Vec<u8>),
    /// An internal `about:` page's gemtext source, generated locally (no
    /// network). Rides the fetch pipe so history deep-travel refetches of
    /// `about:` entries flow through the same completion path as the rest.
    About(String),
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
    /// Whether the raster has real transparency (mirrored into `image_alpha`
    /// for layout's overlap compositor — LAYOUT_OVERHAUL_PLAN.md P8). SVG and
    /// opaque rasters are `false`, so they never trigger a composite group.
    has_alpha: bool,
}

/// One layer's owned inputs for a composite encode (P8): its decoded bytes plus
/// the geometry the compositor recorded, moved onto the blocking encode thread.
struct CompositeInput {
    raw: std::sync::Arc<[u8]>,
    box_cells: (u16, u16),
    off_cells: (u16, u16),
    crop: bool,
    pixelated: bool,
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
    /// `image-rendering: pixelated` on the element: encode upscales with
    /// nearest-neighbor (hard-edged blocks — a scannable QR), not Lanczos.
    pub(crate) pixelated: bool,
    /// Recolor for an SVG box: its role (link accent vs. body text) silhouette.
    /// Ignored when the bytes decode to a raster. Part of the key so a recolor
    /// re-encodes (and the renderer keys identically).
    pub(crate) tint: Option<crate::img::SvgTint>,
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
            pixelated: item.pixelated,
            tint: Some(svg_tint()),
        }
    }
}

/// The silhouette recolor for every SVG: a single flat WHITE over the UI
/// background. We deliberately do NOT vary by role (link vs. body) — one neutral
/// color keeps inline icons consistent and dodges the context-color mess (her
/// call). The SVG's own colors are dropped too (same as not honoring HTML/CSS
/// color); only the shape's coverage is painted, in white.
pub(crate) fn svg_tint() -> crate::img::SvgTint {
    crate::img::SvgTint {
        fg: [0xff, 0xff, 0xff],
        bg: color_rgb(crate::ui::theme::BG),
    }
}

/// Whether two http(s) URLs address the SAME document — equal but for their
/// fragment. A `#anchor` link between two such URLs scrolls in place rather than
/// re-fetching.
fn same_document(a: &url::Url, b: &url::Url) -> bool {
    let mut a = a.clone();
    let mut b = b.clone();
    a.set_fragment(None);
    b.set_fragment(None);
    a == b
}

/// Percent-decode a URL fragment to raw UTF-8 (a non-ASCII `id` anchor arrives
/// `%XX`-encoded in the href). Lossy on invalid sequences; unknown `%` escapes
/// pass through literally.
fn pct_decode_utf8(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn color_rgb(c: ratatui::style::Color) -> [u8; 3] {
    match c {
        ratatui::style::Color::Rgb(r, g, b) => [r, g, b],
        _ => [0, 0, 0],
    }
}

/// One inline-image box finished encoding to a sliced terminal protocol.
struct EncMsg {
    key: EncKey,
    protocol: Option<SlicedProtocol>,
    /// `App::enc_epoch` at spawn time. `set image <proto>` bumps the epoch,
    /// so an encode still in flight under the OLD protocol is recognized and
    /// dropped on arrival instead of caching a stale-protocol image.
    epoch: u64,
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
    /// Diagnostic (`TRUST_DIAG_FRAME`): redraws this second + their summed draw
    /// time (µs), to size the main-thread render/encode cost.
    static FRAME_DIAG: std::cell::Cell<(u64, u64)> = const { std::cell::Cell::new((0, 0)) };
    static FRAME_DIAG_LAST: std::cell::Cell<std::time::Instant> = std::cell::Cell::new(std::time::Instant::now());
    /// (on_page_evt µs, #calls, #full-replaces, #event-drains) this second.
    static PAGE_WORK: std::cell::Cell<(u64, u64, u64, u64)> = const { std::cell::Cell::new((0, 0, 0, 0)) };
    /// (image-relayout µs, #relayouts) this second.
    static IMG_RELAYOUT: std::cell::Cell<(u64, u64)> = const { std::cell::Cell::new((0, 0)) };
    /// (Updated, Patched, Scrolled, Settled, other) page events this second —
    /// to see what wakes the redraw at rest (the sixel-cost trigger).
    static EVT_TALLY: std::cell::Cell<[u64; 5]> = const { std::cell::Cell::new([0; 5]) };
    /// Draws this second whose VISIBLE browser frame was byte-identical to the
    /// prior draw — wasted redraws (the sixel re-render cost with nothing on
    /// screen changing). The `TRUST_DIAG_FRAME` lever for the at-rest peg.
    static REDUNDANT_DRAWS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Diagnostic (`TRUST_DIAG_FRAME`): count of `SlicedImage` widget renders during
/// the current draw — the main-thread sixel cost is ~linear in this. `ui::draw`
/// increments it per image; the run loop reads+resets it after each draw.
pub(crate) static IMG_RENDERS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A live scroll region's retained state (INCREMENTAL_LAYOUT_PLAN.md §14): the
/// last patch fragment HTML (so the region can be re-laid in place — on an image
/// decode, or to refresh it from current content after a full re-render) plus the
/// per-child row cache that makes that re-lay O(changed messages). The region's
/// image-URL set (for decode routing) lives on the layout `Region` itself, not
/// here — it's populated on every full render so routing works before a region
/// ever patches (`Region::image_urls`).
#[derive(Default)]
struct RegionLive {
    html: Vec<u8>,
    cache: crate::layout::RegionRowCache,
}

/// `TRUST_DIAG_FRAME` present in the environment, read ONCE. The per-call
/// `env::var_os` (an allocation + platform lookup) sat on the run loop (twice
/// per iteration) and the page-event/decode/patch paths.
static DIAG_FRAME: std::sync::LazyLock<bool> =
    std::sync::LazyLock::new(|| std::env::var_os("TRUST_DIAG_FRAME").is_some());

/// Bump one `EVT_TALLY` slot (the per-second page-event breakdown printed by
/// `TRUST_DIAG_FRAME`).
fn tally_evt(slot: usize) {
    EVT_TALLY.with(|t| {
        let mut a = t.get();
        a[slot] += 1;
        t.set(a);
    });
}

/// Everything under the pointer at a screen cell, found by ONE walk of the
/// pinned fixed layer and the visible rows (each row's `visual_columns`
/// computed once). Mouse motion needs all three every event; scanning
/// separately for the selection and the hover target did the whole walk
/// twice per move.
#[derive(Default)]
struct PointerHit {
    /// Interactive item in the pinned fixed layer, `(fixed, row, item)`.
    fixed: Option<(usize, usize, usize)>,
    /// Interactive item in the scrolled doc, `(row, item)` — the selection.
    item: Option<(usize, usize)>,
    /// The live actor node that should hear the pointer (ANY item kind — a
    /// hover-only div is not interactive but is a hover target).
    hover: Option<usize>,
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
    /// The document row last pushed to the live page as its scroll position
    /// (`PageCmd::Scroll`). Diffed each run-loop tick so a scroll command is
    /// sent only when the viewport actually moved; `None` = nothing sent yet
    /// (reset on each new live page). Drives infinite scroll / scroll-based
    /// lazy loading: the site's own handler reacts to the threaded position.
    last_scroll_sent: Option<usize>,
    /// How many decoded-image sizes have been pushed to the live page
    /// (`PageCmd::ImageSizes`), diffed against `image_sizes.len()` each tick —
    /// sizes only accrue during a page's life, so the length IS the change
    /// signal. `None` = nothing sent yet (reset with the live page), which
    /// makes the first tick push the whole current map (revive-on-back starts
    /// with a warm cache). Keeps the actor's geometry pass laying images
    /// exactly like the app's render, so the boxes page JS measures match the
    /// page on screen (CSSOM View reports the actual layout).
    image_sizes_sent: Option<usize>,
    /// The browser viewport (cells) last pushed to the live page
    /// (`PageCmd::Viewport`), diffed each tick. The engine's fetch-time size
    /// comes from whatever view was on screen when the fetch started — at
    /// startup the session layout, a few rows TALLER than the browser view —
    /// so the engine must adopt the true content area once the page displays
    /// (and on resize; it fires `resize` at the Window per CSSOM View §4.1).
    /// `None` = nothing sent yet (reset with the live page).
    viewport_sent: Option<(u16, u16)>,
    /// The hover target last SENT to the live page (`PageCmd::Hover`): the
    /// actor node under the terminal's pointer, or `None` after a clear (or
    /// nothing ever sent — the two are equivalent: a clear is only worth
    /// sending after a `Some`). Reset with the live page.
    hover_sent: Option<usize>,
    /// A pointer-target change awaiting the dwell: `(actor node or None =
    /// clear, viewport CSS px of the hovered cell)`. The run loop arms a
    /// `HOVER_DWELL` one-shot when this changes (the resize-at-rest pattern)
    /// and commits it once the pointer rests — so a fast sweep across many
    /// targets, or arrow-key repeat down a list, dispatches only the target
    /// it settles on. Mouse motion and keyboard selection both feed this:
    /// the selection IS the terminal's pointer (her call).
    hover_want: Option<(Option<usize>, f64, f64)>,
    /// The scroll row the user INTENDS to be at on the HTTP laid-out doc, kept
    /// independent of the rendered `scroll`. A living page re-renders on its own
    /// (a timer, a fetch, an infinite-scroll load), and such a re-layout can
    /// momentarily SHRINK the document — an archive.org collection load blanks
    /// the grid to a "Searching…" placeholder, then refills it. Clamping the
    /// rendered scroll to the shrunken `max_scroll` (and never restoring it when
    /// the doc grows back) is what made scrolling "pop up" then settle higher
    /// than before. `scroll_intent` is the position to RESTORE toward on every
    /// live re-render (clamped only for display), so a transient shrink no longer
    /// destroys the reader's place. Synced to `scroll` after any user-driven
    /// scroll/resize/navigation (run-loop tail + `navigate_to`); a live
    /// re-render's "keep" path reads it but does NOT overwrite it.
    scroll_intent: usize,
    /// Per-region CLIP box last pushed to the live page (`PageCmd::RegionGeom`),
    /// keyed by the actor node id; value is the px `(clientHeight, clientWidth)`
    /// quantized to ints, so a geometry command is sent only when the box changed
    /// (rarely — it's viewport-tied). Cleared with the live page. (Phase 3.)
    region_geom_sent: std::collections::HashMap<usize, (i64, i64)>,
    /// Per-region `scrollTop` (rows) last written BACK to the live page on a
    /// wheel/page scroll (`PageCmd::SetScroll`), keyed by the actor node id — so
    /// the same offset isn't re-sent and the page's `scroll` handler fires only
    /// on a real move. Cleared with the live page. (Phase 3 inner scroll.)
    region_scroll_sent: std::collections::HashMap<usize, usize>,
    /// Per-region live state (INCREMENTAL_LAYOUT_PLAN.md §14 — the inner-scroll
    /// de-lag), keyed by the actor node id so it survives the per-message re-parse
    /// AND a full re-render: the last patch FRAGMENT HTML + the memoized child-row
    /// cache. The cache lets a region patch reuse the laid rows of every unchanged
    /// chat message and lay only the new one (O(one message) instead of O(all)).
    /// The retained HTML lets an IMAGE DECODE inside the region re-lay ONLY that
    /// region (an emote's box is contained by the region's formatting context),
    /// instead of re-laying the whole page — and is the CURRENT content, so it
    /// also keeps a region's chat messages from reverting to the stale `doc.raw`
    /// content on a full re-render. Cleared with the live page.
    region_live: std::collections::HashMap<usize, RegionLive>,
    /// Image URLs that finished decoding since the last run-loop coalesce point,
    /// to drive a SCOPED re-lay (region-only when the image is region-confined;
    /// see `apply_pending_image_decodes`) instead of a blanket full-page relayout.
    pending_decoded_urls: Vec<String>,
    /// The set of live clipped-region actor nodes last sent to the page actor
    /// (`PageCmd::LiveRegions`), so it patches a mutation ONLY when confined to a
    /// real region (INCREMENTAL_LAYOUT_PLAN.md §4b). Re-sent only when it changes.
    live_regions_sent: std::collections::HashSet<usize>,
    /// The set of cached inline IFC-boundary actor nodes last sent to the page
    /// actor (`PageCmd::LiveBoundaries`, the `Doc.boundaries` node set), so it
    /// proposes a general inline patch ONLY when the app has the box cached
    /// (INCREMENTAL_LAYOUT_PLAN.md §14). Re-sent only when it changes.
    live_boundaries_sent: std::collections::HashSet<usize>,
    /// A page-script dispatch is in flight (drives the loading heart).
    page_busy: bool,
    /// Static form submit to perform if the live page does not prevent
    /// the submitted form's default action.
    pending_live_submit: Option<(usize, usize)>,
    /// DISTINCT page-JS error messages seen on the live page, for the status
    /// badge. A dedup set, not a running count: one error that recurs every
    /// frame — e.g. a poll whose data is gated behind a bot wall, which the live
    /// engine re-attempts each tick — reads as `JS:1!`, not a climbing `JS:30!`
    /// that would overstate the number of distinct problems.
    page_js_errors: std::collections::HashSet<String>,
    /// The next fetched document replaces the current one instead of
    /// pushing history (`reload`).
    replace_nav: bool,
    /// A deep back/forward is refetching an evicted trail entry: the next
    /// fetched document completes the travel (pop the entry, park the
    /// current doc on the opposite stack) instead of pushing history.
    /// `Some(true)` = forward. Cleared by any other navigation intent.
    pending_travel: Option<bool>,
    /// The document on screen is a direct POST result (`Response.from_post`)
    /// — recorded so its trail entry is marked exempt when navigated away
    /// from. Restored from the entry on travel.
    current_from_post: bool,
    /// `from_post` of the response `navigate_to` is about to show; consumed
    /// there into `current_from_post` (the push needs the OLD value first).
    nav_from_post: bool,
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
    /// Last mouse cell `(col, row)` seen, in absolute terminal coordinates —
    /// so a keyboard PgUp/PgDn can target the scroll region under the cursor
    /// ("scroll the hovered region", CSS Overscroll target). `None` until the
    /// first mouse event.
    last_mouse: Option<(u16, u16)>,
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
    /// URL→cell-box mirror of `image_cache` (both are insert-only, synced in
    /// `on_img_load`) — the map every layout pass reads. Persistent so
    /// re-parses and patches borrow it instead of rebuilding it (cloning
    /// every URL string) per call: that clone ran on EVERY live re-render and
    /// even on the O(one message) region-patch path, where a session's
    /// accumulated cache could outweigh the patch itself.
    image_sizes: crate::layout::ImageSizes,
    /// URL→`has_alpha` mirror of `image_cache` (same insert-only sync), threaded
    /// into layout so the overlap compositor (LAYOUT_OVERHAUL_PLAN.md P8) groups
    /// only genuinely-transparent overlaps. An absent entry means "opaque / not
    /// yet decoded" — no grouping, the always-correct default.
    image_alpha: std::collections::HashMap<String, bool>,
    /// Persistent channel for finished page-image fetch+decodes. Batches are
    /// ADDITIVE: overlapping batches (the initial page load + a live patch's
    /// new emote) all deliver here. The old model — a per-batch receiver that
    /// each new batch REPLACED — silently discarded the remainder of an
    /// in-flight batch whenever a later one started.
    imgs_tx: mpsc::Sender<ImgLoadMsg>,
    imgs_rx: mpsc::Receiver<ImgLoadMsg>,
    /// Image URLs currently in the fetch+decode pipeline: dedups requests
    /// across overlapping batches, and non-empty drives the loading pulse.
    imgs_in_flight: HashSet<String>,
    /// Image URLs whose fetch/decode FAILED for the current page. Without
    /// this a live page that re-renders (a chat, a timer) refetched every
    /// broken image once per render — `start_image_loads` receives the full
    /// URL list each update. Cleared on navigation, so a reload retries.
    failed_images: HashSet<String>,
    /// Handles to in-flight image batch tasks, so Esc (`stop_loading`) and
    /// navigation ABORT the network work itself — not just the delivery.
    imgs_tasks: Vec<tokio::task::JoinHandle<()>>,
    /// Encoded inline-image protocols, keyed by `(url, cell_w, cell_h, crop)`.
    /// Each is a `SlicedProtocol` (encoded once for the whole box; the renderer
    /// clips it to any vertical slice), so scroll never re-encodes. Bounded to
    /// the on-screen set by `sync_image_encodes` (entries scrolled away evict).
    pub(crate) image_protocols: HashMap<EncKey, SlicedProtocol>,
    /// Boxes currently being encoded (one async encode per key in flight).
    /// Non-empty drives the loading pulse (the channel is persistent so it
    /// can't gate the pulse itself).
    image_encoding: HashSet<EncKey>,
    /// Boxes whose encode FAILED (a malformed image, or a sixel/box edge case).
    /// Without this, a failed encode is neither in `image_protocols` nor
    /// `image_encoding`, so `sync_image_encodes` re-requests it EVERY loop tick —
    /// cloning the decoded bytes and re-spawning forever, a CPU spin that pegs a
    /// core on an image-heavy page where one image won't encode. Evicted with the
    /// on-screen set, so scrolling a box back gets exactly one retry.
    failed_encodes: HashSet<EncKey>,
    /// Persistent channel for finished inline-image encodes.
    enc_tx: mpsc::Sender<EncMsg>,
    enc_rx: mpsc::Receiver<EncMsg>,
    /// Generation counter for inline-image encodes, bumped by
    /// `set image <proto>` — see `EncMsg::epoch`.
    enc_epoch: u64,
    /// The gopher type-7 item, gemini 1x URL, or form field awaiting
    /// input (pub so the UI can label the prompt accordingly).
    pub(crate) search_target: Option<Link>,
    /// The pending Search prompt is for a secret (gemini status 11
    /// "sensitive input", an HTML password field): the UI renders bullets
    /// in place of the typed text. Set by EVERY site that enters
    /// `Mode::Search` — it is only read while that mode is active.
    pub(crate) masked_input: bool,
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
        let (imgs_tx, imgs_rx) = mpsc::channel(64);
        Self {
            mode,
            // In memory only, like the entry histories.
            vt: new_vt(24, 80),
            encoding: Encoding::Utf8,
            js_enabled: true,
            web_storage: Default::default(),
            live_page: None,
            page_rx: None,
            last_scroll_sent: None,
            image_sizes_sent: None,
            viewport_sent: None,
            hover_sent: None,
            hover_want: None,
            scroll_intent: 0,
            region_geom_sent: std::collections::HashMap::new(),
            live_boundaries_sent: std::collections::HashSet::new(),
            region_scroll_sent: std::collections::HashMap::new(),
            region_live: std::collections::HashMap::new(),
            pending_decoded_urls: Vec::new(),
            live_regions_sent: std::collections::HashSet::new(),
            page_busy: false,
            pending_live_submit: None,
            page_js_errors: std::collections::HashSet::new(),
            replace_nav: false,
            pending_travel: None,
            current_from_post: false,
            nav_from_post: false,
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
            last_mouse: None,
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
            image_sizes: crate::layout::ImageSizes::new(),
            image_alpha: std::collections::HashMap::new(),
            imgs_tx,
            imgs_rx,
            imgs_in_flight: HashSet::new(),
            failed_images: HashSet::new(),
            imgs_tasks: Vec::new(),
            image_protocols: HashMap::new(),
            image_encoding: HashSet::new(),
            failed_encodes: HashSet::new(),
            enc_tx,
            enc_rx,
            enc_epoch: 0,
            search_target: None,
            masked_input: false,
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
        // Publish the real cell box so CSS px lengths convert to the same
        // physical extent a browser gives them on THIS terminal font, and so
        // the render pass converts an overflow-clip box's declared px height
        // back to the same rows the JS geometry (which measures rows × this
        // cell size) reported — see `layout::set_cell_px_w`/`set_cell_px_h`.
        crate::layout::set_cell_px_w(picker.font_size().width);
        crate::layout::set_cell_px_h(picker.font_size().height);
        self.picker = picker;
    }

    /// A fetch or image encode is in flight (drives the loading pulse).
    pub fn loading(&self) -> bool {
        self.fetch_rx.is_some()
            || self.img_rx.is_some()
            || !self.imgs_in_flight.is_empty()
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
        // The hover dwell one-shot + the target it was armed for (a further
        // change restarts the timer, so the dwell measures REST, not age).
        let mut hover_sleep: Option<std::pin::Pin<Box<tokio::time::Sleep>>> = None;
        let mut pending_hover_target: Option<Option<usize>> = None;
        // Redraw economy: the last drawn visible-frame signature + terminal size,
        // so a wake that paints an identical browser frame skips the draw (and its
        // sixel re-render). `TRUST_NO_FRAME_SKIP` forces the old always-draw path.
        let mut last_frame_sig: Option<u64> = None;
        let mut last_term_size: Option<ratatui::layout::Size> = None;
        let frame_skip_off = std::env::var_os("TRUST_NO_FRAME_SKIP").is_some();
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
            // Redraw economy: skip the draw entirely when it would paint a
            // byte-identical visible browser frame (same signature AND same
            // terminal size). On an immediate-mode TUI every draw re-renders all
            // on-screen images from scratch — cheap for halfblocks, but each sixel
            // image rebuilds its full byte string per draw, so a background timer
            // ticking an off-screen node every second otherwise pegs a core
            // re-emitting unchanged emotes. A `None` signature (image viewer,
            // menus, prompts, loading) always draws. Kill switch:
            // `TRUST_NO_FRAME_SKIP`.
            let term_size = terminal.size().ok();
            let sig = self.browser_frame_sig();
            let skip = !frame_skip_off
                && sig.is_some()
                && sig == last_frame_sig
                && term_size == last_term_size;
            if *DIAG_FRAME && skip {
                REDUNDANT_DRAWS.with(|c| c.set(c.get() + 1));
            }
            last_frame_sig = sig;
            last_term_size = term_size;
            let _t_draw = std::time::Instant::now();
            if !skip {
                terminal.draw(|frame| ui::draw(frame, &mut self))?;
            }
            if *DIAG_FRAME {
                use std::io::Write;
                FRAME_DIAG.with(|d| {
                    let (mut n, mut us) = d.get();
                    if !skip {
                        n += 1;
                        us += _t_draw.elapsed().as_micros() as u64;
                    }
                    let now = std::time::Instant::now();
                    if FRAME_DIAG_LAST.with(|l| now.duration_since(l.get()).as_secs_f64()) >= 1.0 {
                        let (pus, pn, pfull, pdr) = PAGE_WORK.with(|w| w.replace((0, 0, 0, 0)));
                        let (ius, icount) = IMG_RELAYOUT.with(|c| c.replace((0, 0)));
                        let (raw_kb, doc_rows, reg_rows) = self.browser.as_ref().map_or((0, 0, 0), |g| {
                            (
                                g.doc.raw.len() / 1024,
                                g.doc.rows.len(),
                                g.doc.regions.iter().map(|r| r.buffer.len()).sum::<usize>(),
                            )
                        });
                        let imgr = IMG_RENDERS.swap(0, std::sync::atomic::Ordering::Relaxed);
                        // Per-image sixel sequence cache (vendored ratatui-image):
                        // at rest with on-screen sixels, hits≈img_renders and
                        // builds≈0 — that's the per-image-redraw cost being killed.
                        let seq_builds = ratatui_image::sliced::SIXEL_SEQ_BUILDS
                            .swap(0, std::sync::atomic::Ordering::Relaxed);
                        let seq_hits = ratatui_image::sliced::SIXEL_SEQ_HITS
                            .swap(0, std::sync::atomic::Ordering::Relaxed);
                        let seq_prewarms = ratatui_image::sliced::SIXEL_SEQ_PREWARMS
                            .swap(0, std::sync::atomic::Ordering::Relaxed);
                        let ev = EVT_TALLY.with(|t| t.replace([0; 5]));
                        let redun = REDUNDANT_DRAWS.with(|c| c.replace(0));
                        let cur_scroll = self.browser.as_ref().map_or(0, |g| g.scroll);
                        let cur_max = doc_rows.saturating_sub(self.last_inner.1 as usize);
                        let lss = self.last_scroll_sent;
                        let sint = self.scroll_intent;
                        let _ = writeln!(
                            std::io::stderr(),
                            "DIAGFRAME draws/s={n} (redundant={redun}) draw={}ms | page_evt: {pn} calls {}ms (full_replaces={pfull} drains={pdr}) | img_relayout: {icount}x {}ms | raw={raw_kb}KB doc_rows={doc_rows} reg_rows={reg_rows} | SCROLL[pos={cur_scroll} max={cur_max} intent={sint} last_sent={lss:?}] | img_renders/s={imgr} sixel_seq[build={seq_builds} hit={seq_hits} prewarm={seq_prewarms}] evts[upd={} pat={} scr={} set={}]",
                            us / 1000,
                            pus / 1000,
                            ius / 1000,
                            ev[0], ev[1], ev[2], ev[3],
                        );
                        FRAME_DIAG_LAST.with(|l| l.set(now));
                        n = 0;
                        us = 0;
                    }
                    d.set((n, us));
                });
            }
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
            // The hover dwell (same one-shot pattern as the resize re-wrap): a
            // pending pointer-target change arms it, a FURTHER change restarts
            // it, and it fires only once the pointer rests — so a sweep across
            // many targets dispatches only the one it settles on.
            match self.hover_want.map(|(t, _, _)| t) {
                Some(target) if pending_hover_target != Some(target) => {
                    pending_hover_target = Some(target);
                    hover_sleep = Some(Box::pin(tokio::time::sleep(HOVER_DWELL)));
                }
                Some(_) => {}
                None => {
                    pending_hover_target = None;
                    hover_sleep = None;
                }
            }
            self.sync_viewer_size();
            self.sync_image_encodes();

            // Did THIS iteration come from the live page (a self-driven
            // re-render)? Such a render restores `scroll` toward `scroll_intent`
            // and must NOT then overwrite the intent with the (transiently
            // shrink-clamped) display scroll — that's what the tail-sync below
            // skips, so a placeholder frame can't destroy the reader's place.
            let mut from_page = false;
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
                Some(msg) = self.imgs_rx.recv() => self.on_img_load(msg),
                Some(msg) = self.enc_rx.recv() => self.on_enc(msg),
                evt = recv_opt(&mut self.page_rx) => match evt {
                    Some(evt) => {
                        from_page = true;
                        self.on_page_evt(evt);
                    }
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
                _ = async {
                    if let Some(sleep) = hover_sleep.as_mut() {
                        sleep.as_mut().await;
                    }
                }, if hover_sleep.is_some() => {
                    pending_hover_target = None;
                    hover_sleep = None;
                    self.commit_page_hover();
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
            // Coalesce inline-image decodes the same way: the `select!` took ONE
            // `imgs_rx` message; drain the rest that are already ready, then do a
            // SINGLE relayout. A page load fetches dozens of images that finish in
            // a burst, and a churning live page (Twitch's rotating previews) keeps
            // streaming them — relaying out the whole 396KB doc once PER image
            // pegged a core. Cache them all, relayout once.
            let mut drained_imgs = 0;
            while drained_imgs < 512 {
                match self.imgs_rx.try_recv() {
                    Ok(msg) => {
                        self.on_img_load(msg);
                        drained_imgs += 1;
                    }
                    // Empty (the channel never closes — the app holds a sender).
                    Err(_) => break,
                }
            }
            self.apply_pending_image_decodes();
            // Record the user's intended scroll, the value a self-driven live
            // re-render restores toward (so a transient document shrink can't pop
            // the view permanently — see `scroll_intent`). Sync after any user/
            // resize/fetch-driven iteration, but NOT after a pure live re-render
            // (which manages the intent itself); a re-render that also coincided
            // with drained user input (`drained > 0`) still syncs, so a scroll
            // during a render frame isn't lost.
            if (!from_page || drained > 0)
                && let Some(s) = self.browser.as_ref().map(|g| g.scroll)
            {
                self.scroll_intent = s;
            }
            // After the input burst is coalesced, push the (now settled) scroll
            // position to the live page so its infinite-scroll / lazy-load logic
            // reacts. A no-op when the scroll row didn't move (the common case).
            // The true viewport goes FIRST (the engine's scroll clamp and IO
            // root read innerHeight), then the settled scroll position.
            self.sync_page_viewport();
            self.sync_page_scroll();
            // And any freshly decoded image sizes, so the engine measures
            // images exactly like the app renders them (one geometry truth).
            self.sync_page_image_sizes();
            // Push each inner-scroll region's measured geometry back to the page
            // so its `scrollHeight`/`clientHeight` getters read TRUE values (the
            // conditional chat-pin reads them). Diffed — a no-op when unchanged.
            self.sync_region_state();
            // Tell the actor which scroll boxes are CURRENTLY clipped regions, so
            // it patches a mutation only when confined to one (and a non-region
            // scroll box takes the full path, never a failed patch). Diffed.
            self.sync_live_regions();
            // Tell the actor which inline IFC boundaries the app has cached, so
            // it proposes a general inline patch only when the box is splice-able
            // (INCREMENTAL_LAYOUT_PLAN.md §14). Diffed.
            self.sync_live_boundaries();
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
        // Remember the cursor cell so a later keyboard PgUp/PgDn can scroll the
        // region under it.
        self.last_mouse = Some((mouse.column, mouse.row));
        // A left-click on the chrome opens the command console — a
        // discoverable alternative to Tab/Ctrl-], in EVERY mode and view
        // (handled before the viewer/dropdown/browser grabs below, since the
        // chrome rows never overlap them). The top address bar prefills the
        // console with the current page's address (an editable address bar);
        // the bottom status line opens it as-is.
        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            let title_row = self.last_content_area.y.saturating_sub(1);
            if mouse.row == title_row {
                self.open_command_with_address();
                return;
            }
            if mouse.row == self.last_status_row {
                self.open_command();
                return;
            }
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
            (MouseEventKind::ScrollUp, true) => self.browser_wheel(-3, mouse.column, mouse.row),
            (MouseEventKind::ScrollDown, true) => self.browser_wheel(3, mouse.column, mouse.row),
            (MouseEventKind::Down(MouseButton::Mouse4), true) if self.mode == Mode::Session => {
                self.browser_back();
            }
            (MouseEventKind::Down(MouseButton::Mouse5), true) if self.mode == Mode::Session => {
                self.browser_forward();
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

    /// Open the command console (the same state Tab/Ctrl-] enters).
    fn open_command(&mut self) {
        self.mode = Mode::Command;
        self.cert_for = None;
        self.select_menu = None;
    }

    /// Open the command console prefilled with the current page's address,
    /// cursor at the end — an editable address bar. With nothing addressable
    /// on screen it behaves exactly like `open_command`.
    fn open_command_with_address(&mut self) {
        self.open_command();
        if let Some(addr) = self.current_address() {
            self.cursor = addr.chars().count();
            self.input = addr;
            self.select_anchor = None;
        }
    }

    /// The address shown in the top title bar, if any: the image viewer's
    /// URL, else the browser document's URL, else the connected telnet
    /// `host:port`. Mirrors the title logic in `ui::draw` so a top-bar click
    /// prefills the console with exactly what the bar displays.
    fn current_address(&self) -> Option<String> {
        match (&self.viewer, &self.browser) {
            (Some(v), _) => Some(v.url.to_string()),
            (None, Some(g)) => Some(g.doc.url.to_string()),
            (None, None) => self
                .host
                .as_ref()
                .map(|host| format!("{host}:{port}", port = self.port)),
        }
    }

    /// Ctrl-] / Tab: toggle between the command console and the session.
    /// From find it dismisses the find box (clearing its query) on the way
    /// to the console; any pending identity prompt or dropdown is cancelled.
    fn toggle_command_mode(&mut self) {
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
    }

    /// Mouse hover/click target in the browser, dispatching by layout model:
    /// HTTP laid-out docs use the 2D item hit-test (which also feeds the live
    /// hover pipeline — one scan serves both), gopher/gemini the line-based
    /// one. Returns whether an interactive target was hit.
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
            TermEvent::Paste(text) => {
                self.on_paste(text).await;
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
            self.toggle_command_mode();
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
            self.toggle_command_mode();
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
            // Collected ONCE per rescan; the per-item collection was an
            // allocation for every item in the doc on every keystroke.
            let q: Vec<char> = query.chars().collect();
            if g.doc.laid_out() {
                for (r, row) in g.doc.rows.iter().enumerate() {
                    for (i, item) in row.items.iter().enumerate() {
                        push_text_matches(
                            &item.text,
                            &q,
                            FindLoc::Item { row: r, item: i },
                            &mut matches,
                        );
                    }
                    // A scroll region's WHOLE buffer is searched where its
                    // band starts (document order: content above < region
                    // content < content below), not just the visible window.
                    for (ri, rg) in g.doc.regions.iter().enumerate() {
                        if rg.start_row != r {
                            continue;
                        }
                        for (br, brow) in rg.buffer.iter().enumerate() {
                            for (bi, item) in brow.items.iter().enumerate() {
                                push_text_matches(
                                    &item.text,
                                    &q,
                                    FindLoc::Region {
                                        region: ri,
                                        brow: br,
                                        bitem: bi,
                                    },
                                    &mut matches,
                                );
                            }
                        }
                    }
                }
                // Pinned rails last: they're chrome, always on screen.
                for (fi, f) in g.doc.fixed.iter().enumerate() {
                    for (r, row) in f.rows.iter().enumerate() {
                        for (i, item) in row.items.iter().enumerate() {
                            push_text_matches(
                                &item.text,
                                &q,
                                FindLoc::Fixed {
                                    layer: fi,
                                    row: r,
                                    item: i,
                                },
                                &mut matches,
                            );
                        }
                    }
                }
            } else {
                for (l, line) in g.doc.lines.iter().enumerate() {
                    push_text_matches(&line.text, &q, FindLoc::Line(l), &mut matches);
                }
            }
        }
        // Jump to the first match at or after the current scroll, else the top.
        let scroll = self.browser.as_ref().map_or(0, |g| g.scroll);
        let regions = self
            .browser
            .as_ref()
            .map(|g| g.doc.regions.as_slice())
            .unwrap_or(&[]);
        let current = (!matches.is_empty()).then(|| {
            matches
                .iter()
                .position(|m| m.order_row(regions) >= scroll)
                .unwrap_or(0)
        });
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

    /// Centre the viewport on the active match. A match inside a scroll
    /// region also scrolls the REGION (`voffset`) so the match's buffer row
    /// is in its window; a fixed-layer match is pinned on screen already.
    fn scroll_to_current_match(&mut self) {
        let loc = {
            let Some(f) = self.find.as_ref() else { return };
            let Some(ci) = f.current else { return };
            match f.matches.get(ci) {
                Some(m) => m.loc,
                None => return,
            }
        };
        let height = self.last_inner.1 as usize;
        let Some(g) = self.browser.as_mut() else {
            return;
        };
        let mut writeback = None;
        let line = match loc {
            FindLoc::Line(l) => l,
            FindLoc::Item { row, .. } => row,
            FindLoc::Region { region, brow, .. } => {
                let Some(rg) = g.doc.regions.get_mut(region) else {
                    return;
                };
                // Scroll the region so the match's buffer row sits mid-window
                // (clamped), then aim the doc at the band row it lands on.
                let rh = (rg.height as usize).max(1);
                rg.voffset = brow
                    .saturating_sub(rh / 2)
                    .min(rg.buffer.len().saturating_sub(rh));
                // The user (via find) owns this position now, like a wheel
                // scroll — a page-dictated stale signal must not yank it back,
                // and the live element hears the new scrollTop.
                rg.voffset_from_page = false;
                writeback = Some((rg.live_node, rg.voffset));
                rg.start_row + (brow - rg.voffset)
            }
            FindLoc::Fixed { .. } => return, // pinned: always visible
        };
        let max_scroll = g.doc.extent().saturating_sub(height.max(1));
        g.scroll = line.saturating_sub(height / 2).min(max_scroll);
        if let Some((node, voff)) = writeback {
            self.region_writeback(node, voff);
        }
    }

    /// Scroll the browser view so the element with `id`/`<a name>` == `frag` is
    /// at the top of the viewport — HTML's "scroll to the fragment". An empty
    /// (or `top`) fragment scrolls to the document top. Resolves against the
    /// laid-out doc's `anchor_rows`; a fragment that names no element is a no-op
    /// (the browser stays put). Returns whether the anchor was found.
    fn scroll_to_fragment(&mut self, frag: &str) -> bool {
        let height = self.last_inner.1 as usize;
        let Some(g) = self.browser.as_mut() else {
            return false;
        };
        // Only the 2D layout has a row model + anchors; the line-model docs
        // (gopher/gemini) don't do fragment scrolling.
        if !g.doc.laid_out() {
            return false;
        }
        if frag.is_empty() || frag.eq_ignore_ascii_case("top") {
            g.scroll = 0;
            return true;
        }
        // Ids carry raw text; a URL fragment may arrive percent-encoded (a
        // non-ASCII id — a Japanese section anchor). Try the literal form first,
        // then a decoded one.
        let row = g
            .doc
            .anchor_rows
            .get(frag)
            .or_else(|| {
                frag.contains('%')
                    .then(|| pct_decode_utf8(frag))
                    .and_then(|d| g.doc.anchor_rows.get(&d))
            })
            .copied();
        let Some(row) = row else {
            return false;
        };
        let max_scroll = g.doc.extent().saturating_sub(height.max(1));
        g.scroll = row.min(max_scroll);
        true
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

    /// Insert a run of text at the cursor as ONE edit (the paste path):
    /// replaces the selection; control characters are dropped (tabs land
    /// as spaces) so they can never act as keystrokes.
    fn insert_text(&mut self, text: &str) {
        let clean: String = text
            .chars()
            .map(|c| if c == '\t' { ' ' } else { c })
            .filter(|c| !c.is_control())
            .collect();
        if clean.is_empty() {
            return;
        }
        self.delete_selection();
        self.input.insert_str(self.byte_cursor(), &clean);
        self.cursor += clean.chars().count();
        self.active_history().detach();
    }

    /// One bracketed paste, applied atomically. Without this, a paste
    /// replays as keystrokes — a Tab in it toggled the console, an Esc
    /// closed the page. Text lands where typed text would; control
    /// characters never act.
    async fn on_paste(&mut self, text: String) {
        match self.mode {
            // Browsing / image viewer: keys are navigation — there is no
            // text target, so swallow the paste rather than scatter it.
            Mode::Session if self.browser.is_some() || self.viewer.is_some() => {
                self.status = String::from("Paste ignored — Tab opens the console for a URL.");
                self.notice = true;
            }
            // Character-at-a-time: the remote owns editing. Newlines go as
            // CR (the terminal paste rule); when the remote app enabled
            // bracketed paste itself (mode 2004, tracked by the emulator),
            // it gets the markers a real terminal would send.
            Mode::Session if self.char_mode() => {
                let mut bytes = text.replace("\r\n", "\r").replace('\n', "\r").into_bytes();
                if self.vt.screen().bracketed_paste() {
                    let mut wrapped = b"\x1b[200~".to_vec();
                    wrapped.append(&mut bytes);
                    wrapped.extend_from_slice(b"\x1b[201~");
                    bytes = wrapped;
                }
                self.send_bytes(bytes).await;
            }
            // Session line editor: embedded newlines send the line, so a
            // multi-line paste types like it does into a real terminal.
            Mode::Session => {
                let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
                for (i, part) in normalized.split('\n').enumerate() {
                    if i > 0 {
                        let line = std::mem::take(&mut self.input);
                        self.cursor = 0;
                        self.select_anchor = None;
                        self.active_history().push(&line);
                        self.send_line(&line).await;
                    }
                    self.insert_text(part);
                }
            }
            // Command console / prompts / find: insert only — a pasted
            // newline must never RUN anything.
            Mode::Command | Mode::Search | Mode::Find => {
                let joined = text.replace("\r\n", " ").replace(['\n', '\r'], " ");
                self.insert_text(joined.trim_end());
                if self.mode == Mode::Find {
                    self.recompute_find();
                }
            }
        }
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
                (Some("layout2"), Some("on")) => {
                    crate::layout2::set_enabled(true);
                    self.relayout_browser();
                    self.status = String::from(
                        "layout2 on: the NEW engine (P0 block/inline flow; flex/grid/positioned still stack).",
                    );
                }
                (Some("layout2"), Some("off")) => {
                    crate::layout2::set_enabled(false);
                    self.relayout_browser();
                    self.status = String::from("layout2 off (the default): the current engine.");
                }
                _ => {
                    self.status = String::from(
                        "usage: set encoding cp437|utf8 · set image <protocol>|auto · set js on|off · set cookies on|off · set borders on|off · set layout2 on|off",
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
            // GNU parity: `status` prints into the session feed — but that
            // feed is hidden while a browser doc is up, so route to the
            // about:status page there (visible, scrollable).
            Some("status" | "st") => {
                if self.browser.is_some() {
                    self.open_about("status");
                } else {
                    self.show_status();
                }
            }
            Some("help" | "?") => self.open_about("help"),
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
                self.status =
                    format!("unknown command: {other} (help lists commands — or just type a URL)")
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
        // Inline page images were encoded under the OLD protocol; drop them
        // (and failed attempts) so `sync_image_encodes` re-encodes the
        // on-screen set under the new one. The epoch bump makes any encode
        // still in flight land stale and get dropped (`on_enc`).
        self.enc_epoch += 1;
        self.image_protocols.clear();
        self.failed_encodes.clear();
        self.image_encoding.clear();
    }

    /// The `status` report body, one plain-text fact per line — shared by
    /// the vt session feed print and the `about:status` page.
    fn status_report(&self) -> String {
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
        format!(
            "{connection}\n\
             Escape character: Ctrl-]\n\
             Input mode: {mode}\n\
             Enter sends: {eol}\n\
             Encoding: {enc}\n\
             JavaScript: {js}\n\
             Cookies: {cookies}\n\
             Remote options (WILL): {remote}\n\
             Local options (DO): {local}",
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
        )
    }

    /// GNU telnet's `status` command: print connection state into the
    /// session feed, the way GNU telnet prints to the terminal. While a
    /// browser doc is on screen the feed is hidden, so `execute_command`
    /// routes to the `about:status` page instead.
    fn show_status(&mut self) {
        let report = format!(
            "\r\n\x1b[36m--- TRUST STATUS ---\x1b[0m\r\n{}\r\n\x1b[36m--------------------\x1b[0m\r\n",
            self.status_report().replace('\n', "\r\n"),
        );
        self.vt.process(report.as_bytes());
    }

    /// Gemtext source of an internal `about:` page, or None for a name we
    /// don't serve. Generated fresh each time — `about:status` reflects the
    /// state at the moment it's (re)fetched.
    fn about_body(&self, page: &str) -> Option<String> {
        match page {
            "help" => Some(String::from(HELP_PAGE)),
            "status" => Some(format!("# TRust status\n\n{}\n", self.status_report())),
            _ => None,
        }
    }

    /// Build an internal page from its gemtext source. The Doc's URL is
    /// `Link::External("about:…")`: never fetched from the network —
    /// `start_fetch_opts` regenerates the body locally (history deep-travel),
    /// and `sync_browser_wrap` re-wraps from the raw gemtext on resize.
    fn about_doc(url: String, body: String, width: usize) -> Doc {
        let width = width.max(10);
        let lines =
            gemini::parse_gemtext(body.as_bytes(), width, &|t| Link::External(t.to_string()));
        Doc::from_lines(
            Link::External(url),
            lines,
            body.into_bytes(),
            width,
            false,
            None,
        )
    }

    /// Open an internal `about:` page as a browser document (scrollable,
    /// findable, in history) — how `help` always shows, and how `status`
    /// shows when the session feed is hidden behind a browser doc.
    fn open_about(&mut self, page: &str) {
        let Some(body) = self.about_body(page) else {
            self.status = format!("no such page: about:{page}");
            return;
        };
        let width = (self.last_inner.0 as usize).max(10);
        let doc = Self::about_doc(format!("about:{page}"), body, width);
        self.status = format!("about:{page}");
        self.navigate_to(doc);
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
        // A new fetch intent supersedes a pending deep-travel completion
        // (the deep-travel path itself re-sets the flag after this call).
        self.pending_travel = None;
        // Internal pages regenerate locally; the body is built HERE (it
        // needs App state) and echoed through the task so the completion
        // path — including the deep-travel trail shuffle — stays one road.
        let about = match &target {
            Link::External(s) => s
                .strip_prefix("about:")
                .and_then(|page| self.about_body(page)),
            _ => None,
        };
        let (tx, rx) = mpsc::channel(1);
        self.fetch_rx = Some(rx);
        self.status = format!("Fetching {target} ...");
        let f = self.picker.font_size();
        let viewport = self.last_inner;
        let cell_px = (f.width, f.height);
        let storage = self.web_storage.clone();
        let js_on = self.js_enabled;
        let task = tokio::spawn(async move {
            let result = if let Some(body) = about {
                Ok(Payload::About(body))
            } else {
                match &target {
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
                    Link::Media(_) => Err(String::from("media plays in mpv, not fetched")),
                }
            };
            let _ = tx.send(FetchMsg { target, result }).await;
        });
        self.fetch_task = Some(task);
    }

    /// POST a form-encoded body to a web URL (her use case; the UX can
    /// grow content-type options once the target application is known).
    fn start_post(&mut self, url: url::Url, body: String, referrer: Option<url::Url>) {
        self.pending_travel = None;
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
        // A directly-opened SVG is recolored neutrally (body-text silhouette);
        // a raster ignores the tint.
        let tint = svg_tint();
        tokio::task::spawn_blocking(move || {
            let panel = ratatui::layout::Size::new(size.0, size.1);
            // The full-screen viewer fits (contains) the panel. SVG is
            // rasterized directly at this box, so resize keeps vector quality.
            let result = img::encode_bytes(&picker, &raw, panel, false, Some(tint)).map(
                |(protocol, image)| {
                    (
                        protocol,
                        format!("{}×{} {}", image.width, image.height, image.mime),
                    )
                },
            );
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

    /// Start the parallel fetch+decode of every page image not already
    /// cached, in flight, or known-failed. Fetches overlap (pooled,
    /// `buffer_unordered`) and each decode runs on a blocking task — no
    /// serial wall, results stream back over the persistent `imgs_rx` and
    /// re-layout as they land. Batches are additive (see `imgs_tx`); the
    /// task handle is kept so Esc/navigation can abort the network work.
    fn start_image_loads(&mut self, page: Url, urls: Vec<String>) {
        let todo: Vec<String> = urls
            .into_iter()
            .filter(|u| {
                !self.image_cache.contains_key(u)
                    && !self.imgs_in_flight.contains(u)
                    && !self.failed_images.contains(u)
            })
            .collect();
        if todo.is_empty() {
            return;
        }
        let font = self.picker.font_size();
        // The current doc's blob byte mirror, for `blob:` srcs (the batches
        // all load for the displayed browser doc).
        let blobs = self
            .browser
            .as_ref()
            .and_then(|g| g.doc.blobs.as_ref().map(|b| b.0.clone()));
        for u in &todo {
            self.imgs_in_flight.insert(u.clone());
        }
        // Keep the handle list bounded: completed batches prune here.
        self.imgs_tasks.retain(|t| !t.is_finished());
        let tx = self.imgs_tx.clone();
        let task = tokio::spawn(async move {
            futures::stream::iter(todo.into_iter().map(|url| {
                let tx = tx.clone();
                let page = page.clone();
                let blobs = blobs.clone();
                async move {
                    let decoded = load_one_image(&page, &url, font, blobs.as_ref()).await;
                    let _ = tx.send(ImgLoadMsg { url, decoded }).await;
                }
            }))
            .buffer_unordered(IMG_FETCH_CONCURRENCY)
            .for_each(|_| async {})
            .await;
        });
        self.imgs_tasks.push(task);
    }

    /// Abort every in-flight image batch — the network work itself, not just
    /// the result delivery — and forget what was in flight. A result already
    /// queued on the channel still arrives; `apply_pending_image_decodes`
    /// ignores it unless the current doc references its URL.
    fn abort_image_loads(&mut self) {
        for task in self.imgs_tasks.drain(..) {
            task.abort();
        }
        self.imgs_in_flight.clear();
    }

    /// One image finished decoding: cache it and record its URL so the run loop's
    /// coalesce point re-flows the box. A burst of decodes (a page load fetches
    /// dozens; a churning live page streams chat emotes) collapses into ONE scoped
    /// re-lay, not one per image.
    fn on_img_load(&mut self, msg: ImgLoadMsg) {
        self.imgs_in_flight.remove(&msg.url);
        let Some(decoded) = msg.decoded else {
            // The alt text stands — and the failure is REMEMBERED, so a live
            // page's every-render batch can't refetch a broken image forever.
            self.failed_images.insert(msg.url);
            return;
        };
        self.pending_decoded_urls.push(msg.url.clone());
        self.image_sizes.insert(msg.url.clone(), decoded.cell);
        self.image_alpha.insert(msg.url.clone(), decoded.has_alpha);
        self.image_cache.insert(msg.url, decoded);
    }

    /// Re-flow for the images decoded since the last run-loop turn — SCOPED to
    /// where they sit (the coalesce point for `on_img_load`). An image whose box
    /// lives only inside a scroll region (a chat emote/avatar/badge) re-lays JUST
    /// that region: its formatting context is independent (CSS Containment / a
    /// BFC), so its intrinsic-size reflow can't change anything outside it, and
    /// the region re-lay is O(the messages whose image decoded) via the row cache
    /// — NOT the whole page. Only an image in the main document flow triggers the
    /// (O(document)) full relayout, which then refreshes every region from its
    /// current retained HTML (so chat content + emote boxes stay right). This is
    /// what stopped Twitch's 135 streaming chat images from pegging a core with a
    /// full-page relayout per decode.
    fn apply_pending_image_decodes(&mut self) {
        let urls = std::mem::take(&mut self.pending_decoded_urls);
        if urls.is_empty() {
            return;
        }
        let t = std::time::Instant::now();
        // Route each decoded URL by WHERE its `<img>` lives, off the LAYOUT
        // region's image-URL set (`Region::image_urls`, populated on every full
        // render by walking the region's subtree — so routing works before the
        // region ever patches, and for EVERY region, not only the chat). A URL
        // inside a LIVE region (one we can re-lay in place) re-lays just that
        // region; a URL in no live region is in the main document flow (or a
        // static region we can't re-lay) and takes the full relayout, as before.
        let Some(g) = self.browser.as_ref() else {
            return;
        };
        // Only decodes the CURRENT doc references can affect its layout: a
        // result that raced a navigation (queued before its batch was
        // aborted) must not trigger a full relayout of the unrelated new
        // page. The cache entry stays — it's just a cache.
        let urls: Vec<String> = urls
            .into_iter()
            .filter(|u| g.doc.image_urls.contains(u))
            .collect();
        if urls.is_empty() {
            return;
        }
        let confined = |u: &String| {
            g.doc
                .regions
                .iter()
                .any(|r| r.live_node.is_some() && r.image_urls.contains(u))
        };
        let regions_hit: Vec<usize> = g
            .doc
            .regions
            .iter()
            .filter(|r| r.live_node.is_some() && urls.iter().any(|u| r.image_urls.contains(u)))
            .filter_map(|r| r.live_node)
            .collect();
        let main_flow = urls.iter().any(|u| !confined(u));
        if *DIAG_FRAME {
            let sample = urls
                .iter()
                .find(|u| !confined(u))
                .cloned()
                .unwrap_or_default();
            eprintln!(
                "DIAGDECODE urls={} main_flow={main_flow} regions_hit={} live_regions={} region_url_sets={} sample={}",
                urls.len(),
                regions_hit.len(),
                g.doc
                    .regions
                    .iter()
                    .filter(|r| r.live_node.is_some())
                    .count(),
                g.doc
                    .regions
                    .iter()
                    .map(|r| r.image_urls.len())
                    .sum::<usize>(),
                sample.chars().take(60).collect::<String>(),
            );
        }
        if main_flow {
            // A main-flow image changes document flow → full relayout
            // (`relayout_browser` then refreshes every region from its
            // CURRENT retained HTML, since the relayout rebuilt them from
            // possibly-stale doc.raw).
            self.relayout_browser();
        } else {
            for ln in regions_hit {
                self.relay_region(ln);
            }
        }
        if *DIAG_FRAME {
            IMG_RELAYOUT.with(|c| {
                let (us, n) = c.get();
                c.set((us + t.elapsed().as_micros() as u64, n + 1));
            });
        }
    }

    /// Re-lay a live region in place from its RETAINED patch HTML (the current
    /// content) + row cache, keeping its scroll position — used to refresh a
    /// region after an image inside it decoded, or after a full re-render rebuilt
    /// it from stale `doc.raw`. A no-op if the region has no retained HTML yet.
    fn relay_region(&mut self, node: usize) -> bool {
        let Some(html) = self
            .region_live
            .get(&node)
            .map(|r| r.html.clone())
            .filter(|h| !h.is_empty())
        else {
            return false;
        };
        self.relay_region_from_html(node, &html, true)
    }

    /// Re-lay-out the current HTTP doc with the decoded-image sizes,
    /// preserving the selected item and scroll (same as a resize re-flow).
    fn relayout_browser(&mut self) {
        let width = (self.last_inner.0 as usize).max(10);
        let height = self.last_inner.1.max(1) as usize;
        let Some(g) = &mut self.browser else { return };
        let Link::Http(url) = g.doc.url.clone() else {
            return;
        };
        if g.doc.raw.is_empty() {
            return;
        }
        let item_target = g.sel_item.and_then(|(r, i)| {
            crate::layout::effective_row(&g.doc.rows, &g.doc.regions, r)
                .items
                .get(i)
                .cloned()
        });
        // CSS Scroll Anchoring: a decoded image ABOVE the viewport changes
        // heights up there, which would shift the content the reader sees.
        // Capture the topmost visible followable item (like the live-render
        // path) and re-pin it to its screen offset after the re-flow.
        let view_anchor: Option<(usize, crate::doc::Link)> = {
            let bot = (g.scroll + height).min(g.doc.rows.len());
            (g.scroll..bot).find_map(|r| {
                crate::layout::effective_row(&g.doc.rows, &g.doc.regions, r)
                    .items
                    .iter()
                    .find_map(|it| it.link.clone().map(|l| (r, l)))
            })
        };
        let mut old_offsets = Vec::new();
        Self::collect_region_offsets(&g.doc.regions, &mut old_offsets);
        let meta = g.doc.meta.clone().unwrap_or_default();
        let forms = std::mem::take(&mut g.doc.forms);
        let raw = std::mem::take(&mut g.doc.raw);
        let blobs = g.doc.blobs.take();
        g.doc = http::parse_seeded(
            &url,
            &meta,
            &raw,
            width,
            height,
            Some(&forms),
            &self.image_sizes,
            &self.image_alpha,
        );
        // Same page, same blob mirror (a re-parse must not orphan blob: images).
        g.doc.blobs = blobs;
        // A re-wrap of the SAME HTML (image relayout): the baked scroll signal is
        // stale, so the on-screen offset wins (prefer_signal = false).
        Self::carry_region_offsets(&old_offsets, &mut g.doc.regions, false);
        g.sel_item = item_target.and_then(|t| Self::find_item_like(&g.doc, &t));
        let max_scroll = g.doc.rows.len().saturating_sub(height);
        if let Some((old_r, link)) = &view_anchor
            && let Some(new_r) = Self::find_row_by_link(&g.doc, link, *old_r)
        {
            let offset = old_r.saturating_sub(g.scroll);
            // The adjustment IS the new scroll position (CSS Scroll Anchoring):
            // move the intent with the content, as the live paths do.
            let shift = new_r as isize - *old_r as isize;
            self.scroll_intent = (self.scroll_intent as isize + shift).max(0) as usize;
            g.scroll = new_r.saturating_sub(offset).min(max_scroll);
        } else {
            g.scroll = g.scroll.min(max_scroll);
        }
        // The re-parse rebuilt every region from `doc.raw` — the last FULL
        // render, stale by exactly the content patched into live regions
        // since (a chat's recent messages). Restore each from its retained
        // patch HTML.
        self.refresh_live_regions();
    }

    /// Re-lay every live scroll region from its RETAINED patch HTML after a
    /// full re-layout rebuilt them from `doc.raw` (the last FULL render —
    /// stale by exactly the content patched in since). A no-op for regions
    /// without retained HTML, and for static pages (`region_live` empty).
    fn refresh_live_regions(&mut self) {
        let live: Vec<usize> = self.region_live.keys().copied().collect();
        for ln in live {
            self.relay_region(ln);
        }
    }

    /// A hash of everything `ui::draw` paints for the VISIBLE browser frame —
    /// scroll, selection, status/notice/input, the find overlay, each on-screen
    /// row's content (col + height + text + image, AND whether that image's
    /// encoded protocol is ready so an alt-text→image reveal redraws), every
    /// region's visible window, and carousel offsets. Two equal signatures ⇒ a
    /// redraw would paint a byte-identical frame, so it (and its per-image sixel
    /// rebuild) is wasted — the at-rest peg when a background timer ticks an
    /// off-screen node every second.
    ///
    /// `None` (⇒ always draw, never skip) unless we're in the plain browser
    /// display state: a browser doc showing, no image viewer, Session mode (not
    /// the Search/Find input line), no `<select>` dropdown, and not loading (the
    /// loading heart animates). The caller additionally guards terminal SIZE, so
    /// this need not hash it. Computed without touching the terminal — far cheaper
    /// than the draw it elides.
    fn browser_frame_sig(&self) -> Option<u64> {
        use std::hash::{Hash, Hasher};
        if self.viewer.is_some()
            || self.mode != Mode::Session
            || self.select_menu.is_some()
            || self.loading()
        {
            return None;
        }
        let g = self.browser.as_ref()?;
        let vh = self.last_inner.1 as usize;
        let mut h = std::collections::hash_map::DefaultHasher::new();
        g.scroll.hash(&mut h);
        g.sel_item.hash(&mut h);
        // The gopher/gemini LINE-model selection: its highlight and the
        // status-bar link preview both render from it, and a selection step
        // often moves WITHOUT scrolling (a menu that fits on screen, any step
        // above the center row) — leaving it out froze the highlight while
        // Enter followed the invisibly-moved selection.
        g.selected.hash(&mut h);
        // The PINNED fixed layer draws every frame independent of scroll, so its
        // selection (hover highlight + status link preview) and content must be
        // in the signature — else hovering a pinned rail link changes `sel_fixed`
        // but the draw is skipped, so no highlight/preview shows until a click
        // changes the page and forces a redraw.
        g.sel_fixed.hash(&mut h);
        for f in &g.doc.fixed {
            (f.col, f.row).hash(&mut h);
            for row in &f.rows {
                for it in &row.items {
                    (it.col, &it.text).hash(&mut h);
                }
            }
        }
        self.status.hash(&mut h);
        self.notice.hash(&mut h);
        self.input.hash(&mut h);
        let end = (g.scroll + vh).min(g.doc.rows.len());
        for r in g.scroll..end {
            let row = crate::layout::effective_row(&g.doc.rows, &g.doc.regions, r);
            for it in &row.items {
                it.col.hash(&mut h);
                it.height.hash(&mut h);
                it.text.hash(&mut h);
                it.image.hash(&mut h);
                // A decoded image reveals when its protocol lands — the row text
                // is unchanged across that transition, so hash readiness too.
                if let Some(url) = &it.image {
                    self.image_protocols
                        .contains_key(&EncKey::for_item(url, it))
                        .hash(&mut h);
                }
            }
        }
        // A region's window can scroll, and a carousel can shift, without the
        // page scroll moving.
        for rg in &g.doc.regions {
            rg.start_row.hash(&mut h);
            rg.voffset.hash(&mut h);
        }
        for c in &g.doc.carousels {
            c.offset.hash(&mut h);
        }
        Some(h.finish())
    }

    fn on_enc(&mut self, msg: EncMsg) {
        if msg.epoch != self.enc_epoch {
            // Encoded under a protocol setting that changed while it was in
            // flight: obsolete. Don't touch `image_encoding` either — the key
            // may have been re-requested under the new epoch.
            return;
        }
        self.image_encoding.remove(&msg.key);
        match msg.protocol {
            Some(protocol) => {
                self.image_protocols.insert(msg.key, protocol);
            }
            // Encode failed: remember it so it isn't re-requested every tick.
            None => {
                self.failed_encodes.insert(msg.key);
            }
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
        // Region buffer images: a vertical scroll region holds its content — text
        // AND images — in its own buffer, not in `doc.rows`, so the loop above
        // never sees them. Scan each on-screen region's windowed buffer (plus the
        // lookback, for a tall image whose top scrolled off the band's top) and
        // mark those boxes live too, else they'd never encode (or get evicted).
        for rg in &g.doc.regions {
            // Skip a region whose band is entirely outside the viewport window.
            if rg.start_row >= end || rg.start_row + rg.height as usize <= start {
                continue;
            }
            let top = rg.voffset.saturating_sub(crate::layout::MAX_IMAGE_LOOKBACK);
            let bot = rg.voffset + rg.height as usize;
            for br in top..bot {
                let Some(brow) = rg.buffer.get(br) else {
                    continue;
                };
                for item in &brow.items {
                    let Some(url) = &item.image else { continue };
                    if item.col >= rg.width {
                        continue; // past the scrollport's right edge — never shown
                    }
                    live.insert(EncKey::for_item(url, item));
                }
            }
        }
        // Fixed-layer images: the pinned `position:fixed` rails hold their rows
        // in `doc.fixed`, not `doc.rows`, so the scans above never see them. A
        // rail is always on-screen (viewport-pinned), so every image box in it
        // is live (the panel `Rect` clips at render).
        for fixed in &g.doc.fixed {
            for row in &fixed.rows {
                for item in &row.items {
                    let Some(url) = &item.image else { continue };
                    live.insert(EncKey::for_item(url, item));
                }
            }
        }
        // Bound the caches: drop protocols/failures for boxes no longer in range
        // (a re-scrolled box gets one fresh encode attempt).
        self.image_protocols.retain(|k, _| live.contains(k));
        self.failed_encodes.retain(|k| live.contains(k));
        let wanted: Vec<EncKey> = live
            .into_iter()
            .filter(|k| {
                !self.image_protocols.contains_key(k)
                    && !self.image_encoding.contains(k)
                    && !self.failed_encodes.contains(k)
            })
            .collect();
        for key in wanted {
            self.request_image_encode(key);
        }
    }

    /// Spawn one blocking encode of a decoded image to a sliced terminal
    /// protocol for the given cell box; the result lands over `enc_rx`.
    fn request_image_encode(&mut self, key: EncKey) {
        // A synthetic composite box (P8): alpha-blend its layers into one
        // protocol instead of encoding a single cached image.
        if key.url.starts_with("x-trust-composite:") {
            self.request_composite_encode(key);
            return;
        }
        let Some(decoded) = self.image_cache.get(&key.url) else {
            return;
        };
        let raw = decoded.raw.clone();
        let picker = self.picker.clone();
        let tx = self.enc_tx.clone();
        let epoch = self.enc_epoch;
        self.image_encoding.insert(key.clone());
        tokio::task::spawn_blocking(move || {
            // Runs on a tokio BLOCKING thread. Sandbox it: a decode/encode
            // panic (a malformed image, or a ratatui-image sixel edge case
            // on an odd box) must fail this ONE image to alt text, never
            // unwind the worker. The terminal is safe regardless — only the
            // run-loop thread restores it (see TERMINAL_OWNER).
            let box_size = ratatui::layout::Size::new(key.w, key.h);
            let protocol = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let protocol = crate::img::encode_sliced_bytes(
                    &picker,
                    &raw,
                    box_size,
                    key.crop,
                    key.pixelated,
                    key.tint,
                )
                .ok()
                .map(|(protocol, _)| protocol)?;
                // Build the at-rest slice HERE, on the encode thread, so the
                // first on-screen draw of this image is a cache hit instead of a
                // render-thread `to_sequence` build — matters for a streaming
                // chat's steady flow of newly-appearing emotes (see
                // SlicedProtocol::prewarm_sixel_cache).
                protocol.prewarm_sixel_cache();
                Some(protocol)
            }))
            .ok()
            .flatten();
            let _ = tx.blocking_send(EncMsg {
                key,
                protocol,
                epoch,
            });
        });
    }

    /// Spawn one blocking alpha-composite encode for a synthetic composite box
    /// (P8): gather each layer's decoded bytes + used box from `image_cache`,
    /// then `img::encode_composite` blends them into one protocol keyed by the
    /// synthetic URL. If any layer isn't decoded yet, do nothing — a later tick
    /// retries once every layer is cached (the composite key stays uncached, so
    /// `sync_image_encodes` re-requests it).
    fn request_composite_encode(&mut self, key: EncKey) {
        let Some(layers) = self
            .browser
            .as_ref()
            .and_then(|g| g.doc.composites.get(&key.url).cloned())
        else {
            return;
        };
        // Each layer's owned encode inputs, bottom first. If any layer is not
        // decoded yet, bail — a later tick retries once all are cached.
        let mut inputs: Vec<CompositeInput> = Vec::with_capacity(layers.len());
        for l in &layers {
            let Some(dec) = self.image_cache.get(&l.url) else {
                return; // a layer is not decoded yet — retry next tick
            };
            inputs.push(CompositeInput {
                raw: dec.raw.clone(),
                box_cells: (l.w, l.h),
                off_cells: (l.dcol, l.drow),
                crop: l.crop,
                pixelated: l.pixelated,
            });
        }
        let picker = self.picker.clone();
        let tx = self.enc_tx.clone();
        let epoch = self.enc_epoch;
        let tint = svg_tint();
        let box_size = ratatui::layout::Size::new(key.w, key.h);
        self.image_encoding.insert(key.clone());
        tokio::task::spawn_blocking(move || {
            let protocol = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let refs: Vec<crate::img::CompositeInput<'_>> = inputs
                    .iter()
                    .map(|i| crate::img::CompositeInput {
                        bytes: &i.raw,
                        box_cells: ratatui::layout::Size::new(i.box_cells.0, i.box_cells.1),
                        off_cells: i.off_cells,
                        crop: i.crop,
                        pixelated: i.pixelated,
                    })
                    .collect();
                let protocol =
                    crate::img::encode_composite(&picker, box_size, &refs, Some(tint)).ok()?;
                protocol.prewarm_sixel_cache();
                Some(protocol)
            }))
            .ok()
            .flatten();
            let _ = tx.blocking_send(EncMsg {
                key,
                protocol,
                epoch,
            });
        });
    }

    fn on_fetch(&mut self, msg: FetchMsg) {
        self.fetch_rx = None;
        self.fetch_task = None;
        self.notice = false;
        let failed = msg.result.is_err();
        if failed {
            // A failed reload must not make the NEXT navigation replace,
            // and a failed deep-travel refetch leaves the trail untouched.
            self.replace_nav = false;
            self.pending_travel = None;
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
            (Ok(Payload::About(body)), Link::External(url)) => {
                self.status = url.clone();
                let doc = Self::about_doc(url, body, width);
                self.navigate_to(doc);
            }
            (Err(err), target) => {
                self.status = format!("{target} — {err}");
                self.notice = true;
            }
            _ => {}
        }
        // A deep-travel fetch that ended somewhere other than a document
        // (image viewer, mpv, bot wall, a gemini input prompt) is spent —
        // navigate_to consumed the flag if it ran; anything left is stale.
        self.pending_travel = None;
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
            // 11 is "sensitive input" — the UI masks the typed text.
            10..=19 => {
                self.status = if response.meta.is_empty() {
                    String::from("Input requested.")
                } else {
                    response.meta.clone()
                };
                self.search_target = Some(Link::Gemini(response.url));
                self.masked_input = response.status == 11;
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
                    self.masked_input = false;
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
        let mut doc = crate::http::parse(
            &response.url,
            &response.content_type,
            &response.body,
            width,
            self.last_inner.1 as usize,
            &self.image_sizes,
        );
        // The blob byte mirror rides the Doc (into history too) so the image
        // pipeline can decode this page's `<img src="blob:…">` at any time.
        doc.blobs = response.blobs.clone().map(crate::doc::BlobsHandle);
        let image_urls = doc.image_urls.clone();
        let page = response.url.clone();
        // JS visibility: a clean run gets a quiet badge; script errors
        // get a count (the page still rendered — no notice).
        let js_note = match &response.js {
            Some(o) if !o.errors.is_empty() => format!(
                " · JS:{}!",
                o.errors
                    .iter()
                    .collect::<std::collections::HashSet<_>>()
                    .len()
            ),
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
        // A direct POST result's trail entry is exempt from doc eviction
        // (refetching would re-POST); navigate_to consumes this.
        self.nav_from_post = response.from_post;
        self.navigate_to(doc);
        // navigate_to dropped the previous living page; install this one.
        if let Some(live) = live {
            self.live_page = Some(live.handle);
            self.page_rx = Some(live.events);
            // Force the next tick to push the current scroll position (0 on a
            // fresh nav; a restored row on revive-on-back) to the new engine.
            self.last_scroll_sent = None;
            // And the decoded-image sizes (warm on revive-on-back), so the
            // engine's geometry pass lays images like the render does.
            self.image_sizes_sent = None;
            // And the true browser viewport (the engine only has the
            // fetch-time size until the first push).
            self.viewport_sent = None;
            // A fresh engine holds no hover chain; start the app's view clean.
            self.hover_sent = None;
            self.hover_want = None;
            self.page_js_errors = response
                .js
                .as_ref()
                .map(|o| o.errors.iter().cloned().collect())
                .unwrap_or_default();
        }
        // Landed on a URL carrying a `#fragment` (a followed cross-page anchor
        // link, or an address typed with one): scroll the named element to the
        // top, HTML's "scroll to the fragment". The shell doc's `anchor_rows` are
        // already built; a live page keeps this scroll across its later renders
        // (scroll is stable across `Updated`). Unknown fragment → stay at top.
        if let Some(frag) = response.url.fragment().filter(|f| !f.is_empty()) {
            self.scroll_to_fragment(frag);
        }
        // Kick off the parallel image pipeline; decoded images re-flow in.
        self.start_image_loads(page, image_urls);
    }

    fn drop_live_page(&mut self) {
        self.live_page = None;
        self.page_rx = None;
        self.last_scroll_sent = None;
        self.image_sizes_sent = None;
        self.viewport_sent = None;
        self.hover_sent = None;
        self.hover_want = None;
        self.region_geom_sent.clear();
        self.region_scroll_sent.clear();
        self.region_live.clear();
        self.pending_decoded_urls.clear();
        self.live_regions_sent.clear();
        self.live_boundaries_sent.clear();
        self.page_busy = false;
        self.pending_live_submit = None;
    }

    /// Push the current browser scroll position to the live page when it moved
    /// since the last send. The engine updates `window.scrollY`, fires the
    /// `scroll` event, and re-runs IntersectionObserver, so a site's own
    /// infinite-scroll / lazy-load logic reacts to the threaded position. Called
    /// once per run-loop iteration (after input is coalesced) — a single
    /// chokepoint that catches every path that moves `scroll` (wheel, PageUp/Down,
    /// Home/End, Ctrl-F jumps, selection). Cheap and idle-safe: a no-op unless a
    /// live laid-out doc's first visible row actually changed. `y` is in CSS px,
    /// `row * cell_height` — the SAME quantization `layout::measure_boxes` uses
    /// for element geometry, so the engine's scroll window and box coordinates
    /// agree.
    fn sync_page_scroll(&mut self) {
        let viewport_rows = self.last_inner.1 as usize;
        let cell_h = f64::from(self.picker.font_size().height.max(1));
        let (row, y) = {
            let Some(g) = self.browser.as_ref() else {
                return;
            };
            // Only HTTP laid-out docs use the 2D row model + a live engine; gopher/
            // gemini keep the line model and never carry an engine.
            if !g.doc.laid_out() {
                return;
            }
            let row = g.scroll;
            if self.last_scroll_sent == Some(row) {
                return;
            }
            let total = g.doc.rows.len();
            let max_scroll = total.saturating_sub(viewport_rows);
            // AT the app's bottom row, request the full document height and let
            // the engine clamp to ITS exact bottom (`setScroll` clamps to
            // `scrollHeight − innerHeight`, as CSSOM View clamps any
            // over-large scrollTo) — robust to residual sub-row rounding
            // between the two layouts, and it lands the viewport where an
            // infinite-scroll sentinel sits. Everywhere else, the TRUE
            // position. This used to be a whole-viewport BAND ("within a
            // viewport of the bottom ⇒ tell the engine it's at the bottom"),
            // compensating for two coordinate lies since fixed — the
            // fetch-time innerHeight (PageCmd::Viewport now corrects it) and
            // decode-blind image geometry (PageCmd::ImageSizes now unifies
            // it). The band itself was a third lie: for the last two
            // viewports of the doc the engine believed the reader was at the
            // very bottom, so a virtualized feed (Mastodon) revealed the
            // bottom articles and UN-revealed the ones actually on screen —
            // the reader watched their posts collapse into placeholders.
            let y = if row >= max_scroll {
                total as f64 * cell_h
            } else {
                row as f64 * cell_h
            };
            (row, y)
        };
        let Some(handle) = self.live_page.as_ref() else {
            return;
        };
        // Clone the (Arc-backed) sender so the borrow of `self.live_page` ends
        // before we record `last_scroll_sent`.
        let sender = handle.cmds.clone();
        if sender
            .try_send(crate::js::PageCmd::Scroll { x: 0.0, y })
            .is_ok()
        {
            self.last_scroll_sent = Some(row);
        }
        // On a full channel we leave `last_scroll_sent` stale: the next tick
        // retries with the latest position, so the final scroll is never lost.
    }

    /// Push the browser's true content-area size (cells) to the live page when
    /// it changed since the last send (same discipline as `sync_page_scroll`:
    /// record only on a successful enqueue). The engine adopts it —
    /// `innerWidth`/`innerHeight`, the geometry measure viewport — and fires
    /// `resize` at the Window (CSSOM View §4.1). The fetch-time size the
    /// engine was created with belongs to whatever view was on screen when
    /// the fetch started (at startup: the session layout, taller than the
    /// browser view), so the first push after display corrects it.
    fn sync_page_viewport(&mut self) {
        {
            let Some(g) = self.browser.as_ref() else {
                return;
            };
            if !g.doc.laid_out() {
                return;
            }
        }
        let vp = self.last_inner;
        if vp.0 == 0 || vp.1 == 0 || self.viewport_sent == Some(vp) {
            return;
        }
        let Some(handle) = self.live_page.as_ref() else {
            return;
        };
        if handle
            .cmds
            .try_send(crate::js::PageCmd::Viewport {
                cols: vp.0,
                rows: vp.1,
            })
            .is_ok()
        {
            self.viewport_sent = Some(vp);
        }
    }

    /// Push the decoded-image size map to the live page when it grew since the
    /// last send (`sync_page_scroll`'s send discipline: record only on a
    /// successful enqueue, so a full channel retries next tick). The actor
    /// merges it into the geometry pass's `ImageSizes`, making the boxes page
    /// JS measures match the rendered page — one layout truth for both
    /// coordinate systems. Sizes only accrue while a page is displayed, so the
    /// map LENGTH is the change signal.
    fn sync_page_image_sizes(&mut self) {
        let Some(handle) = self.live_page.as_ref() else {
            return;
        };
        let n = self.image_sizes.len();
        if self.image_sizes_sent == Some(n) {
            return;
        }
        if n == 0 {
            // Nothing to push; just mark the empty map as current.
            self.image_sizes_sent = Some(0);
            return;
        }
        let sizes: Vec<(String, (u16, u16))> = self
            .image_sizes
            .iter()
            .map(|(u, &d)| (u.clone(), d))
            .collect();
        if handle
            .cmds
            .try_send(crate::js::PageCmd::ImageSizes(sizes))
            .is_ok()
        {
            self.image_sizes_sent = Some(n);
        }
    }

    /// The viewport-relative CSS-px center of a screen cell — the
    /// `clientX`/`clientY` a hover dispatch carries (the same `cell_px`
    /// quantization `measure_boxes`/`sync_page_scroll` use, so the engine's
    /// coordinates agree).
    fn viewport_px_of(&self, col: u16, row: u16) -> (f64, f64) {
        let f = self.picker.font_size();
        let x = (f64::from(col.saturating_sub(self.last_content_area.x)) + 0.5)
            * f64::from(f.width.max(1));
        let y = (f64::from(row.saturating_sub(self.last_content_area.y)) + 0.5)
            * f64::from(f.height.max(1));
        (x, y)
    }

    /// The live actor node an item resolves to for hover: the parse-time
    /// `Doc.hover_ids` map (nearest `data-trust-hover` / `x-trust-js` marker
    /// — deepest wins structurally), with the item's own `JsClick` link as a
    /// fallback. Shared by the pointer hit-test and the keyboard-selection
    /// hover path.
    fn hover_resolve(g: &BrowserView, item: &crate::layout::Item) -> Option<usize> {
        if let Some(&actor) = g.doc.hover_ids.get(&item.node) {
            return Some(actor);
        }
        match &item.link {
            Some(crate::doc::Link::JsClick { node, .. }) => Some(*node),
            _ => None,
        }
    }

    /// Everything under the pointer at screen `(col, row)` — see `PointerHit`.
    /// The pinned fixed layer draws on top, so it resolves first (an
    /// interactive fixed hit takes BOTH the selection and the hover; a rail
    /// miss falls through to the doc, matching the old two-scan behavior).
    /// The doc walk goes newest-row-first over the viewport rows reaching the
    /// cursor cell, each row's `visual_columns` computed ONCE; within the
    /// walk the first covering interactive item becomes the selection and the
    /// first covering item that resolves to an actor becomes the hover —
    /// identical results to the two independent scans this replaced, at half
    /// the work per mouse-move event.
    fn pointer_hit(&self, col: u16, row: u16) -> PointerHit {
        let mut hit = PointerHit::default();
        let Some(g) = self.browser.as_ref() else {
            return hit;
        };
        if let Some((fi, r, i)) = self.fixed_hit_test(col, row) {
            hit.hover = Self::hover_resolve(g, &g.doc.fixed[fi].rows[r].items[i]);
            hit.fixed = Some((fi, r, i));
            return hit;
        }
        if !self.mouse_in_content_area(col, row) || !g.doc.laid_out() {
            return hit;
        }
        let local_row = row.saturating_sub(self.last_content_area.y) as usize;
        let doc_row = g.scroll + local_row;
        if doc_row >= g.doc.rows.len() {
            return hit;
        }
        let local_col = col.saturating_sub(self.last_content_area.x);
        'rows: for r in (g.scroll..=doc_row).rev() {
            if r >= g.doc.rows.len() {
                continue;
            }
            let row_offset = doc_row.saturating_sub(r);
            // The effective row merges any scroll-region buffer window over
            // the reserved band, so the hit lands on region content; the
            // visual columns are the SAME on-screen placement the renderer
            // draws (carousel clip + gap-fill + overlap-append).
            let row = crate::layout::effective_row(&g.doc.rows, &g.doc.regions, r);
            for (i, start) in crate::layout::visual_columns(&row, &g.doc.carousels, r) {
                let item = &row.items[i];
                let end = start.saturating_add(item.width);
                let covers = row_offset < item.height.max(1) as usize
                    && local_col >= start
                    && local_col < end;
                if !covers {
                    continue;
                }
                if hit.item.is_none() && item.is_interactive() {
                    hit.item = Some((r, i));
                }
                if hit.hover.is_none()
                    && let Some(actor) = Self::hover_resolve(g, item)
                {
                    hit.hover = Some(actor);
                }
                if hit.item.is_some() && hit.hover.is_some() {
                    break 'rows;
                }
            }
        }
        hit
    }

    /// Record a pointer-target change for the live page. The run loop arms the
    /// `HOVER_DWELL` one-shot when `hover_want` differs from what was sent;
    /// `commit_page_hover` dispatches once the pointer rests. A no-change
    /// request is free (the diff is the load-bearing throttle), and a clear
    /// (`None`) is only worth recording after a `Some` was sent.
    fn request_page_hover(&mut self, target: Option<usize>, x: f64, y: f64) {
        if self.live_page.is_none() {
            return;
        }
        let current = self.hover_want.map_or(self.hover_sent, |(t, _, _)| t);
        if current == target {
            return;
        }
        self.hover_want = Some((target, x, y));
    }

    /// The dwell elapsed: send the pending hover target to the live page.
    /// Mirrors `sync_page_scroll`'s send discipline — only record `hover_sent`
    /// on a successful enqueue; on a full channel the want stays pending and
    /// the run loop re-arms, so the final rest target is never lost.
    fn commit_page_hover(&mut self) {
        let Some((target, x, y)) = self.hover_want else {
            return;
        };
        if target == self.hover_sent {
            self.hover_want = None;
            return;
        }
        let Some(handle) = self.live_page.as_ref() else {
            self.hover_want = None;
            return;
        };
        let sender = handle.cmds.clone();
        if sender
            .try_send(crate::js::PageCmd::Hover { node: target, x, y })
            .is_ok()
        {
            self.hover_sent = target;
            self.hover_want = None;
        }
    }

    /// Feed the keyboard selection into the hover pipeline: the selection IS
    /// the terminal's pointer (her call), so arrowing onto an element hovers
    /// it — Steam's preview pane switches from the keyboard. Resolves the
    /// selected item (fixed layer first, mirroring the draw order) to its
    /// actor node + on-screen cell center.
    fn sync_page_hover_selection(&mut self) {
        if self.live_page.is_none() {
            return;
        }
        let Some(g) = self.browser.as_ref() else {
            return;
        };
        let resolve = |item: &crate::layout::Item| Self::hover_resolve(g, item);
        let (target, cell) = if let Some((fi, r, i)) = g.sel_fixed {
            let f = &g.doc.fixed[fi];
            let item = &f.rows[r].items[i];
            let col = self.last_content_area.x as usize
                + f.col as usize
                + item.col as usize
                + item.width.max(1) as usize / 2;
            let row = self.last_content_area.y as usize + f.row as usize + r;
            (resolve(item), (col as u16, row as u16))
        } else if let Some((r, i)) = g.sel_item {
            if r < g.scroll || r >= g.scroll + self.last_inner.1 as usize {
                return; // selection is off-screen; no pointer position to report
            }
            let row = crate::layout::effective_row(&g.doc.rows, &g.doc.regions, r);
            let Some(item) = row.items.get(i) else {
                return;
            };
            let col = self.last_content_area.x as usize
                + item.col as usize
                + item.width.max(1) as usize / 2;
            let srow = self.last_content_area.y as usize + (r - g.scroll);
            (resolve(item), (col as u16, srow as u16))
        } else {
            return; // no selection → leave the hover state alone
        };
        let (x, y) = self.viewport_px_of(cell.0, cell.1);
        self.request_page_hover(target, x, y);
    }

    /// Push each inner-scroll region's app-measured box geometry (CSSOM View, px)
    /// to the live page, so the element's `scrollHeight`/`scrollWidth`/
    /// `clientHeight`/`clientWidth` getters read TRUE values — required for the
    /// conditional pin idiom (`if scrollTop + clientHeight >= scrollHeight`). The
    /// app is the only party that lays the region out (clips it to its definite
    /// height + retains the full buffer), so it owns the geometry; the actor's
    /// in-process measure pass flows region content inline and can't distinguish
    /// the clip box from the content extent. Diffed per node (quantized to ints)
    /// so a `RegionGeom` command goes out only when a box changed. Mirrors
    /// `sync_page_scroll`: only mark a node sent on a successful enqueue.
    fn sync_region_state(&mut self) {
        if self.live_page.is_none() {
            return;
        }
        let cell_w = f64::from(self.picker.font_size().width.max(1));
        let cell_h = f64::from(self.picker.font_size().height.max(1));
        let measured: Vec<(usize, f64, f64)> = {
            let Some(g) = self.browser.as_ref() else {
                return;
            };
            // `scroll_clips` covers EVERY definite-height scroll-y box — the
            // overflowing regions AND the ones whose content currently fits (no
            // region) — so the page's `clientHeight` is right from the first
            // message, before content overflows. The CLIP box only: clientHeight =
            // the band, clientWidth = the scrollport (no horizontal inner scroll).
            // scrollHeight is NOT pushed — the actor reads it fresh from `__dom_rect`.
            g.doc
                .scroll_clips
                .iter()
                .map(|&(node, h_rows, w_cells)| {
                    (
                        node,
                        f64::from(h_rows) * cell_h,
                        f64::from(w_cells) * cell_w,
                    )
                })
                .collect()
        };
        // Drop entries for regions that vanished, so a re-appearing node re-sends.
        self.region_geom_sent
            .retain(|node, _| measured.iter().any(|&(n, ..)| n == *node));
        let mut to_send: Vec<(usize, f64, f64)> = Vec::new();
        let mut keys: Vec<(usize, (i64, i64))> = Vec::new();
        for &(node, ch, cw) in &measured {
            let key = (ch as i64, cw as i64);
            if self.region_geom_sent.get(&node) != Some(&key) {
                to_send.push((node, ch, cw));
                keys.push((node, key));
            }
        }
        if to_send.is_empty() {
            return;
        }
        if let Some(handle) = self.live_page.as_ref()
            && handle
                .cmds
                .try_send(crate::js::PageCmd::RegionGeom { items: to_send })
                .is_ok()
        {
            for (node, key) in keys {
                self.region_geom_sent.insert(node, key);
            }
        }
    }

    /// Tell the page actor which scroll boxes are CURRENTLY clipped regions (the
    /// live nodes of `Doc.regions`), so it patches a mutation ONLY when confined
    /// to a real region (INCREMENTAL_LAYOUT_PLAN.md §4b). A box whose content
    /// fits (no `Region`) is NOT included, so a mutation inside it takes the full
    /// path — never a failed patch + resync. Re-sent only when the set changes.
    fn sync_live_regions(&mut self) {
        if self.live_page.is_none() {
            return;
        }
        let current: std::collections::HashSet<usize> = self
            .browser
            .as_ref()
            .map(|g| g.doc.regions.iter().filter_map(|r| r.live_node).collect())
            .unwrap_or_default();
        if current == self.live_regions_sent {
            return;
        }
        if let Some(handle) = self.live_page.as_ref()
            && handle
                .cmds
                .try_send(crate::js::PageCmd::LiveRegions(
                    current.iter().copied().collect(),
                ))
                .is_ok()
        {
            self.live_regions_sent = current;
        }
    }

    /// Tell the page actor which inline IFC boundaries the app has cached as
    /// splice-able boxes (the `Doc.boundaries` node set), so it proposes a
    /// general inline `Patched` ONLY when the box is in this set
    /// (INCREMENTAL_LAYOUT_PLAN.md §14). A boundary the app hasn't cached (it
    /// overlapped a region/carousel, or hasn't been captured yet) takes the full
    /// path — never a failed patch + resync. Re-sent only when the set changes.
    fn sync_live_boundaries(&mut self) {
        if self.live_page.is_none() {
            return;
        }
        let current: std::collections::HashSet<usize> = self
            .browser
            .as_ref()
            .map(|g| g.doc.boundaries.iter().map(|b| b.node).collect())
            .unwrap_or_default();
        if current == self.live_boundaries_sent {
            return;
        }
        if let Some(handle) = self.live_page.as_ref()
            && handle
                .cmds
                .try_send(crate::js::PageCmd::LiveBoundaries(
                    current.iter().copied().collect(),
                ))
                .is_ok()
        {
            self.live_boundaries_sent = current;
        }
    }

    /// Write an inner-scroll region's new `scrollTop` (rows → px) BACK into the
    /// live element (CSSOM View — the region→page write-back) so the page's
    /// `scroll` handler fires and a conditional pin-to-bottom learns the user
    /// scrolled up. A static region (`live_node == None`, no engine) needs no
    /// write-back — the app's own offset persistence carries it. Deduped per node
    /// so an unchanged offset (a boundary scroll) isn't re-sent.
    fn region_writeback(&mut self, node: Option<usize>, voffset: usize) {
        let Some(node) = node else { return };
        if self.region_scroll_sent.get(&node) == Some(&voffset) {
            return;
        }
        let cell_h = f64::from(self.picker.font_size().height.max(1));
        let top = voffset as f64 * cell_h;
        if let Some(handle) = self.live_page.as_ref()
            && handle
                .cmds
                .try_send(crate::js::PageCmd::SetScroll {
                    node,
                    top,
                    left: 0.0,
                })
                .is_ok()
        {
            self.region_scroll_sent.insert(node, voffset);
        }
    }

    /// Apply a page-initiated inner-scroll write (`PageEvt::Scrolled`): move the
    /// region whose live node matches to the page's `scrollTop` (px → rows,
    /// clamped) — a cheap re-blit, no re-parse. The page dictated this offset, so
    /// mark it `voffset_from_page` (a subsequent re-layout keeps it).
    fn apply_region_scroll(&mut self, node: usize, top: f64) {
        let cell_h = f64::from(self.picker.font_size().height.max(1));
        let rows = (top / cell_h).round().max(0.0) as usize;
        let Some(g) = self.browser.as_mut() else {
            return;
        };
        for rg in g.doc.regions.iter_mut() {
            if rg.live_node == Some(node) {
                rg.voffset = rows.min(rg.max_voffset());
                rg.voffset_from_page = true;
                // Keep the write-back dedup in sync so a wheel right after a
                // page pin still re-sends (the page owns this value now).
                self.region_scroll_sent.insert(node, rg.voffset);
            }
        }
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
            || !self.imgs_in_flight.is_empty()
            || self.live_page.is_some()
            || self.page_busy;
        if let Some(task) = self.fetch_task.take() {
            task.abort();
        }
        self.fetch_rx = None;
        self.pending_travel = None;
        self.abort_image_loads();
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
        let _t_evt = std::time::Instant::now();
        let r = self.on_page_evt_inner(evt);
        if *DIAG_FRAME {
            PAGE_WORK.with(|w| {
                let (us, n, full, drains) = w.get();
                w.set((
                    us + _t_evt.elapsed().as_micros() as u64,
                    n + 1,
                    full + r.0 as u64,
                    drains + r.1 as u64,
                ));
            });
        }
    }

    /// (did a full replace_live_doc, events drained this call) — for the frame
    /// diagnostic.
    fn on_page_evt_inner(&mut self, evt: crate::js::PageEvt) -> (bool, u32) {
        use crate::js::PageEvt;
        self.page_busy = false;
        let mut latest_update: Option<(String, crate::js::Outcome)> = None;
        // Incremental-layout patches accumulated this batch (newest per boundary
        // wins; a full Updated supersedes them all). INCREMENTAL_LAYOUT_PLAN.md.
        let mut patches: Vec<crate::js::SubtreePatch> = Vec::new();
        let mut trouble: Vec<String> = Vec::new();
        let mut navigate: Option<String> = None;
        // A same-document fragment scroll the page requested; applied AFTER the
        // render below so it targets the freshly-rendered doc's `anchor_rows`.
        // Newest wins.
        let mut scroll_fragment: Option<String> = None;
        let mut submit_default = false;
        // A click-triggered native submit carries its form/submitter arena
        // nodes (the app didn't pre-record them the way the Submit path does).
        let mut submit_nodes: Option<(usize, usize)> = None;
        // Page-initiated inner-scroll writes (CSSOM View) without a DOM mutation:
        // applied AFTER any content update so they land on the new regions.
        let mut scrolled: Vec<(usize, f64)> = Vec::new();
        let mut pending = Some(evt);
        let mut drains = 0u32;
        loop {
            drains += 1;
            match pending {
                Some(PageEvt::Updated { html, outcome } | PageEvt::Static { html, outcome }) => {
                    tally_evt(0);
                    latest_update = Some((html, outcome));
                    // A full render supersedes any patches queued before it.
                    patches.clear();
                }
                Some(PageEvt::Patched {
                    patches: ps,
                    outcome,
                }) => {
                    tally_evt(1);
                    self.page_js_errors.extend(outcome.errors.iter().cloned());
                    for p in ps {
                        // Coalesce: only the newest patch per boundary matters.
                        patches.retain(|q| q.node != p.node);
                        patches.push(p);
                    }
                }
                Some(PageEvt::Trouble(errors)) => trouble.extend(errors),
                Some(PageEvt::Settled) => tally_evt(3),
                Some(PageEvt::Scrolled { node, top, .. }) => {
                    tally_evt(2);
                    scrolled.push((node, top));
                }
                Some(PageEvt::SubmitDefault) => submit_default = true,
                Some(PageEvt::SubmitForm { form, submitter }) => {
                    submit_nodes = Some((form, submitter));
                }
                Some(PageEvt::Navigate(url)) => navigate = Some(url),
                Some(PageEvt::ScrollToFragment(frag)) => scroll_fragment = Some(frag),
                None => break,
            }
            pending = self.page_rx.as_mut().and_then(|rx| rx.try_recv().ok());
        }

        let mut rendered = false;
        let full_replace = latest_update.is_some();
        if let Some((html, outcome)) = latest_update {
            self.page_js_errors.extend(outcome.errors.iter().cloned());
            // The actor only emits an Updated when what we PAINT changed
            // (`extract_changed` dedups non-rendered mutations via
            // `render_canonical`), so an update reaching here is real work — apply
            // it at once.
            self.replace_live_doc(html.into_bytes());
            rendered = true;
        }
        // Incremental-layout patches (INCREMENTAL_LAYOUT_PLAN.md): re-lay ONLY the
        // changed scroll region(s), leaving the rest of the document untouched.
        // A patch that can't apply (the boundary isn't a live region) asks the
        // actor to resync with a full render, then we stop (the resync supersedes
        // any remaining stale patches in this batch).
        for p in &patches {
            if self.patch_live_doc(p) {
                rendered = true;
            } else {
                self.request_resync();
                break;
            }
        }
        if rendered {
            self.status = if self.page_js_errors.is_empty() {
                String::from("page updated · JS")
            } else {
                format!("page updated · JS:{}!", self.page_js_errors.len())
            };
        }
        // Page-initiated scroll writes (a chat pinning to bottom) re-window the
        // region cheaply — after any content update, so they target the new
        // regions. Latest write per node wins (the loop preserves order).
        for (node, top) in scrolled {
            self.apply_region_scroll(node, top);
        }
        // A same-document fragment scroll (`#anchor` link inside the live page),
        // applied after any content update so it resolves against the new doc.
        if let Some(frag) = scroll_fragment {
            self.scroll_to_fragment(&frag);
        }
        if !trouble.is_empty() {
            self.page_js_errors.extend(trouble.iter().cloned());
            self.status = format!(
                "page JS: {} (JS:{}!)",
                trouble[0],
                self.page_js_errors.len()
            );
            self.notice = true;
        }
        // Consume the recorded submit target only when its SubmitDefault
        // actually arrived. Since the engine runs at rest, an AUTONOMOUS
        // render (a timer tick) can land between the Submit dispatch and the
        // actor's answer — taking the record on every batch dropped it, so
        // the fallback static submit silently never ran. Stale records can't
        // mis-fire: every dispatch overwrites it, and `drop_live_page` clears
        // it (a page-JS-owned submit just leaves it parked until then).
        if submit_default && let Some((form, field)) = self.pending_live_submit.take() {
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
        (full_replace, drains)
    }

    /// Swap the living page's fresh render into the browser doc:
    /// history untouched, scroll kept, selection re-found by TARGET
    /// (line indices shift under a mutating page; the gopherus
    /// navigation model must not jumble).
    fn replace_live_doc(&mut self, raw: Vec<u8>) {
        // DIAG (TRUST_DUMP_RAW=<dir>): dump each live render for offline
        // diffing/replay through the layout_dump/measure_dump harnesses —
        // how the Steam delayed-image regression was cracked.
        if let Some(dir) = std::env::var_os("TRUST_DUMP_RAW") {
            let n = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0);
            let _ = std::fs::write(
                std::path::Path::new(&dir).join(format!("render_{n}.html")),
                &raw,
            );
        }
        let width = (self.last_inner.0 as usize).max(10);
        let height = self.last_inner.1.max(1) as usize;
        let scroll_intent = self.scroll_intent;
        let Some(g) = &mut self.browser else { return };
        let Link::Http(url) = g.doc.url.clone() else {
            return;
        };
        // Remember the selected item by its arena node (and link) so the
        // selection survives the DOM mutating under it.
        let selected_target = g.sel_item.and_then(|(r, i)| {
            crate::layout::effective_row(&g.doc.rows, &g.doc.regions, r)
                .items
                .get(i)
                .cloned()
        });
        // CSS Scroll Anchoring: the topmost followable item the reader currently
        // sees (its row + link), and how far below the viewport top it sits. If it
        // survives the re-render we re-pin it to that same screen offset (below),
        // so a transient height change (archive's "Searching…" placeholder, an
        // image-driven reflow) or content inserted above can't bounce the view.
        // Captured from the OLD doc, before the re-parse; matched by LINK (a fresh
        // re-parse reassigns layout node ids, so they can't anchor across it).
        let view_anchor: Option<(usize, crate::doc::Link)> = {
            let bot = (g.scroll + height).min(g.doc.rows.len());
            (g.scroll..bot).find_map(|r| {
                crate::layout::effective_row(&g.doc.rows, &g.doc.regions, r)
                    .items
                    .iter()
                    .find_map(|it| it.link.clone().map(|l| (r, l)))
            })
        };
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
        // Did the page have a selection at all? In mouse mode with nothing
        // hovered there's none, and an AUTONOMOUS live update (a timer/anim
        // tick) must not conjure one — popping a selection onto a link, and
        // possibly dragging the viewport, while the user just reads. Only a
        // page that ALREADY had a selection earns the lost-it fallback below.
        let had_selection = g.sel_item.is_some();
        let mut doc = http::parse_seeded(
            &url,
            "text/html; charset=utf-8",
            &raw,
            width,
            height,
            None,
            &self.image_sizes,
            &self.image_alpha,
        );
        // The live page keeps minting into the SAME map (shared Arc), so the
        // re-parsed doc must keep carrying it.
        doc.blobs = g.doc.blobs.clone();
        // Carry scroll-region scroll positions across the re-layout (restored
        // below, before the selection re-anchor reads the windowed rows).
        let mut old_offsets = Vec::new();
        Self::collect_region_offsets(&g.doc.regions, &mut old_offsets);
        g.doc = doc;
        // A live re-render carries a FRESH baked scroll signal, so a page-pinned
        // region keeps the page's position (prefer_signal = true); only an
        // un-signalled region restores the user's wheel offset.
        Self::carry_region_offsets(&old_offsets, &mut g.doc.regions, true);
        // A rail selection survives only if its address still resolves in the
        // re-rendered fixed layers (they can reshape under a live update).
        if let Some((fi, r, i)) = g.sel_fixed
            && g.doc
                .fixed
                .get(fi)
                .and_then(|f| f.rows.get(r))
                .and_then(|row| row.items.get(i))
                .is_none()
        {
            g.sel_fixed = None;
        }
        g.sel_item = selected_target
            .and_then(|target| Self::find_item_like(&g.doc, &target))
            // Lost a selection we HAD? Fall back to the first interactive item
            // in view. Had none? Keep it None (see `had_selection`).
            .or_else(|| {
                had_selection
                    .then(|| Self::http_first_visible_item(g, height))
                    .flatten()
            });
        let max_scroll = g.doc.rows.len().saturating_sub(height);
        // The position to restore toward. We RESTORE from `scroll_intent`, not
        // the (possibly already shrink-clamped) live `scroll`, so a transient
        // shrink — a placeholder frame the page paints before it refills (e.g.
        // archive.org's "Searching…") — no longer permanently pops the view up:
        // when the doc grows back, `intent.min(max_scroll)` lands where the
        // reader was. The display value is still clamped to the current content.
        let new_intent: Option<usize> = match g.sel_item {
            // The update pushed a SELECTION that was visible off-screen: follow
            // it (a deliberate reposition, so it becomes the new intent).
            Some((r, _)) if sel_was_visible && (r < g.scroll || r >= g.scroll + height) => {
                g.scroll = r.saturating_sub(height / 2).min(max_scroll);
                Some(g.scroll)
            }
            // Keep the reader's place. Prefer to pin the viewport-top anchor to
            // its old screen offset (CSS Scroll Anchoring — content the reader
            // sees stays put across the re-render); fall back to restoring toward
            // the user's intent, clamped, when the anchor didn't survive (the
            // transient-shrink safety net — there intent stays untouched so the
            // regrow recovers it).
            _ => {
                g.scroll = match &view_anchor {
                    Some((old_r, link)) => match Self::find_row_by_link(&g.doc, link, *old_r) {
                        Some(new_r) => {
                            let offset = old_r.saturating_sub(g.scroll);
                            // CSS Scroll Anchoring: the adjustment IS the new
                            // scroll position — the intent must move WITH the
                            // anchored content (the patch path already does
                            // this). Leaving it behind made the NEXT update's
                            // intent-restore undo this pin: the view oscillated
                            // between the pinned and stale rows with no user
                            // input (observed live on a Mastodon feed as posts
                            // above the viewport reveal/unreveal).
                            let shift = new_r as isize - *old_r as isize;
                            self.scroll_intent =
                                (self.scroll_intent as isize + shift).max(0) as usize;
                            new_r.saturating_sub(offset).min(max_scroll)
                        }
                        None => scroll_intent.min(max_scroll),
                    },
                    None => scroll_intent.min(max_scroll),
                };
                None
            }
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
        // A deliberate reposition (following an off-screen selection) adopts its
        // new scroll as the intent; the "keep" path left it None to preserve the
        // reader's intended place across a transient shrink.
        if let Some(v) = new_intent {
            self.scroll_intent = v;
        }
        // An open <select> dropdown stays up across unrelated live updates,
        // but closes when the update removed (or retyped) the field it was
        // bound to — its indices would otherwise commit into a stranger.
        if let Some(menu) = &self.select_menu {
            let still_a_select = self
                .browser
                .as_ref()
                .and_then(|g| g.doc.forms.get(menu.form))
                .and_then(|f| f.fields.get(menu.field))
                .is_some_and(|f| matches!(f.kind, crate::doc::FieldKind::Select(_)));
            if !still_a_select {
                self.select_menu = None;
                self.status = String::from("Select closed — the page updated that control.");
                self.notice = true;
            }
        }
        self.start_image_loads(url, image_urls);
    }

    /// Apply one incremental-layout patch (INCREMENTAL_LAYOUT_PLAN.md): re-lay
    /// ONLY the boundary's subtree and splice it back, leaving the rest of the
    /// document untouched. Two arms: a live scroll `Region` swaps its buffer
    /// (Tier 1, structurally row-count-invariant); a general inline IFC boundary
    /// splices its rows into `Doc.rows` (Tier 1 in-place / Tier 2 shift). Returns
    /// false when neither applies (the cache is out of sync, the box reshaped, or
    /// it grew a sub-frame) and the caller resyncs to the full path.
    fn patch_live_doc(&mut self, patch: &crate::js::SubtreePatch) -> bool {
        // A region boundary (its content lives in a side buffer) takes the
        // buffer-swap arm; everything else is an inline boundary spliced into
        // `Doc.rows`.
        let is_region = self.browser.as_ref().is_some_and(|g| {
            g.doc
                .regions
                .iter()
                .any(|r| r.live_node == Some(patch.node))
        });
        if *DIAG_FRAME {
            let (nreg, live) = self.browser.as_ref().map_or((0, 0), |g| {
                (
                    g.doc.regions.len(),
                    g.doc
                        .regions
                        .iter()
                        .filter(|r| r.live_node.is_some())
                        .count(),
                )
            });
            eprintln!(
                "DIAGROUTE patch.node={} is_region={is_region} tier={:?} doc.regions={nreg} with_live_node={live}",
                patch.node, patch.tier
            );
        }
        if is_region {
            self.patch_live_region(patch)
        } else {
            self.patch_live_boundary(patch)
        }
    }

    /// The region arm of `patch_live_doc`: swap a live scroll `Region`'s buffer
    /// with the boundary's freshly laid content. The band reserves a fixed
    /// `Region.height` rows regardless of the buffer's length, so the outer
    /// layout is invariant by construction — `Doc.rows`, the page scroll, and
    /// every other region stay untouched.
    fn patch_live_region(&mut self, patch: &crate::js::SubtreePatch) -> bool {
        self.relay_region_from_html(patch.node, patch.html.as_bytes(), false)
    }

    /// Lay a live scroll `Region`'s buffer from a fragment HTML — shared by the
    /// actor's patch (`patch_live_region`, `keep_scroll=false`: honor the page's
    /// fresh `scrollTop` pin) and an in-place region refresh (`relay_region`,
    /// `keep_scroll=true`: an image decode / post-full-render refresh keeps the
    /// reader's offset). The band reserves a fixed `Region.height` rows regardless
    /// of the buffer's length, so the outer layout is invariant by construction —
    /// `Doc.rows`, the page scroll, and every other region stay untouched. Retains
    /// the fragment HTML + refreshed row cache in `region_live` for the next
    /// re-lay.
    fn relay_region_from_html(&mut self, node: usize, html: &[u8], keep_scroll: bool) -> bool {
        let viewport = (
            (self.last_inner.0 as usize).max(10),
            self.last_inner.1.max(1) as usize,
        );
        let url = match self.browser.as_ref().map(|g| g.doc.url.clone()) {
            Some(Link::Http(u)) => u,
            _ => return false,
        };
        // Locate the live region this targets (by the actor node id baked as
        // `data-trust-node`).
        let Some(ri) = self
            .browser
            .as_ref()
            .and_then(|g| g.doc.regions.iter().position(|r| r.live_node == Some(node)))
        else {
            return false;
        };
        let region_width = self.browser.as_ref().unwrap().doc.regions[ri].width as usize;
        // The memoized child-row cache from the previous re-lay (or an empty one),
        // so unchanged messages are reused and only the new/decoded one is laid.
        let old = self.region_live.remove(&node).unwrap_or_default();
        let Some(rp) = http::lay_region_patch(
            &url,
            html,
            region_width,
            viewport,
            &self.image_sizes,
            node,
            &old.cache,
        ) else {
            // Re-insert so a transient miss (boundary not found) doesn't discard
            // the retained HTML/memo for the next attempt.
            self.region_live.insert(node, old);
            return false;
        };
        self.region_live.insert(
            node,
            RegionLive {
                html: html.to_vec(),
                cache: rp.row_cache.clone(),
            },
        );
        if *DIAG_FRAME {
            eprintln!(
                "DIAGRELAY stored node={node} keep_scroll={keep_scroll} region_img_urls={} region_live={}",
                rp.image_urls.len(),
                self.region_live.len(),
            );
        }
        let g = self.browser.as_mut().unwrap();
        // Capture a selection sitting INSIDE this region's band, to re-anchor it
        // after the swap. A selection elsewhere keeps its row/item indices — the
        // band's row COUNT is invariant, so nothing outside the region moves.
        let rg = &g.doc.regions[ri];
        let band = rg.start_row..rg.start_row + rg.height as usize;
        let selected_target = g
            .sel_item
            .filter(|&(r, _)| band.contains(&r))
            .and_then(|(r, i)| {
                crate::layout::effective_row(&g.doc.rows, &g.doc.regions, r)
                    .items
                    .get(i)
                    .cloned()
            });
        // Carry the scroll position. An actor patch (`keep_scroll=false`) prefers
        // the page's fresh `scrollTop` signal (a chat pinning to bottom); an
        // in-place refresh (`keep_scroll=true` — an image decode or post-render
        // refresh, no fresh signal) keeps the reader's current offset. Both clamp
        // to the new content height (mirrors `flow_region`/`carry_region_offsets`).
        let new_max = rp.rows.len().saturating_sub(rg.height as usize);
        let (voffset, from_page) = match (keep_scroll, rp.scroll_top) {
            (false, Some(s)) => (s.min(new_max), true),
            _ => (rg.voffset.min(new_max), rg.voffset_from_page),
        };
        let rg = &mut g.doc.regions[ri];
        rg.buffer = rp.rows;
        rg.carousels = rp.carousels;
        rg.voffset = voffset;
        rg.voffset_from_page = from_page;
        // Refresh the region's decode-routing set from the patch fragment, so a
        // newly-arrived chat emote routes to this region (not the full document)
        // even though the last full render predates it.
        rg.image_urls = rp.image_urls.clone();
        // Merge the fragment's nested scroll-clip boxes so `sync_region_state`
        // pushes a fresh `clientHeight` for a scroller living INSIDE this region
        // (replace-or-insert by node; a full re-lay still rebuilds the set).
        for &(node, ch, cw) in &rp.scroll_clips {
            match g.doc.scroll_clips.iter_mut().find(|c| c.0 == node) {
                Some(c) => *c = (node, ch, cw),
                None => g.doc.scroll_clips.push((node, ch, cw)),
            }
        }
        // Re-anchor an in-region selection by target in the new windowed content.
        if let Some(t) = selected_target {
            g.sel_item = Self::find_item_like(&g.doc, &t).or(g.sel_item);
        }
        // New images the patch introduced (chat avatars): merge into the doc +
        // kick decodes (deduped; `start_image_loads` skips anything cached).
        let mut new_urls = Vec::new();
        for u in rp.image_urls {
            if !g.doc.image_urls.contains(&u) {
                g.doc.image_urls.push(u.clone());
                new_urls.push(u);
            }
        }
        if !new_urls.is_empty() {
            self.start_image_loads(url, new_urls);
        }
        true
    }

    /// The general inline arm of `patch_live_doc` (INCREMENTAL_LAYOUT_PLAN.md §14):
    /// re-lay a block-filling IFC boundary's subtree and splice its rows into
    /// `Doc.rows` at the cached box. The boundary fills its containing block, so
    /// its width is stable; only its HEIGHT can change. Tier 1 (height unchanged)
    /// splices in place — nothing outside moves. Tier 2 (height changed) splices
    /// then SHIFTS every following `Doc.rows`-anchored index by the row delta and
    /// scroll-anchors (CSS Scroll Anchoring) — no relayout of anything outside the
    /// box. Returns false (→ resync) when the cache is out of sync, the box grew a
    /// sub-frame (region/carousel), or the splice would fall outside the doc.
    fn patch_live_boundary(&mut self, patch: &crate::js::SubtreePatch) -> bool {
        let viewport = (
            (self.last_inner.0 as usize).max(10),
            self.last_inner.1.max(1) as usize,
        );
        let scroll_intent = self.scroll_intent;
        let url = match self.browser.as_ref().map(|g| g.doc.url.clone()) {
            Some(Link::Http(u)) => u,
            _ => return false,
        };
        // Look up the cached box for this boundary (by the actor node id baked as
        // `data-trust-node`). A miss = the cache is out of sync (a full render
        // hasn't captured it yet, or it was dropped) → resync.
        let (old_range, origin_col, content_width, old_width, sub_box) = {
            let Some(g) = self.browser.as_ref() else {
                return false;
            };
            let Some(b) = g.doc.boundaries.iter().find(|b| b.node == patch.node) else {
                return false;
            };
            (
                b.row_range.clone(),
                b.origin_col,
                b.content_width as usize,
                b.width,
                b.sub_box,
            )
        };
        let Some(laid) = http::lay_subtree_patch(
            &url,
            patch.html.as_bytes(),
            content_width,
            viewport,
            &self.image_sizes,
            patch.node,
            sub_box,
        ) else {
            return false;
        };
        // The box grew a scroll region / carousel since capture — it's no longer a
        // pure-`Doc.rows` inline boundary, so the full path must re-capture it.
        if laid.has_subframes {
            return false;
        }
        // Width verify (INCREMENTAL_LAYOUT_PLAN.md §14 step 4.3). A SUB-BOX (flex/
        // grid item, inline-block) is content-sized: if its width CHANGED it
        // reshaped its siblings, so the in-place row splice is no longer valid →
        // resync (strict). A block-filling box fills its band regardless of
        // content, so only an over-band content extent (a wide unbreakable token)
        // is a reshape; a narrower extent is fine.
        let width_ok = if sub_box {
            laid.width == old_width
        } else {
            laid.width as usize <= content_width
        };
        if !width_ok {
            return false;
        }
        let g = self.browser.as_mut().unwrap();
        // The cached range must still be valid against the current doc (a prior
        // patch in the same batch could have shifted it; the actor coalesces to
        // one patch per boundary, but guard anyway).
        if old_range.end > g.doc.rows.len() {
            return false;
        }
        let splice_at = old_range.start;
        let old_len = old_range.len();
        let new_len = laid.height;
        let delta = new_len as isize - old_len as isize;
        // Capture a selection sitting INSIDE the patched box, to re-anchor by
        // target after the splice. A selection outside keeps its item index and
        // only its row shifts (below).
        let selected_target = g
            .sel_item
            .filter(|&(r, _)| old_range.contains(&r))
            .and_then(|(r, i)| {
                crate::layout::effective_row(&g.doc.rows, &g.doc.regions, r)
                    .items
                    .get(i)
                    .cloned()
            });
        // CSS Scroll Anchoring L1 (specialized to the vertical-row model): when
        // the patched box SPANS the viewport top, capture the topmost interactive
        // item the reader actually sees as the scroll anchor — BEFORE the splice,
        // from the old rows. After the splice we re-find it and shift `scroll` by
        // how far it moved, so content appended BELOW the anchor (an infinite-
        // scroll grid loading more rows) leaves the viewport put, while content
        // inserted ABOVE it shifts to compensate. (A box wholly above the viewport
        // is handled by the simpler whole-delta shift below; one at/below the
        // viewport top never moves the reader.)
        let scroll_anchor: Option<(usize, crate::doc::Link)> =
            if delta != 0 && splice_at < g.scroll && g.scroll < old_range.end {
                (g.scroll..old_range.end).find_map(|r| {
                    crate::layout::effective_row(&g.doc.rows, &g.doc.regions, r)
                        .items
                        .iter()
                        .find_map(|it| it.link.clone().map(|l| (r, l)))
                })
            } else {
                None
            };
        // Shift the fragment's cols into the box's absolute band, then splice it
        // in place of the old rows (the Vec splice shifts following rows for us).
        let mut new_rows = laid.rows;
        if origin_col > 0 {
            for row in &mut new_rows {
                for it in &mut row.items {
                    it.col += origin_col;
                }
            }
        }
        g.doc.rows.splice(old_range.clone(), new_rows);
        // Tier 2: reposition every OTHER `Doc.rows`-anchored index. A box was
        // excluded at capture if it overlapped a region/carousel, so those sit
        // wholly before or wholly after the splice — shift the after ones. A
        // cached boundary INSIDE the patched box (nested) is invalidated: drop it
        // (re-captured on the next full render); the patched box itself is
        // repositioned to its new span.
        let after = old_range.end;
        let shift = |r: usize| -> usize {
            if r >= after {
                (r as isize + delta).max(0) as usize
            } else {
                r
            }
        };
        if delta != 0 {
            for rg in &mut g.doc.regions {
                rg.start_row = shift(rg.start_row);
            }
            for c in &mut g.doc.carousels {
                c.start = shift(c.start);
                c.end = shift(c.end);
            }
        }
        g.doc.boundaries.retain(|b| {
            b.node == patch.node || b.row_range.start < splice_at || b.row_range.start >= after
        });
        for b in &mut g.doc.boundaries {
            if b.node == patch.node {
                b.row_range = splice_at..splice_at + new_len;
            } else if delta != 0 {
                b.row_range = shift(b.row_range.start)..shift(b.row_range.end);
            }
        }
        // Selection: re-anchor an in-box selection by target; shift an after-box
        // one by delta; leave a before-box one. (The viewport itself is scroll-
        // anchored just below.)
        let height = viewport.1;
        if let Some(target) = selected_target {
            g.sel_item = Self::find_item_like(&g.doc, &target).or(g.sel_item);
        } else if delta != 0
            && let Some((r, i)) = g.sel_item
        {
            g.sel_item = Some((shift(r), i));
        }
        // How far the content at the viewport top moved (CSS Scroll Anchoring):
        //  - box wholly ABOVE the viewport ⇒ everything below it (the viewport
        //    included) moved by the full `delta`;
        //  - box SPANS the viewport top ⇒ measure the captured anchor's real
        //    movement (0 for an append below it, `delta`-ish for an insert above);
        //    a lost anchor falls back to 0 (never the runaway whole-`delta` drag);
        //  - box at/below the viewport top ⇒ the reader doesn't move.
        let shift_by: isize = if delta == 0 {
            0
        } else if old_range.end <= g.scroll {
            delta
        } else if let Some((old_r, link)) = &scroll_anchor {
            Self::find_row_by_link(&g.doc, link, *old_r)
                .map_or(0, |new_r| new_r as isize - *old_r as isize)
        } else {
            0
        };
        let max_scroll = g.doc.rows.len().saturating_sub(height);
        if shift_by != 0 {
            // Content moved ABOVE the viewport: shift `scroll` to keep the
            // reader's content visually fixed.
            g.scroll = ((g.scroll as isize + shift_by).max(0) as usize).min(max_scroll);
        } else {
            // The viewport-top content didn't move (an append/change BELOW the
            // viewport, or a transient virtualization shrink — archive.org
            // un-renders an off-screen section, shrinking the doc): restore toward
            // the reader's INTENT, clamped. So a section shrinking below the
            // viewport can't permanently yank the view up (the "snaps up a
            // section" bug) — it clamps for display, then the regrow restores
            // intent. Identical to an append below for the no-shrink case
            // (intent == the reader's row ⇒ no drag, preserving the tilvids fix).
            g.scroll = scroll_intent.min(max_scroll);
        }
        // New images the patch introduced: merge + kick decodes (deduped).
        let mut new_urls = Vec::new();
        for u in laid.image_urls {
            if !g.doc.image_urls.contains(&u) {
                g.doc.image_urls.push(u.clone());
                new_urls.push(u);
            }
        }
        // Keep the scroll intent tracking the same content: when the viewport was
        // scroll-anchored, shift the intent by the same amount. Like the full
        // path, the intent is NOT clamped here — it's the value a later re-render
        // restores toward, so a transient shrink can't lose it.
        if shift_by != 0 {
            self.scroll_intent = (self.scroll_intent as isize + shift_by).max(0) as usize;
        }
        if !new_urls.is_empty() {
            self.start_image_loads(url, new_urls);
        }
        true
    }

    /// Ask the resident page actor to re-emit the whole document (a full
    /// `Updated`) because an incremental patch couldn't be applied
    /// (INCREMENTAL_LAYOUT_PLAN.md §7). Unreachable when the predicate is correct.
    fn request_resync(&mut self) {
        if let Some(handle) = self.live_page.as_ref() {
            let _ = handle.cmds.try_send(crate::js::PageCmd::Resync);
        }
    }

    /// Row of the item following `link` that is CLOSEST to `near` (the anchor's
    /// old row) — the scroll-anchor re-find that keeps the content the reader sees
    /// fixed across a re-layout. Matched by LINK (a fresh re-parse reassigns
    /// layout node ids, so a node match would catch a renumbered DIFFERENT
    /// element) and disambiguated by PROXIMITY, because a link can REPEAT in the
    /// document — archive.org lists the same collection in a featured row AND the
    /// main grid, so taking the first occurrence pops the viewport to an
    /// unrelated copy (the "snaps up a section, never reaches the bottom" bug).
    fn find_row_by_link(doc: &Doc, link: &crate::doc::Link, near: usize) -> Option<usize> {
        let mut best: Option<usize> = None;
        for r in 0..doc.rows.len() {
            let here = crate::layout::effective_row(&doc.rows, &doc.regions, r)
                .items
                .iter()
                .any(|it| it.link.as_ref() == Some(link));
            if here && best.is_none_or(|b| r.abs_diff(near) < b.abs_diff(near)) {
                best = Some(r);
            }
        }
        best
    }

    /// Find the `(row, item)` matching `target` in fresh rows: same arena
    /// node wins (a control that moved), else same link target.
    fn find_item_like(doc: &Doc, target: &crate::layout::Item) -> Option<(usize, usize)> {
        // Search the EFFECTIVE rows (region buffer windows merged in) so a
        // selection on scroll-region content re-anchors after a re-layout — the
        // returned `(r, i)` indexes the effective row, consistent with how the
        // selection is later read (`selected_link`/render through `effective_row`).
        for r in 0..doc.rows.len() {
            let row = crate::layout::effective_row(&doc.rows, &doc.regions, r);
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

    /// Carry scroll-region offsets across a re-layout: match each new region to
    /// an old one by node id and restore its `voffset` (clamped to the new
    /// content). So a re-parse (the chat's per-message update) or a re-wrap
    /// (resize) doesn't reset the reader's scroll position — the region-level
    /// analogue of `find_item_like`'s selection stability.
    ///
    /// `prefer_signal` distinguishes the two re-layout kinds (Phase 3):
    /// - A live re-render (`replace_live_doc`) re-bakes a FRESH
    ///   `data-trust-scroll-top` signal, so a region whose `voffset_from_page` is
    ///   set keeps the page's signalled position (a chat re-pinning to bottom);
    ///   only an un-signalled region restores the user's wheel offset.
    /// - A re-wrap of the SAME HTML (resize / image relayout) carries a STALE
    ///   signal (the page may have scrolled since), so the on-screen offset (the
    ///   old `voffset`) always wins — the user's position is preserved.
    ///
    /// NO heuristic about where a GROWING region should scroll — whether new
    /// content pins to the bottom (a chat) or stays put (a log you're reading) is
    /// the PAGE's call, via its own `element.scrollTop` writes; we never guess it.
    fn carry_region_offsets(
        old: &[(crate::dom::NodeId, usize)],
        regions: &mut [crate::layout::Region],
        prefer_signal: bool,
    ) {
        for rg in regions.iter_mut() {
            // Keep the page's fresh signal on a live re-render; else restore the
            // old offset by node (regions are keyed by their stable element id).
            // The PRINCIPAL region is exempt from the signal override: the READER
            // scrolls it as "the page", not the page, so its user offset is
            // always restored — a lagging `data-trust-scroll-top` signal must
            // never snap the main content back to the top on a live re-render
            // (the "kicked back up" bug). Same guarantee `g.scroll` already has.
            if rg.node != crate::layout::NO_NODE
                && !(prefer_signal && rg.voffset_from_page && !rg.principal)
                && let Some(&(_, voff)) = old.iter().find(|&&(n, _)| n == rg.node)
            {
                rg.voffset = voff.min(rg.max_voffset());
            }
            // Nested scrollers persist too — a scroll region inside another.
            Self::carry_region_offsets(old, &mut rg.regions, prefer_signal);
        }
    }

    /// Flatten every region's `(node, voffset)` — nested scrollers included —
    /// so the restore above re-seats them all after a full re-layout.
    fn collect_region_offsets(
        regions: &[crate::layout::Region],
        out: &mut Vec<(crate::dom::NodeId, usize)>,
    ) {
        for rg in regions {
            if rg.node != crate::layout::NO_NODE {
                out.push((rg.node, rg.voffset));
            }
            Self::collect_region_offsets(&rg.regions, out);
        }
    }

    /// Show a fetched document, pushing the current one onto the back
    /// history (RAM-only, dropped when the view closes). A pending deep
    /// back/forward (`pending_travel`) completes its trail shuffle here
    /// instead; a pending reload (`replace_nav`) swaps in place.
    fn navigate_to(&mut self, doc: Doc) {
        // A new page replaces any image that was being viewed, and ends
        // whatever living page came before it (freeze: its last render
        // is already the doc going into history).
        self.viewer = None;
        // An open <select> dropdown is anchored to the OLD doc's field
        // indices; a live `Navigate` can land here while it's up.
        self.select_menu = None;
        // The old page's image batches die with it (the network work, not
        // just the delivery), and its failure memory is dropped — so a
        // reload retries images that failed last time.
        self.abort_image_loads();
        self.failed_images.clear();
        // Capture before dropping: a doc going into history that had a
        // living engine should be revived (not restored static) on back.
        let was_live = self.live_page.is_some();
        self.drop_live_page();
        let travel = std::mem::take(&mut self.pending_travel);
        let replace = std::mem::take(&mut self.replace_nav);
        let from_post = std::mem::take(&mut self.nav_from_post);
        let height = self.last_inner.1.max(1) as usize;
        match &mut self.browser {
            // A deep back/forward refetch landing: complete the travel —
            // pop the (evicted) entry this doc was fetched for, park the
            // doc we're leaving on the opposite stack. Falls through to a
            // plain push if the trail changed underneath the fetch (every
            // trail mutation clears `pending_travel`, so it shouldn't).
            Some(g)
                if travel
                    .is_some_and(|fwd| !if fwd { &g.forward } else { &g.history }.is_empty()) =>
            {
                let fwd = travel == Some(true);
                let entry = if fwd {
                    g.forward.pop()
                } else {
                    g.history.pop()
                }
                .unwrap();
                let old = std::mem::replace(&mut g.doc, doc);
                let parked = HistEntry {
                    url: old.url.clone(),
                    pos: ViewPos {
                        selected: g.selected,
                        sel_item: g.sel_item,
                        was_live,
                    },
                    scroll: g.scroll,
                    post: self.current_from_post,
                    doc: Some(old),
                };
                if fwd {
                    g.history.push(parked);
                } else {
                    g.forward.push(parked);
                }
                // The content is fresh, so saved selection indices are stale
                // (same rule as revive): restore the scroll clamped to the
                // new extent and let the tail re-pick a visible selection.
                g.selected = None;
                g.sel_item = None;
                g.sel_fixed = None;
                g.scroll = entry.scroll.min(g.doc.extent().saturating_sub(height));
            }
            Some(g) if replace => {
                // Reload: swap the document in place, keep history and
                // ride the scroll — clamped to the fresh content's MAX
                // scroll (extent − viewport), not its last row, so a
                // shorter reload can't park the view on a lone top row
                // with blank space below.
                g.doc = doc;
                g.selected = None;
                g.sel_item = None;
                g.sel_fixed = None;
                g.scroll = g.scroll.min(g.doc.extent().saturating_sub(height));
            }
            Some(g) => {
                let old = std::mem::replace(&mut g.doc, doc);
                let entry = HistEntry {
                    url: old.url.clone(),
                    pos: ViewPos {
                        selected: g.selected,
                        sel_item: g.sel_item,
                        was_live,
                    },
                    scroll: g.scroll,
                    post: self.current_from_post,
                    doc: Some(old),
                };
                g.history.push(entry);
                // A new navigation abandons the forward branch (browser
                // model); only back/forward themselves preserve it.
                g.forward.clear();
                g.selected = None;
                g.sel_item = None;
                // A stale pinned-rail selection must not outlive its doc:
                // `selected_link` consults it FIRST, so a leftover index makes
                // Enter dead (or follows the wrong rail link on a page that
                // also has fixed layers).
                g.sel_fixed = None;
                g.scroll = 0;
            }
            None => {
                self.browser = Some(BrowserView {
                    doc,
                    selected: None,
                    sel_item: None,
                    sel_fixed: None,
                    scroll: 0,
                    history: Vec::new(),
                    forward: Vec::new(),
                });
            }
        }
        // The shown doc's POST-ness (for the entry it'll be parked into
        // later); a travel refetch is a GET, so `from_post` is false there.
        self.current_from_post = from_post;
        // Strict memory: evict docs that just became non-adjacent, then
        // drop decoded images nothing retained references anymore.
        if let Some(g) = &mut self.browser {
            Self::enforce_retention(g);
        }
        self.sweep_image_caches();
        // A fresh page selects its first interactive target.
        if let Some(g) = &mut self.browser {
            if g.doc.laid_out() {
                g.sel_item = Self::http_first_visible_item(g, height);
            } else {
                g.selected = Self::browser_visible_links(g, height).first().copied();
            }
        }
        // A navigation resets the scroll intent to the new page's position
        // (0, or the clamped reload scroll). The JS-driven navigate path runs
        // inside a live-page event, where the run-loop tail skips the sync, so
        // set it here too rather than relying on the tail.
        self.scroll_intent = self.browser.as_ref().map_or(0, |g| g.scroll);
    }

    /// gopherus keys: Up/Down scroll the page (the highlight rides the
    /// visible links), Right follows, Left goes back, Alt-Right goes
    /// forward, Esc closes.
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
            // Alt-Right = forward (the traditional browser pair; plain Left
            // is already back here, so Alt-Left needs no arm of its own).
            KeyCode::Right if key.modifiers.contains(KeyModifiers::ALT) => self.browser_forward(),
            KeyCode::Right | KeyCode::Enter => self.browser_follow(),
            KeyCode::Left => self.browser_back(),
            KeyCode::Char('v' | 'V') => self.open_in_mpv(),
            KeyCode::Char('y' | 'Y') => self.yank_selected_url(),
            KeyCode::Esc => self.stop_loading(),
            _ => {}
        }
    }

    fn http_mouse_hover(&mut self, col: u16, row: u16) -> bool {
        // ONE walk finds the selection target (fixed layer first — it draws
        // on top — then the doc) AND the live hover target; scanning twice
        // per mouse-move was the old cost.
        let hit = self.pointer_hit(col, row);
        let in_area = self.mouse_in_content_area(col, row);
        let laid_out = self.browser.as_ref().is_some_and(|g| g.doc.laid_out());
        if let Some(g) = &mut self.browser {
            if let Some(target) = hit.fixed {
                g.sel_fixed = Some(target);
                g.sel_item = None;
            } else {
                g.sel_fixed = None;
                if let Some(target) = hit.item {
                    g.sel_item = Some(target);
                } else if in_area && laid_out {
                    g.sel_item = None;
                }
            }
        }
        // The pointer moved: feed the live page's hover pipeline (independent
        // of the SELECTION hit — a hover-only div is not interactive but is a
        // hover target; unmarked cells clear).
        let (x, y) = self.viewport_px_of(col, row);
        self.request_page_hover(hit.hover, x, y);
        hit.fixed.is_some() || hit.item.is_some()
    }

    /// Hit-test the PINNED fixed layer at screen `(col, row)`: returns the
    /// `(fixed, row, item)` index of an INTERACTIVE item under the cursor, or
    /// `None`. Fixed items pin to the viewport, so they map by SCREEN position
    /// (content-area origin + the item's `col`/`row`), NOT the scroll offset.
    /// Later items draw on top, so scan in reverse.
    fn fixed_hit_test(&self, col: u16, row: u16) -> Option<(usize, usize, usize)> {
        if !self.mouse_in_content_area(col, row) {
            return None;
        }
        let g = self.browser.as_ref()?;
        let (ox, oy) = (self.last_content_area.x, self.last_content_area.y);
        for (fi, f) in g.doc.fixed.iter().enumerate().rev() {
            for (r, frow) in f.rows.iter().enumerate() {
                if oy as usize + f.row as usize + r != row as usize {
                    continue;
                }
                for (i, item) in frow.items.iter().enumerate() {
                    let start = ox as usize + f.col as usize + item.col as usize;
                    let end = start + item.width.max(1) as usize;
                    if item.is_interactive() && (col as usize) >= start && (col as usize) < end {
                        return Some((fi, r, i));
                    }
                }
            }
        }
        None
    }

    fn mouse_in_content_area(&self, col: u16, row: u16) -> bool {
        let a = self.last_content_area;
        col >= a.x
            && col < a.x.saturating_add(a.width)
            && row >= a.y
            && row < a.y.saturating_add(a.height)
    }

    /// HTTP 2D navigation: Enter follows, Backspace goes back (Alt-Left/
    /// Alt-Right are history back/forward), Up/Down move the selection to
    /// the nearest interactive item in an adjacent row, Left/Right step
    /// between items (spilling to adjacent rows), Esc closes. The arrows
    /// are free here because nav lives on Enter/Backspace — the HTTP-only
    /// layout model.
    fn http_nav(&mut self, key: KeyEvent) {
        self.notice = false;
        let page = i64::from(self.last_inner.1.max(2)) - 1;
        match key.code {
            KeyCode::Up => self.http_move(-1, false),
            KeyCode::Down => self.http_move(1, false),
            // Alt-Left/Alt-Right = history back/forward (the traditional
            // browser pair), checked before the plain-arrow selection moves.
            KeyCode::Left if key.modifiers.contains(KeyModifiers::ALT) => self.browser_back(),
            KeyCode::Right if key.modifiers.contains(KeyModifiers::ALT) => self.browser_forward(),
            // In a carousel, ←/→ scroll the strip a card at a time; elsewhere
            // they move the selection laterally.
            KeyCode::Left if self.scroll_selected_carousel(-1) => {}
            KeyCode::Right if self.scroll_selected_carousel(1) => {}
            KeyCode::Left => self.http_move(-1, true),
            KeyCode::Right => self.http_move(1, true),
            // PgUp/PgDn scroll the region under the cursor, if any (chaining to
            // the document at its boundary); otherwise they page the document.
            KeyCode::PageUp if self.region_page_scroll(-1) => {}
            KeyCode::PageDown if self.region_page_scroll(1) => {}
            KeyCode::PageUp => self.http_scroll(-page),
            KeyCode::PageDown => self.http_scroll(page),
            KeyCode::Home => self.http_scroll(i64::MIN / 2),
            KeyCode::End => self.http_scroll(i64::MAX / 2),
            KeyCode::Enter => self.browser_follow(),
            KeyCode::Backspace => self.browser_back(),
            KeyCode::Char('v' | 'V') => self.open_in_mpv(),
            KeyCode::Char('y' | 'Y') => self.yank_selected_url(),
            KeyCode::Esc => self.stop_loading(),
            _ => {}
        }
    }

    /// If the selection sits in a horizontal carousel, scroll it one card
    /// (`dir` ±1) and re-anchor the selection to the first card now visible.
    /// Returns whether a carousel handled the key.
    fn scroll_selected_carousel(&mut self, dir: i32) -> bool {
        // The strip to scroll: the one holding the current selection, else the
        // one under the mouse cursor (a seamless hover-scroll, like the region
        // wheel — no injected buttons required; the page defines itself).
        let Some(idx) = self.carousel_to_scroll() else {
            return false;
        };
        let Some(g) = self.browser.as_mut() else {
            return false;
        };
        g.doc.carousels[idx].scroll_cards(dir);
        // Re-anchor onto the first interactive card now in the band, so the
        // highlight stays visible and Enter follows something on screen.
        let c = g.doc.carousels[idx].clone();
        let target = (c.start..c.end).find_map(|r| {
            g.doc
                .rows
                .get(r)?
                .items
                .iter()
                .enumerate()
                .find_map(|(i, it)| {
                    (it.link.is_some() && it.col >= c.left && c.shows(it.col, it.width))
                        .then_some((r, i))
                })
        });
        if let Some(sel) = target {
            g.sel_item = Some(sel);
        }
        true
    }

    /// The carousel `←/→` should scroll: the one holding the current selection,
    /// else the one under the mouse cursor (hover-scroll).
    fn carousel_to_scroll(&self) -> Option<usize> {
        let g = self.browser.as_ref()?;
        if let Some((row, _)) = g.sel_item
            && let Some(idx) = g.doc.carousels.iter().position(|c| c.contains_row(row))
        {
            return Some(idx);
        }
        let (col, row) = self.last_mouse?;
        if !self.mouse_in_content_area(col, row) || !g.doc.laid_out() {
            return None;
        }
        let doc_row = g.scroll + row.saturating_sub(self.last_content_area.y) as usize;
        let local_col = col.saturating_sub(self.last_content_area.x);
        g.doc
            .carousels
            .iter()
            .position(|c| c.contains_row(doc_row) && local_col >= c.left && local_col < c.right)
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

    /// The first interactive item in the viewport, `(row, item)`. Reads
    /// through `effective_row` so scroll-region content (merged into its
    /// band) is selectable by keyboard exactly as it is by mouse.
    fn http_first_visible_item(g: &BrowserView, height: usize) -> Option<(usize, usize)> {
        let end = (g.scroll + height).min(g.doc.rows.len());
        (g.scroll..end).find_map(|r| {
            Self::row_interactives(&crate::layout::effective_row(
                &g.doc.rows,
                &g.doc.regions,
                r,
            ))
            .first()
            .map(|&i| (r, i))
        })
    }

    /// Move the item selection. `horizontal` steps within/between rows in
    /// document order; otherwise it jumps to the column-nearest item in an
    /// adjacent row. The page scrolls to keep the new selection visible.
    /// Every INTERACTIVE item in the pinned fixed layer, as `(fixed, row, item)`
    /// addresses in reading order — the keyboard-navigable rail links.
    fn fixed_interactives(g: &BrowserView) -> Vec<(usize, usize, usize)> {
        let mut out = Vec::new();
        for (fi, f) in g.doc.fixed.iter().enumerate() {
            for (r, row) in f.rows.iter().enumerate() {
                for (i, it) in row.items.iter().enumerate() {
                    if it.is_interactive() {
                        out.push((fi, r, i));
                    }
                }
            }
        }
        out
    }

    fn http_move(&mut self, dir: i64, horizontal: bool) {
        self.http_move_inner(dir, horizontal);
        // The selection is the terminal's pointer (her call): arrowing onto an
        // element hovers it, so hover-driven UI (Steam's preview pane) works
        // from the keyboard. Dwell-gated like the mouse path — key repeat
        // commits only the target the selection rests on.
        self.sync_page_hover_selection();
    }

    fn http_move_inner(&mut self, dir: i64, horizontal: bool) {
        let height = self.last_inner.1.max(1) as usize;
        let Some(g) = &mut self.browser else { return };
        // The pinned rail links are keyboard-navigable like any other link — no
        // special-casing beyond WHERE they draw. The selection cycles: document
        // items → (boundary) → rail links → (boundary) → document items.
        let fixed = Self::fixed_interactives(g);

        // Selection is in a pinned rail: step through the rail links; past the
        // end, drop back into the document.
        if let Some(cur) = g.sel_fixed {
            let next = fixed
                .iter()
                .position(|&t| t == cur)
                .map(|p| p as i64 + dir)
                .filter(|&n| n >= 0 && (n as usize) < fixed.len());
            match next {
                Some(n) => g.sel_fixed = Some(fixed[n as usize]),
                None => {
                    g.sel_fixed = None;
                    g.sel_item = Self::http_first_visible_item(g, height);
                }
            }
            self.http_keep_visible();
            return;
        }

        // No selection yet: the first document item, else the first rail link.
        let Some((cr, ci)) = g.sel_item else {
            g.sel_item = Self::http_first_visible_item(g, height);
            if g.sel_item.is_none() && !fixed.is_empty() {
                g.sel_fixed = Some(fixed[0]);
            }
            self.http_keep_visible();
            return;
        };

        // A selection on scroll-region content steps through the region's
        // WHOLE buffer vertically (auto-scrolling its window), not just the
        // rows it happens to show; only when the buffer is exhausted does
        // the step fall out into the surrounding document.
        if !horizontal {
            let origin = crate::layout::item_origin(&g.doc.rows, &g.doc.regions, cr, ci);
            if let crate::layout::ItemOrigin::Region {
                region,
                brow,
                bitem,
            } = origin
                && self.region_step_vertical(region, brow, bitem, dir)
            {
                return;
            }
        }

        let Some(g) = &mut self.browser else { return };
        let target = if horizontal {
            Self::http_step_horizontal(&g.doc.rows, &g.doc.regions, cr, ci, dir)
        } else {
            Self::http_step_vertical(&g.doc.rows, &g.doc.regions, cr, ci, dir)
        };
        if let Some(next) = target {
            g.sel_item = Some(next);
        } else if !fixed.is_empty() {
            // No further document item this way → enter the pinned rails.
            g.sel_item = None;
            g.sel_fixed = Some(if dir > 0 {
                fixed[0]
            } else {
                fixed[fixed.len() - 1]
            });
        }
        self.http_keep_visible();
    }

    /// Vertical keyboard step within a scroll region's buffer: find the
    /// column-nearest interactive on the nearest buffer row in `dir`, shift
    /// the window minimally to reveal it (a wheel-like user scroll, written
    /// back to the live element), and select it at its band position.
    /// Returns false when no further interactive exists in the buffer that
    /// way — the caller steps out into the document.
    fn region_step_vertical(&mut self, region: usize, brow: usize, bitem: usize, dir: i64) -> bool {
        let Some(g) = &mut self.browser else {
            return false;
        };
        let Some(rg) = g.doc.regions.get_mut(region) else {
            return false;
        };
        let cur = rg.buffer.get(brow).and_then(|r| r.items.get(bitem));
        let cur_col = cur.map_or(0, |it| it.col);
        let cur_node = cur.map_or(crate::layout::NO_NODE, |it| it.node);
        let candidates: Box<dyn Iterator<Item = usize>> = if dir > 0 {
            Box::new((brow + 1)..rg.buffer.len())
        } else {
            Box::new((0..brow).rev())
        };
        let mut hit = None;
        for br in candidates {
            // Only items the merge will show (inside the scrollport's right
            // edge) — a clipped item has no merged index to select.
            let best = rg.buffer[br]
                .items
                .iter()
                .enumerate()
                .filter(|(_, it)| {
                    it.link.is_some()
                        && it.col < rg.width
                        && (cur_node == crate::layout::NO_NODE || it.node != cur_node)
                })
                .min_by_key(|(_, it)| (i32::from(it.col) - i32::from(cur_col)).abs())
                .map(|(bi, _)| bi);
            if let Some(bi) = best {
                hit = Some((br, bi));
                break;
            }
        }
        let Some((br, bi)) = hit else { return false };
        // Minimal window shift to reveal the target row.
        let rh = (rg.height as usize).max(1);
        if br < rg.voffset {
            rg.voffset = br;
        } else if br >= rg.voffset + rh {
            rg.voffset = br + 1 - rh;
        }
        rg.voffset_from_page = false;
        let band = rg.start_row + (br - rg.voffset);
        let writeback = (rg.live_node, rg.voffset);
        // The target's merged index in the (freshly scrolled) effective row.
        let row = crate::layout::effective_row(&g.doc.rows, &g.doc.regions, band);
        let idx = (0..row.items.len()).find(|&i| {
            matches!(
                crate::layout::item_origin(&g.doc.rows, &g.doc.regions, band, i),
                crate::layout::ItemOrigin::Region { region: r2, brow: b2, bitem: i2 }
                    if r2 == region && b2 == br && i2 == bi
            )
        });
        let Some(idx) = idx else { return false };
        g.sel_item = Some((band, idx));
        g.sel_fixed = None;
        self.http_keep_visible();
        self.region_writeback(writeback.0, writeback.1);
        true
    }

    /// Next interactive item in document order from `(cr, ci)`, scanning
    /// items on the current row first, then spilling into later/earlier
    /// rows. Rows are read through `effective_row`, so a scroll region's
    /// visible window is stepped through like any other content.
    fn http_step_horizontal(
        rows: &[crate::layout::Row],
        regions: &[crate::layout::Region],
        cr: usize,
        ci: usize,
        dir: i64,
    ) -> Option<(usize, usize)> {
        let eff = |r: usize| crate::layout::effective_row(rows, regions, r);
        let here_row = eff(cr);
        let cur_node = here_row
            .items
            .get(ci)
            .map_or(crate::layout::NO_NODE, |it| it.node);
        let here = Self::row_interactives_excluding(&here_row, cur_node);
        if dir > 0 {
            if let Some(&i) = here.iter().find(|&&i| i > ci) {
                return Some((cr, i));
            }
            ((cr + 1)..rows.len()).find_map(|r| {
                Self::row_interactives_excluding(&eff(r), cur_node)
                    .first()
                    .map(|&i| (r, i))
            })
        } else {
            if let Some(&i) = here.iter().rev().find(|&&i| i < ci) {
                return Some((cr, i));
            }
            (0..cr).rev().find_map(|r| {
                Self::row_interactives_excluding(&eff(r), cur_node)
                    .last()
                    .map(|&i| (r, i))
            })
        }
    }

    /// The interactive item in the next row (in `dir`) whose column is
    /// nearest the current item's column. Rows are read through
    /// `effective_row` (scroll-region windows included).
    fn http_step_vertical(
        rows: &[crate::layout::Row],
        regions: &[crate::layout::Region],
        cr: usize,
        ci: usize,
        dir: i64,
    ) -> Option<(usize, usize)> {
        let eff = |r: usize| crate::layout::effective_row(rows, regions, r);
        let here_row = eff(cr);
        let cur_col = here_row.items.get(ci).map_or(0, |it| it.col);
        let cur_node = here_row
            .items
            .get(ci)
            .map_or(crate::layout::NO_NODE, |it| it.node);
        let candidates: Box<dyn Iterator<Item = usize>> = if dir > 0 {
            Box::new((cr + 1)..rows.len())
        } else {
            Box::new((0..cr).rev())
        };
        for r in candidates {
            let row = eff(r);
            let inter = Self::row_interactives_excluding(&row, cur_node);
            if let Some(&best) = inter
                .iter()
                .min_by_key(|&&i| (i32::from(row.items[i].col) - i32::from(cur_col)).abs())
            {
                return Some((r, best));
            }
        }
        None
    }

    /// Scroll the HTTP viewport by `delta` rows, dropping any selection
    /// that scrolls out of view onto the nearest visible interactive item.
    fn http_scroll(&mut self, delta: i64) {
        // A locked-viewport page scrolls its principal region as "the page"
        // (PgUp/PgDn off a region, Home/End); its `rows` hold no scroll content.
        if self.scroll_principal_region(delta) {
            return;
        }
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

    /// Route a wheel scroll. A wheel with the cursor over a scroll region
    /// scrolls ONLY that region — and TRAPS the scroll there: even at the
    /// region's boundary the wheel never leaks to the page (her call — a region
    /// must never scroll the whole canvas while the cursor is inside it; this is
    /// `overscroll-behavior: contain`, NOT the chaining `auto`). Off any region,
    /// the document scrolls as before. `delta` rows, positive = down.
    fn browser_wheel(&mut self, delta: i64, col: u16, row: u16) {
        if let Some(path) = self.region_path_under(col, row) {
            // A user wheel overrides the page's pin: write the new scrollTop back
            // so a conditional chat learns we scrolled and stops following.
            if let Some((node, voff)) = self.scroll_region_path(&path, delta) {
                self.region_writeback(node, voff);
            }
            return; // trapped in the (deepest) region — never chains to the doc
        }
        // Off any region: a locked-viewport page scrolls its principal region as
        // "the page" (its content isn't in `rows`); else the document scrolls.
        if self.scroll_principal_region(delta) {
            return;
        }
        self.browser_scroll(delta, true);
    }

    /// Scroll the region addressed by `path` (a chain of nested-region indices,
    /// outermost first) by `delta`; returns the deepest region's live node +
    /// new offset when it moved (for the scrollTop write-back). This is what
    /// makes a scroll container NESTED inside another independently scrollable.
    fn scroll_region_path(&mut self, path: &[usize], delta: i64) -> Option<(Option<usize>, usize)> {
        let g = self.browser.as_mut()?;
        let (&last, parents) = path.split_last()?;
        let mut regions = &mut g.doc.regions;
        for &idx in parents {
            regions = &mut regions.get_mut(idx)?.regions;
        }
        let rg = regions.get_mut(last)?;
        let moved = rg.scroll_by(delta);
        if moved {
            rg.voffset_from_page = false;
        }
        moved.then_some((rg.live_node, rg.voffset))
    }

    /// Scroll the page's PRINCIPAL scroll region (a locked-viewport app shell —
    /// Twitch and every SPA — keeps its whole content in a region, not in
    /// `rows`) by `delta` rows, AS "the page": write the new scrollTop back to
    /// the live engine and mark it user-driven so `carry_region_offsets` keeps
    /// this position across the re-render. Returns whether a principal region
    /// existed — when it does, the page-level gesture (wheel off a nested
    /// region, PgUp/PgDn, Home/End) is consumed by it (there is nothing else to
    /// scroll), even at a boundary.
    fn scroll_principal_region(&mut self, delta: i64) -> bool {
        let Some(g) = self.browser.as_mut() else {
            return false;
        };
        let Some(rg) = g.doc.principal_region_mut() else {
            return false;
        };
        let wb = rg.scroll_by(delta).then(|| {
            rg.voffset_from_page = false;
            (rg.live_node, rg.voffset)
        });
        if let Some((node, voff)) = wb {
            self.region_writeback(node, voff);
        }
        true
    }

    /// The DEEPEST scroll region under the cursor, as a path of nested-region
    /// indices (outermost first) — a wheel/key routes here, so the innermost
    /// scroller wins (CSS: the event targets the deepest scroll container).
    fn region_path_under(&self, col: u16, row: u16) -> Option<Vec<usize>> {
        if !self.mouse_in_content_area(col, row) {
            return None;
        }
        let g = self.browser.as_ref()?;
        if !g.doc.laid_out() {
            return None;
        }
        let mut r = g.scroll + row.saturating_sub(self.last_content_area.y) as usize;
        let mut c = col.saturating_sub(self.last_content_area.x);
        let mut regions: &[crate::layout::Region] = &g.doc.regions;
        let mut path = Vec::new();
        loop {
            let idx = regions
                .iter()
                .position(|rg| rg.contains_row(r) && rg.contains_col(c))?;
            path.push(idx);
            let rg = &regions[idx];
            // Descend into this region's buffer coordinates.
            let buf_row = rg.voffset + (r - rg.start_row);
            let buf_col = c.saturating_sub(rg.left);
            if rg
                .regions
                .iter()
                .any(|nr| nr.contains_row(buf_row) && nr.contains_col(buf_col))
            {
                regions = &rg.regions;
                r = buf_row;
                c = buf_col;
            } else {
                return Some(path);
            }
        }
    }

    /// The region addressed by `path` (read-only) — for reading the deepest
    /// region's page size.
    fn region_at_path(&self, path: &[usize]) -> Option<&crate::layout::Region> {
        let g = self.browser.as_ref()?;
        let mut rg = g.doc.regions.get(*path.first()?)?;
        for &idx in &path[1..] {
            rg = rg.regions.get(idx)?;
        }
        Some(rg)
    }

    /// The index of the scroll region whose scrollport is under the cursor
    /// `(col, row)` (absolute terminal cells), if any.
    /// PgUp/PgDn over a scroll region scrolls THAT region by a page (its own
    /// height), using the last hovered cell ("scroll the hovered region") — the
    /// deepest one when scrollers are nested. Returns whether the cursor was
    /// over a region (which then TRAPS the page key — never pages the document
    /// — to match the wheel); otherwise `false` and the document pages as usual.
    fn region_page_scroll(&mut self, dir: i64) -> bool {
        let Some((col, row)) = self.last_mouse else {
            return false;
        };
        let Some(path) = self.region_path_under(col, row) else {
            return false;
        };
        // A page is the scrollport height less one row of overlap (mirrors the
        // document's `last_inner.1 - 1` page step).
        let page = self
            .region_at_path(&path)
            .map_or(1, |rg| (i64::from(rg.height) - 1).max(1));
        if let Some((node, voff)) = self.scroll_region_path(&path, dir * page) {
            self.region_writeback(node, voff);
        }
        true // over a region ⇒ trap the key (don't fall through to the document)
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
        // For laid-out docs, carry the selection over by its item identity, and
        // the scroll-region offsets so a resize keeps each region's scroll spot.
        let item_target = g.sel_item.and_then(|(r, i)| {
            crate::layout::effective_row(&g.doc.rows, &g.doc.regions, r)
                .items
                .get(i)
                .cloned()
        });
        let mut old_offsets = Vec::new();
        Self::collect_region_offsets(&g.doc.regions, &mut old_offsets);
        let raw = std::mem::take(&mut g.doc.raw);
        let blobs = g.doc.blobs.take();
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
                http::parse_seeded(
                    &url,
                    &meta,
                    &raw,
                    width,
                    height,
                    Some(&forms),
                    &self.image_sizes,
                    &self.image_alpha,
                )
            }
            Link::OneShot(url) => oneshot::parse(&url, raw, width),
            // Internal pages re-wrap from their raw gemtext source.
            Link::External(s) if s.starts_with("about:") => {
                let body = String::from_utf8_lossy(&raw).into_owned();
                Self::about_doc(s, body, width)
            }
            Link::Form { .. } | Link::JsClick { .. } | Link::External(_) => return,
            Link::CarouselScroll(_) | Link::Media(_) => return,
        };
        // Same page, same blob mirror (a re-wrap must not orphan blob: images).
        g.doc.blobs = blobs;
        if g.doc.laid_out() {
            // Re-flow at the new width re-laid the rows; re-anchor the
            // selected item by identity and restore region scroll positions. A
            // resize re-wraps the SAME HTML, so the baked scroll signal is stale —
            // the on-screen offset wins (prefer_signal = false).
            Self::carry_region_offsets(&old_offsets, &mut g.doc.regions, false);
            g.sel_item = item_target.and_then(|t| Self::find_item_like(&g.doc, &t));
            let max_scroll = g.doc.rows.len().saturating_sub(height);
            g.scroll = match g.sel_item {
                Some((r, _)) => r.saturating_sub(height / 2).min(max_scroll),
                None => g.scroll.min(max_scroll),
            };
            // The re-wrap rebuilt every region from `doc.raw` — the last
            // FULL render, stale by the content patched into live regions
            // since (a chat's recent messages reverted on resize without
            // this). Restore each from its retained patch HTML, at the new
            // width.
            self.refresh_live_regions();
            return;
        }
        g.selected = link_ordinal.and_then(|n| {
            g.doc
                .lines
                .iter()
                .enumerate()
                .filter(|(_, l)| l.link.is_some())
                .map(|(i, _)| i)
                // `n == 0` (the invariant says `selected` is always a link
                // line, but don't underflow on it) → no selection.
                .nth(n.checked_sub(1)?)
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
        // Auto-route recognized video links straight to mpv, in EVERY view:
        // YouTube in its various formats (people post these on gopher and
        // gemini too — following one should play it, not try to render
        // YouTube), and direct links to media mpv can play (a video/audio
        // file, or the source behind a `<video>`/`<audio>` representation).
        // Manual `v` covers any other web link. One resolve serves both
        // checks (`selected_web_url` walks the doc and allocates).
        if let Some(url) = self.selected_web_url()
            && (is_youtube_video_url(&url) || is_playable_media_url(&url))
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
                    self.masked_input = false;
                    self.mode = Mode::Search;
                    self.input.clear();
                    self.cursor = 0;
                    self.select_anchor = None;
                }
                other => self.status = format!("item type '{other}' not supported yet"),
            },
            Link::Gemini(url) => self.start_fetch(Link::Gemini(url)),
            Link::Http(url) => {
                // A same-document `#fragment` link scrolls to the anchor instead
                // of re-fetching (HTML "navigate to a fragment"). A fragment link
                // to a DIFFERENT page falls through and fetches; its fragment is
                // applied after that page renders (see `start_fetch`).
                if url.fragment().is_some()
                    && let Some(Link::Http(cur)) = self.browser.as_ref().map(|g| &g.doc.url)
                    && same_document(&url, cur)
                {
                    let frag = url.fragment().unwrap_or("").to_string();
                    if !self.scroll_to_fragment(&frag) {
                        self.status = format!("no such anchor: #{frag}");
                    }
                    return;
                }
                let referrer = self.http_referrer();
                self.start_fetch_opts(Link::Http(url), false, referrer);
            }
            Link::OneShot(url) => self.start_fetch(Link::OneShot(url)),
            // A `<video>`/`<audio>` representation: hand its URL to mpv (a direct
            // file, or — for a streaming player — the page URL that yt-dlp
            // resolves). The terminal can't play it inline.
            Link::Media(url) => self.launch_mpv(url.to_string()),
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
        // A selection in the PINNED fixed layer resolves to that item's link.
        if let Some((fi, r, i)) = g.sel_fixed {
            return g.doc.fixed.get(fi)?.rows.get(r)?.items.get(i)?.link.clone();
        }
        if g.doc.laid_out() {
            let (r, i) = g.sel_item?;
            // Through `effective_row` so a selection that landed on scroll-region
            // content (whose items live in the region buffer, not the blank
            // reserved doc row) resolves to its link.
            crate::layout::effective_row(&g.doc.rows, &g.doc.regions, r)
                .items
                .get(i)?
                .link
                .clone()
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
            Link::Media(url) => url.to_string(),
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

    /// The selected link as the URL string `y` copies: any followable
    /// scheme — web URLs resolved absolute, gopher/gemini/one-shot spelled
    /// out, foreign schemes (mailto:, irc:) verbatim (copying is exactly
    /// what a scheme we don't speak is for). Form controls and carousel
    /// buttons aren't URLs.
    fn yank_target(&self) -> Option<String> {
        if let Some(url) = self.selected_web_url() {
            return Some(url);
        }
        match self.selected_link()? {
            Link::Gopher(u) => Some(u.to_string()),
            Link::Gemini(u) => Some(u.to_string()),
            Link::OneShot(u) => Some(u.to_string()),
            Link::Media(u) => Some(u.to_string()),
            Link::External(s) => Some(s),
            Link::JsClick { href, .. } if !href.is_empty() => Some(href),
            _ => None,
        }
    }

    /// `y` in the browser: copy the selected link's URL to the system
    /// clipboard via OSC 52 (foot supports it; nothing touches disk —
    /// RAM-only friendly). The sequence bypasses ratatui's cell buffer:
    /// it's a pure control string, invisible to the screen.
    fn yank_selected_url(&mut self) {
        let Some(url) = self.yank_target() else {
            self.status = String::from("No link selected to copy.");
            self.notice = true;
            return;
        };
        use std::io::Write;
        let mut out = std::io::stdout();
        let _ = out.write_all(osc52_copy(&url).as_bytes());
        let _ = out.flush();
        self.status = format!("Copied {url}");
        self.notice = true;
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
                // The page widget already bullets password values; the edit
                // prompt must not undo that by echoing the secret.
                self.masked_input = kind == FieldKind::Password;
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
        // Re-resolve instead of indexing: a live re-render while the dropdown
        // was open can reshape `forms`, and the menu's indices are from the
        // doc it opened over — a miss drops the write (never a panic).
        if let Some(f) = self
            .browser
            .as_mut()
            .and_then(|g| g.doc.forms.get_mut(form))
            .and_then(|f| f.fields.get_mut(field))
        {
            f.value = value;
            self.refresh_forms();
        } else {
            self.status = String::from("That control changed under a page update.");
            self.notice = true;
        }
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
        self.browser_travel(false);
    }

    fn browser_forward(&mut self) {
        self.browser_travel(true);
    }

    /// Shared back/forward. An entry that still holds its doc (adjacent, or
    /// a retained POST result) restores instantly from RAM, parking the
    /// leaving doc on the opposite stack; an EVICTED entry starts a refetch
    /// and the arriving response completes the travel in `navigate_to` —
    /// until then the trail is untouched, so a failed fetch changes nothing.
    fn browser_travel(&mut self, forward: bool) {
        let js = self.js_enabled;
        // Captured before the drop below: the doc we're leaving should be
        // revived (not restored static) when travelled back onto.
        let was_live = self.live_page.is_some();
        // Any manual travel supersedes an in-flight deep-travel fetch —
        // its doc must not shuffle a trail that changed underneath it
        // (it degrades to a plain navigation push if it still lands).
        self.pending_travel = None;
        let Some(g) = &mut self.browser else { return };
        let src = if forward {
            &mut g.forward
        } else {
            &mut g.history
        };
        let Some(top) = src.last() else {
            self.status = if forward {
                String::from("Nothing forward in history.")
            } else {
                String::from("History empty (Esc returns to terminal).")
            };
            // Without the notice flag the selected-link hint overrides
            // the message and the key looks dead.
            self.notice = true;
            return;
        };
        if top.doc.is_none() {
            // Deep travel: the doc was evicted (strict memory, depth-1
            // retention). Refetch it; the response completes the shuffle.
            let url = top.url.clone();
            self.start_fetch(url);
            self.pending_travel = Some(forward);
            return;
        }
        let entry = src.pop().expect("just peeked");
        let old = std::mem::replace(&mut g.doc, entry.doc.expect("just checked"));
        let parked = HistEntry {
            url: old.url.clone(),
            pos: ViewPos {
                selected: g.selected,
                sel_item: g.sel_item,
                was_live,
            },
            scroll: g.scroll,
            post: self.current_from_post,
            doc: Some(old),
        };
        if forward {
            g.history.push(parked);
        } else {
            g.forward.push(parked);
        }
        // The old top of the receiving stack just became non-adjacent.
        Self::enforce_retention(g);
        g.selected = entry.pos.selected;
        g.sel_item = entry.pos.sel_item;
        // Not carried in ViewPos: the restored doc's fixed layers are
        // a different array, so a leftover rail selection is stale.
        g.sel_fixed = None;
        g.scroll = entry.scroll;
        self.current_from_post = entry.post;
        // Revive a page that was interactive when we left it: re-run its
        // JS so links/forms work again, rather than restoring a frozen
        // snapshot with dead script links. The frozen doc shows meanwhile;
        // the reload replaces it in place (history already adjusted).
        // Needs JS on + http(s) — and never a POST result: a GET of the
        // action URL is a different page, the retained doc stands.
        let revive = entry.pos.was_live && js && !entry.post;
        let url = g.doc.url.clone();
        let image_urls = g.doc.image_urls.clone();
        // Whatever living page was foreground froze when we left.
        self.drop_live_page();
        self.sweep_image_caches();
        if revive && let Link::Http(u) = &url {
            self.replace_nav = true;
            self.start_fetch(Link::Http(u.clone()));
            // After start_fetch (which sets its own "Fetching"
            // status) so the user sees why we're reloading.
            self.status = String::from("Reviving page scripts …");
        } else if let Link::Http(u) = &url {
            // Restored from RAM: refetch any images the sweep dropped
            // while this doc sat deep in the trail (a retained POST
            // result keeps text only); still-cached URLs no-op.
            self.start_image_loads(u.clone(), image_urls);
        }
    }

    /// Depth-1 doc retention: drop the parsed doc of every trail entry
    /// that is not the top of its stack (not one step from the shown
    /// page), except POST results (see `HistEntry`). Idempotent; runs
    /// after every trail mutation.
    fn enforce_retention(g: &mut BrowserView) {
        for stack in [&mut g.history, &mut g.forward] {
            let top = stack.len().saturating_sub(1);
            for e in stack.iter_mut().take(top) {
                if !e.post {
                    e.doc = None;
                }
            }
        }
    }

    /// Strict image memory: keep decoded images (and their size mirror)
    /// only for URLs referenced by the docs held ADJACENT to the screen —
    /// the current doc and the two stack tops. Anything else (including a
    /// deep retained POST result's images) refetches on restore, like a
    /// page load. Runs after every trail mutation; no LRU, no byte
    /// accounting — bounded by construction.
    fn sweep_image_caches(&mut self) {
        let Some(g) = &self.browser else {
            self.image_cache.clear();
            self.image_sizes.clear();
            return;
        };
        let adjacent = [
            g.history.last().and_then(|e| e.doc.as_ref()),
            g.forward.last().and_then(|e| e.doc.as_ref()),
        ];
        let keep: HashSet<&str> = g
            .doc
            .image_urls
            .iter()
            .chain(
                adjacent
                    .into_iter()
                    .flatten()
                    .flat_map(|d| d.image_urls.iter()),
            )
            .map(String::as_str)
            .collect();
        self.image_cache.retain(|u, _| keep.contains(u.as_str()));
        self.image_sizes.retain(|u, _| keep.contains(u.as_str()));
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
        // The dropped browser's image batches have nothing to deliver to,
        // and its decoded images (the whole trail's) die with it.
        self.abort_image_loads();
        self.failed_images.clear();
        self.sweep_image_caches();
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
        // (.ts is NOT here: on today's web it's a TypeScript source far more
        // often than MPEG-TS — manual `v` still plays a real transport
        // stream; .m2ts is unambiguous and stays.)
        ".mp4", ".m4v", ".webm", ".mkv", ".mov", ".avi", ".flv", ".wmv", ".mpg", ".mpeg", ".ogv",
        ".m2ts", ".3gp", ".ogm", // adaptive-streaming manifests mpv plays
        ".m3u8", ".mpd", // audio
        ".mp3", ".m4a", ".m4b", ".aac", ".ogg", ".oga", ".opus", ".flac", ".wav", ".wma", ".mka",
        ".weba", ".aiff", ".aif",
    ];
    EXTS.iter().any(|e| path.ends_with(e))
}

const SEND_USAGE: &str = "usage: send brk|ip|ao|ayt|ec|el|ga|nop|escape";

/// The OSC 52 set-clipboard control string for `text` (base64 payload,
/// `c` = the clipboard selection, BEL-terminated).
fn osc52_copy(text: &str) -> String {
    format!(
        "\x1b]52;c;{}\x07",
        crate::img::base64_encode(text.as_bytes())
    )
}

/// The `about:help` page source (gemtext). Command lines live in
/// preformatted blocks so their alignment survives any terminal width.
const HELP_PAGE: &str = "\
# TRust help

Tab or Ctrl-] opens the command console; Enter runs a line.
A bare URL or hostname opens directly, like an address bar.

## Commands

```
open <host> [port]        telnet (telnets:// for TLS)
open <url>                gopher gemini http(s) finger …
close                     drop the connection
reload                    refetch the page on screen
post <url> [body]         POST a form body to a web URL
finger [user]@<host>      finger query
whois <domain> [server]   whois lookup
dict <word> [server]      dictionary lookup
status                    connection and options report
help                      this page
quit                      exit
```

## Settings

```
set encoding cp437|utf8   BBS art mode
set image sixel|halfblocks|kitty|iterm2|auto
set js on|off             page JavaScript (default on)
set cookies on|off        RAM-only cookies (default on)
set borders on|off        CSS borders (default off)
mode character|line|auto  telnet input mode
send escape|<iac>         Ctrl-] or an IAC (brk/ip/ayt/…)
toggle crlf               what Enter sends
```

## Browsing keys

```
Up/Down        move the selection (page scrolls along)
Enter/Right    follow the selected link
Left/Backspace back · Alt-Left/Alt-Right back/forward
PgUp/PgDn      page · Home/End top/bottom
Ctrl-F         find in page (Enter next, Shift-Enter prev)
v              play the selected link in mpv
y              copy the selected link URL (OSC 52)
Esc            stop loading / close the page
```

Mouse: hover selects, click follows, wheel scrolls,
back/forward side buttons travel history.

## Telnet sessions

Line mode edits locally; character mode sends every key to
the remote (Ctrl-] still opens the console). Esc reaches the
remote in character mode — full-screen apps depend on it.

## Image viewer

Left/Backspace/q/Esc close it. `set image` picks the protocol.
";

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
fn push_text_matches(text: &str, query: &[char], loc: FindLoc, out: &mut Vec<FindMatch>) {
    // Cheap pre-filter before any allocation: a text with fewer BYTES than
    // the query has CHARS can't hold it (chars ≤ bytes). Most items are short
    // words, so find-as-you-type skips the per-item char collection for them.
    if query.is_empty() || text.len() < query.len() {
        return;
    }
    let lower: Vec<char> = text.chars().map(|c| c.to_ascii_lowercase()).collect();
    if query.len() > lower.len() {
        return;
    }
    let mut i = 0;
    while i + query.len() <= lower.len() {
        if lower[i..i + query.len()] == *query {
            out.push(FindMatch {
                loc,
                start: i,
                end: i + query.len(),
            });
            i += query.len();
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
    blobs: Option<&crate::js::BlobMap>,
) -> Option<DecodedImage> {
    // A `data:` image (a rewritten inline SVG, or a page's own data image)
    // carries its bytes — decode locally, no fetch, no SSRF concern.
    if url.starts_with("data:") {
        let raw: std::sync::Arc<[u8]> = crate::img::decode_data_url(url)?.into();
        let (cell, has_alpha) = decoded_cell_box(raw.clone(), font).await?;
        return Some(DecodedImage {
            raw,
            cell,
            has_alpha,
        });
    }
    // A `blob:` image resolves from the page's blob byte mirror — bytes the
    // page's own JS minted via `URL.createObjectURL` (Steam's login QR).
    // Keyed without a fragment, like the JS-side store; never the wire.
    if url.starts_with("blob:") {
        let key = url.split('#').next().unwrap_or(url);
        let raw: std::sync::Arc<[u8]> = blobs
            .and_then(|m| m.lock().unwrap().get(key).map(|(b, _)| b.clone()))?
            .into();
        let (cell, has_alpha) = decoded_cell_box(raw.clone(), font).await?;
        return Some(DecodedImage {
            raw,
            cell,
            has_alpha,
        });
    }
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
    // Record an EXTERNAL SVG's ratio-only ratio (a `viewBox` with no intrinsic
    // width/height) so replaced sizing can apply CSS 2.1 §10.3.2 rule 3 to it —
    // layout can't read an external SVG's markup the way it reads a `data:` one.
    if let Some(ratio) = crate::img::svg_bytes_ratio_only(&raw) {
        crate::img::note_svg_ratio_only(url, ratio);
    }
    let (cell, has_alpha) = decoded_cell_box(raw.clone(), font).await?;
    Some(DecodedImage {
        raw,
        cell,
        has_alpha,
    })
}

/// Decode an image's intrinsic cell box on a blocking thread (sandboxed: a bad
/// image fails to `None`, never unwinds the worker — the terminal is safe
/// regardless, only the run loop restores it). The intrinsic box is the
/// fallback size; an SVG (or any image) whose element carries a CSS/attr
/// `width`/`height` is sized by that in `image_used_box`, which is why a
/// rewritten inline `<svg>` keeps the original element's box (see
/// `Dom::rewrite_inline_svgs`) instead of being clamped here.
async fn decoded_cell_box(
    bytes: std::sync::Arc<[u8]>,
    font: ratatui_image::FontSize,
) -> Option<((u16, u16), bool)> {
    tokio::task::spawn_blocking(move || {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            crate::img::info(&bytes).ok().map(|info| {
                (
                    natural_cell_box_dimensions(info.width, info.height, font),
                    info.has_alpha,
                )
            })
        }))
        .ok()
        .flatten()
    })
    .await
    .ok()?
}

/// The cell box an image occupies: its natural size at the terminal font,
/// scaled down (never up) to fit `IMG_MAX_CELLS` while preserving aspect.
/// Layout clamps the width further to the content width (rescaling height).
#[cfg(test)]
fn natural_cell_box(image: &image::DynamicImage, font: ratatui_image::FontSize) -> (u16, u16) {
    natural_cell_box_dimensions(image.width(), image.height(), font)
}

fn natural_cell_box_dimensions(
    width: u32,
    height: u32,
    font: ratatui_image::FontSize,
) -> (u16, u16) {
    // Match ratatui-image's natural_size rounding exactly, without forcing SVG
    // through a throwaway intrinsic-size rasterization just to read dimensions.
    let cw = (width as f32 / f32::from(font.width.max(1)))
        .ceil()
        .max(1.0);
    let ch = (height as f32 / f32::from(font.height.max(1)))
        .ceil()
        .max(1.0);
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
        // Shift-Tab, as CSI Z (back-tab) — BBS forms navigate fields with it.
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => b"\x1b[A".to_vec(),
        KeyCode::Down => b"\x1b[B".to_vec(),
        KeyCode::Right => b"\x1b[C".to_vec(),
        KeyCode::Left => b"\x1b[D".to_vec(),
        KeyCode::Home => b"\x1b[H".to_vec(),
        KeyCode::End => b"\x1b[F".to_vec(),
        KeyCode::Insert => b"\x1b[2~".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
        // Function keys, xterm/VT-style: SS3 P..S for F1-F4 (the VT100 PF
        // keys), CSI n ~ for F5-F12 (with the historic VT220 gaps at 16/22).
        // BBS door games and full-screen menus bind these; dropping them made
        // the keys silently dead in char-mode sessions.
        KeyCode::F(n) => match n {
            1 => b"\x1bOP".to_vec(),
            2 => b"\x1bOQ".to_vec(),
            3 => b"\x1bOR".to_vec(),
            4 => b"\x1bOS".to_vec(),
            5 => b"\x1b[15~".to_vec(),
            6 => b"\x1b[17~".to_vec(),
            7 => b"\x1b[18~".to_vec(),
            8 => b"\x1b[19~".to_vec(),
            9 => b"\x1b[20~".to_vec(),
            10 => b"\x1b[21~".to_vec(),
            11 => b"\x1b[23~".to_vec(),
            12 => b"\x1b[24~".to_vec(),
            _ => return None,
        },
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

    /// Char-mode sessions forward the keys a full-screen BBS app binds:
    /// F1-F4 as the VT100 PF keys (SS3), F5-F12 as the VT220-style CSI codes
    /// (with the historic gaps at 16/22), Insert, and Shift-Tab as CSI Z.
    #[test]
    fn char_mode_encodes_function_and_editing_keys() {
        use crossterm::event::{KeyCode, KeyEvent};
        let k = |code| super::encode_key(KeyEvent::from(code), false);
        assert_eq!(k(KeyCode::F(1)), Some(b"\x1bOP".to_vec()));
        assert_eq!(k(KeyCode::F(4)), Some(b"\x1bOS".to_vec()));
        assert_eq!(k(KeyCode::F(5)), Some(b"\x1b[15~".to_vec()));
        assert_eq!(k(KeyCode::F(6)), Some(b"\x1b[17~".to_vec()));
        assert_eq!(k(KeyCode::F(10)), Some(b"\x1b[21~".to_vec()));
        assert_eq!(k(KeyCode::F(12)), Some(b"\x1b[24~".to_vec()));
        assert_eq!(k(KeyCode::F(13)), None);
        assert_eq!(k(KeyCode::Insert), Some(b"\x1b[2~".to_vec()));
        assert_eq!(k(KeyCode::BackTab), Some(b"\x1b[Z".to_vec()));
    }

    /// Regression (live-verified before the fix): the redraw-economy frame
    /// signature must track the gopher/gemini LINE-model selection. A Down
    /// that steps the highlight without scrolling hashed identical, so the
    /// draw was skipped — the highlight and status preview froze while Enter
    /// followed the invisibly-moved selection.
    #[test]
    fn frame_sig_tracks_the_line_model_selection() {
        let mut app = super::App::new(None, 23);
        app.mode = super::Mode::Session;
        app.navigate_to(gopher_doc("xxxx"));
        let before = app.browser_frame_sig().expect("plain browser state hashes");
        // The gopherus walk: the selection steps, the scroll stays pinned
        // (the page fits on screen).
        app.browser_walk(1);
        assert_ne!(selected(&app), Some(0), "walk moved the selection");
        let after = app.browser_frame_sig().expect("still hashable");
        assert_ne!(before, after, "selection change must change the sig");
    }

    /// Navigating (push, back, or reload) drops a pinned-rail selection with
    /// the doc it addressed — `selected_link` consults `sel_fixed` FIRST, so
    /// a stale address left Enter dead (or aimed at the wrong rail link on a
    /// page that also has fixed layers). An open <select> dropdown is
    /// likewise anchored to the old doc and closes.
    #[test]
    fn navigation_clears_stale_rail_selection_and_select_menu() {
        let mut app = super::App::new(None, 23);
        app.navigate_to(gopher_doc("x"));
        app.browser.as_mut().unwrap().sel_fixed = Some((3, 2, 1));
        app.select_menu = Some(super::SelectMenu {
            form: 0,
            field: 0,
            options: vec![(String::from("A"), String::from("a"))],
            highlight: 0,
            scroll: 0,
            anchor_row: 0,
            anchor_col: 0,
        });
        app.navigate_to(gopher_doc("xx"));
        let g = app.browser.as_ref().unwrap();
        assert_eq!(g.sel_fixed, None, "push cleared the rail selection");
        assert!(app.select_menu.is_none(), "push closed the dropdown");

        app.browser.as_mut().unwrap().sel_fixed = Some((3, 2, 1));
        app.browser_back();
        assert_eq!(app.browser.as_ref().unwrap().sel_fixed, None, "back too");

        app.browser.as_mut().unwrap().sel_fixed = Some((3, 2, 1));
        app.replace_nav = true;
        app.navigate_to(gopher_doc("xxx"));
        assert_eq!(
            app.browser.as_ref().unwrap().sel_fixed,
            None,
            "reload (replace) too"
        );
    }

    /// A page-event batch that is NOT the submit answer (an autonomous timer
    /// render, a bare Settled) must not consume the recorded submit target:
    /// the engine runs at rest, so such batches routinely land between the
    /// Submit dispatch and its SubmitDefault answer — taking the record on
    /// every batch silently dropped the fallback static submit.
    #[tokio::test]
    async fn intervening_page_events_do_not_drop_the_pending_submit() {
        let mut app = app_browsing(
            "text/html",
            r#"<form action="/go" method="get"><input type="text" name="q" value="hi"><input type="submit" value="Go"></form>"#,
        );
        app.pending_live_submit = Some((0, 1));
        // An unrelated batch (what a timer tick's settle looks like).
        app.on_page_evt(crate::js::PageEvt::Settled);
        assert_eq!(app.pending_live_submit, Some((0, 1)), "record survives");
        // The real answer arrives: the static GET submit fires.
        app.on_page_evt(crate::js::PageEvt::SubmitDefault);
        assert_eq!(app.pending_live_submit, None, "record consumed");
        assert!(app.loading(), "static submit kicked off the fetch");
        assert!(
            app.status.contains("/go"),
            "fetching the form action: {}",
            app.status
        );
    }

    /// Committing a <select> whose indices went stale (a live re-render
    /// reshaped `forms` while the dropdown was open) drops the write with a
    /// notice — it used to index straight into `forms[form].fields[field]`
    /// and panic the main thread.
    #[test]
    fn select_commit_with_stale_indices_is_dropped_not_a_panic() {
        let mut app = app_browsing("text/html", "<p>no forms here</p>");
        app.select_menu = Some(super::SelectMenu {
            form: 3,
            field: 7,
            options: vec![(String::from("A"), String::from("a"))],
            highlight: 0,
            scroll: 0,
            anchor_row: 0,
            anchor_col: 0,
        });
        app.commit_select_highlight(); // must not panic
        assert!(app.notice, "the dropped write is surfaced");
        assert!(app.select_menu.is_none());
    }

    /// A live re-render closes an open <select> dropdown only when it
    /// removed (or retyped) the field the menu is bound to; an unrelated
    /// update leaves the user's open menu alone.
    #[test]
    fn live_render_closes_the_select_menu_only_when_its_field_vanishes() {
        let html = r#"<form action="/f"><select name="s"><option value="1">one</option></select></form><p id="x">hi</p>"#;
        let mut app = app_browsing("text/html", html);
        let field = app.browser.as_ref().unwrap().doc.forms[0]
            .fields
            .iter()
            .position(|f| matches!(f.kind, crate::doc::FieldKind::Select(_)))
            .expect("select field");
        app.select_menu = Some(super::SelectMenu {
            form: 0,
            field,
            options: vec![(String::from("one"), String::from("1"))],
            highlight: 0,
            scroll: 0,
            anchor_row: 0,
            anchor_col: 0,
        });
        app.replace_live_doc(html.as_bytes().to_vec());
        assert!(app.select_menu.is_some(), "unrelated update keeps the menu");
        app.replace_live_doc(b"<p>form gone</p>".to_vec());
        assert!(app.select_menu.is_none(), "vanished field closes the menu");
    }

    /// `set image <proto>` drops inline-image encodes (they're baked for the
    /// OLD protocol) and stale in-flight results land ignored — the page
    /// otherwise kept rendering sixel after `set image halfblocks`.
    #[test]
    fn protocol_switch_drops_cached_and_inflight_encodes() {
        let mut app = super::App::new(None, 23);
        let key = super::EncKey {
            url: String::from("https://example.com/a.png"),
            w: 4,
            h: 2,
            crop: false,
            pixelated: false,
            tint: Some(super::svg_tint()),
        };
        app.failed_encodes.insert(key.clone());
        app.image_encoding.insert(key.clone());
        app.set_image_protocol("halfblocks");
        assert!(app.failed_encodes.is_empty(), "failure cache dropped");
        assert!(app.image_encoding.is_empty(), "in-flight markers dropped");
        assert!(app.image_protocols.is_empty());
        // A result from before the switch is recognized as stale: nothing is
        // recorded, and a re-requested key's fresh in-flight marker survives.
        app.image_encoding.insert(key.clone());
        app.on_enc(super::EncMsg {
            key: key.clone(),
            protocol: None,
            epoch: 0, // pre-switch epoch
        });
        assert!(app.failed_encodes.is_empty(), "stale result not recorded");
        assert!(app.image_encoding.contains(&key), "fresh marker survives");
        // A current-epoch result applies normally.
        app.on_enc(super::EncMsg {
            key: key.clone(),
            protocol: None,
            epoch: app.enc_epoch,
        });
        assert!(app.failed_encodes.contains(&key));
        assert!(app.image_encoding.is_empty());
    }

    /// Esc must abort the image fetch batch itself — the old code only
    /// dropped the receiver, so a 100-image page kept fetching them all in
    /// the background after "Stopped — load cancelled".
    #[tokio::test]
    async fn stop_loading_aborts_image_batches() {
        let mut app = app_browsing("text/html", "<p>x</p>");
        let page = url::Url::parse("http://127.0.0.1:9/").unwrap();
        app.start_image_loads(page, vec![String::from("http://127.0.0.1:9/a.png")]);
        assert!(!app.imgs_in_flight.is_empty());
        assert!(app.loading(), "a batch drives the loading pulse");
        assert_eq!(app.imgs_tasks.len(), 1);
        app.stop_loading();
        assert!(app.imgs_in_flight.is_empty(), "Esc forgets in-flight urls");
        assert!(app.imgs_tasks.is_empty(), "Esc aborted the batch task");
        assert!(!app.loading());
    }

    /// A failed fetch/decode is REMEMBERED for the page: a live page that
    /// re-renders every second passes the full URL list to
    /// `start_image_loads` each time, and without the negative cache it
    /// refetched every broken image once per render. Navigation clears the
    /// memory, so a reload retries.
    #[tokio::test]
    async fn a_failed_image_is_not_refetched_until_navigation() {
        let mut app = app_browsing("text/html", "<p>x</p>");
        let url = String::from("https://example.com/broken.png");
        app.on_img_load(super::ImgLoadMsg {
            url: url.clone(),
            decoded: None,
        });
        assert!(app.failed_images.contains(&url), "failure remembered");
        let page = url::Url::parse("https://example.com/").unwrap();
        app.start_image_loads(page, vec![url.clone()]);
        assert!(
            app.imgs_in_flight.is_empty(),
            "no refetch of a known-bad url"
        );
        assert!(app.imgs_tasks.is_empty(), "no batch spawned");
        app.navigate_to(gopher_doc("x"));
        assert!(app.failed_images.is_empty(), "navigation clears the memory");
    }

    /// A reload that lands a shorter document clamps the ridden scroll to
    /// the new MAX scroll (extent − viewport), not to the last row — which
    /// parked the viewport on a lone top row with blank space below.
    #[test]
    fn reload_clamps_scroll_by_viewport_height() {
        let mut app = super::App::new(None, 23);
        app.navigate_to(gopher_doc(&"i".repeat(100)));
        app.browser.as_mut().unwrap().scroll = 90;
        app.replace_nav = true;
        app.navigate_to(gopher_doc(&"i".repeat(50)));
        let height = app.last_inner.1.max(1) as usize;
        let g = app.browser.as_ref().unwrap();
        assert_eq!(g.scroll, 50usize.saturating_sub(height));
    }

    /// A decode that raced a navigation (its result was already queued when
    /// the batch was aborted) must not trigger a relayout of the unrelated
    /// NEW page — only URLs the current doc references count.
    #[test]
    fn stale_decodes_do_not_relayout_the_new_page() {
        let mut app = app_browsing("text/html", "<p>no images here</p>");
        app.pending_decoded_urls
            .push(String::from("https://old.example/x.png"));
        // Sentinel: a relayout re-parses and overwrites `wrapped_to` with
        // the real width; the stale URL must not trigger one.
        app.browser.as_mut().unwrap().doc.wrapped_to = 7777;
        app.apply_pending_image_decodes();
        assert_eq!(
            app.browser.as_ref().unwrap().doc.wrapped_to,
            7777,
            "no relayout for a URL the doc doesn't reference"
        );
        assert!(app.pending_decoded_urls.is_empty(), "pending drained");
    }

    /// A resize re-wrap rebuilds regions from `doc.raw` — the last FULL
    /// render, which on a live page is stale by exactly the content patched
    /// into regions since (a chat's recent messages). The re-wrap must
    /// restore each live region from its retained patch HTML, or resizing
    /// during a chat rolled the messages back until the next patch.
    #[test]
    fn resize_restores_live_region_content_from_retained_html() {
        let base = url::Url::parse("https://ex.com/").unwrap();
        // A definite-height overflow-y box with enough content to become a
        // Region, carrying the actor node id the live serializer bakes.
        let stale: String = (0..40).map(|i| format!("<div>STALE{i}</div>")).collect();
        let body = format!(
            "<body><div data-trust-node=\"7\" style=\"height:96px;overflow-y:auto\">{stale}</div></body>"
        );
        let mut app = super::App::new(None, 23);
        app.last_inner = (40, 50);
        let doc = crate::http::parse(
            &base,
            "text/html; charset=utf-8",
            body.as_bytes(),
            40,
            50,
            &crate::layout::ImageSizes::new(),
        );
        app.navigate_to(doc);
        {
            let g = app.browser.as_ref().unwrap();
            assert_eq!(g.doc.regions.len(), 1, "the overflow box is a region");
            assert_eq!(g.doc.regions[0].live_node, Some(7));
        }
        // The retained patch HTML holds the CURRENT content (what the live
        // actor last patched in).
        let fresh: String = (0..40).map(|i| format!("<div>FRESH{i}</div>")).collect();
        let fragment = format!(
            "<div data-trust-node=\"7\" style=\"height:96px;overflow-y:auto\">{fresh}</div>"
        );
        app.region_live.insert(
            7,
            super::RegionLive {
                html: fragment.into_bytes(),
                cache: Default::default(),
            },
        );
        // Resize: the re-wrap re-parses stale raw, then must restore the
        // region from the retained HTML.
        app.last_inner = (60, 50);
        app.sync_browser_wrap();
        let g = app.browser.as_ref().unwrap();
        let texts: Vec<&str> = g.doc.regions[0]
            .buffer
            .iter()
            .flat_map(|r| &r.items)
            .map(|it| it.text.as_str())
            .collect();
        assert!(
            texts.iter().any(|t| t.contains("FRESH0")),
            "region shows the retained (current) content: {texts:?}"
        );
        assert!(
            !texts.iter().any(|t| t.contains("STALE0")),
            "stale raw content did not survive the resize: {texts:?}"
        );
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

    #[tokio::test]
    async fn help_command_opens_the_about_page() {
        use crate::doc::{Kind, Link};
        let mut app = super::App::new(None, 23);
        app.execute_command("help").await;
        let g = app.browser.as_ref().expect("help opens a browser doc");
        assert!(matches!(&g.doc.url, Link::External(s) if s == "about:help"));
        assert!(
            g.doc
                .lines
                .iter()
                .any(|l| l.kind == Kind::Heading(1) && l.text.contains("TRust help"))
        );
        assert!(
            g.doc
                .lines
                .iter()
                .any(|l| l.kind == Kind::Pre && l.text.contains("open <host>")),
            "the command table renders preformatted"
        );

        // `?` is an alias.
        let mut app = super::App::new(None, 23);
        app.execute_command("?").await;
        assert!(app.browser.is_some());
    }

    #[tokio::test]
    async fn status_while_browsing_shows_a_page_and_back_returns() {
        use crate::doc::Link;
        let mut app = super::App::new(None, 23);
        app.navigate_to(gopher_doc("x.x"));
        app.execute_command("status").await;
        let g = app.browser.as_ref().unwrap();
        assert!(
            matches!(&g.doc.url, Link::External(s) if s == "about:status"),
            "status over a hidden feed renders as a page"
        );
        assert!(
            g.doc
                .lines
                .iter()
                .any(|l| l.text.contains("No connection."))
        );
        app.browser_back();
        let g = app.browser.as_ref().unwrap();
        assert!(
            matches!(&g.doc.url, Link::Gopher(_)),
            "back returns to the page"
        );
    }

    #[test]
    fn about_pages_rewrap_on_resize() {
        let mut app = super::App::new(None, 23);
        app.open_about("help");
        let before = app.browser.as_ref().unwrap().doc.wrapped_to;
        app.last_inner = (40, 20);
        app.sync_browser_wrap();
        let g = app.browser.as_ref().unwrap();
        assert_ne!(before, 40, "the test needs a real width change");
        assert_eq!(g.doc.wrapped_to, 40);
        assert!(!g.doc.lines.is_empty());
    }

    #[tokio::test]
    async fn deep_travel_regenerates_about_pages_locally() {
        use crate::doc::Link;
        let mut app = super::App::new(None, 23);
        app.open_about("help");
        app.navigate_to(http_doc("/a"));
        app.navigate_to(http_doc("/b"));
        let g = app.browser.as_ref().unwrap();
        assert!(g.history[0].doc.is_none(), "the deep about entry evicts");
        app.browser_back(); // /a, adjacent
        app.browser_back(); // about:help — deep, refetches
        assert_eq!(app.pending_travel, Some(false));
        let msg = app
            .fetch_rx
            .as_mut()
            .expect("a local regeneration was dispatched")
            .recv()
            .await
            .expect("the about fetch settles");
        app.on_fetch(msg);
        let g = app.browser.as_ref().unwrap();
        assert!(
            matches!(&g.doc.url, Link::External(s) if s == "about:help"),
            "travel landed back on the regenerated page"
        );
        assert!(!g.doc.lines.is_empty());
        assert!(g.history.is_empty());
        assert_eq!(g.forward.len(), 2);
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
        // synthetic docs are never re-wrapped (empty `raw`)
        Doc::from_lines(Link::Gopher(url), lines, Vec::new(), 80, false, None)
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
    fn following_a_same_page_anchor_scrolls_without_refetching() {
        // A `<a href="#id">` on the current page scrolls the view to that anchor
        // (HTML "navigate to a fragment") instead of re-fetching. `anchor_rows`
        // resolves the id → row; the view moves and the doc is unchanged.
        let mut app = super::App::new(None, 23);
        app.last_inner = (80, 6); // small viewport so the anchor is below the fold
        let url = url::Url::parse("https://example.com/page").unwrap();
        let mut html = String::from("<body><a href=\"#bottom\">jump</a>");
        for i in 0..40 {
            html.push_str(&format!("<p>filler {i}</p>"));
        }
        html.push_str("<h2 id=bottom>END-MARKER</h2></body>");
        app.navigate_to(crate::http::parse(
            &url,
            "text/html",
            html.as_bytes(),
            80,
            6,
            &Default::default(),
        ));
        let bottom_row = *app
            .browser
            .as_ref()
            .unwrap()
            .doc
            .anchor_rows
            .get("bottom")
            .expect("anchor_rows has the #bottom section");
        assert!(bottom_row > 6, "anchor is below the 6-row fold");
        select_item(
            &mut app,
            |it| matches!(&it.link, Some(crate::doc::Link::Http(u)) if u.fragment() == Some("bottom")),
        );
        app.browser_follow();
        let g = app.browser.as_ref().unwrap();
        let max_scroll = g.doc.extent().saturating_sub(6);
        assert_eq!(
            g.scroll,
            bottom_row.min(max_scroll),
            "the view scrolled to the anchor (clamped near the end)"
        );
        assert!(g.scroll > 0, "the view actually moved");
        assert!(
            matches!(&g.doc.url, crate::doc::Link::Http(u) if u.as_str() == "https://example.com/page"),
            "still the same document — no refetch"
        );
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
            0,
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
            0,
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
            0,
            &Default::default(),
        ));
        app.browser.as_mut().unwrap().sel_item = None;
        let (x, y, target) = item_point(&app, |it| it.link.is_some());

        app.on_mouse_event(mouse(crossterm::event::MouseEventKind::Moved, x, y));

        assert_eq!(app.browser.as_ref().unwrap().sel_item, Some(target));
    }

    #[test]
    fn mouse_and_keyboard_hover_feed_the_live_page_diffed() {
        // The hover pipeline end-to-end on the app side: mouse motion over a
        // marked target records a pending hover; the (dwell-elapsed) commit
        // sends ONE PageCmd::Hover; an unchanged target is free; a hover-only
        // (non-interactive) element resolves via the parse-time map; leaving
        // to chrome sends a single clear — and only after a Some was sent.
        let mut app = super::App::new(None, 23);
        app.mode = super::Mode::Session;
        app.last_inner = (80, 10);
        app.last_content_area = ratatui::layout::Rect::new(2, 1, 80, 10);
        let url = url::Url::parse("https://example.com/").unwrap();
        // The x-trust-js anchor marker (a live clickable, actor node 42) and a
        // data-trust-hover host (hover-only div, actor node 77) — exactly what
        // the live serializer bakes.
        let html = b"<body><p>plain <a href='x-trust-js:42:/next'>next</a></p>\
             <div data-trust-hover='77'>hotzone</div></body>";
        app.navigate_to(crate::http::parse(
            &url,
            "text/html",
            html,
            80,
            0,
            &Default::default(),
        ));
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        app.live_page = Some(crate::js::PageHandle { cmds: tx });

        // Mouse over the anchor → pending target 42; the commit sends it.
        let (x, y, _) = item_point(&app, |it| it.text.contains("next"));
        app.on_mouse_event(mouse(crossterm::event::MouseEventKind::Moved, x, y));
        assert_eq!(app.hover_want.map(|(t, _, _)| t), Some(Some(42)));
        app.commit_page_hover();
        match rx.try_recv() {
            Ok(crate::js::PageCmd::Hover { node, .. }) => assert_eq!(node, Some(42)),
            other => panic!("expected Hover(42), got {other:?}"),
        }
        // The same target again is a no-op (the diff is the throttle).
        app.on_mouse_event(mouse(crossterm::event::MouseEventKind::Moved, x, y));
        assert_eq!(app.hover_want, None, "unchanged target must not re-arm");

        // The hover-only div is NOT interactive (never the selection), but it
        // IS a hover target via the parse-time map.
        let (hx, hy, _) = item_point(&app, |it| it.text.contains("hotzone"));
        app.on_mouse_event(mouse(crossterm::event::MouseEventKind::Moved, hx, hy));
        assert_eq!(app.hover_want.map(|(t, _, _)| t), Some(Some(77)));
        app.commit_page_hover();
        match rx.try_recv() {
            Ok(crate::js::PageCmd::Hover { node, .. }) => assert_eq!(node, Some(77)),
            other => panic!("expected Hover(77), got {other:?}"),
        }

        // Leaving to chrome clears — once.
        app.on_mouse_event(mouse(crossterm::event::MouseEventKind::Moved, 0, 0));
        assert_eq!(app.hover_want.map(|(t, _, _)| t), Some(None));
        app.commit_page_hover();
        match rx.try_recv() {
            Ok(crate::js::PageCmd::Hover { node, .. }) => assert_eq!(node, None),
            other => panic!("expected Hover(None), got {other:?}"),
        }
        app.on_mouse_event(mouse(crossterm::event::MouseEventKind::Moved, 0, 0));
        assert_eq!(app.hover_want, None, "a second clear must not be queued");

        // Keyboard: arrowing the selection onto the link hovers it too (the
        // selection is the terminal's pointer).
        app.browser.as_mut().unwrap().sel_item = None;
        app.http_move(1, false);
        assert_eq!(
            app.hover_want.map(|(t, _, _)| t),
            Some(Some(42)),
            "keyboard selection feeds the hover pipeline"
        );
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
            0,
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
            0,
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

    /// An app showing a page with a fixed-height (96px ≈ 6 rows)
    /// `overflow-y:auto` scroll region holding 40 link rows, followed by a tall
    /// footer (so the document itself is ALSO scrollable — for chain tests). The
    /// content area starts at row 1 (row 0 is the title chrome).
    fn region_app() -> super::App {
        let mut app = super::App::new(None, 23);
        app.mode = super::Mode::Session;
        app.last_inner = (80, 10);
        app.last_content_area = ratatui::layout::Rect::new(0, 1, 80, 10);
        let url = url::Url::parse("https://example.com/").unwrap();
        let mut links = String::new();
        for i in 0..40 {
            links.push_str(&format!("<div><a href='/L{i:02}'>L{i:02}</a></div>"));
        }
        let foot: String = (0..20).map(|i| format!("<p>F{i:02}</p>")).collect();
        let html = format!(
            "<html><body><div id='s' style='height:96px;overflow-y:auto'>{links}</div>{foot}</body></html>"
        );
        app.navigate_to(crate::http::parse(
            &url,
            "text/html",
            html.as_bytes(),
            80,
            10,
            &Default::default(),
        ));
        app
    }

    /// The absolute screen `(col, row)` of the first link in the region's
    /// current top visible row (the same placement the renderer draws).
    fn region_top_link_point(app: &super::App) -> (u16, u16) {
        let g = app.browser.as_ref().unwrap();
        let rg = &g.doc.regions[0];
        let row = crate::layout::effective_row(&g.doc.rows, &g.doc.regions, rg.start_row);
        let start = crate::layout::visual_columns(&row, &g.doc.carousels, rg.start_row)
            .into_iter()
            .find(|&(i, _)| row.items[i].is_interactive())
            .map(|(_, c)| c)
            .expect("a link in the region top row");
        (
            app.last_content_area.x + start,
            app.last_content_area.y + (rg.start_row - g.scroll) as u16,
        )
    }

    #[test]
    fn wheel_over_a_region_scrolls_it_not_the_document() {
        let mut app = region_app();
        let g = app.browser.as_ref().unwrap();
        assert_eq!(g.doc.regions.len(), 1, "the overflow box is a region");
        assert_eq!(g.scroll, 0);
        assert_eq!(g.doc.regions[0].voffset, 0);
        let start_row = g.doc.regions[0].start_row;
        let rg_row = app.last_content_area.y + start_row as u16 + 1;
        app.on_mouse_event(mouse(
            crossterm::event::MouseEventKind::ScrollDown,
            3,
            rg_row,
        ));
        let g = app.browser.as_ref().unwrap();
        assert_eq!(g.doc.regions[0].voffset, 3, "the wheel scrolled the region");
        assert_eq!(g.scroll, 0, "the document did NOT scroll");
        app.on_mouse_event(mouse(crossterm::event::MouseEventKind::ScrollUp, 3, rg_row));
        assert_eq!(
            app.browser.as_ref().unwrap().doc.regions[0].voffset,
            0,
            "and back up"
        );
    }

    #[test]
    fn a_region_at_its_bottom_traps_the_wheel_does_not_scroll_the_page() {
        // Her call: a wheel with the cursor inside a region scrolls ONLY the
        // region — even at its boundary it never leaks to the page (NOT the
        // chaining `auto`; this is `overscroll-behavior: contain`).
        let mut app = region_app();
        let (start_row, max) = {
            let g = app.browser.as_mut().unwrap();
            let max = g.doc.regions[0].max_voffset();
            assert!(max > 0, "the region overflows");
            g.doc.regions[0].voffset = max; // pin to the bottom
            (g.doc.regions[0].start_row, max)
        };
        let rg_row = app.last_content_area.y + start_row as u16 + 1;
        app.on_mouse_event(mouse(
            crossterm::event::MouseEventKind::ScrollDown,
            3,
            rg_row,
        ));
        let g = app.browser.as_ref().unwrap();
        assert_eq!(
            g.doc.regions[0].voffset, max,
            "the region stayed at its bottom"
        );
        assert_eq!(
            g.scroll, 0,
            "the page did NOT scroll — the wheel was trapped"
        );
    }

    #[test]
    fn a_link_inside_a_region_is_hoverable_and_resolves() {
        let mut app = region_app();
        let (x, y) = region_top_link_point(&app);
        app.on_mouse_event(mouse(crossterm::event::MouseEventKind::Moved, x, y));
        assert!(
            app.browser.as_ref().unwrap().sel_item.is_some(),
            "hover selected the region's content"
        );
        assert_eq!(
            app.selected_link(),
            Some(crate::doc::Link::Http(
                url::Url::parse("https://example.com/L00").unwrap()
            )),
            "the selected link resolves through the region buffer"
        );
    }

    #[test]
    fn pgup_pgdn_scroll_the_hovered_region() {
        let mut app = region_app();
        let start_row = app.browser.as_ref().unwrap().doc.regions[0].start_row;
        app.last_mouse = Some((3, app.last_content_area.y + start_row as u16 + 1));
        // PgDn pages the hovered region (height 6 → a 5-row step).
        assert!(
            app.region_page_scroll(1),
            "the hovered region consumed PgDn"
        );
        assert_eq!(app.browser.as_ref().unwrap().doc.regions[0].voffset, 5);
        assert!(app.region_page_scroll(-1), "PgUp pages it back");
        assert_eq!(app.browser.as_ref().unwrap().doc.regions[0].voffset, 0);
        // At the top, PgUp is still TRAPPED (consumed) — it must not page the
        // document while the cursor is inside the region — but the offset stays.
        assert!(
            app.region_page_scroll(-1),
            "the key stays trapped in the region at its boundary"
        );
        assert_eq!(app.browser.as_ref().unwrap().doc.regions[0].voffset, 0);
        // With nothing under the cursor it falls through to the document.
        app.last_mouse = Some((3, app.last_content_area.y + 200));
        assert!(!app.region_page_scroll(1));
    }

    #[test]
    fn region_scroll_offset_persists_across_relayout() {
        let mut app = region_app();
        app.browser.as_mut().unwrap().doc.regions[0].voffset = 5;
        // A relayout (image decode / resize) re-parses from raw — the region's
        // scroll position is carried over by node id.
        app.relayout_browser();
        let g = app.browser.as_ref().unwrap();
        assert_eq!(g.doc.regions.len(), 1);
        assert_eq!(
            g.doc.regions[0].voffset, 5,
            "the region kept its scroll position across the re-layout"
        );
    }

    /// A region app whose scroll container carries the live serializer's
    /// `data-trust-node` (so the app can correlate it with the resident actor),
    /// and optionally a baked `data-trust-scroll-top` signal (rows).
    fn region_app_with_node(scroll_top: Option<u32>) -> super::App {
        let mut app = super::App::new(None, 23);
        app.mode = super::Mode::Session;
        app.last_inner = (80, 10);
        app.last_content_area = ratatui::layout::Rect::new(0, 1, 80, 10);
        let url = url::Url::parse("https://example.com/").unwrap();
        let mut links = String::new();
        for i in 0..40 {
            links.push_str(&format!("<div><a href='/L{i:02}'>L{i:02}</a></div>"));
        }
        let sig = scroll_top
            .map(|t| format!(" data-trust-scroll-top='{t}'"))
            .unwrap_or_default();
        let html = format!(
            "<html><body><div id='s' data-trust-node='99'{sig} style='height:96px;overflow-y:auto'>{links}</div></body></html>"
        );
        app.navigate_to(crate::http::parse(
            &url,
            "text/html",
            html.as_bytes(),
            80,
            10,
            &Default::default(),
        ));
        app
    }

    #[test]
    fn a_baked_scroll_top_signal_seeds_the_region_voffset() {
        // The page set `element.scrollTop`; the live serializer baked it (rows)
        // + the actor node id, and `flow_region` opens the region there (CSSOM
        // View — obey the page, no top-vs-bottom heuristic).
        let app = region_app_with_node(Some(7));
        let rg = &app.browser.as_ref().unwrap().doc.regions[0];
        assert_eq!(
            rg.voffset, 7,
            "the region opened at the page's signalled scrollTop"
        );
        assert_eq!(rg.live_node, Some(99), "the actor node id round-tripped");
        assert!(rg.voffset_from_page, "the offset came from the page signal");
    }

    #[test]
    fn a_baked_scroll_top_signal_clamps_to_the_content() {
        // A signal past the bottom (e.g. `scrollTop = scrollHeight` before the
        // app re-measured) clamps to `scrollHeight − clientHeight` = the bottom.
        let app = region_app_with_node(Some(9999));
        let rg = &app.browser.as_ref().unwrap().doc.regions[0];
        assert_eq!(rg.voffset, rg.max_voffset(), "clamped to the bottom");
    }

    #[test]
    fn carry_region_offsets_respects_the_page_signal_only_on_a_live_render() {
        use crate::layout::Region;
        let mk = |voffset: usize, from_page: bool| Region {
            node: 5,
            start_row: 0,
            left: 0,
            width: 10,
            height: 4,
            buffer: vec![crate::layout::Row::default(); 20], // max_voffset = 16
            voffset,
            live_node: Some(99),
            voffset_from_page: from_page,
            principal: false,
            carousels: Vec::new(),
            regions: Vec::new(),
            image_urls: Vec::new(),
        };
        let old = [(5usize, 9usize)];
        // Live render + the page dictated the offset → keep the fresh signal (3).
        let mut regions = [mk(3, true)];
        super::App::carry_region_offsets(&old, &mut regions, true);
        assert_eq!(regions[0].voffset, 3, "the page's signal is kept");
        // Live render, no page signal → restore the user's wheel offset (9).
        let mut regions = [mk(3, false)];
        super::App::carry_region_offsets(&old, &mut regions, true);
        assert_eq!(
            regions[0].voffset, 9,
            "un-signalled region restores the wheel offset"
        );
        // A resize (same HTML, stale signal) → the on-screen offset always wins.
        let mut regions = [mk(3, true)];
        super::App::carry_region_offsets(&old, &mut regions, false);
        assert_eq!(
            regions[0].voffset, 9,
            "resize preserves the on-screen position"
        );
        // The PRINCIPAL region is exempt from the signal override — the reader
        // scrolls it as "the page", so a live re-render restores the user's
        // offset (9) even though the page baked a fresh signal (the "kicked back
        // up" fix). Same lock `g.scroll` has.
        let mut principal = mk(3, true);
        principal.principal = true;
        let mut regions = [principal];
        super::App::carry_region_offsets(&old, &mut regions, true);
        assert_eq!(
            regions[0].voffset, 9,
            "the principal region ignores the page signal and stays where the reader put it"
        );
    }

    #[test]
    fn the_principal_region_is_scrolled_as_the_page() {
        // A locked-viewport app shell (Twitch) keeps its whole content in a
        // principal region, not in `rows`. The page-level scroll gestures drive
        // THAT region, and the document itself never moves.
        let mut app = region_app();
        assert_eq!(app.browser.as_ref().unwrap().doc.regions.len(), 1);
        // No principal region yet ⇒ the gesture isn't consumed (document scroll).
        assert!(
            !app.scroll_principal_region(3),
            "no principal region → the page-level gesture falls through to the document"
        );
        // Promote the region to the page's principal scroller.
        app.browser.as_mut().unwrap().doc.regions[0].principal = true;
        // A page key (PgDn/Home/End all route through http_scroll) pages it, and
        // the gesture is consumed — the document rows hold no scroll content.
        app.http_scroll(4);
        {
            let g = app.browser.as_ref().unwrap();
            assert_eq!(
                g.doc.regions[0].voffset, 4,
                "the key scrolled the principal region"
            );
            assert_eq!(g.scroll, 0, "the document stayed put");
            // The reader drove it ⇒ marked user-owned so a live re-render keeps it.
            assert!(
                !g.doc.regions[0].voffset_from_page,
                "a user scroll of the principal region is not a page signal"
            );
        }
        // End (a huge delta) clamps to the bottom, still on the region.
        app.http_scroll(i64::MAX / 2);
        let g = app.browser.as_ref().unwrap();
        assert_eq!(
            g.doc.regions[0].voffset,
            g.doc.regions[0].max_voffset(),
            "End clamps the principal region to its bottom"
        );
        assert_eq!(g.scroll, 0, "the document never moved");
    }

    #[test]
    fn a_wheel_off_the_principal_region_still_scrolls_it_as_the_page() {
        // A wheel over the chrome around the principal region (not over any
        // region band) scrolls the principal region — it IS "the page".
        let mut app = region_app();
        app.browser.as_mut().unwrap().doc.regions[0].principal = true;
        let (band_row, below) = {
            let g = app.browser.as_ref().unwrap();
            let rg = &g.doc.regions[0];
            let below = app.last_content_area.y + (rg.start_row + rg.height as usize + 1) as u16;
            (app.last_content_area.y + rg.start_row as u16, below)
        };
        // A wheel BELOW the region band (off it) drives the principal region.
        app.browser_wheel(2, 3, below);
        assert_eq!(
            app.browser.as_ref().unwrap().doc.regions[0].voffset,
            2,
            "an off-region wheel scrolls the principal region"
        );
        assert_eq!(app.browser.as_ref().unwrap().scroll, 0, "not the document");
        // A wheel INSIDE the band scrolls the same region directly (unchanged).
        app.browser_wheel(1, 3, band_row);
        assert_eq!(app.browser.as_ref().unwrap().doc.regions[0].voffset, 3);
    }

    #[test]
    fn a_user_wheel_writes_the_scroll_back_to_the_live_page() {
        // The region→page write-back (CSSOM View): a wheel over a live region
        // sends its new scrollTop back so a conditional pin learns we scrolled.
        let mut app = region_app_with_node(None);
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        app.live_page = Some(crate::js::PageHandle { cmds: tx });
        let start_row = app.browser.as_ref().unwrap().doc.regions[0].start_row;
        let rg_row = app.last_content_area.y + start_row as u16 + 1;
        app.on_mouse_event(mouse(
            crossterm::event::MouseEventKind::ScrollDown,
            3,
            rg_row,
        ));
        assert_eq!(app.browser.as_ref().unwrap().doc.regions[0].voffset, 3);
        match rx.try_recv().expect("a SetScroll write-back") {
            crate::js::PageCmd::SetScroll { node, top, .. } => {
                assert_eq!(node, 99);
                let cell_h = f64::from(app.picker.font_size().height);
                assert_eq!(top, 3.0 * cell_h, "scrollTop in px = voffset × cell height");
            }
            other => panic!("expected SetScroll, got {other:?}"),
        }
    }

    #[test]
    fn sync_region_state_pushes_geometry_then_dedups() {
        let mut app = region_app_with_node(None);
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        app.live_page = Some(crate::js::PageHandle { cmds: tx });
        app.sync_region_state();
        match rx.try_recv().expect("a RegionGeom push") {
            crate::js::PageCmd::RegionGeom { items } => {
                let (node, ch, _cw) = items[0];
                assert_eq!(node, 99);
                let cell_h = f64::from(app.picker.font_size().height);
                assert_eq!(ch, 6.0 * cell_h, "clientHeight = the 6-row viewport");
            }
            other => panic!("expected RegionGeom, got {other:?}"),
        }
        // Unchanged geometry isn't re-sent.
        app.sync_region_state();
        assert!(rx.try_recv().is_err(), "geometry deduped when unchanged");
    }

    #[test]
    fn apply_region_scroll_windows_the_region_from_the_page_signal() {
        let mut app = region_app_with_node(None);
        let cell_h = f64::from(app.picker.font_size().height);
        app.apply_region_scroll(99, 5.0 * cell_h);
        let rg = &app.browser.as_ref().unwrap().doc.regions[0];
        assert_eq!(rg.voffset, 5);
        assert!(rg.voffset_from_page);
        // A pin past the bottom clamps (CSSOM View).
        app.apply_region_scroll(99, 9999.0 * cell_h);
        let rg = &app.browser.as_ref().unwrap().doc.regions[0];
        assert_eq!(rg.voffset, rg.max_voffset(), "clamped to the bottom");
    }

    #[test]
    fn a_region_patch_swaps_only_the_buffer_leaving_doc_rows_untouched() {
        // INCREMENTAL_LAYOUT_PLAN.md Tier 1: a patch re-lays ONLY the region's
        // buffer; the document rows (the fixed band + everything outside) are
        // untouched, so nothing on the page moves.
        let mut app = region_app_with_node(None);
        let (rows_before, buf_before, region_w) = {
            let g = app.browser.as_ref().unwrap();
            let rg = &g.doc.regions[0];
            assert_eq!(rg.live_node, Some(99), "the region carries its actor node");
            (g.doc.rows.len(), rg.buffer.len(), rg.width)
        };
        // A patch replacing the region's 40 links with 2 new ones.
        let frag = r#"<div data-trust-frag=""><div data-trust-node="99" style="height:96px;overflow-y:auto"><div><a href="/N00">N00</a></div><div><a href="/N01">N01</a></div></div></div>"#;
        let patch = crate::js::SubtreePatch {
            node: 99,
            html: frag.to_string(),
            tier: crate::js::BoundaryTier::Size,
        };
        assert!(app.patch_live_doc(&patch), "the region patch applies");
        let g = app.browser.as_ref().unwrap();
        // The document row COUNT is unchanged — the band is invariant.
        assert_eq!(g.doc.rows.len(), rows_before, "doc rows untouched");
        let rg = &g.doc.regions[0];
        assert_eq!(rg.width, region_w, "band width unchanged");
        assert_ne!(rg.buffer.len(), buf_before, "the buffer was replaced");
        assert!(
            rg.buffer
                .iter()
                .flat_map(|r| &r.items)
                .any(|it| it.text.contains("N00")),
            "the patched content is in the new buffer"
        );
        assert!(
            !rg.buffer
                .iter()
                .flat_map(|r| &r.items)
                .any(|it| it.text.contains("L00")),
            "the old content is gone"
        );
        // A patch for a node that ISN'T a live region or cached boundary asks for
        // a resync (false).
        let bogus = crate::js::SubtreePatch {
            node: 12345,
            html: frag.to_string(),
            tier: crate::js::BoundaryTier::Size,
        };
        assert!(
            !app.patch_live_doc(&bogus),
            "an uncached boundary can't be patched"
        );
    }

    #[test]
    fn an_inline_boundary_patch_splices_like_a_full_relayout() {
        // INCREMENTAL_LAYOUT_PLAN.md §9/§14 (the splice-level differential): a
        // grown inline IFC boundary patched into Doc.rows is byte-for-byte the
        // same as a FULL re-layout of the mutated page — the patched box's rows
        // replace the old ones, and content OUTSIDE (HEADER above, FOOTER below)
        // is identity-shifted, never re-laid.
        let mut app = super::App::new(None, 23);
        app.mode = super::Mode::Session;
        app.last_inner = (80, 10);
        let url = url::Url::parse("https://example.com/").unwrap();
        let page = |lines: usize| -> String {
            let mut s = String::from(
                r#"<html><body><p>HEADER</p><div data-trust-node="42" style="display:flow-root">"#,
            );
            for i in 0..lines {
                s.push_str(&format!("<div>line{i}</div>"));
            }
            s.push_str("</div><p>FOOTER</p></body></html>");
            s
        };
        // Initial render (3 lines) captures Doc.boundaries[42].
        app.navigate_to(crate::http::parse(
            &url,
            "text/html",
            page(3).as_bytes(),
            80,
            10,
            &Default::default(),
        ));
        assert!(
            app.browser
                .as_ref()
                .unwrap()
                .doc
                .boundaries
                .iter()
                .any(|b| b.node == 42),
            "the flow-root boundary was captured at load"
        );
        let rows_before = app.browser.as_ref().unwrap().doc.rows.len();
        // Patch: the boundary grew to 5 lines (Tier 2 — height changed).
        let frag = r#"<div data-trust-frag=""><div data-trust-node="42" style="display:flow-root"><div>line0</div><div>line1</div><div>line2</div><div>line3</div><div>line4</div></div></div>"#;
        let patch = crate::js::SubtreePatch {
            node: 42,
            html: frag.to_string(),
            tier: crate::js::BoundaryTier::WidthStable,
        };
        assert!(
            app.patch_live_doc(&patch),
            "the inline boundary patch applies"
        );
        // The mutated page laid the full way is the oracle.
        let full = crate::http::parse(
            &url,
            "text/html",
            page(5).as_bytes(),
            80,
            10,
            &Default::default(),
        );
        let got = &app.browser.as_ref().unwrap().doc;
        assert_eq!(
            got.rows.len(),
            full.rows.len(),
            "row count matches a full relayout (Tier-2 shift added 2 rows)"
        );
        assert_eq!(got.rows.len(), rows_before + 2, "two lines were added");
        for (i, (g, f)) in got.rows.iter().zip(full.rows.iter()).enumerate() {
            assert_eq!(
                crate::layout::render_row(g),
                crate::layout::render_row(f),
                "spliced row {i} matches the full relayout"
            );
        }
        // The cached boundary box was repositioned to its new span.
        let b = got.boundaries.iter().find(|b| b.node == 42).unwrap();
        assert_eq!(b.row_range.len(), 5, "the boundary now spans 5 rows");
    }

    #[test]
    fn an_inline_boundary_tier1_patch_leaves_outside_rows_identical() {
        // INCREMENTAL_LAYOUT_PLAN.md §14 Tier 1: a content change that keeps the
        // box's HEIGHT splices in place — every row OUTSIDE the boundary is
        // byte-identical (nothing shifts).
        let mut app = super::App::new(None, 23);
        app.mode = super::Mode::Session;
        app.last_inner = (80, 10);
        let url = url::Url::parse("https://example.com/").unwrap();
        let html = r#"<html><body><p>HEADER</p><div data-trust-node="42" style="display:flow-root"><div>old0</div><div>old1</div></div><p>FOOTER</p></body></html>"#;
        app.navigate_to(crate::http::parse(
            &url,
            "text/html",
            html.as_bytes(),
            80,
            10,
            &Default::default(),
        ));
        let before: Vec<String> = app
            .browser
            .as_ref()
            .unwrap()
            .doc
            .rows
            .iter()
            .map(crate::layout::render_row)
            .collect();
        let footer_row = before
            .iter()
            .position(|r| r.contains("FOOTER"))
            .expect("footer present");
        // Same line COUNT, different text → Tier 1 (delta 0), no shift.
        let frag = r#"<div data-trust-frag=""><div data-trust-node="42" style="display:flow-root"><div>new0</div><div>new1</div></div></div>"#;
        let patch = crate::js::SubtreePatch {
            node: 42,
            html: frag.to_string(),
            tier: crate::js::BoundaryTier::WidthStable,
        };
        assert!(app.patch_live_doc(&patch), "the Tier-1 patch applies");
        let after: Vec<String> = app
            .browser
            .as_ref()
            .unwrap()
            .doc
            .rows
            .iter()
            .map(crate::layout::render_row)
            .collect();
        assert_eq!(after.len(), before.len(), "row count unchanged (delta 0)");
        assert_eq!(
            after[footer_row], before[footer_row],
            "FOOTER row is byte-identical (nothing outside moved)"
        );
        assert_eq!(after[0], before[0], "HEADER row is byte-identical");
        assert!(
            after.iter().any(|r| r.contains("new0")),
            "the new content was spliced in"
        );
        assert!(
            !after.iter().any(|r| r.contains("old0")),
            "the old content is gone"
        );
    }

    #[test]
    fn a_sub_box_boundary_patch_splices_like_a_full_relayout() {
        // INCREMENTAL_LAYOUT_PLAN.md §14 (the widening): a flex-COLUMN ITEM (a
        // sub-box, re-laid with subtree_root) that grows is patched into Doc.rows
        // byte-for-byte the same as a full re-layout — the sibling flex item below
        // it ("after") and the FOOTER are identity-shifted, never re-laid.
        let mut app = super::App::new(None, 23);
        app.mode = super::Mode::Session;
        app.last_inner = (80, 10);
        let url = url::Url::parse("https://example.com/").unwrap();
        let page = |rows: usize| -> String {
            let mut s = String::from(
                r#"<html><body><p>HEADER</p><div style="display:flex;flex-direction:column"><div>before</div><div data-trust-node="42">"#,
            );
            for i in 0..rows {
                s.push_str(&format!("<div>row{i}</div>"));
            }
            s.push_str(r#"</div><div>after</div></div><p>FOOTER</p></body></html>"#);
            s
        };
        app.navigate_to(crate::http::parse(
            &url,
            "text/html",
            page(2).as_bytes(),
            80,
            10,
            &Default::default(),
        ));
        let cached = app
            .browser
            .as_ref()
            .unwrap()
            .doc
            .boundaries
            .iter()
            .find(|b| b.node == 42)
            .cloned();
        assert!(
            cached.as_ref().is_some_and(|b| b.sub_box),
            "the flex-column item was captured as a sub-box boundary"
        );
        // Patch: the item grew from 2 to 4 rows (same width → Tier-2 shift).
        let frag = r#"<div data-trust-frag=""><div data-trust-node="42"><div>row0</div><div>row1</div><div>row2</div><div>row3</div></div></div>"#;
        let patch = crate::js::SubtreePatch {
            node: 42,
            html: frag.to_string(),
            tier: crate::js::BoundaryTier::WidthStable,
        };
        assert!(app.patch_live_doc(&patch), "the sub-box patch applies");
        let full = crate::http::parse(
            &url,
            "text/html",
            page(4).as_bytes(),
            80,
            10,
            &Default::default(),
        );
        let got = &app.browser.as_ref().unwrap().doc.rows;
        assert_eq!(
            got.len(),
            full.rows.len(),
            "row count matches a full relayout"
        );
        for (i, (g, f)) in got.iter().zip(full.rows.iter()).enumerate() {
            assert_eq!(
                crate::layout::render_row(g),
                crate::layout::render_row(f),
                "spliced row {i} matches the full relayout"
            );
        }
    }

    /// A list-boundary fixture spanning far past the viewport: HEADER, an IFC
    /// boundary (node 42) of `n` one-row link items, then FOOTER.
    fn infinite_list_app(n: usize) -> (super::App, url::Url) {
        let mut app = super::App::new(None, 23);
        app.mode = super::Mode::Session;
        app.last_inner = (80, 10);
        let url = url::Url::parse("https://example.com/").unwrap();
        let mut s = String::from(
            r#"<html><body><p>HEADER</p><div data-trust-node="42" style="display:flow-root">"#,
        );
        for i in 0..n {
            s.push_str(&format!(r#"<div><a href="/v/{i}">item{i}</a></div>"#));
        }
        s.push_str("</div><p>FOOTER</p></body></html>");
        app.navigate_to(crate::http::parse(
            &url,
            "text/html",
            s.as_bytes(),
            80,
            10,
            &Default::default(),
        ));
        (app, url)
    }

    fn list_patch(n: usize) -> crate::js::SubtreePatch {
        let mut frag = String::from(
            r#"<div data-trust-frag=""><div data-trust-node="42" style="display:flow-root">"#,
        );
        for i in 0..n {
            frag.push_str(&format!(r#"<div><a href="/v/{i}">item{i}</a></div>"#));
        }
        frag.push_str("</div></div>");
        crate::js::SubtreePatch {
            node: 42,
            html: frag,
            tier: crate::js::BoundaryTier::WidthStable,
        }
    }

    #[test]
    fn infinite_scroll_append_below_does_not_drag_the_viewport() {
        // THE suck-down regression (tilvids/peertube, archive grids): an
        // infinite-scroll list is an IFC boundary that STARTS above the viewport
        // and EXTENDS below it. Appending a batch at its BOTTOM must leave the
        // reader's viewport exactly where it is. The old anchor heuristic shifted
        // `scroll` by the boundary's WHOLE growth whenever it merely started above
        // the viewport — pinning the reader to the new bottom every load, which
        // re-fired the at-bottom infinite-scroll anchor into a runaway "suck
        // down". CSS Scroll Anchoring keys on the CONTENT the reader sees, and an
        // append below it doesn't move that content.
        let (mut app, _url) = infinite_list_app(40);
        let b = app
            .browser
            .as_ref()
            .unwrap()
            .doc
            .boundaries
            .iter()
            .find(|b| b.node == 42)
            .cloned()
            .expect("the list boundary was captured at load");
        // Park the viewport wholly inside the list (top inside the boundary).
        let scroll = b.row_range.start + 18;
        assert!(
            scroll > b.row_range.start && scroll + 10 < b.row_range.end,
            "the 10-row viewport sits wholly inside the list ({:?})",
            b.row_range
        );
        app.browser.as_mut().unwrap().scroll = scroll;
        app.scroll_intent = scroll;
        let top_before = crate::layout::render_row(&app.browser.as_ref().unwrap().doc.rows[scroll]);
        // The list appends a batch at the bottom (40 → 60).
        assert!(
            app.patch_live_doc(&list_patch(60)),
            "the append patch applies"
        );
        assert_eq!(
            app.browser.as_ref().unwrap().scroll,
            scroll,
            "the viewport did NOT drift when content appended below it"
        );
        assert_eq!(
            app.scroll_intent, scroll,
            "the scroll intent held too (no runaway re-anchor to the new bottom)"
        );
        assert_eq!(
            crate::layout::render_row(&app.browser.as_ref().unwrap().doc.rows[scroll]),
            top_before,
            "the reader still sees the same item at the viewport top"
        );
    }

    #[test]
    fn content_growth_above_the_viewport_shifts_scroll_to_keep_it_pinned() {
        // The complement (the legitimate scroll-anchoring case the old heuristic
        // existed for, preserved): when a boundary ENTIRELY ABOVE the viewport
        // grows, the content below it (what the reader sees) moves down, so
        // `scroll` shifts by the same amount to keep it visually fixed.
        let mut app = super::App::new(None, 23);
        app.mode = super::Mode::Session;
        app.last_inner = (80, 10);
        let url = url::Url::parse("https://example.com/").unwrap();
        let page = |n: usize| -> String {
            let mut s =
                String::from(r#"<html><body><div data-trust-node="42" style="display:flow-root">"#);
            for i in 0..n {
                s.push_str(&format!("<div>top{i}</div>"));
            }
            s.push_str("</div>");
            for i in 0..40 {
                s.push_str(&format!("<div>filler{i}</div>"));
            }
            s.push_str("</body></html>");
            s
        };
        app.navigate_to(crate::http::parse(
            &url,
            "text/html",
            page(3).as_bytes(),
            80,
            10,
            &Default::default(),
        ));
        let b = app
            .browser
            .as_ref()
            .unwrap()
            .doc
            .boundaries
            .iter()
            .find(|b| b.node == 42)
            .cloned()
            .expect("the top boundary was captured");
        // Park the viewport in the filler, wholly BELOW the boundary.
        let scroll = b.row_range.end + 8;
        app.browser.as_mut().unwrap().scroll = scroll;
        app.scroll_intent = scroll;
        let top_before = crate::layout::render_row(&app.browser.as_ref().unwrap().doc.rows[scroll]);
        // The top boundary grows 3 → 6 (three rows inserted above the viewport).
        let mut frag = String::from(
            r#"<div data-trust-frag=""><div data-trust-node="42" style="display:flow-root">"#,
        );
        for i in 0..6 {
            frag.push_str(&format!("<div>top{i}</div>"));
        }
        frag.push_str("</div></div>");
        let patch = crate::js::SubtreePatch {
            node: 42,
            html: frag,
            tier: crate::js::BoundaryTier::WidthStable,
        };
        assert!(app.patch_live_doc(&patch), "the growth patch applies");
        let new_scroll = app.browser.as_ref().unwrap().scroll;
        assert_eq!(
            new_scroll,
            scroll + 3,
            "scroll shifted down by the growth so the reader's content stays put"
        );
        assert_eq!(
            crate::layout::render_row(&app.browser.as_ref().unwrap().doc.rows[new_scroll]),
            top_before,
            "the same filler line is still at the viewport top"
        );
    }

    #[tokio::test]
    async fn a_full_rerender_pins_the_viewport_to_what_the_reader_sees() {
        // CSS Scroll Anchoring through the FULL-replace path (the archive.org
        // "bounce up at the end" family): a live re-render that inserts content
        // ABOVE the viewport keeps the content the reader sees visually fixed (it
        // does NOT keep the raw scroll number, which would shove the page up under
        // them). Same mechanism re-pins the viewport-top item across a transient
        // shrink/regrow instead of bouncing it.
        let url = url::Url::parse("https://example.com/").unwrap();
        let body = |extra: usize| -> String {
            let mut s = String::from("<body>");
            for i in 0..extra {
                s.push_str(&format!(r#"<div><a href="/x/{i}">x{i}</a></div>"#));
            }
            for i in 0..40 {
                s.push_str(&format!(r#"<div><a href="/v/{i}">item{i}</a></div>"#));
            }
            s.push_str("</body>");
            s
        };
        let mut app = super::App::new(None, 23);
        app.mode = super::Mode::Session;
        app.last_inner = (80, 10);
        app.navigate_to(crate::http::parse(
            &url,
            "text/html",
            body(0).as_bytes(),
            80,
            10,
            &Default::default(),
        ));
        let scroll = 20usize;
        app.browser.as_mut().unwrap().scroll = scroll;
        app.scroll_intent = scroll;
        let top_before = crate::layout::render_row(&app.browser.as_ref().unwrap().doc.rows[scroll]);
        // A re-render inserts 5 items above the viewport.
        app.replace_live_doc(body(5).into_bytes());
        let new_scroll = app.browser.as_ref().unwrap().scroll;
        assert_eq!(
            new_scroll,
            scroll + 5,
            "scroll shifted by the inserted-above count to keep the reader's content"
        );
        assert_eq!(
            crate::layout::render_row(&app.browser.as_ref().unwrap().doc.rows[new_scroll]),
            top_before,
            "the same item is still at the viewport top after the re-render"
        );
    }

    #[tokio::test]
    async fn an_anchor_adjustment_moves_the_scroll_intent_with_it() {
        // CSS Scroll Anchoring: the anchoring adjustment IS the new scroll
        // position, so `scroll_intent` must move with it. The full-replace
        // path used to pin `scroll` but leave the intent stale; the NEXT
        // update's intent-restore (`patch_live_boundary`'s shift_by==0 arm,
        // or another render's lost-anchor fallback) then snapped the view
        // back — on a Mastodon feed the viewport oscillated between the
        // pinned and stale rows with no user input.
        let url = url::Url::parse("https://example.com/").unwrap();
        let body = |extra: usize| -> String {
            let mut s = String::from("<body>");
            for i in 0..extra {
                s.push_str(&format!(r#"<div><a href="/x/{i}">x{i}</a></div>"#));
            }
            for i in 0..40 {
                s.push_str(&format!(r#"<div><a href="/v/{i}">item{i}</a></div>"#));
            }
            s.push_str("</body>");
            s
        };
        let mut app = super::App::new(None, 23);
        app.mode = super::Mode::Session;
        app.last_inner = (80, 10);
        app.navigate_to(crate::http::parse(
            &url,
            "text/html",
            body(0).as_bytes(),
            80,
            10,
            &Default::default(),
        ));
        let scroll = 20usize;
        app.browser.as_mut().unwrap().scroll = scroll;
        app.scroll_intent = scroll;
        // A re-render inserts 5 items above the viewport: the pin shifts
        // scroll by 5, and the intent must ride along.
        app.replace_live_doc(body(5).into_bytes());
        assert_eq!(app.browser.as_ref().unwrap().scroll, scroll + 5);
        assert_eq!(
            app.scroll_intent,
            scroll + 5,
            "the intent moves with the anchoring adjustment"
        );
    }

    #[tokio::test]
    async fn an_image_decode_reflow_keeps_the_readers_content_pinned() {
        // CSS Scroll Anchoring through the image-decode reflow: a decoded
        // image ABOVE the viewport grows from its one-row alt line to its
        // real box; the content the reader sees must stay visually fixed
        // (`relayout_browser` used to keep the raw scroll number, shoving
        // the page down under them on every decode burst).
        let url = url::Url::parse("https://example.com/").unwrap();
        let mut body = String::from(r#"<body><img src="/big.png" alt="big">"#);
        for i in 0..40 {
            body.push_str(&format!(r#"<div><a href="/v/{i}">item{i}</a></div>"#));
        }
        body.push_str("</body>");
        let mut app = super::App::new(None, 23);
        app.mode = super::Mode::Session;
        app.last_inner = (80, 10);
        app.navigate_to(crate::http::parse(
            &url,
            "text/html",
            body.as_bytes(),
            80,
            10,
            &Default::default(),
        ));
        let scroll = 20usize;
        app.browser.as_mut().unwrap().scroll = scroll;
        app.scroll_intent = scroll;
        let top_before = crate::layout::render_row(&app.browser.as_ref().unwrap().doc.rows[scroll]);
        // The image decodes at 10×7 cells (+6 rows above the viewport).
        app.image_sizes
            .insert(String::from("https://example.com/big.png"), (10, 7));
        app.relayout_browser();
        let new_scroll = app.browser.as_ref().unwrap().scroll;
        assert_eq!(
            crate::layout::render_row(&app.browser.as_ref().unwrap().doc.rows[new_scroll]),
            top_before,
            "the same item is still at the viewport top after the decode reflow"
        );
        assert!(
            new_scroll > scroll,
            "the pin shifted scroll down past the grown image"
        );
        assert_eq!(
            app.scroll_intent, new_scroll,
            "the intent moves with the adjustment"
        );
    }

    #[tokio::test]
    async fn the_scroll_anchor_picks_the_nearest_duplicate_link_not_the_first() {
        // archive.org's "snaps up a section, never reaches the bottom" bug: it
        // lists the same collection in a featured row AND the main grid, so the
        // viewport-top tile's link is NOT unique. The scroll anchor must re-find
        // the occurrence CLOSEST to where the reader was; taking the FIRST
        // (higher) copy yanked the whole page up to an unrelated tile every
        // re-render, so the reader could never reach the bottom (and the
        // infinite-scroll sentinel there never fired).
        let url = url::Url::parse("https://example.com/").unwrap();
        let body = || {
            let mut s = String::from("<body>");
            // an earlier copy of the duplicated link (a "featured" row)
            s.push_str(r#"<div><a href="/dup">dup-featured</a></div>"#);
            for i in 1..25 {
                s.push_str(&format!(r#"<div><a href="/b{i}">b{i}</a></div>"#));
            }
            // the same link again, deep in the doc (the main grid) — row 25
            s.push_str(r#"<div><a href="/dup">dup-grid</a></div>"#);
            for i in 26..60 {
                s.push_str(&format!(r#"<div><a href="/b{i}">b{i}</a></div>"#));
            }
            s.push_str("</body>");
            s
        };
        let mut app = super::App::new(None, 23);
        app.mode = super::Mode::Session;
        app.last_inner = (80, 10);
        app.navigate_to(crate::http::parse(
            &url,
            "text/html",
            body().as_bytes(),
            80,
            10,
            &Default::default(),
        ));
        let scroll = 25usize; // the viewport top sits on the SECOND `/dup`
        assert_eq!(
            app.browser.as_ref().unwrap().doc.rows[scroll].items[0]
                .text
                .as_str(),
            "dup-grid",
            "fixture: the reader is parked on the grid copy of the duplicated link"
        );
        app.browser.as_mut().unwrap().scroll = scroll;
        app.scroll_intent = scroll;
        // A re-render with identical content (an image-decode reflow on archive).
        app.replace_live_doc(body().into_bytes());
        assert_eq!(
            app.browser.as_ref().unwrap().scroll,
            scroll,
            "the viewport stayed on the NEAR duplicate, not snapped up to the first copy"
        );
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
        app.navigate_to(crate::http::parse(&url, "text/html", html, 80, 0, &images));

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
    fn a_pinned_fixed_rail_link_hovers_and_resolves_at_its_viewport_position() {
        // A `position:fixed` rail (Mastodon's side nav) is captured into the
        // pinned overlay; a hover over it (SCREEN position, not scroll-offset)
        // selects the rail link so a click activates it.
        let mut app = super::App::new(None, 23);
        app.mode = super::Mode::Session;
        app.last_inner = (80, 20);
        app.last_content_area = ratatui::layout::Rect::new(0, 1, 80, 20);
        let url = url::Url::parse("https://example.com/").unwrap();
        let images = crate::layout::ImageSizes::new();
        let html = b"<body><div style='display:flex;justify-content:center'>\
            <div style='min-width:120px'>\
              <div style='position:fixed;width:120px'><a href='/dest'>RAILLINK</a></div>\
            </div>\
            <main style='width:100px'>FEED</main>\
          </div></body>";
        app.navigate_to(crate::http::parse(&url, "text/html", html, 80, 20, &images));

        let g = app.browser.as_ref().unwrap();
        assert!(!g.doc.fixed.is_empty(), "a fixed rail was captured");
        // Locate the RAILLINK item + its screen position in the fixed layer.
        let mut hit = None;
        for (fi, f) in g.doc.fixed.iter().enumerate() {
            for (r, row) in f.rows.iter().enumerate() {
                for (i, it) in row.items.iter().enumerate() {
                    if it.text.contains("RAILLINK") && it.link.is_some() {
                        hit = Some((fi, r, i, f.col + it.col, f.row + r as u16));
                    }
                }
            }
        }
        let (fi, r, i, col_in_box, row_in_box) = hit.expect("RAILLINK captured with a link");
        let sx = app.last_content_area.x + col_in_box;
        let sy = app.last_content_area.y + row_in_box;

        app.on_mouse_event(mouse(crossterm::event::MouseEventKind::Moved, sx, sy));
        assert_eq!(
            app.browser.as_ref().unwrap().sel_fixed,
            Some((fi, r, i)),
            "hover selects the pinned rail link at its screen position"
        );
        assert_eq!(app.browser.as_ref().unwrap().sel_item, None);
        // The click path (browser_follow) resolves through selected_link.
        assert!(
            matches!(app.selected_link(), Some(crate::doc::Link::Http(u)) if u.as_str() == "https://example.com/dest"),
            "the rail link resolves to its href: {:?}",
            app.selected_link()
        );

        // KEYBOARD reaches the rail too: with no document link to take, an arrow
        // step enters the pinned rail (same link, same activation path).
        app.browser.as_mut().unwrap().sel_fixed = None;
        app.http_move(1, false);
        assert_eq!(
            app.browser.as_ref().unwrap().sel_fixed,
            Some((fi, r, i)),
            "an arrow key selects the pinned rail link"
        );
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
            0,
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

    #[test]
    fn top_bar_click_prefills_the_current_address() {
        use crossterm::event::{MouseButton, MouseEventKind};
        let mut app = super::App::new(None, 23);
        app.mode = super::Mode::Session;
        app.last_inner = (80, 10);
        app.last_content_area = ratatui::layout::Rect::new(2, 1, 80, 10);
        app.last_status_row = 13;
        let click = |row| mouse(MouseEventKind::Down(MouseButton::Left), 5, row);

        // A browser page on screen: clicking the top title row opens the
        // console prefilled with the page's address, cursor at the end — an
        // editable address bar.
        app.navigate_to(gopher_doc("xxx"));
        let addr = app.browser.as_ref().unwrap().doc.url.to_string();
        assert!(!addr.is_empty());
        app.on_mouse_event(click(0));
        assert_eq!(app.mode, super::Mode::Command);
        assert_eq!(app.input, addr);
        assert_eq!(app.cursor, addr.chars().count());

        // The bottom status line still opens the console without prefilling.
        app.mode = super::Mode::Session;
        app.input.clear();
        app.cursor = 0;
        app.on_mouse_event(click(13));
        assert_eq!(app.mode, super::Mode::Command);
        assert!(app.input.is_empty());
    }

    /// A trail entry holding `doc` (as every freshly-parked entry does),
    /// with an empty saved selection.
    fn entry(doc: crate::doc::Doc, was_live: bool, scroll: usize) -> super::HistEntry {
        super::HistEntry {
            url: doc.url.clone(),
            pos: super::ViewPos {
                selected: None,
                sel_item: None,
                was_live,
            },
            scroll,
            post: false,
            doc: Some(doc),
        }
    }

    fn http_doc(path: &str) -> crate::doc::Doc {
        let url = url::Url::parse(&format!("https://example.com{path}")).unwrap();
        crate::http::parse(
            &url,
            "text/html",
            b"<body><p>hi</p></body>",
            80,
            0,
            &Default::default(),
        )
    }

    /// Like `http_doc`, with an `<img>` so the doc references an image URL.
    fn http_doc_img(path: &str, img: &str) -> crate::doc::Doc {
        let url = url::Url::parse(&format!("https://example.com{path}")).unwrap();
        crate::http::parse(
            &url,
            "text/html",
            format!(r#"<body><p>hi</p><img src="{img}" alt="pic"></body>"#).as_bytes(),
            80,
            0,
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
            80, 0,
            &images,
        );
        let g = super::BrowserView {
            doc,
            selected: None,
            sel_item: None,
            sel_fixed: None,
            scroll: 0,
            history: vec![],
            forward: vec![],
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
        let doc = crate::http::parse(&url, mime, body.as_bytes(), 80, 0, &images);
        let mut app = super::App::new(None, 23);
        app.mode = super::Mode::Session;
        app.browser = Some(super::BrowserView {
            doc,
            selected: None,
            sel_item: None,
            sel_fixed: None,
            scroll: 0,
            history: vec![],
            forward: vec![],
        });
        app
    }

    #[test]
    fn push_text_matches_is_case_insensitive_and_nonoverlapping() {
        use super::FindLoc;
        let q: Vec<char> = "foo".chars().collect();
        let mut out = Vec::new();
        super::push_text_matches("Foo foo fOo bar", &q, FindLoc::Line(7), &mut out);
        assert_eq!(out.len(), 3);
        assert_eq!((out[0].start, out[0].end), (0, 3));
        assert_eq!((out[1].start, out[1].end), (4, 7));
        assert_eq!((out[2].start, out[2].end), (8, 11));
        assert_eq!(out[0].loc, FindLoc::Line(7));
        // Overlapping query advances past each hit (no double-count).
        let q: Vec<char> = "aa".chars().collect();
        let mut out = Vec::new();
        super::push_text_matches("aaaa", &q, FindLoc::Line(0), &mut out);
        assert_eq!(out.len(), 2);
        // The byte-length fast path can't reject a match it shouldn't: a
        // multi-byte text shorter in chars than in bytes still scans.
        let q: Vec<char> = "héllo".chars().collect();
        let mut out = Vec::new();
        super::push_text_matches("say Héllo", &q, FindLoc::Line(0), &mut out);
        assert_eq!(out.len(), 1, "non-ASCII exact-case match still found");
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
        assert_eq!(
            f.matches.iter().map(|m| m.loc).collect::<Vec<_>>(),
            [
                super::FindLoc::Line(0),
                super::FindLoc::Line(1),
                super::FindLoc::Line(2)
            ]
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
        assert!(
            f.matches
                .iter()
                .all(|m| matches!(m.loc, super::FindLoc::Item { .. }))
        );
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

    #[test]
    fn keyboard_walks_links_inside_a_scroll_region() {
        let mut app = region_app();
        // The fresh page selected its first interactive: region content
        // (reached through the effective row, like the mouse).
        let g = app.browser.as_ref().unwrap();
        let (cr, ci) = g.sel_item.expect("initial selection");
        assert!(
            matches!(
                crate::layout::item_origin(&g.doc.rows, &g.doc.regions, cr, ci),
                crate::layout::ItemOrigin::Region { .. }
            ),
            "keyboard selection reaches region content"
        );
        assert!(
            matches!(app.selected_link(), Some(Link::Http(u)) if u.path() == "/L00"),
            "starts on the first buffered link: {:?}",
            app.selected_link()
        );

        // Walk deeper than the window: the region auto-scrolls under the
        // selection instead of the selection skipping past the region.
        for _ in 0..10 {
            app.http_move(1, false);
        }
        let g = app.browser.as_ref().unwrap();
        let rg = &g.doc.regions[0];
        assert!(
            matches!(app.selected_link(), Some(Link::Http(u)) if u.path() == "/L10"),
            "ten steps land on the tenth link: {:?}",
            app.selected_link()
        );
        assert!(rg.voffset > 0, "the window scrolled to follow");
        assert!(!rg.voffset_from_page, "keyboard owns the offset now");

        // Walking back up returns to the top of the buffer.
        for _ in 0..10 {
            app.http_move(-1, false);
        }
        let g = app.browser.as_ref().unwrap();
        assert!(
            matches!(app.selected_link(), Some(Link::Http(u)) if u.path() == "/L00"),
            "the walk retraces to the first link: {:?}",
            app.selected_link()
        );
        assert_eq!(
            g.doc.regions[0].voffset, 0,
            "window followed back to the top"
        );
    }

    #[test]
    fn find_searches_a_regions_whole_buffer_and_scrolls_it() {
        use super::FindLoc;
        let mut app = region_app();
        app.open_find();
        app.input = String::from("l35"); // deep in the buffer, case-folded
        app.cursor = 3;
        app.recompute_find();

        let f = app.find.as_ref().unwrap();
        assert_eq!(f.matches.len(), 1, "the whole buffer is searched");
        let FindLoc::Region {
            region,
            brow,
            bitem,
        } = f.matches[0].loc
        else {
            panic!("expected a region match");
        };
        assert_eq!(region, 0);

        // Revealed: the region scrolled its window onto the match …
        let g = app.browser.as_ref().unwrap();
        let rg = &g.doc.regions[0];
        assert!(
            rg.voffset <= brow && brow < rg.voffset + rg.height as usize,
            "buffer row {brow} inside window at voffset {}",
            rg.voffset
        );
        assert!(!rg.voffset_from_page, "find owns the offset like a wheel");
        // … and the doc scrolled the band row into the viewport.
        let band = rg.start_row + (brow - rg.voffset);
        let height = app.last_inner.1 as usize;
        assert!(
            g.scroll <= band && band < g.scroll + height,
            "band row {band} visible at scroll {}",
            g.scroll
        );

        // The renderer's origin translation reaches the same item, so the
        // highlight lands on the matched text.
        let row = crate::layout::effective_row(&g.doc.rows, &g.doc.regions, band);
        let hit = row.items.iter().enumerate().find(|(i, _)| {
            matches!(
                crate::layout::item_origin(&g.doc.rows, &g.doc.regions, band, *i),
                crate::layout::ItemOrigin::Region { region: r, brow: b, bitem: bi }
                    if r == region && b == brow && bi == bitem
            )
        });
        assert!(
            hit.is_some_and(|(_, it)| it.text.contains("L35")),
            "origin maps back to the matched region item"
        );
    }

    #[test]
    fn find_matches_on_a_pinned_rail_without_scrolling() {
        let mut app = super::App::new(None, 23);
        app.mode = super::Mode::Session;
        app.last_inner = (80, 20);
        let url = url::Url::parse("https://example.com/").unwrap();
        let html = b"<body><div style='display:flex;justify-content:center'>\
            <div style='min-width:120px'>\
              <div style='position:fixed;width:120px'><a href='/dest'>RAILLINK</a></div>\
            </div>\
            <main style='width:100px'>FEED</main>\
          </div></body>";
        app.navigate_to(crate::http::parse(
            &url,
            "text/html",
            html,
            80,
            20,
            &Default::default(),
        ));
        assert!(!app.browser.as_ref().unwrap().doc.fixed.is_empty());

        app.open_find();
        app.input = String::from("raillink");
        app.cursor = 8;
        app.recompute_find();
        let f = app.find.as_ref().unwrap();
        assert_eq!(f.matches.len(), 1, "rail text is searched");
        assert!(matches!(f.matches[0].loc, super::FindLoc::Fixed { .. }));

        // A pinned match never scrolls the document (it's always on screen).
        let before = app.browser.as_ref().unwrap().scroll;
        app.find_next();
        assert_eq!(app.browser.as_ref().unwrap().scroll, before);
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
            sel_fixed: None,
            scroll: 0,
            history: vec![entry(http_doc("/a"), false, 7)],
            forward: vec![],
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
            sel_fixed: None,
            scroll: 0,
            history: vec![entry(http_doc("/a"), true, 0)],
            forward: vec![],
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
            sel_fixed: None,
            scroll: 0,
            history: vec![entry(http_doc("/a"), false, 0)],
            forward: vec![],
        });

        app.browser_back();

        assert!(!app.replace_nav, "static back does not reload");
        assert!(app.fetch_rx.is_none(), "no fetch for a static back");
    }

    #[test]
    fn mouse5_goes_forward_in_browser_history() {
        let mut app = super::App::new(None, 23);
        app.mode = super::Mode::Session;
        app.browser = Some(super::BrowserView {
            doc: http_doc("/b"),
            selected: None,
            sel_item: None,
            sel_fixed: None,
            scroll: 0,
            history: vec![],
            forward: vec![entry(http_doc("/c"), false, 3)],
        });

        app.on_mouse_event(mouse(
            crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Mouse5),
            0,
            0,
        ));

        let g = app.browser.as_ref().unwrap();
        assert!(
            matches!(&g.doc.url, Link::Http(u) if u.path() == "/c"),
            "Mouse5 should navigate forward to the parked page"
        );
        assert_eq!(g.scroll, 3);
        assert!(
            matches!(&g.history.last().unwrap().url, Link::Http(u) if u.path() == "/b"),
            "the page we left goes onto the back stack"
        );
    }

    #[test]
    fn back_then_forward_round_trips() {
        // Back parks the current doc on the forward stack (it used to be
        // dropped outright); forward restores it, position and all.
        let mut app = super::App::new(None, 23);
        app.browser = Some(super::BrowserView {
            doc: http_doc("/b"),
            selected: None,
            sel_item: Some((0, 0)),
            sel_fixed: None,
            scroll: 5,
            history: vec![entry(http_doc("/a"), false, 0)],
            forward: vec![],
        });

        app.browser_back();
        {
            let g = app.browser.as_ref().unwrap();
            assert!(matches!(&g.doc.url, Link::Http(u) if u.path() == "/a"));
            assert_eq!(g.forward.len(), 1, "back parks the doc for forward");
            assert!(g.history.is_empty());
        }

        app.browser_forward();
        let g = app.browser.as_ref().unwrap();
        assert!(matches!(&g.doc.url, Link::Http(u) if u.path() == "/b"));
        assert_eq!(g.scroll, 5, "forward restores the scroll we left at");
        assert_eq!(g.sel_item, Some((0, 0)), "and the selection");
        assert!(g.forward.is_empty());
        assert_eq!(g.history.len(), 1, "/a is reachable by back again");
    }

    #[test]
    fn new_navigation_clears_forward_history_but_reload_keeps_it() {
        let mut app = super::App::new(None, 23);
        app.navigate_to(http_doc("/a"));
        app.navigate_to(http_doc("/b"));
        app.browser_back();
        assert_eq!(app.browser.as_ref().unwrap().forward.len(), 1);

        // A reload (replace-flagged, also the revive-on-back path) swaps in
        // place and must not abandon the forward branch.
        app.replace_nav = true;
        app.navigate_to(http_doc("/a"));
        assert_eq!(app.browser.as_ref().unwrap().forward.len(), 1);

        // A real navigation from a mid-history position truncates it.
        app.navigate_to(http_doc("/d"));
        let g = app.browser.as_ref().unwrap();
        assert!(g.forward.is_empty(), "navigating abandons the branch");
    }

    #[tokio::test]
    async fn forward_to_a_live_page_revives_it() {
        // Same revive rule as back: a page that had a JS engine when we
        // backed away from it is reloaded on forward, not shown frozen.
        let mut app = super::App::new(None, 23);
        app.js_enabled = true;
        app.browser = Some(super::BrowserView {
            doc: http_doc("/a"),
            selected: None,
            sel_item: None,
            sel_fixed: None,
            scroll: 0,
            history: vec![],
            forward: vec![entry(http_doc("/b"), true, 0)],
        });

        app.browser_forward();

        assert!(app.replace_nav, "revive reloads in place");
        assert!(app.status.contains("Reviving"), "status: {}", app.status);
        assert!(app.fetch_rx.is_some(), "a revive fetch was started");
        assert!(
            matches!(&app.browser.as_ref().unwrap().doc.url, Link::Http(u) if u.path() == "/b"),
            "the parked page shows (static) while reviving",
        );
    }

    #[test]
    fn alt_arrows_travel_history_in_both_nav_models() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = super::App::new(None, 23);
        app.navigate_to(http_doc("/a"));
        app.navigate_to(http_doc("/b"));

        // HTTP 2D model: Alt-Left back, Alt-Right forward (plain arrows
        // keep moving the selection).
        app.http_nav(KeyEvent::new(KeyCode::Left, KeyModifiers::ALT));
        assert!(
            matches!(&app.browser.as_ref().unwrap().doc.url, Link::Http(u) if u.path() == "/a")
        );
        app.http_nav(KeyEvent::new(KeyCode::Right, KeyModifiers::ALT));
        assert!(
            matches!(&app.browser.as_ref().unwrap().doc.url, Link::Http(u) if u.path() == "/b")
        );

        // gopherus line model: plain Left is already back; Alt-Right goes
        // forward instead of following the selected link.
        app.browser_nav(KeyEvent::new(KeyCode::Left, KeyModifiers::empty()));
        assert!(
            matches!(&app.browser.as_ref().unwrap().doc.url, Link::Http(u) if u.path() == "/a")
        );
        app.browser_nav(KeyEvent::new(KeyCode::Right, KeyModifiers::ALT));
        assert!(
            matches!(&app.browser.as_ref().unwrap().doc.url, Link::Http(u) if u.path() == "/b")
        );
    }

    #[test]
    fn forward_on_an_empty_stack_is_a_polite_no_op() {
        let mut app = super::App::new(None, 23);
        app.navigate_to(http_doc("/a"));
        app.browser_forward();
        let g = app.browser.as_ref().unwrap();
        assert!(matches!(&g.doc.url, Link::Http(u) if u.path() == "/a"));
        assert!(app.status.contains("Nothing forward"), "{}", app.status);
    }

    #[test]
    fn deep_trail_docs_are_evicted_but_the_trail_remains() {
        let mut app = super::App::new(None, 23);
        app.navigate_to(http_doc("/a"));
        app.navigate_to(http_doc("/b"));
        app.navigate_to(http_doc("/c"));
        let g = app.browser.as_ref().unwrap();
        assert_eq!(g.history.len(), 2, "the trail keeps every entry");
        assert!(g.history[0].doc.is_none(), "depth-2 doc is evicted");
        assert!(matches!(&g.history[0].url, Link::Http(u) if u.path() == "/a"));
        assert!(g.history[1].doc.is_some(), "the adjacent doc is retained");
    }

    #[tokio::test]
    async fn deep_back_refetches_and_completes_on_arrival() {
        let mut app = super::App::new(None, 23);
        app.navigate_to(http_doc("/a"));
        app.navigate_to(http_doc("/b"));
        app.navigate_to(http_doc("/c"));
        app.browser_back(); // adjacent: instant restore of /b
        assert!(
            matches!(&app.browser.as_ref().unwrap().doc.url, Link::Http(u) if u.path() == "/b")
        );
        assert!(app.fetch_rx.is_none(), "adjacent back needs no fetch");

        app.browser_back(); // deep: /a was evicted → refetch
        assert!(app.fetch_rx.is_some(), "deep back starts a refetch");
        assert_eq!(app.pending_travel, Some(false));
        let g = app.browser.as_ref().unwrap();
        assert!(
            matches!(&g.doc.url, Link::Http(u) if u.path() == "/b"),
            "the current page stays until the refetch lands"
        );
        assert_eq!(g.history.len(), 1, "trail untouched while in flight");
        assert_eq!(g.forward.len(), 1);

        // The refetched doc lands (simulated): the travel completes — /b
        // parks on forward, /a shows, and the old forward top (/c) just
        // became non-adjacent, so its doc drops to trail-only.
        app.navigate_to(http_doc("/a"));
        let g = app.browser.as_ref().unwrap();
        assert!(matches!(&g.doc.url, Link::Http(u) if u.path() == "/a"));
        assert!(g.history.is_empty());
        assert_eq!(g.forward.len(), 2);
        assert!(matches!(&g.forward[1].url, Link::Http(u) if u.path() == "/b"));
        assert!(g.forward[1].doc.is_some(), "the parked doc is adjacent");
        assert!(g.forward[0].doc.is_none(), "/c fell out of adjacency");

        // And the deep-forward mirror: /b restores instantly, /c refetches.
        app.browser_forward();
        assert!(
            matches!(&app.browser.as_ref().unwrap().doc.url, Link::Http(u) if u.path() == "/b")
        );
        app.browser_forward();
        assert_eq!(app.pending_travel, Some(true), "deep forward refetches");
        assert!(app.fetch_rx.is_some());
    }

    #[tokio::test]
    async fn deep_travel_fetch_failure_leaves_the_trail_untouched() {
        let mut app = super::App::new(None, 23);
        app.navigate_to(http_doc("/a"));
        app.navigate_to(http_doc("/b"));
        app.navigate_to(http_doc("/c"));
        app.browser_back(); // /b, adjacent
        app.browser_back(); // deep → refetch starts
        assert_eq!(app.pending_travel, Some(false));

        let target = app.browser.as_ref().unwrap().history[0].url.clone();
        app.on_fetch(super::FetchMsg {
            target,
            result: Err(String::from("connection refused")),
        });

        assert!(app.pending_travel.is_none(), "failure clears the intent");
        assert!(app.notice);
        let g = app.browser.as_ref().unwrap();
        assert!(
            matches!(&g.doc.url, Link::Http(u) if u.path() == "/b"),
            "still on the page we were on"
        );
        assert_eq!(g.history.len(), 1);
        assert_eq!(g.forward.len(), 1);
        // The trail is intact: a later real navigation pushes normally.
        app.navigate_to(http_doc("/d"));
        let g = app.browser.as_ref().unwrap();
        assert_eq!(g.history.len(), 2);
        assert!(g.forward.is_empty(), "a real navigation truncates forward");
    }

    #[test]
    fn post_results_are_never_evicted_and_restore_from_ram() {
        let mut app = super::App::new(None, 23);
        app.navigate_to(http_doc("/form"));
        // The POST lands: what's shown is a direct POST result.
        app.nav_from_post = true;
        app.navigate_to(http_doc("/submitted"));
        assert!(app.current_from_post);
        // Two more navigations push the POST result deep into the trail.
        app.navigate_to(http_doc("/x"));
        app.navigate_to(http_doc("/y"));
        let g = app.browser.as_ref().unwrap();
        assert_eq!(g.history.len(), 3);
        assert!(g.history[0].doc.is_none(), "/form evicts normally");
        assert!(g.history[1].post, "the POST entry is marked");
        assert!(
            g.history[1].doc.is_some(),
            "and keeps its doc, however deep"
        );

        // Travel back onto it: restored from RAM, no refetch (a re-POST
        // would double-submit; a GET of the URL is a different page).
        app.browser_back(); // /x (adjacent)
        app.browser_back(); // /submitted — deep but retained
        assert!(
            app.fetch_rx.is_none(),
            "no refetch for a retained POST result"
        );
        let g = app.browser.as_ref().unwrap();
        assert!(matches!(&g.doc.url, Link::Http(u) if u.path() == "/submitted"));
        assert!(
            app.current_from_post,
            "its POST-ness survives the round trip"
        );
    }

    #[tokio::test]
    async fn a_live_post_result_restores_frozen_instead_of_get_refetching() {
        let mut app = super::App::new(None, 23);
        app.js_enabled = true;
        app.browser = Some(super::BrowserView {
            doc: http_doc("/next"),
            selected: None,
            sel_item: None,
            sel_fixed: None,
            scroll: 0,
            history: vec![super::HistEntry {
                post: true,
                ..entry(http_doc("/chat"), true, 0)
            }],
            forward: vec![],
        });
        app.browser_back();
        assert!(app.fetch_rx.is_none(), "no GET revive for a POST result");
        assert!(!app.replace_nav);
        assert!(
            matches!(&app.browser.as_ref().unwrap().doc.url, Link::Http(u) if u.path() == "/chat")
        );
    }

    #[test]
    fn image_caches_are_swept_to_adjacent_docs() {
        let mut app = super::App::new(None, 23);
        app.navigate_to(http_doc_img("/a", "/a.png"));
        app.navigate_to(http_doc_img("/b", "/b.png"));
        for u in ["/a.png", "/b.png", "/c.png", "/stale.png"] {
            let url = format!("https://example.com{u}");
            app.image_cache.insert(
                url.clone(),
                super::DecodedImage {
                    raw: std::sync::Arc::from(&b"px"[..]),
                    cell: (1, 1),
                    has_alpha: false,
                },
            );
            app.image_sizes.insert(url, (8, 16));
        }
        // Navigating to /c makes /a non-adjacent: its decoded image (and the
        // never-referenced stale entry) drop; current + adjacent survive.
        app.navigate_to(http_doc_img("/c", "/c.png"));
        let kept = |u: &str| {
            app.image_cache
                .contains_key(&format!("https://example.com{u}"))
        };
        assert!(!kept("/a.png"), "deep page's image dropped");
        assert!(kept("/b.png"), "adjacent page's image kept");
        assert!(kept("/c.png"), "current page's image kept");
        assert!(!kept("/stale.png"), "unreferenced entry dropped");
        assert!(
            !app.image_sizes.contains_key("https://example.com/a.png"),
            "the size mirror is swept too"
        );
        assert!(app.image_sizes.contains_key("https://example.com/b.png"));
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
        // `.ts` is TypeScript source on today's web far more often than
        // MPEG-TS — dropped from auto-play (manual `v` still plays a real
        // transport stream); `.m2ts` is unambiguous and stays.
        assert!(!m("https://example.com/src/app.ts"));
        assert!(m("https://example.com/cam/recording.m2ts"));
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
            0,
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
    fn osc52_copy_encodes_the_clipboard_sequence() {
        // "hi" → aGk= (RFC 4648); c = the clipboard selection.
        assert_eq!(super::osc52_copy("hi"), "\x1b]52;c;aGk=\x07");
    }

    #[test]
    fn yank_copies_the_selected_link_url() {
        // gopher line model: the selected link's full URL.
        let mut app = super::App::new(None, 23);
        app.mode = super::Mode::Session;
        app.navigate_to(gopher_doc("x"));
        assert_eq!(app.yank_target().as_deref(), Some("gopher://test.host/1/0"));
        app.yank_selected_url();
        assert!(app.status.starts_with("Copied gopher://"), "{}", app.status);
        assert!(app.notice);

        // http laid-out model: absolute web URL.
        let html = r#"<a href="/next">next</a> <form action="/f"><input name="q"></form>"#;
        let base = url::Url::parse("https://example.com/page").unwrap();
        app.navigate_to(crate::http::parse(
            &base,
            "text/html",
            html.as_bytes(),
            60,
            0,
            &Default::default(),
        ));
        select_item(
            &mut app,
            |it| matches!(&it.link, Some(Link::Http(u)) if u.path() == "/next"),
        );
        assert_eq!(
            app.yank_target().as_deref(),
            Some("https://example.com/next")
        );

        // A form control is not a URL: yank refuses with a notice.
        select_item(&mut app, |it| matches!(&it.link, Some(Link::Form { .. })));
        assert_eq!(app.yank_target(), None);
        app.yank_selected_url();
        assert_eq!(app.status, "No link selected to copy.");
    }

    #[tokio::test]
    async fn paste_inserts_atomically_into_the_console() {
        use crossterm::event::Event;
        let mut app = super::App::new(None, 23);
        app.mode = super::Mode::Command;
        // Via the real event path: the Tab inside must not toggle the
        // console, the newline must not run the command.
        app.on_terminal_event(Event::Paste(String::from("open\texample.com\n")))
            .await;
        assert_eq!(app.mode, super::Mode::Command, "paste runs nothing");
        assert_eq!(app.input, "open example.com");
        assert_eq!(app.cursor, app.input.chars().count());

        // A paste lands as one edit over the selection.
        app.select_anchor = Some(0);
        app.cursor = app.input.chars().count();
        app.on_paste(String::from("gopher://sdf.org")).await;
        assert_eq!(app.input, "gopher://sdf.org");
    }

    #[tokio::test]
    async fn paste_while_browsing_is_swallowed() {
        let mut app = super::App::new(None, 23);
        app.mode = super::Mode::Session;
        app.navigate_to(gopher_doc("x.x"));
        app.on_paste(String::from("stray\x1btext")).await;
        assert!(app.input.is_empty(), "no field to paste into");
        assert!(app.notice, "the user is told where paste goes");
        assert!(app.browser.is_some(), "the page stayed put");
    }

    #[tokio::test]
    async fn char_mode_paste_hits_the_wire_atomically() {
        use crate::telnet;
        use tokio::sync::mpsc;
        let mut app = super::App::new(None, 23);
        app.mode = super::Mode::Session;
        app.connected = true; // char_mode needs a live session
        app.mode_override = Some(super::InputMode::Character);
        let (tx, mut rx) = mpsc::channel(8);
        app.conn = Some(telnet::Handle { commands: tx });

        // Newlines travel as CR, the terminal paste rule.
        app.on_paste(String::from("two\nlines\n")).await;
        let telnet::Command::Send(bytes) = rx.recv().await.unwrap() else {
            panic!("expected a Send");
        };
        assert_eq!(bytes, b"two\rlines\r".to_vec());

        // A remote app that enabled bracketed paste (mode 2004) gets the
        // markers a real terminal would send it.
        app.vt.process(b"\x1b[?2004h");
        app.on_paste(String::from("marked")).await;
        let telnet::Command::Send(bytes) = rx.recv().await.unwrap() else {
            panic!("expected a Send");
        };
        assert_eq!(bytes, b"\x1b[200~marked\x1b[201~".to_vec());
    }

    #[tokio::test]
    async fn line_mode_paste_sends_completed_lines() {
        use crate::telnet;
        use tokio::sync::mpsc;
        let mut app = super::App::new(None, 23);
        app.mode = super::Mode::Session;
        let (tx, mut rx) = mpsc::channel(8);
        app.conn = Some(telnet::Handle { commands: tx });
        app.input = String::from("say ");
        app.cursor = 4;
        app.on_paste(String::from("hello\nworld")).await;
        // The newline completed the first line, like typing Enter …
        let telnet::Command::Send(bytes) = rx.recv().await.unwrap() else {
            panic!("expected a Send");
        };
        assert_eq!(bytes, b"say hello\r\n".to_vec());
        // … and the remainder stays in the editor, unsent.
        assert_eq!(app.input, "world");
        assert_eq!(app.cursor, 5);
    }

    #[test]
    fn gemini_sensitive_input_masks_the_prompt() {
        use ratatui::{Terminal, backend::TestBackend};
        let mut app = super::App::new(None, 23);
        app.last_inner = (80, 10);
        let url = crate::gemini::GeminiUrl::parse("gemini://astro.test/pw").unwrap();
        app.on_gemini_response(
            crate::gemini::Response {
                url: url.clone(),
                status: 11,
                meta: String::from("Password"),
                body: Vec::new(),
                identity: false,
            },
            80,
        );
        assert_eq!(app.mode, super::Mode::Search);
        assert!(app.masked_input, "status 11 is sensitive input");

        // Whatever is typed renders as bullets, never as the secret.
        app.input = String::from("hunter2");
        app.cursor = 7;
        let mut term = Terminal::new(TestBackend::new(80, 12)).unwrap();
        term.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
        let screen: String = term
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(!screen.contains("hunter2"), "the secret must not echo");
        assert!(screen.contains("•••••••"), "bullets stand in for it");
        assert!(screen.contains("SECRET"), "the prompt is labeled");

        // Plain status 10 input is NOT masked.
        app.on_gemini_response(
            crate::gemini::Response {
                url,
                status: 10,
                meta: String::from("Search term"),
                body: Vec::new(),
                identity: false,
            },
            80,
        );
        assert!(!app.masked_input);
    }

    #[test]
    fn password_field_edit_masks_the_prompt() {
        let html = r#"
            <form method="POST" action="/login">
              <input type="text" name="user">
              <input type="password" name="pw" value="s3cret">
            </form>"#;
        let base = url::Url::parse("https://example.com/login").unwrap();
        let mut app = super::App::new(None, 23);
        app.last_inner = (60, 10);
        app.navigate_to(crate::http::parse(
            &base,
            "text/html",
            html.as_bytes(),
            60,
            0,
            &Default::default(),
        ));
        app.form_interact(0, 1);
        assert_eq!(app.mode, super::Mode::Search);
        assert!(app.masked_input, "a password field masks its editor");
        assert_eq!(app.input, "s3cret", "the value is still editable");

        // A plain text field is not masked (the flag re-arms per prompt).
        app.mode = super::Mode::Session;
        app.form_interact(0, 0);
        assert!(!app.masked_input);
    }

    #[test]
    fn a_bot_challenge_surfaces_a_notice_without_navigating() {
        let mut app = super::App::new(None, 23);
        app.last_inner = (80, 10);
        let response = crate::http::Response {
            url: url::Url::parse("https://www.imdb.com/list/ls123/").unwrap(),
            status: 202,
            content_type: String::from("text/html"),
            headers: Vec::new(),
            blobs: None,
            // The challenge interstitial: an empty shell with no real content.
            body: b"<html><body><div id=\"challenge-container\"></div></body></html>".to_vec(),
            js: None,
            live: None,
            challenge: Some(String::from("AWS WAF (challenge)")),
            from_post: false,
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
            headers: Vec::new(),
            blobs: None,
            body: html.as_bytes().to_vec(),
            js: None,
            live: None,
            challenge: None,
            from_post: false,
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
        assert!(app.imgs_in_flight.is_empty(), "shell carried no images");
        // Drain the filled render the settle (setTimeout) produced.
        drain_page_event(&mut app).await;
        let g = app.browser.as_ref().unwrap();
        assert!(
            g.doc.image_urls.iter().any(|u| u.ends_with("tile.png")),
            "filled render carries the mounted tile image: {:?}",
            g.doc.image_urls
        );
        assert!(
            !app.imgs_in_flight.is_empty(),
            "the live update kicked off the image pipeline for the new tile"
        );
    }

    #[test]
    fn the_js_error_badge_counts_distinct_errors_not_occurrences() {
        // The live engine runs at rest, so a page whose data is gated behind a bot
        // wall re-attempts (and re-throws the same error) every tick. The badge must
        // count DISTINCT errors, not occurrences — otherwise one recurring problem
        // reads as a climbing `JS:30!` that overstates how many things are wrong.
        let mut app = super::App::new(None, 23);
        for _ in 0..6 {
            app.on_page_evt(crate::js::PageEvt::Trouble(vec![
                "timer: unknown value".to_string(),
            ]));
        }
        assert_eq!(
            app.page_js_errors.len(),
            1,
            "the same error six times is one distinct problem"
        );
        assert!(
            app.status.contains("JS:1!"),
            "badge shows the distinct count: {}",
            app.status
        );
        // A genuinely different error increments the distinct count.
        app.on_page_evt(crate::js::PageEvt::Trouble(vec![
            "timer: something else".to_string(),
        ]));
        assert_eq!(app.page_js_errors.len(), 2);
        assert!(app.status.contains("JS:2!"), "{}", app.status);
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
        let doc = crate::http::parse(
            &url,
            "text/html",
            body.as_bytes(),
            60,
            0,
            &Default::default(),
        );
        let mut app = super::App::new(None, 23);
        app.mode = super::Mode::Session;
        app.last_inner = (60, 10);
        app.browser = Some(super::BrowserView {
            doc,
            selected: None,
            sel_item: None,
            sel_fixed: None,
            scroll: 0,
            history: Vec::new(),
            forward: Vec::new(),
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
        // A real user scroll records its intent (the run-loop tail does this);
        // mirror it here since the test pokes `scroll` directly.
        app.scroll_intent = 30;
        // A timer tick re-renders with unchanged content.
        app.replace_live_doc(body.into_bytes());
        assert_eq!(
            app.browser.as_ref().unwrap().scroll,
            30,
            "the user's scroll survived the autonomous re-render"
        );
    }

    #[tokio::test]
    async fn an_autonomous_rerender_does_not_pop_a_selection_onto_a_link() {
        // Mouse mode, nothing hovered: there is NO selection — even though a
        // link sits in the viewport (`tall_browser_app` puts `/top` at row 0,
        // scroll 0, so it's on-screen). A live timer/anim re-render must leave
        // the selection empty: it must NOT pop one onto that visible link (the
        // bug — `http_first_visible_item` would have found it and grabbed it).
        let (mut app, body) = tall_browser_app(50);
        {
            let g = app.browser.as_ref().unwrap();
            assert!(g.sel_item.is_none(), "no link is selected to begin with");
            assert!(
                super::App::http_first_visible_item(g, 10).is_some(),
                "a link IS visible in the viewport (so the old fallback would fire)"
            );
        }
        app.replace_live_doc(body.into_bytes());
        let g = app.browser.as_ref().unwrap();
        assert!(
            g.sel_item.is_none(),
            "the autonomous re-render left the selection empty (got {:?})",
            g.sel_item
        );
        assert_eq!(g.scroll, 0, "the scroll position survived untouched");
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
    async fn a_transient_shrink_restores_scroll_when_the_doc_grows_back() {
        // The infinite-scroll judder (archive.org "every time"): a living page
        // momentarily SHRINKS — it paints a short "Searching…" placeholder — and
        // then refills. The reader's scroll must not pop PERMANENTLY: it can only
        // clamp for the placeholder frame (the doc is genuinely shorter then) and
        // must be RESTORED when the content grows back. The old code kept the
        // shrink-clamped row index, so the view settled at the top forever.
        let (mut app, body) = tall_browser_app(50); // ~51 rows, viewport 10
        let height = 10usize;
        {
            let g = app.browser.as_mut().unwrap();
            g.sel_item = None; // a pure wheel scroll, no selection
            g.scroll = 30; // the user scrolled well down
        }
        // A real wheel scroll records the intent (the run-loop tail does this);
        // the test pokes `scroll` directly, so mirror it.
        app.scroll_intent = 30;

        // The page blanks to a tiny placeholder, shorter than the viewport.
        app.replace_live_doc(b"<body><p>loading</p></body>".to_vec());
        {
            let g = app.browser.as_ref().unwrap();
            let max = g.doc.rows.len().saturating_sub(height);
            assert!(
                g.scroll <= max,
                "the display scroll is clamped to the shrunken content (scroll={}, max={max})",
                g.scroll
            );
        }

        // The page refills with the original content; the reader returns to place.
        app.replace_live_doc(body.into_bytes());
        assert_eq!(
            app.browser.as_ref().unwrap().scroll,
            30,
            "scroll was restored to the reader's intent after the regrow"
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
                0,
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
                if let Ok(proto) =
                    crate::img::encode_sliced(&app.picker, image, size, key.crop, key.pixelated)
                {
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
            0,
            &images,
        );
        app.navigate_to(doc);
        // Seed the decoded cache + ONE box-keyed encode, as the pipeline would.
        app.image_cache.insert(
            "https://ex.com/banner.png".to_string(),
            super::DecodedImage {
                raw: png.clone().into(),
                cell,
                has_alpha: false,
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
        let proto = crate::img::encode_sliced(
            &app.picker,
            decoded,
            Size::new(key.w, key.h),
            key.crop,
            key.pixelated,
        )
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

    #[tokio::test]
    async fn a_region_buffer_image_is_marked_live_for_encoding() {
        // A vertical scroll region holds its content (incl. images) in its own
        // buffer, NOT in `doc.rows`. The encode pass must scan that buffer, else a
        // region image would never encode — or, once encoded, get evicted because
        // the doc-rows scan never marks it live. (Phase 4 images-in-regions.)
        use ratatui::layout::Size;
        let png = crate::img::red_png();
        let (decoded, _) = crate::img::decode(&png).unwrap();
        let cell = super::natural_cell_box(&decoded, (8u16, 16u16).into());
        let base = url::Url::parse("https://ex.com/").unwrap();
        let mut images = crate::layout::ImageSizes::new();
        images.insert("https://ex.com/av.png".to_string(), cell);
        // A definite-height `overflow-y:auto` region whose content includes an
        // image plus enough rows to overflow (so it becomes a Region + buffer).
        let mut content = String::from(r#"<img src="/av.png">"#);
        for i in 0..40 {
            content.push_str(&format!("<div>L{i}</div>"));
        }
        let body = format!("<body><div style='height:96px;overflow-y:auto'>{content}</div></body>");
        let mut app = super::App::new(None, 23);
        app.last_inner = (40, 50); // tall enough that the region band is on-screen
        let doc = crate::http::parse(
            &base,
            "text/html; charset=utf-8",
            body.as_bytes(),
            40,
            50,
            &images,
        );
        app.navigate_to(doc);
        let key = {
            let g = app.browser.as_ref().unwrap();
            assert_eq!(g.doc.regions.len(), 1, "the overflow box is a region");
            g.doc.regions[0]
                .buffer
                .iter()
                .flat_map(|r| &r.items)
                .find_map(|it| it.image.as_deref().map(|u| super::EncKey::for_item(u, it)))
                .expect("the region buffer holds the image item")
        };
        // Seed the decoded cache + a box-keyed encode, as the pipeline would.
        app.image_cache.insert(
            "https://ex.com/av.png".to_string(),
            super::DecodedImage {
                raw: png.clone().into(),
                cell,
                has_alpha: false,
            },
        );
        let proto = crate::img::encode_sliced(
            &app.picker,
            decoded,
            Size::new(key.w, key.h),
            key.crop,
            key.pixelated,
        )
        .unwrap();
        app.image_protocols.insert(key.clone(), proto);
        // The encode pass scans the region buffer ⇒ the image is LIVE ⇒ kept.
        app.sync_image_encodes();
        assert!(
            app.image_protocols.contains_key(&key),
            "the region image's encode is kept (the region scan marks it live)"
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
        let size = ratatui::layout::Size::new(app.last_inner.0, app.last_inner.1);
        let (protocol, image) =
            crate::img::encode_bytes(&app.picker, &raw, size, false, Some(super::svg_tint()))
                .unwrap();
        let info = format!("{}×{} {}", image.width, image.height, image.mime);
        app.on_img(super::ImgMsg {
            url,
            raw: raw.into(),
            size: app.last_inner,
            result: Ok((protocol, info)),
        });
    }

    fn svg_fixture() -> Vec<u8> {
        br##"<svg xmlns="http://www.w3.org/2000/svg" width="80" height="32"
                    viewBox="0 0 80 32">
                <rect width="80" height="32" fill="#00ff00"/>
              </svg>"##
            .to_vec()
    }

    /// A `blob:` image resolves from the page's blob byte mirror (`Doc.blobs`)
    /// — never the wire. This is how a client-GENERATED image renders: the
    /// page mints `URL.createObjectURL(blob)` (Steam's login QR), the prelude
    /// mirrors the bytes via `__blob_mirror`, and the app's image pipeline
    /// decodes from the shared map. A URL missing from the map (revoked before
    /// mirroring existed, or another page's) decodes to nothing, like a 404.
    #[tokio::test]
    async fn blob_image_urls_decode_from_the_doc_blob_mirror() {
        let page = url::Url::parse("https://example.com/login").unwrap();
        let blob_url = "blob:https://example.com/0f7c18cd-0475-4912-9865-1cd4adacebaa";
        let blobs = crate::js::BlobMap::default();
        blobs.lock().unwrap().insert(
            blob_url.to_string(),
            (svg_fixture(), String::from("image/svg+xml")),
        );
        let decoded = super::load_one_image(&page, blob_url, (8, 16).into(), Some(&blobs))
            .await
            .expect("blob image decoded from the mirror");
        assert_eq!(decoded.raw.as_ref(), svg_fixture().as_slice());
        // A fragment is ignored when keying the store (File API).
        let with_frag = format!("{blob_url}#frag");
        assert!(
            super::load_one_image(&page, &with_frag, (8, 16).into(), Some(&blobs))
                .await
                .is_some(),
            "fragment-carrying blob URL resolves"
        );
        // Unknown URL / no map: no decode, no network.
        assert!(
            super::load_one_image(
                &page,
                "blob:https://example.com/missing",
                (8, 16).into(),
                Some(&blobs)
            )
            .await
            .is_none()
        );
        assert!(
            super::load_one_image(&page, blob_url, (8, 16).into(), None)
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn standalone_http_svg_routes_through_the_image_viewer() {
        let mut app = super::App::new(None, 23);
        app.last_inner = (40, 12);
        let url = url::Url::parse("https://example.com/logo.svg").unwrap();
        app.on_http_response(
            crate::http::Response {
                url: url.clone(),
                status: 200,
                content_type: String::from("image/svg+xml"),
                headers: Vec::new(),
                blobs: None,
                body: svg_fixture(),
                js: None,
                live: None,
                challenge: None,
                from_post: false,
            },
            40,
        );
        let msg = app
            .img_rx
            .as_mut()
            .expect("SVG viewer encode started")
            .recv()
            .await
            .expect("SVG viewer encode completed");
        app.on_img(msg);

        let viewer = app.viewer.as_ref().expect("SVG viewer opened");
        assert_eq!(viewer.url, Link::Http(url));
        assert!(
            viewer.info.contains("80×32 image/svg+xml"),
            "{}",
            viewer.info
        );
        assert_eq!(viewer.encoded_for, (40, 12));
    }

    #[tokio::test]
    async fn svg_decodes_to_its_intrinsic_box_uncapped() {
        let font = ratatui_image::picker::Picker::halfblocks().font_size();
        // An SVG decodes to its INTRINSIC cell box, never an artificial cap: a
        // 200×40 viewBox (no element CSS size) yields the same box a 200×40
        // raster would. The element's own CSS/attr `width`/`height` — carried
        // onto the rewritten <img> in `Dom::rewrite_inline_svgs` — is what
        // resizes it, applied later in `image_used_box`, not here.
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="200" height="40" viewBox="0 0 200 40"><path d="M0 0h200v40H0z"/></svg>"#;
        let (cell, svg_alpha) = super::decoded_cell_box(std::sync::Arc::from(&svg[..]), font)
            .await
            .unwrap();
        assert_eq!(cell, super::natural_cell_box_dimensions(200, 40, font));
        assert!(!svg_alpha, "SVG rasterizes to an opaque silhouette");
        // A raster keeps its natural box too — same path, no special-casing.
        let png = crate::img::red_png();
        let (raster, raster_alpha) = super::decoded_cell_box(std::sync::Arc::from(&png[..]), font)
            .await
            .unwrap();
        assert_eq!(raster, super::natural_cell_box_dimensions(4, 4, font));
        assert!(!raster_alpha, "an opaque RGB PNG has no transparency");
    }

    #[tokio::test]
    async fn inline_http_svg_fetches_decodes_and_reflows_as_an_image_box() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let svg = svg_fixture();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 1024];
            let _ = socket.read(&mut request).await.unwrap();
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: image/svg+xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                svg.len()
            );
            socket.write_all(head.as_bytes()).await.unwrap();
            socket.write_all(&svg).await.unwrap();
        });

        let page = url::Url::parse(&format!("http://{address}/page")).unwrap();
        let image_url = format!("http://{address}/logo.svg");
        let html =
            format!(r#"<body><p>before</p><img src="{image_url}" alt="logo"><p>after</p></body>"#);
        let mut app = super::App::new(None, 23);
        app.last_inner = (40, 12);
        let doc = crate::http::parse(
            &page,
            "text/html; charset=utf-8",
            html.as_bytes(),
            40,
            0,
            &crate::layout::ImageSizes::new(),
        );
        app.navigate_to(doc);
        let expected_cell = super::natural_cell_box_dimensions(80, 32, app.picker.font_size());
        app.start_image_loads(page, vec![image_url.clone()]);
        assert!(!app.imgs_in_flight.is_empty(), "inline SVG load started");
        let msg = app.imgs_rx.recv().await.expect("inline SVG load completed");
        app.on_img_load(msg);
        // The run loop coalesces image-load relayouts to one per turn; drive it.
        app.apply_pending_image_decodes();
        server.await.unwrap();

        let decoded = app.image_cache.get(&image_url).expect("SVG cached");
        assert_eq!(decoded.cell, expected_cell);
        let item = app
            .browser
            .as_ref()
            .unwrap()
            .doc
            .rows
            .iter()
            .flat_map(|row| &row.items)
            .find(|item| item.image.as_deref() == Some(image_url.as_str()))
            .expect("reflow produced a real image item");
        assert_eq!((item.width, item.height), expected_cell);
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
            0,
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
