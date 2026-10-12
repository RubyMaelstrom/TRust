//! Engine-neutral browser contract and Lumen-backed page actor.
//!
//! The browser-facing contract delegates JavaScript execution to the sibling
//! Lumen engine in every build configuration.

use crate::dom::Dom;
use std::time::{Duration, Instant};

/// What a page's scripts did, for status and diagnostics.
#[derive(Default, Clone)]
pub struct Outcome {
    pub errors: Vec<String>,
    pub elapsed: Duration,
    pub modules_skipped: usize,
    pub panicked: bool,
    pub fetches: usize,
    pub console: Vec<String>,
    pub(crate) rendered: Option<Box<crate::http::RenderedPage>>,
    /// This presentation only advanced style-origin CSS animations; no DOM,
    /// resource or environment change contributed to it.
    pub(crate) animation_frame: bool,
    /// The top-level Document's policy container referrer policy, after its
    /// `<meta name=referrer>` elements (HTML #meta-referrer), for navigations
    /// the frontend starts from this presentation.
    pub referrer_policy: Option<crate::referrer_policy::ReferrerPolicy>,
}

impl std::fmt::Debug for Outcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Outcome")
            .field("errors", &self.errors)
            .field("elapsed", &self.elapsed)
            .field("modules_skipped", &self.modules_skipped)
            .field("panicked", &self.panicked)
            .field("fetches", &self.fetches)
            .field("console", &self.console)
            .field("rendered", &self.rendered.is_some())
            .field("animation_frame", &self.animation_frame)
            .finish()
    }
}

impl Outcome {
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn notice(&self) -> Option<String> {
        let first = self.errors.first()?;
        match self.errors.len() {
            1 => Some(format!("JS: {first}")),
            n => Some(format!("JS: {first} (+{} more)", n - 1)),
        }
    }
}

/// Origin-bucketed RAM storage; the site storage service persists only localStorage.
/// sessionStorage buckets are `session:<storage shed>:<origin>`, one shed per
/// top-level traversable (Storage #traversable-storage-shed).
pub type WebStorage = std::sync::Arc<
    std::sync::Mutex<std::collections::HashMap<String, std::collections::HashMap<String, String>>>,
>;

/// A new top-level traversable's storage shed (Storage
/// #traversable-storage-shed): its sessionStorage, which no other
/// traversable shares. Shed 0 is that of frontends that present a single
/// traversable.
pub fn new_storage_shed() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Storage #legacy-clone-a-traversable-storage-shed: `to` begins with a copy
/// of `from`'s session storage (HTML #copy-session-storage).
pub(crate) fn clone_storage_shed(storage: &WebStorage, from: u64, to: u64) {
    let source = format!("session:{from}:");
    let mut storage = storage
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let copies: Vec<(String, std::collections::HashMap<String, String>)> = storage
        .iter()
        .filter_map(|(bucket, entries)| {
            let origin = bucket.strip_prefix(&source)?;
            Some((format!("session:{to}:{origin}"), entries.clone()))
        })
        .collect();
    storage.extend(copies);
}

/// A closed top-level traversable's session storage goes with it.
pub fn forget_storage_shed(shed: u64) {
    if shed == 0 {
        return;
    }
    let prefix = format!("session:{shed}:");
    crate::site_storage::web_storage()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .retain(|bucket, _| !bucket.starts_with(&prefix));
}

#[derive(Debug)]
pub enum WorkerOut {
    Message(String),
    Error(String),
    /// Developer-console lines the worker's console and unhandled-rejection
    /// reporting produced during one task, separated by U+001E.
    Console(String),
}

/// The page's blob URL store (File API #url-model), shared by its Windows, frames and workers
/// and by the frontend that presents the page's `blob:` images.
pub type BlobMap = std::sync::Arc<crate::blob_url_store::BlobUrlStore>;

/// After this many actual or speculative cache-miss fetches, stop issuing additional optional
/// module prefetches. Required Fetch/HTML requests must never consult this optimization threshold:
/// Fetch Standard §5.6 calls Fetch for every valid `fetch()` request and only a network error
/// rejects the resulting promise.
pub(crate) const MAX_PAGE_FETCHES_WITH_SPECULATION: usize = 256;
pub(crate) const MAX_SPECULATIVE_IMPORTS: usize = 64;

pub(crate) fn host_boundary_signatures() -> impl Iterator<Item = (&'static str, usize)> {
    crate::js_host_boundary::HOST_BOUNDARY_SIGNATURES
        .iter()
        .copied()
}

pub(crate) fn parse_header_blob(blob: &str) -> Vec<(String, String)> {
    if blob.is_empty() {
        return Vec::new();
    }
    blob.split('\n')
        .collect::<Vec<_>>()
        .chunks(2)
        .filter_map(|chunk| match chunk {
            [name, value] if !name.is_empty() => Some(((*name).to_string(), (*value).to_string())),
            _ => None,
        })
        .collect()
}

pub(crate) fn response_body_is_binary(content_type: &str) -> bool {
    let essence = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if essence.is_empty() || essence.starts_with("text/") {
        return false;
    }
    if essence.starts_with("image/")
        || essence.starts_with("audio/")
        || essence.starts_with("video/")
        || essence.starts_with("font/")
    {
        return true;
    }
    if essence.starts_with("application/") {
        return !(essence == "application/json"
            || essence.ends_with("+json")
            || essence == "application/xml"
            || essence.ends_with("+xml")
            || essence == "application/javascript"
            || essence == "application/ecmascript"
            || essence == "application/x-www-form-urlencoded"
            || essence == "application/graphql")
            && !matches!(
                essence.as_str(),
                "application/xhtml+xml" | "application/sql" | "application/rtf"
            );
    }
    false
}

/// MIME Sniffing #xml-mime-type: a MIME type whose subtype ends in "+xml",
/// or whose essence is "text/xml" or "application/xml".
pub(crate) fn is_xml_mime_type(essence: &str) -> bool {
    essence == "text/xml" || essence == "application/xml" || essence.ends_with("+xml")
}

pub(crate) fn headers_to_blob(headers: &[(String, String)]) -> String {
    let mut blob = String::new();
    for (name, value) in headers {
        if name.is_empty() {
            continue;
        }
        if !blob.is_empty() {
            blob.push('\n');
        }
        blob.push_str(name);
        blob.push('\n');
        blob.push_str(&value.replace('\n', " "));
    }
    blob
}

pub struct PageEnv {
    pub url: String,
    pub navigation_timing: Option<Box<crate::http::FetchTiming>>,
    pub(crate) resource_timings: Vec<crate::performance::ResourceTiming>,
    pub viewport: (u16, u16),
    pub cell_px: (u16, u16),
    pub device_pixel_ratio: f32,
    /// Retain metadata for CSS-pixel-to-terminal-cell adaptation. Native
    /// windows consume the graphical product directly and can omit this work.
    pub(crate) terminal_presentation: bool,
    /// Client-window origin in CSS pixels; zero when the frontend has no
    /// screen coordinates (CSSOM View #dom-window-screenx).
    pub screen_position: (i32, i32),
    pub externals: Vec<(String, Option<std::sync::Arc<Vec<u8>>>)>,
    pub sheets: Vec<crate::http::PageSheet>,
    pub cache: std::sync::Arc<crate::http::PageCache>,
    pub net: Option<tokio::runtime::Handle>,
    pub storage: Option<WebStorage>,
    pub blobs: BlobMap,
    /// A frame document of this page has scripts. Nested documents run
    /// their own scripts (HTML #the-iframe-element), which only the live
    /// actor can do, so the page stays live even without scripts of its own.
    pub(crate) scripted_frames: bool,
    /// The document's last modification time in ms since the epoch, from
    /// its `Last-Modified` header or file metadata (HTML
    /// #dom-document-lastmodified); None when unknown.
    pub(crate) last_modified: Option<f64>,
    /// HTML #documentEncoding: the encoding the document's bytes were
    /// decoded with (`document.characterSet`).
    pub(crate) document_encoding: &'static encoding_rs::Encoding,
    /// The response's MIME type essence: an XML MIME type makes the page an
    /// XML document built by the XML parser (HTML #read-xml).
    pub(crate) content_type: String,
    /// HTML #policy-container-referrer-policy of the Document's policy container: from
    /// the navigation response's `Referrer-Policy` header, or cloned from the
    /// initiator for a local URL (HTML #determining-navigation-params-policy-container).
    pub(crate) referrer_policy: crate::referrer_policy::ReferrerPolicy,
    /// HTML #dom-document-referrer: the navigation request's final referrer, or empty.
    pub(crate) referrer: String,
    /// Fullscreen API #fullscreen-is-supported: whether the frontend can
    /// present this page fullscreen (`document.fullscreenEnabled` and
    /// `requestFullscreen()` consult it).
    pub(crate) fullscreen_supported: bool,
    /// How the frontend presents the page's top-level traversable as the
    /// document is created: its initial visibility state and system focus.
    pub(crate) traversable: TraversableState,
}

/// The frontend's presentation of a top-level traversable, which its pages
/// observe: HTML #page-visibility's system visibility state, HTML
/// #system-focus, and Page Lifecycle #discarding.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TraversableState {
    /// The traversable cannot be seen: a background tab, or an occluded or
    /// minimized window.
    pub hidden: bool,
    /// The traversable has system focus: it is shown in the focused window.
    pub focused: bool,
    /// This document replaces one the user agent discarded to save memory
    /// (Page Lifecycle #dom-document-wasdiscarded).
    pub was_discarded: bool,
    /// The user agent can create new top-level traversables (tabs) beside
    /// this one. Without them it is configured to choose the current
    /// navigable whenever a page requests a new one (HTML
    /// #the-rules-for-choosing-a-navigable).
    pub new_traversables: bool,
    /// The traversable's storage shed (`new_storage_shed`), which holds its
    /// sessionStorage across its Documents.
    pub storage_shed: u64,
}

impl TraversableState {
    /// A traversable that can be seen and has the keyboard, as a single-page
    /// frontend's always does.
    pub const SHOWN: Self = Self {
        hidden: false,
        focused: true,
        was_discarded: false,
        new_traversables: false,
        storage_shed: 0,
    };
}

impl PageEnv {
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn bare(url: &str) -> Self {
        Self {
            url: url.to_string(),
            navigation_timing: None,
            resource_timings: Vec::new(),
            viewport: (80, 24),
            cell_px: (8, 16),
            device_pixel_ratio: 1.0,
            terminal_presentation: true,
            screen_position: (0, 0),
            externals: Vec::new(),
            sheets: Vec::new(),
            cache: Default::default(),
            net: None,
            storage: None,
            blobs: Default::default(),
            scripted_frames: false,
            last_modified: None,
            document_encoding: encoding_rs::UTF_8,
            content_type: String::from("text/html"),
            referrer_policy: Default::default(),
            referrer: String::new(),
            fullscreen_supported: true,
            traversable: TraversableState::SHOWN,
        }
    }
}

/// Best-effort static-module import scanner used to overlap graph fetches.
pub(crate) fn scan_module_imports(src: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let is_ident = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'$';
    let mut index = 0;
    while index < src.len() {
        let keyword_len = if src[index..].starts_with(b"from") {
            4
        } else if src[index..].starts_with(b"import") {
            6
        } else {
            index += 1;
            continue;
        };
        if index > 0 && is_ident(src[index - 1]) {
            index += 1;
            continue;
        }
        let mut cursor = index + keyword_len;
        while cursor < src.len() && (src[cursor] as char).is_whitespace() {
            cursor += 1;
        }
        if keyword_len == 6 && src.get(cursor) == Some(&b'(') {
            index += keyword_len;
            continue;
        }
        if src
            .get(cursor)
            .is_some_and(|byte| matches!(byte, b'"' | b'\'' | b'`'))
        {
            let quote = src[cursor];
            let start = cursor + 1;
            let mut end = start;
            while end < src.len() && src[end] != quote && src[end] != b'\n' {
                end += 1;
            }
            if src.get(end) == Some(&quote) {
                if let Ok(specifier) = std::str::from_utf8(&src[start..end]) {
                    let path_like = specifier.starts_with('/')
                        || specifier.starts_with("./")
                        || specifier.starts_with("../")
                        || specifier.starts_with("http");
                    if path_like && !specifier.contains("${") {
                        out.push(specifier.to_string());
                    }
                }
                index = end + 1;
                continue;
            }
        }
        index += keyword_len;
    }
    out
}

pub fn css_bake(
    html: &str,
    sheets: &[crate::http::PageSheet],
    viewport: (u16, u16),
    cell_px: (u16, u16),
) -> String {
    css_finish(css_prepare(html, viewport, cell_px), sheets)
}

pub fn css_prepare(html: &str, viewport: (u16, u16), cell_px: (u16, u16)) -> Dom {
    css_prepare_css(
        html,
        crate::layout2::Viewport::new(
            f32::from(viewport.0) * f32::from(cell_px.0.max(1)),
            f32::from(viewport.1) * f32::from(cell_px.1.max(1)),
        ),
    )
}

pub fn css_prepare_css(html: &str, viewport: crate::layout2::Viewport) -> Dom {
    let mut dom = Dom::parse_document(html);
    dom.set_viewport_px(viewport.width, viewport.height);
    dom
}

pub fn css_finish(mut dom: Dom, sheets: &[crate::http::PageSheet]) -> String {
    if !sheets.is_empty() {
        dom.attach_external_sheets(sheets);
    }
    dom.serialize(dom.document())
}

pub fn transform(html: &str, env: &PageEnv) -> (String, Outcome) {
    crate::lumen_backend::transform(html, env)
}

#[derive(Clone, Debug, PartialEq)]
pub struct TextEdit {
    pub input_type: String,
    pub data: Option<String>,
    pub selection: Option<crate::doc::ControlSelection>,
    pub composing: bool,
}

/// Native presentation of the focused editing host. Text offsets are UTF-16;
/// the resident DOM Selection remains the authority for structural endpoints.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
pub struct EditableState {
    pub node: usize,
    pub text: String,
    pub selection: crate::doc::ControlSelection,
}

#[derive(Debug)]
pub enum PageCmd {
    Click(usize),
    /// HTML same-document navigation keeps the resident Document and realm.
    NavigateFragment {
        url: String,
        replace: bool,
    },
    TraverseHistory {
        url: String,
        delta: i32,
    },
    /// The top-level traversable's session history, which the frontend owns:
    /// the current entry's `index` and the number of entries (`length`),
    /// after the frontend applied `applied` of this page's
    /// `PageEvt::HistoryUpdate`s. The frontend reports it whenever it changes
    /// (HTML #dom-history-length and #script-closable).
    SessionHistory {
        index: usize,
        length: usize,
        applied: u64,
    },
    PointerLockResult {
        request: u64,
        error: Option<String>,
    },
    /// The frontend's answer to `PageEvt::ClipboardWrite`: whether the text reached its system
    /// clipboard. A frontend without one answers `false`.
    ClipboardResult {
        request: u64,
        ok: bool,
    },
    /// The frontend's answer to `PageEvt::FilePicker` (HTML #show-the-picker,-if-applicable):
    /// the paths the user chose, or `None` when the prompt was dismissed. Only these paths
    /// become readable by the page.
    FilePickerResult {
        request: u64,
        files: Option<Vec<std::path::PathBuf>>,
    },
    /// Native files dragged over the page from another application.
    FileDrag(FileDrag),
    /// Unconditional native release (Escape, focus loss, or retired window).
    ReleasePointerLock,
    /// HTML #close-requests: the user's close request for the page (the
    /// Escape key press). The actor runs the close request steps (exit
    /// fullscreen, else the Escape keydown and the close watchers) and
    /// answers `PageEvt::CloseRequestHandled`.
    CloseRequest {
        modifiers: crate::core::Modifiers,
    },
    /// The user agent's "stop loading" of the top-level navigable (HTML
    /// #nav-stop): the actor aborts its active Document and their
    /// descendants' as a task (#abort-a-document-and-its-descendants),
    /// canceling every fetch of theirs, scripts' fetch() and XMLHttpRequest
    /// included. The page and its scripts keep running.
    StopLoading,
    /// HTML #checking-if-unloading-is-canceled for the top-level
    /// traversable's Document and its descendants', before the user closes
    /// the traversable or navigates it from the browser's own interface:
    /// the actor fires beforeunload at each, asks the user when one asks
    /// for that (`PageEvt::UnloadPrompt`), and answers
    /// `PageEvt::UnloadChecked`.
    CheckUnload {
        request: u64,
    },
    /// HTML #unload-a-document-and-its-descendants as the page ends
    /// (`PageHandle::unload`): pagehide, visibilitychange and unload, after
    /// which the actor ends with its Documents.
    Unload,
    /// Page Lifecycle #discarding to free memory (`PageHandle::discard`):
    /// the actor runs the freeze steps for its Documents, then ends without
    /// unloading them.
    Discard,
    /// The user's copy action (Clipboard API #copy-action): the page fires
    /// `copy`, then the selection's text, or the data its listeners set,
    /// reaches the frontend as a `PageEvt::ClipboardWrite`.
    Copy,
    /// The frontend's answer to `PageEvt::FullscreenRequest` (whether its
    /// viewport now fills the screen) or `PageEvt::FullscreenExit`, sent after
    /// the resized viewport.
    FullscreenResult {
        request: u64,
        ok: bool,
    },
    /// The user agent ended fullscreen itself (Fullscreen API #ui), e.g. the
    /// native window left fullscreen; the page fully exits fullscreen.
    ExitFullscreen,
    InputModifiers(crate::core::Modifiers),
    PointerMotion {
        dx: f64,
        dy: f64,
    },
    LockedPointerButton {
        button: i16,
        pressed: bool,
    },
    LockedWheel {
        dx: f64,
        dy: f64,
    },
    /// Actual pointer transitions, before the separate primary click action.
    PointerButton {
        node: Option<usize>,
        pressed: bool,
        x: f64,
        y: f64,
        button: i16,
        metadata: PointerMetadata,
    },
    /// Native focus navigation. None focuses the document viewport; a hit on
    /// an element resolves to its nearest click-focusable ancestor.
    Focus(Option<usize>),
    /// `Focus` by sequential (keyboard) focus navigation, which indicates
    /// focus (Selectors 4 #the-focus-visible-pseudo).
    KeyboardFocus(Option<usize>),
    Key {
        /// None resolves the currently focused area in the resident page.
        node: Option<usize>,
        input: crate::core::KeyInput,
    },
    /// A text-control key whose insertion default runs against the actor's
    /// current value/UTF-16 selection. Releases have no insertion text.
    /// This retains keyboard/input ordering without a presentation round trip.
    EditKey {
        node: usize,
        input: crate::core::KeyInput,
        text: Option<String>,
    },
    SetValue {
        node: usize,
        value: String,
        checked: Option<bool>,
        /// The terminal's whole-field editor has an explicit commit action.
        commit: bool,
    },
    EditText {
        node: usize,
        value: String,
        edit: TextEdit,
    },
    StepNumber {
        node: usize,
        direction: i8,
        click: bool,
    },
    /// HTML #selector-open: the frontend showed (`open`) or closed a
    /// select's drop-down box or an input's picker.
    PickerOpen {
        node: usize,
        open: bool,
    },
    /// HTML #the-select-element: the user picked `option` of `select`
    /// (#concept-select-pick), or toggled it (#concept-select-toggle) in a
    /// select with `multiple`, through the frontend's drop-down box.
    SelectPick {
        select: usize,
        option: usize,
        toggle: bool,
    },
    Submit {
        form: usize,
        submitter: Option<usize>,
    },
    Ws {
        id: usize,
        event: crate::ws::WsIn,
    },
    Worker {
        id: usize,
        event: WorkerOut,
    },
    Scroll {
        x: f64,
        y: f64,
    },
    Hover {
        node: Option<usize>,
        x: f64,
        y: f64,
        metadata: PointerMetadata,
    },
    RegionGeom {
        items: Vec<(usize, f64, f64)>,
    },
    SetScroll {
        node: usize,
        top: f64,
        left: f64,
    },
    Resync,
    LiveRegions(Vec<usize>),
    LiveBoundaries(Vec<usize>),
    ImageSizes(Vec<(String, (u32, u32))>),
    Viewport(crate::layout2::Viewport),
    DevicePixelRatio(f32),
    ScreenPosition(i32, i32),
    /// HTML #page-visibility: the traversable's system visibility state
    /// changed (`true`: hidden, as a background tab or an occluded window).
    /// The actor updates its Documents' visibility state; hidden Documents
    /// get no rendering opportunities.
    SystemVisibility(bool),
    /// HTML #system-focus: the traversable gained or lost system focus. The
    /// actor runs the focus update steps for its current focus chain.
    SystemFocus(bool),
}

impl PageCmd {
    pub(crate) fn is_user_interaction(&self) -> bool {
        matches!(
            self,
            Self::Click(_)
                | Self::NavigateFragment { .. }
                | Self::TraverseHistory { .. }
                | Self::PointerLockResult { .. }
                | Self::ClipboardResult { .. }
                | Self::FilePickerResult { .. }
                | Self::FileDrag(_)
                | Self::ReleasePointerLock
                | Self::CloseRequest { .. }
                | Self::StopLoading
                | Self::CheckUnload { .. }
                | Self::FullscreenResult { .. }
                | Self::ExitFullscreen
                | Self::InputModifiers(_)
                | Self::PointerMotion { .. }
                | Self::LockedPointerButton { .. }
                | Self::LockedWheel { .. }
                | Self::PointerButton { .. }
                | Self::Focus(_)
                | Self::KeyboardFocus(_)
                | Self::PickerOpen { .. }
                | Self::SelectPick { .. }
                | Self::Key { .. }
                | Self::EditKey { .. }
                | Self::SetValue { .. }
                | Self::EditText { .. }
                | Self::StepNumber { .. }
                | Self::Submit { .. }
                | Self::Scroll { .. }
                | Self::Hover { .. }
                | Self::SetScroll { .. }
                | Self::Copy
        )
    }
}

/// HTML #show-the-picker,-if-applicable for a File Upload control: the prompt the page asks
/// its frontend to display. The frontend answers with `PageCmd::FilePickerResult`, one prompt
/// at a time ("optionally, wait until any prior execution of this algorithm has terminated").
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FilePickerRequest {
    pub request: u64,
    /// The control's `accept` attribute (HTML #attr-input-accept), a hint for the prompt.
    pub accept: String,
    /// Without `multiple` the user may choose no more than one file.
    pub multiple: bool,
    /// File and Directory Entries API `webkitdirectory`: the user chooses a directory, which
    /// selects every file below it.
    pub directory: bool,
}

/// A drag of native files from another application (HTML #drag-and-drop-processing-model:
/// "the drag actually started in another document or application"). Frontends translate their
/// platform's drag notifications into these steps; the page actor runs the processing model.
#[derive(Clone, Debug, PartialEq)]
pub enum FileDrag {
    /// Files entered the window. One drag data item per file, with its type string: the MIME
    /// type of the file if known, or `application/octet-stream`.
    Enter { types: Vec<String> },
    /// One iteration of the processing model with the immediate user selection under the
    /// pointer (CSS pixels in the viewport).
    Over {
        node: Option<usize>,
        x: f64,
        y: f64,
        metadata: PointerMetadata,
    },
    /// The user ended the drag over the page by dropping these files.
    Drop {
        node: Option<usize>,
        x: f64,
        y: f64,
        metadata: PointerMetadata,
        files: Vec<std::path::PathBuf>,
    },
    /// The drag left the window or was canceled.
    Leave,
}

impl FileDrag {
    /// The drag data item type strings for `paths` (HTML #drag-and-drop-processing-model).
    pub fn item_types<P: AsRef<std::path::Path>>(paths: &[P]) -> Vec<String> {
        paths
            .iter()
            .map(|path| {
                if path.as_ref().is_dir() {
                    return String::new();
                }
                crate::file_select::mime_type_for_name(&crate::file_select::file_name_of(
                    path.as_ref(),
                ))
                .unwrap_or("application/octet-stream")
                .to_string()
            })
            .collect()
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PageHover {
    pub(crate) node: Option<usize>,
    pub(crate) x: f64,
    pub(crate) y: f64,
    pub(crate) metadata: PointerMetadata,
}

/// Observed native input, kept intact through coalescing and iframe routing.
/// Terminals without an exposed screen origin leave `screen` unspecified.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PointerMetadata {
    pub screen: Option<(f64, f64)>,
    pub modifiers: crate::core::Modifiers,
    /// The frontend's text selection under the pointer, for a frontend that
    /// lets the user select page text: the page runs Selection API §2's
    /// user interactions as the default actions of the primary button's
    /// press, the motion while it is held, and its release.
    pub selection: Option<PointerSelection>,
    /// Pointer Events #maybe-show-context-menu, bypassed by the user (the
    /// desktop's Shift+secondary button): the user agent shows its own menu
    /// without firing `contextmenu`, which the page could cancel.
    pub bypass_context_menu: bool,
}

/// The boundary point the frontend's layout paints nearest the pointer
/// (`None` where no selectable text is), and the serial of the frontend's
/// selection gesture, which the page's `PageEvt::Selection` reports echo.
/// Kept flat so pointer commands stay small enough to queue by value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PointerSelection {
    node: usize,
    offset: u32,
    pub serial: u32,
}

impl PointerSelection {
    pub fn new(point: Option<crate::core::TextPoint>, serial: u32) -> Self {
        let (node, offset) = point.map_or((usize::MAX, 0), |point| (point.node, point.offset));
        Self {
            node,
            offset,
            serial,
        }
    }

    pub fn point(&self) -> Option<crate::core::TextPoint> {
        (self.node != usize::MAX).then_some(crate::core::TextPoint {
            node: self.node,
            offset: self.offset,
        })
    }
}

impl Default for PointerSelection {
    fn default() -> Self {
        Self::new(None, 0)
    }
}

/// The selections of a page's Documents for its frontend to paint and copy
/// (Selection API §2): the selected parts of each Text node and the
/// replaced elements selected whole, with `user-select: none` content left
/// out, and the anchor and focus where they are in Text nodes.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SelectionReport {
    /// The last frontend selection gesture (`PointerSelection::serial`) the
    /// page processed before this report.
    pub serial: u32,
    pub segments: std::sync::Arc<crate::render::SelectionSegments>,
    pub anchor: Option<crate::core::TextPoint>,
    pub focus: Option<crate::core::TextPoint>,
}

impl PointerMetadata {
    pub(crate) fn modifier_bits(self) -> u8 {
        u8::from(self.modifiers.shift)
            | (u8::from(self.modifiers.control) << 1)
            | (u8::from(self.modifiers.alt) << 2)
            | (u8::from(self.modifiers.meta) << 3)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoundaryTier {
    Paint,
    Size,
    WidthStable,
}

#[derive(Debug)]
pub struct SubtreePatch {
    pub node: usize,
    pub html: String,
    pub tier: BoundaryTier,
}

/// A form submission planned by the page (HTML #form-submission-algorithm):
/// the parsed action URL and method; for GET, `body` is the
/// application/x-www-form-urlencoded query that replaces the action URL's
/// query ("mutate action URL"); for POST it is the entity body, in the MIME
/// type `content_type` selected by the submitter's enctype
/// ("submit as entity body").
#[derive(Debug, Clone)]
pub struct FormSubmission {
    pub action: String,
    pub method: String,
    pub content_type: String,
    pub body: Vec<u8>,
    /// The form's node document as the navigation's source, with the form's
    /// `noreferrer` referrer policy (`None`: the frontend's current document).
    pub initiator: Option<Box<crate::http::NavigationInitiator>>,
}

impl FormSubmission {
    /// The GET query, which the page's urlencoded serializer keeps ASCII.
    /// HTML #submit-mutate-action sets the action URL's query to it even
    /// when the entry list is empty.
    pub fn query(&self) -> Option<&str> {
        std::str::from_utf8(&self.body).ok()
    }
}

/// A navigation the page asks to happen in a new top-level traversable
/// (HTML #the-rules-for-choosing-a-navigable: "a new top-level traversable
/// is being requested"), or a hyperlink the user asked to follow in one.
#[derive(Debug)]
pub struct NewTraversable {
    pub url: String,
    /// The navigation's source Document's request context (`None`: none).
    pub initiator: Option<Box<crate::http::NavigationInitiator>>,
    /// The new traversable's target name; empty for `_blank`.
    pub name: String,
    /// The new traversable starts a new browsing context group, without an
    /// opener (HTML #get-an-element's-noopener).
    pub noopener: bool,
    /// Show it at once; a link the user opened in the background is not.
    pub foreground: bool,
    /// A form's submission to run there instead of a GET of `url`.
    pub submission: Option<FormSubmission>,
}

/// A top-level traversable a page created in its own browsing context
/// group (HTML #creating-a-new-auxiliary-browsing-context: `window.open`, or
/// a hyperlink's or form's named target without `noopener`). Its initial
/// `about:blank` Document runs in the opener's page engine, where the
/// opener and it reach each other synchronously; `handle` and `events` are
/// its page, as `spawn_page` returns them. The frontend presents it beside
/// the opener and keeps `handle.link()` while it shares the group.
#[derive(Debug)]
pub struct AuxiliaryTraversable {
    pub handle: PageHandle,
    pub events: tokio::sync::mpsc::Receiver<PageEvt>,
    /// Its target name (`window.name`); empty for `_blank`.
    pub name: String,
    /// The page asked for a popup window (HTML #popup-window-is-requested),
    /// whose bars `BarProp.visible` reports hidden.
    pub popup: bool,
    /// Its storage shed, a copy of the opener's (HTML #copy-session-storage).
    pub storage_shed: u64,
}

/// A page's request that the user confirm unloading it (HTML
/// #steps-to-fire-beforeunload). The page is paused (HTML #pause) until the
/// frontend answers; the question is the user agent's own, never the page's
/// text. Dropping it unanswered lets the page unload.
#[derive(Debug)]
pub struct UnloadPrompt {
    answer: tokio::sync::oneshot::Sender<bool>,
}

impl UnloadPrompt {
    pub(crate) fn new(answer: tokio::sync::oneshot::Sender<bool>) -> Self {
        Self { answer }
    }

    /// The user's answer: `true` when they confirmed leaving the page.
    pub fn answer(self, leave: bool) {
        let _ = self.answer.send(leave);
    }
}

/// The MIME type of form-encoded POST bodies TRust sends itself.
pub const FORM_URLENCODED: &str = "application/x-www-form-urlencoded";

#[derive(Debug)]
pub enum PageEvt {
    Updated {
        html: String,
        outcome: Outcome,
    },
    Patched {
        patches: Vec<SubtreePatch>,
        outcome: Outcome,
    },
    Static {
        html: String,
        outcome: Outcome,
    },
    /// A top-level navigation with its source Document's request context
    /// (`None`: the frontend's current document).
    Navigate(String, Option<Box<crate::http::NavigationInitiator>>),
    Replace(String, Option<Box<crate::http::NavigationInitiator>>),
    Reload(String),
    HistoryUpdate {
        url: String,
        replace: bool,
    },
    HistoryTraverse {
        delta: i32,
    },
    ScrollToFragment(String),
    /// Script-requested viewport offset, in CSS pixels (CSSOM View scroll()).
    ViewportScrolled {
        x: f64,
        y: f64,
    },
    PointerLock {
        request: u64,
        node: Option<usize>,
        unadjusted: bool,
    },
    /// Clipboard API #dom-clipboard-writetext after its permission check: place `text` on the
    /// frontend's system clipboard and answer with `PageCmd::ClipboardResult`.
    ClipboardWrite {
        request: u64,
        text: String,
    },
    /// HTML #show-the-picker,-if-applicable: display a file prompt and answer with
    /// `PageCmd::FilePickerResult`. A frontend without a picker answers `None` (dismissed).
    FilePicker(FilePickerRequest),
    Trouble(Vec<String>),
    Settled,
    /// The top-level Document's title (HTML #document.title) changed.
    Title(String),
    /// The page started or stopped making sound: a media element plays
    /// audibly or an audio context renders sound.
    Audible(bool),
    /// The user started or stopped having unsubmitted edits in the page's
    /// form controls or editing hosts.
    UserEdits(bool),
    /// A navigation in a new top-level traversable.
    OpenTraversable(Box<NewTraversable>),
    /// The page opened an auxiliary browsing context: a new top-level
    /// traversable of its browsing context group (`AuxiliaryTraversable`).
    OpenAuxiliary(Box<AuxiliaryTraversable>),
    /// The page asked to close its top-level traversable and nothing
    /// stopped it (HTML #definitely-close-a-top-level-traversable): the
    /// frontend closes it as it does for the user.
    CloseTraversable,
    /// The page asked that its top-level traversable be shown (HTML
    /// #dom-window-focus): the frontend brings it to the front.
    FocusTraversable,
    /// Whether the page listens for beforeunload, so that closing it or
    /// navigating it away must first check whether unloading is canceled
    /// (`PageCmd::CheckUnload`).
    BeforeUnloadListeners(bool),
    /// HTML #steps-to-fire-beforeunload: a Document asks the user to
    /// confirm that they want to unload it. The page waits for the answer.
    UnloadPrompt(UnloadPrompt),
    /// The answer to `PageCmd::CheckUnload`: whether unloading continues.
    UnloadChecked {
        request: u64,
        proceed: bool,
    },
    /// The close request steps for `PageCmd::CloseRequest` finished.
    /// `consumed` is false when nothing handled the request: HTML's
    /// "alternative processing", which the frontend performs (it stops
    /// loading, keeping the page and its scripts).
    CloseRequestHandled {
        consumed: bool,
    },
    /// Pointer Events #maybe-show-context-menu: the page fired `contextmenu`
    /// for the user's secondary button or menu key; `show` when no listener
    /// canceled it, so the frontend shows its own context menu.
    ContextMenu {
        show: bool,
    },
    /// The page's selection changed (Selection API §2), by the user or by
    /// script, or the DOM changed under a selected range.
    Selection(SelectionReport),
    /// Fullscreen API #dom-element-requestfullscreen: resize the top-level
    /// viewport to the screen (the frontend's fullscreen presentation) and
    /// answer with `PageCmd::FullscreenResult`.
    FullscreenRequest {
        request: u64,
    },
    /// Fullscreen API #exit-fullscreen: restore the normal viewport, then
    /// answer with `PageCmd::FullscreenResult`.
    FullscreenExit {
        request: u64,
    },
    /// Acknowledges a native key. Suppress the frontend default when canceled
    /// by keyboard handlers, already applied by the actor, or inapplicable
    /// (e.g. Enter in a formless input).
    KeyDefault {
        prevented: bool,
        /// Canonical text-control state after this key's checkpoint. This
        /// acknowledges native feedback without waiting for page layout.
        state: Option<EditableState>,
    },
    /// The canonical HTML focused area changed. The desktop editor follows
    /// this actor decision, including focus from label activation and script.
    Focused {
        node: Option<usize>,
    },
    EditableState(Option<EditableState>),
    /// The edit and its input-event microtask checkpoint have completed. This
    /// follows any resulting render, so a native editor can accept script
    /// changes without overwriting newer edits still queued in the actor.
    FormValueApplied {
        node: usize,
    },
    Scrolled {
        node: usize,
        top: f64,
        #[allow(dead_code)]
        left: f64,
    },
    SubmitDefault,
    SubmitForm {
        form: usize,
        submitter: Option<usize>,
        submission: Option<FormSubmission>,
    },
}

#[derive(Debug)]
pub struct PageHandle {
    pub cmds: tokio::sync::mpsc::Sender<PageCmd>,
    state: Box<PageHandleState>,
}

#[derive(Debug)]
struct PageHandleState {
    interactions: Option<tokio::sync::mpsc::Sender<PageCmd>>,
    hover: Option<tokio::sync::watch::Sender<PageHover>>,
    /// The page's top-level traversable, which keeps its engine running.
    link: TraversableLink,
}

/// A page engine's life as its frontends see it: while any of them holds one
/// of its traversables (`TraversableLink`), its scripts and fetches go on;
/// the last one gone cancels both.
#[derive(Debug)]
pub(crate) struct EngineLife {
    interrupt: std::sync::Arc<lumen::RuntimeInterrupt>,
    cache: std::sync::Arc<crate::http::PageCache>,
}

impl EngineLife {
    pub(crate) fn new(
        interrupt: std::sync::Arc<lumen::RuntimeInterrupt>,
        cache: std::sync::Arc<crate::http::PageCache>,
    ) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self { interrupt, cache })
    }

    fn cancel(&self) {
        self.interrupt.cancel();
        self.cache.cancel();
    }
}

impl Drop for EngineLife {
    fn drop(&mut self) {
        self.cancel();
    }
}

/// A browsing context group (HTML #browsing-context-group-set) whose
/// top-level traversables one page actor's Agent runs: what its frontends
/// share with the actor. A group begins with the page actor's first
/// Document; a page opening an auxiliary browsing context (`window.open`,
/// a named target) adds a traversable to it, which a frontend presents
/// beside the opener (`PageEvt::OpenAuxiliary`).
#[derive(Debug)]
pub struct GroupShared {
    commands: tokio::sync::mpsc::UnboundedSender<GroupCmd>,
    /// How many top-level traversables the group has; the actor keeps it.
    members: std::sync::atomic::AtomicUsize,
    life: std::sync::Weak<EngineLife>,
    /// The network state the group's Documents share: their fetches, image
    /// responses and blob URL store, which every frontend of the group
    /// presents from.
    cache: std::sync::Arc<crate::http::PageCache>,
    blobs: BlobMap,
}

impl GroupShared {
    pub(crate) fn new(
        commands: tokio::sync::mpsc::UnboundedSender<GroupCmd>,
        life: &std::sync::Arc<EngineLife>,
        cache: std::sync::Arc<crate::http::PageCache>,
        blobs: BlobMap,
    ) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            commands,
            members: std::sync::atomic::AtomicUsize::new(1),
            life: std::sync::Arc::downgrade(life),
            cache,
            blobs,
        })
    }

    /// The actor's count of the group's traversables changed.
    pub(crate) fn set_members(&self, members: usize) {
        self.members
            .store(members, std::sync::atomic::Ordering::Release);
    }

    /// A frontend's hold on the group's traversable `traversable`, or `None`
    /// once no frontend keeps the engine running.
    pub(crate) fn link(self: &std::sync::Arc<Self>, traversable: u32) -> Option<TraversableLink> {
        let life = self.life.upgrade()?;
        Some(TraversableLink(std::sync::Arc::new(LinkInner {
            group: self.clone(),
            life,
            traversable,
        })))
    }
}

/// Requests a group's frontends make of its page actor.
pub enum GroupCmd {
    /// The frontend's navigation of a traversable produced its next Document
    /// (`spawn_page_in`).
    Load(Box<GroupLoad>),
    /// No frontend holds the traversable any more: once its Document has
    /// unloaded, it is discarded (HTML #destroy-a-top-level-traversable).
    Release { traversable: u32 },
}

/// A Document for a traversable of a page actor's group: what
/// `spawn_page` would give a new actor.
pub struct GroupLoad {
    pub(crate) traversable: u32,
    pub(crate) html: String,
    pub(crate) env: PageEnv,
    pub(crate) cmds: tokio::sync::mpsc::Receiver<PageCmd>,
    pub(crate) interactions: tokio::sync::mpsc::Receiver<PageCmd>,
    pub(crate) hover: tokio::sync::watch::Receiver<PageHover>,
    pub(crate) events: tokio::sync::mpsc::Sender<PageEvt>,
}

/// A frontend's hold on a top-level traversable of a page engine's browsing
/// context group. Every page handle holds its traversable's; a frontend
/// keeps one across its navigations while the traversable shares its group
/// with others (`shared`), so that its next Document is created in the same
/// engine (`spawn_page_in`), where its opener and the Windows it opened can
/// still reach it. The traversable is discarded once its last link is
/// dropped and its Document has unloaded.
#[derive(Clone, Debug)]
pub struct TraversableLink(std::sync::Arc<LinkInner>);

#[derive(Debug)]
struct LinkInner {
    group: std::sync::Arc<GroupShared>,
    life: std::sync::Arc<EngineLife>,
    traversable: u32,
}

impl Drop for LinkInner {
    fn drop(&mut self) {
        let _ = self.group.commands.send(GroupCmd::Release {
            traversable: self.traversable,
        });
    }
}

impl TraversableLink {
    /// Whether the traversable's group has other traversables, which a
    /// navigation must keep it with.
    pub fn shared(&self) -> bool {
        self.0
            .group
            .members
            .load(std::sync::atomic::Ordering::Acquire)
            > 1
    }

    /// The network state the group's Documents share (`GroupShared`).
    pub(crate) fn cache(&self) -> std::sync::Arc<crate::http::PageCache> {
        self.0.group.cache.clone()
    }

    pub(crate) fn blobs(&self) -> BlobMap {
        self.0.group.blobs.clone()
    }

    pub(crate) fn traversable(&self) -> u32 {
        self.0.traversable
    }

    pub(crate) fn send(&self, command: GroupCmd) -> bool {
        self.0.group.commands.send(command).is_ok()
    }

    /// Whether this frontend's hold is the only one keeping the engine
    /// running.
    fn sole(&self) -> bool {
        std::sync::Arc::strong_count(&self.0.life) == 1
    }

    /// The engine's interrupt, which its page thread polls.
    pub(crate) fn interrupt(&self) -> std::sync::Arc<lumen::RuntimeInterrupt> {
        self.0.life.interrupt.clone()
    }

    /// The first traversable of a group no actor serves, for handles that
    /// tests drive themselves.
    #[cfg(test)]
    pub(crate) fn for_test(interrupt: std::sync::Arc<lumen::RuntimeInterrupt>) -> Self {
        let cache = std::sync::Arc::<crate::http::PageCache>::default();
        let life = EngineLife::new(interrupt, cache.clone());
        let (commands, _) = tokio::sync::mpsc::unbounded_channel();
        GroupShared::new(commands, &life, cache, Default::default())
            .link(0)
            .expect("the test group's engine life is held")
    }
}

impl PageHandle {
    pub(crate) fn from_lumen_parts(
        cmds: tokio::sync::mpsc::Sender<PageCmd>,
        interactions: tokio::sync::mpsc::Sender<PageCmd>,
        hover: tokio::sync::watch::Sender<PageHover>,
        link: TraversableLink,
    ) -> Self {
        Self {
            cmds,
            state: Box::new(PageHandleState {
                interactions: Some(interactions),
                hover: Some(hover),
                link,
            }),
        }
    }

    /// The page's top-level traversable, which a frontend keeps across a
    /// navigation while it shares its browsing context group with others.
    pub fn link(&self) -> &TraversableLink {
        &self.state.link
    }

    pub fn try_send_user(
        &self,
        command: PageCmd,
    ) -> Result<(), tokio::sync::mpsc::error::TrySendError<PageCmd>> {
        debug_assert!(command.is_user_interaction());
        self.state
            .interactions
            .as_ref()
            .unwrap_or(&self.cmds)
            .try_send(command)
    }

    pub fn try_send_navigation_click(
        &self,
        node: usize,
    ) -> Result<(), tokio::sync::mpsc::error::TrySendError<PageCmd>> {
        // HTML #user-interaction-task-source / #event-loop-processing-model:
        // a link click is a queued task, not a committed navigation. Its
        // handler may cancel the default and retain this realm. Aborting a
        // running task here skips finally blocks and can strand its scheduler
        // or controlled inputs. Actual navigation/Stop still calls retire().
        self.try_send_user(PageCmd::Click(node))
    }

    pub(crate) fn user_input_sender(&self) -> tokio::sync::mpsc::Sender<PageCmd> {
        self.state
            .interactions
            .as_ref()
            .unwrap_or(&self.cmds)
            .clone()
    }

    pub(crate) fn send_reserved_user(
        &self,
        permit: tokio::sync::mpsc::OwnedPermit<PageCmd>,
        command: PageCmd,
    ) {
        permit.send(command);
    }

    pub fn send_hover(&self, node: Option<usize>, x: f64, y: f64) -> bool {
        self.send_hover_with_metadata(node, x, y, PointerMetadata::default())
    }

    pub fn send_hover_with_metadata(
        &self,
        node: Option<usize>,
        x: f64,
        y: f64,
        metadata: PointerMetadata,
    ) -> bool {
        let hover = PageHover {
            node,
            x,
            y,
            metadata,
        };
        let Some(sender) = self.state.hover.as_ref() else {
            return self
                .try_send_user(PageCmd::Hover {
                    node,
                    x,
                    y,
                    metadata,
                })
                .is_ok();
        };
        sender.send(hover).is_ok()
    }

    /// End the page now, its scripts and fetches with it, unless other
    /// traversables of its group keep its engine running: then only this
    /// Document's frontend goes away when the handle is dropped.
    pub fn retire(&self) {
        if self.state.link.sole() {
            self.state.link.0.life.cancel();
        }
    }

    /// End the page as its Documents unload (HTML
    /// #unload-a-document-and-its-descendants): its pagehide,
    /// visibilitychange and unload handlers run, and keepalive fetches they
    /// start outlive it. A page still running after [`PAGE_END_GRACE`] is
    /// retired regardless.
    pub fn unload(self, runtime: &tokio::runtime::Handle) {
        self.end_with(PageCmd::Unload, runtime);
    }

    /// End the page to free memory (Page Lifecycle #discarding): its
    /// Documents are frozen (their freeze handlers run) and not unloaded.
    pub fn discard(self, runtime: &tokio::runtime::Handle) {
        self.end_with(PageCmd::Discard, runtime);
    }

    fn end_with(self, command: PageCmd, runtime: &tokio::runtime::Handle) {
        if self.cmds.try_send(command).is_err() {
            // The actor cannot take it: retire now (dropping `self`).
            return;
        }
        runtime.spawn(async move {
            let _ = tokio::time::timeout(PAGE_END_GRACE, self.cmds.closed()).await;
            drop(self);
        });
    }

    /// HTML #nav-stop for a page whose actor and scripts stay alive (the
    /// user's stop, or the alternative processing of an unconsumed close
    /// request): the actor runs #abort-a-document-and-its-descendants as a
    /// task (`PageCmd::StopLoading`), which cancels the document's fetches,
    /// its scripts' fetch() and XMLHttpRequest included, with their
    /// script-visible results. Returns whether any fetch was still loading.
    pub fn stop_loading(&self) -> bool {
        let cache = &self.state.link.0.life.cache;
        let loading = cache.loading();
        if self.try_send_user(PageCmd::StopLoading).is_err() {
            // The actor cannot take the task: still stop the browser's own
            // fetches for the document.
            return cache.stop_loading() || loading;
        }
        loading
    }

    #[cfg(test)]
    pub(crate) fn from_test_sender(cmds: tokio::sync::mpsc::Sender<PageCmd>) -> Self {
        Self {
            cmds,
            state: Box::new(PageHandleState {
                interactions: None,
                hover: None,
                link: TraversableLink::for_test(Default::default()),
            }),
        }
    }
}

/// How long a page's unload or freeze handlers may run once it ends before
/// it is retired regardless (`PageHandle::unload`, `PageHandle::discard`).
pub const PAGE_END_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// Finish resident page cleanup before the process unloads native GPU drivers.
/// Call once at process shutdown, after dropping the frontend's page handles
/// and event receivers. Keep the network runtime alive to service cancellation.
/// Further page actors are rejected; ordinary navigation remains asynchronous.
pub fn shutdown_page_threads() {
    crate::lumen_backend::shutdown_spare_engine();
    crate::page_threads::shutdown();
}

/// [`shutdown_page_threads`] for a browser that has just unloaded its pages
/// ([`PageHandle::unload`]): first give their unload handlers, and the
/// keepalive fetches they started (Fetch #request-keepalive-flag), up to
/// `grace` to finish. Keep the network runtime alive meanwhile.
pub fn shutdown_page_threads_after(grace: std::time::Duration) {
    crate::lumen_backend::shutdown_spare_engine();
    let deadline = std::time::Instant::now() + grace;
    while std::time::Instant::now() < deadline
        && (crate::page_threads::running() || crate::http::keepalive_fetches_in_flight() > 0)
    {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    crate::page_threads::shutdown();
}

/// Let navigations in this process claim a pre-warmed spare page engine
/// instead of building a cold one. Browsing frontends call this once at
/// startup. With `refill`, each navigation that renders starts warming the
/// next spare; without it, only [`prewarm_page_engine`] creates one. Returns
/// false when `TRUST_NO_SPARE_ENGINE` keeps every navigation cold. Library
/// users and tests never get a spare implicitly.
pub fn enable_spare_page_engine(refill: bool) -> bool {
    crate::lumen_backend::set_spare_engine_enabled(true, refill)
}

/// Start warming a spare page engine now, if enabled and none is waiting,
/// so that it can serve the next (typically the first) navigation.
pub fn prewarm_page_engine() {
    crate::lumen_backend::prewarm_spare_engine();
}

/// Whether a frontend's start address can lead to a scripted HTML document,
/// so that warming a spare page engine at startup may serve it. A syntactic
/// guess: `http`, `https`, `file` and scheme-less addresses (bare hosts and
/// paths) qualify; other protocols never use a page engine. A wrong guess
/// costs only an idle spare.
pub fn start_address_may_use_page_engine(address: &str) -> bool {
    address.split_once("://").is_none_or(|(scheme, _)| {
        ["http", "https", "file"]
            .iter()
            .any(|web| scheme.eq_ignore_ascii_case(web))
    })
}

/// A navigation's document rendered: warm the next navigation's engine.
pub(crate) fn refill_spare_page_engine() {
    crate::lumen_backend::refill_spare_engine();
}

pub fn spawn_page(
    html: String,
    env: PageEnv,
) -> (PageHandle, tokio::sync::mpsc::Receiver<PageEvt>) {
    crate::lumen_backend::spawn_page(html, env)
}

/// [`spawn_page`] for the next Document of `link`'s traversable, which the
/// engine running its browsing context group creates (HTML #navigate keeps a
/// navigable in its group). The handle and events behave as `spawn_page`'s;
/// the events end at once when the engine has gone.
pub fn spawn_page_in(
    link: &TraversableLink,
    html: String,
    env: PageEnv,
) -> (PageHandle, tokio::sync::mpsc::Receiver<PageEvt>) {
    let (cmd_tx, cmds) = tokio::sync::mpsc::channel(16);
    let (interaction_tx, interactions) = tokio::sync::mpsc::channel(16);
    let (hover_tx, hover) = tokio::sync::watch::channel(PageHover {
        node: None,
        x: 0.0,
        y: 0.0,
        metadata: PointerMetadata::default(),
    });
    let (events, event_rx) = tokio::sync::mpsc::channel(16);
    // Without the engine, `events` is dropped with the command.
    link.send(GroupCmd::Load(Box::new(GroupLoad {
        traversable: link.traversable(),
        html,
        env,
        cmds,
        interactions,
        hover,
        events,
    })));
    (
        PageHandle::from_lumen_parts(cmd_tx, interaction_tx, hover_tx, link.clone()),
        event_rx,
    )
}

/// Wrap a platform bootstrap (Window or worker prelude) so that it owns the host boundary
/// privately. Every TRust host binding is captured into a lexical binding of one enclosing
/// function before the bootstrap runs; after it returns, every `__`-prefixed global property
/// (host bindings, the configuration object and the bootstrap's own rendezvous values) and the
/// engine's Test262 `$262` host object are deleted. `ShadowRealm` (a TC39 proposal no browser
/// exposes) is removed as well: every Realm it creates carries the engine's `$262` and `print`
/// hooks, which author script must not reach. No author script can run before this
/// completes, so page and worker code never observe the host boundary: the global surface is
/// the standard one, and only the platform's closures and the native host retain capabilities.
/// The enclosed code is strict, so author callbacks cannot walk `Function.prototype.caller`
/// or `arguments` into platform frames (ECMA-262 #sec-forbidden-extensions).
/// Native code reaches each Realm's control object through the host-rooted `controls` slot.
pub(crate) fn private_bootstrap(source: &str) -> String {
    let names: Vec<&str> = host_boundary_signatures().map(|(name, _)| name).collect();
    format!(
        "(function () {{\n\
         \"use strict\";\n\
         const {{ {captures} }} = globalThis;\n\
         {source}\n\
         ;for (const name of Object.getOwnPropertyNames(globalThis))\n\
         \x20   if (name.slice(0, 2) === \"__\") Reflect.deleteProperty(globalThis, name);\n\
         Reflect.deleteProperty(globalThis, \"$262\");\n\
         Reflect.deleteProperty(globalThis, \"ShadowRealm\");\n\
         }})();\n",
        captures = names.join(", "),
    )
}

/// The worker platform bootstrap source, before [`private_bootstrap`] encloses it.
pub(crate) fn worker_prelude() -> &'static str {
    static PRELUDE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    PRELUDE
        .get_or_init(|| {
            let ports = platform_block("/*__PORTS_BEGIN__*/", "/*__PORTS_END__*/");
            let codec = platform_block("/*__SC_CODEC_BEGIN__*/", "/*__SC_CODEC_END__*/");
            let features = platform_block("/*__FEATURES_BEGIN__*/", "/*__FEATURES_END__*/");
            let bitmap = platform_block("/*__IMAGE_BITMAP_BEGIN__*/", "/*__IMAGE_BITMAP_END__*/");
            // HTML OffscreenCanvas, its 2D and bitmaprenderer contexts and
            // WebGL are [Exposed=(Window,Worker)].
            let canvas = platform_block("/*__CANVAS_BEGIN__*/", "/*__CANVAS_END__*/");
            let crypto = platform_block("/*__CRYPTO_BEGIN__*/", "/*__CRYPTO_END__*/");
            let domexception = platform_block("/*__DOMEXCEPTION_BEGIN__*/", "/*__DOMEXCEPTION_END__*/");
            let abort = platform_block("/*__ABORT_BEGIN__*/", "/*__ABORT_END__*/");
            let streams = platform_block("/*__STREAMS_BEGIN__*/", "/*__STREAMS_END__*/");
            let file = platform_block("/*__FILE_BEGIN__*/", "/*__FILE_END__*/");
            let blob_url = platform_block("/*__BLOB_URL_BEGIN__*/", "/*__BLOB_URL_END__*/");
            let fetch = platform_block("/*__FETCH_BEGIN__*/", "/*__FETCH_END__*/");
            let xhr = platform_block("/*__XHR_BEGIN__*/", "/*__XHR_END__*/");
            let websocket = platform_block("/*__WEBSOCKET_BEGIN__*/", "/*__WEBSOCKET_END__*/");
            let eventsource = platform_block("/*__EVENTSOURCE_BEGIN__*/", "/*__EVENTSOURCE_END__*/");
            let dedicated_worker =
                platform_block("/*__DEDICATED_WORKER_BEGIN__*/", "/*__DEDICATED_WORKER_END__*/");
            let cache_storage =
                platform_block("/*__CACHE_STORAGE_BEGIN__*/", "/*__CACHE_STORAGE_END__*/");
            let indexed_db = platform_block("/*__INDEXEDDB_BEGIN__*/", "/*__INDEXEDDB_END__*/");
            let rejection = platform_block("/*__REJECTION_BEGIN__*/", "/*__REJECTION_END__*/");
            let wasm = platform_block("/*__WASM_BEGIN__*/", "/*__WASM_END__*/");
            let urlpattern = platform_block("/*__URLPATTERN_BEGIN__*/", "/*__URLPATTERN_END__*/");
            let permissions =
                platform_block("/*__PERMISSIONS_BEGIN__*/", "/*__PERMISSIONS_END__*/");
            let navigator = platform_block("/*__NAVIGATOR_BEGIN__*/", "/*__NAVIGATOR_END__*/");
            let performance = platform_block("/*__PERFORMANCE_BEGIN__*/", "/*__PERFORMANCE_END__*/");
            let geometry = wrapped_platform_block("/*__GEOMETRY_BEGIN__*/", "/*__GEOMETRY_END__*/");
            let string_list =
                wrapped_platform_block("/*__DOM_STRING_LIST_BEGIN__*/", "/*__DOM_STRING_LIST_END__*/");
            let class_strings =
                platform_block("/*__CLASS_STRINGS_BEGIN__*/", "/*__CLASS_STRINGS_END__*/");
            // The URL and text codec interfaces run inside the worker scope's
            // own function, so its later code binds the pristine interfaces.
            let worker_scope = WORKER_SCOPE
                .replacen(
                    "/*__URL_SHARED__*/",
                    platform_block("/*__URL_BEGIN__*/", "/*__URL_END__*/"),
                    1,
                )
                .replacen(
                    "/*__TEXT_CODEC_SHARED__*/",
                    platform_block("/*__TEXT_CODEC_BEGIN__*/", "/*__TEXT_CODEC_END__*/"),
                    1,
                )
                .replacen(
                    "/*__HTML_EVENTS_SHARED__*/",
                    platform_block("/*__HTML_EVENTS_BEGIN__*/", "/*__HTML_EVENTS_END__*/"),
                    1,
                );
            format!(
                "{domexception}\n{worker_scope}\n{geometry}\n{string_list}\n{navigator}\n{permissions}\n{ports}\n{codec}\n{features}\n{abort}\n{streams}\n{file}\n{blob_url}\n{fetch}\n{cache_storage}\n{indexed_db}\n{xhr}\n{websocket}\n{eventsource}\n{dedicated_worker}\n{rejection}\n__rejection_bind(__rejection_api);\n{bitmap}\n{WEBGL}\n{canvas}\n{crypto}\n{urlpattern}\n{wasm}\n\
                 __port_api.setCodec(__sc_serialize, __sc_deserialize);\n\
                 __port_api.setStreams(__streams_api);\n\
                 __port_api.setBitmaps(__bitmap_api); __sc_bitmap_codec(__bitmap_api);\n\
                 delete globalThis.__bitmap_api; delete globalThis.__sc_bitmap_codec;\n\
                 __port_api.setCanvases(__canvas_api.offscreenCodec); __sc_canvas_codec(__canvas_api.offscreenCodec);\n\
                 delete globalThis.__canvas_api; delete globalThis.__sc_canvas_codec;\n\
                 __sc_crypto_codec(__crypto_key_api);\n\
                 __html_events_ports(__port_api.state); delete globalThis.__html_events_ports;\n\
                 delete globalThis.__crypto_key_api; delete globalThis.__sc_crypto_codec;\n\
                 (function (control) {{\n\
                 \x20   control.installPorts(__port_api);\n\
                 \x20   control.messageCodec = {{ serialize: __sc_serialize, deserialize: __sc_deserialize }};\n\
                 }})(Reflect.apply(WeakMap.prototype.get, __platform_slots(\"controls\", new WeakMap()), [globalThis]));\n\
                 delete globalThis.__port_api;\n{performance}\n{class_strings}",
                WEBGL = include_str!("js_webgl.js"),
            )
        })
        .as_str()
}

/// The AudioWorkletGlobalScope bootstrap (Web Audio #AudioWorkletGlobalScope):
/// the worker platform with the worklet layer, which still sees the port
/// API the worker bootstrap hands out.
pub(crate) fn audio_worklet_prelude() -> &'static str {
    static PRELUDE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    PRELUDE
        .get_or_init(|| {
            // The worklet layer sees the port API the worker bootstrap hands
            // out; the interfaces a worklet lacks go after everything else.
            let (layer, cleanup) = include_str!("js_audio_worklet.js")
                .split_once("/*__AUDIO_WORKLET_SCOPE_END__*/")
                .expect("the worklet layer has its cleanup part");
            format!(
                "{}\n{cleanup}",
                worker_prelude().replacen(
                    "delete globalThis.__port_api;",
                    &format!("{layer}\ndelete globalThis.__port_api;"),
                    1,
                )
            )
        })
        .as_str()
}

fn platform_block(begin: &str, end: &str) -> &'static str {
    PRELUDE
        .split_once(begin)
        .and_then(|(_, rest)| rest.split_once(end))
        .map_or("", |(block, _)| block)
}

fn wrapped_platform_block(begin: &str, end: &str) -> String {
    let block = platform_block(begin, end);
    if block.is_empty() {
        String::new()
    } else {
        format!("(function(g) {{\n{block}\n}})(globalThis);")
    }
}

const WORKER_SCOPE: &str = include_str!("js_worker.js");
pub(crate) const PRELUDE: &str = concat!(
    include_str!("js_webgl.js"),
    "\n",
    include_str!("js_platform.js")
);

pub(crate) fn decode_history_updates(json: &str) -> Vec<(String, bool)> {
    let Ok(serde_json::Value::Array(updates)) = serde_json::from_str(json) else {
        return Vec::new();
    };
    updates
        .into_iter()
        .filter_map(|update| {
            let object = update.as_object()?;
            let url = object.get("url")?.as_str()?.trim();
            if url.is_empty() {
                return None;
            }
            let replace = object
                .get("replace")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            Some((url.to_string(), replace))
        })
        .collect()
}

fn phase(label: &str) {
    if std::env::var_os("TRUST_NET_TRACE").is_some() {
        eprintln!("js : @{:>6}ms {label}", crate::http::trace_ms());
    }
}

/// HTML activation behavior is independent of author scripts. Both initial
/// loading paths must retain the canonical page actor for native controls.
pub(crate) fn needs_live_dom(dom: &Dom) -> bool {
    dom.hover_css_affects_rendering()
        || dom.focus_css_affects_rendering()
        // CSS Animations 1 #animations: the page actor samples the animation
        // origin on the document timeline.
        || dom.css_animations_declared()
        || clickable_set_for_dom(dom, &std::collections::HashSet::new()).1
        || element_referrer_policies(dom)
}

/// Whether the document has `audio` or `video` elements, whose players
/// only a page actor runs.
pub(crate) fn has_media_elements(dom: &Dom) -> bool {
    dom.composed_descendants(dom.document())
        .into_iter()
        .any(|node| matches!(dom.tag_name(node), Some("video" | "audio")))
}

/// Whether an element chooses its own requests' referrer policy: a
/// `referrerpolicy` attribute (HTML #referrer-policy-attributes) or a
/// `noreferrer` hyperlink or form (#link-type-noreferrer). The page actor
/// owns those elements' image requests and navigations, so the canonical DOM
/// applies them.
fn element_referrer_policies(dom: &Dom) -> bool {
    dom.composed_descendants(dom.document())
        .into_iter()
        .any(|node| {
            hyperlink_chooses_referrer(dom, node)
                || (matches!(
                    dom.tag_name(node),
                    Some("img" | "iframe" | "link" | "script")
                ) && dom.attr(node, "referrerpolicy").is_some())
                || (dom.tag_name(node) == Some("form") && has_noreferrer(dom, node))
        })
}

/// An `a` or `area` whose hyperlink referrer policy (HTML
/// #hyperlink-referrer-policy) is its own rather than the Document's.
fn hyperlink_chooses_referrer(dom: &Dom, node: usize) -> bool {
    matches!(dom.tag_name(node), Some("a" | "area"))
        && (dom.attr(node, "referrerpolicy").is_some() || has_noreferrer(dom, node))
}

fn has_noreferrer(dom: &Dom, node: usize) -> bool {
    dom.attr(node, "rel").is_some_and(|rel| {
        rel.split_ascii_whitespace()
            .any(|token| token.eq_ignore_ascii_case("noreferrer"))
    })
}

pub(crate) fn clickable_set_for_dom(
    dom: &Dom,
    listeners: &std::collections::HashSet<usize>,
) -> (std::collections::HashSet<usize>, bool) {
    clickable_set_in(dom, &listening_walk(dom, listeners), listeners)
}

/// Every composed descendant of the Document (as `Dom::composed_descendants`
/// visits them), with whether it or a composed ancestor (shadow roots
/// included, as `parent_composed` reaches them) has a click listener or an
/// `onclick` content attribute. One walk replaces a composed-ancestor walk
/// per element.
fn listening_walk(dom: &Dom, listeners: &std::collections::HashSet<usize>) -> Vec<(usize, bool)> {
    let listens = |node: usize| listeners.contains(&node) || dom.attr(node, "onclick").is_some();
    let mut walk = Vec::new();
    let mut stack = vec![(dom.document(), listens(dom.document()))];
    while let Some((node, listening)) = stack.pop() {
        if node != dom.document() {
            walk.push((node, listening));
        }
        let start = stack.len();
        stack.extend(
            dom.child_iter(node)
                .map(|child| (child, listening || listens(child))),
        );
        if let Some(shadow) = dom.shadow_root(node) {
            let listening = listening || listens(shadow);
            stack.extend(
                dom.child_iter(shadow)
                    .map(|child| (child, listening || listens(child))),
            );
        }
        stack[start..].reverse();
    }
    walk
}

fn clickable_set_in(
    dom: &Dom,
    walk: &[(usize, bool)],
    listeners: &std::collections::HashSet<usize>,
) -> (std::collections::HashSet<usize>, bool) {
    use rustc_hash::FxHashSet;
    use std::collections::HashSet;

    let mut candidates: FxHashSet<usize> = walk
        .iter()
        .map(|&(node, _)| node)
        .filter(|&node| match dom.tag_name(node) {
            None => false,
            Some("button") => true,
            Some("summary") if dom.is_details_summary(node) => true,
            Some(_) => {
                dom.attr(node, "onclick").is_some() || dom.attr(node, "role") == Some("button")
            }
        })
        .collect();
    // A hyperlink with its own referrer policy is followed by the page actor,
    // which applies it (HTML #following-hyperlinks-2).
    let anchors: Vec<usize> = walk
        .iter()
        .filter(|&&(node, listening)| {
            dom.tag_name(node) == Some("a") && (listening || hyperlink_chooses_referrer(dom, node))
        })
        .map(|&(node, _)| node)
        .collect();
    candidates.extend(listeners.iter().copied());
    let cursor_started = Instant::now();
    // Only an element with a click listener on itself or a composed ancestor
    // becomes an activation target by its `cursor: pointer`.
    let listening: Vec<usize> = walk
        .iter()
        .filter(|&&(_, listening)| listening)
        .map(|&(node, _)| node)
        .collect();
    for node in dom.cursor_style_candidates(&listening) {
        if dom.computed_style(node, "cursor").as_deref() == Some("pointer") {
            candidates.insert(node);
        }
    }
    phase(&format!(
        "extract_live: cursor loop over {} nodes +{}ms",
        listening.len(),
        cursor_started.elapsed().as_millis()
    ));
    candidates.retain(|&node| !matches!(dom.tag_name(node), None | Some("html" | "body")));
    let mut containers = FxHashSet::default();
    for node in candidates.iter().chain(&anchors) {
        let mut current = dom.parent_composed(*node);
        while let Some(parent) = current {
            // Every recorded container's composed ancestors were recorded
            // along with it; the rest of this chain is already present.
            if !containers.insert(parent) {
                break;
            }
            current = dom.parent_composed(parent);
        }
    }
    let mut clickable: HashSet<usize> = candidates
        .iter()
        .copied()
        .filter(|node| !containers.contains(node))
        .collect();
    clickable.extend(anchors);
    // HTML #tabindex-value / #click-focusable: generic focusable elements
    // need their own native activation identity, including nested menus.
    // Do not prune a focus target merely because it contains another target.
    clickable.extend(
        walk.iter()
            .map(|&(node, _)| node)
            .filter(|&node| dom.has_tabindex(node)),
    );
    let has_forms = walk.iter().any(|&(node, _)| {
        matches!(
            dom.tag_name(node),
            Some("form" | "input" | "button" | "select" | "textarea")
        ) || dom.is_contenteditable_host(node)
    });
    let has_any = !clickable.is_empty() || has_forms;
    (clickable, has_any)
}

/// The rendering update's activation and hover hit-test metadata from one
/// composed walk: `clickable_set_for_dom` and `hover_set_for_dom`.
pub(crate) fn interaction_sets_for_dom(
    dom: &Dom,
    click_listeners: &std::collections::HashSet<usize>,
    hover_listeners: &std::collections::HashSet<usize>,
) -> InteractionSets {
    let walk = listening_walk(dom, click_listeners);
    let (clickable, has_interaction) = clickable_set_in(dom, &walk, click_listeners);
    let (hover, complete_hover_hits) =
        hover_set_in(dom, walk.iter().map(|&(node, _)| node), hover_listeners);
    InteractionSets {
        clickable,
        has_interaction,
        hover,
        complete_hover_hits,
    }
}

pub(crate) struct InteractionSets {
    pub(crate) clickable: std::collections::HashSet<usize>,
    pub(crate) has_interaction: bool,
    pub(crate) hover: std::collections::HashSet<usize>,
    pub(crate) complete_hover_hits: bool,
}

const HOVER_ATTRS: &[&str] = &[
    "onmouseover",
    "onmouseout",
    "onmouseenter",
    "onmouseleave",
    "onmousemove",
    "onpointerover",
    "onpointerout",
    "onpointerenter",
    "onpointerleave",
    "onpointermove",
];

/// The hover hit-test hosts: elements with pointer/mouse boundary listeners
/// or handler attributes, and whether there are any. With such listeners UI
/// Events needs every element as a possible target (`Dom::set_hover_hosts`
/// treats the set as complete); otherwise the CSS `:hover` candidates.
#[cfg(test)]
pub(crate) fn hover_set_for_dom(
    dom: &Dom,
    listener_hosts: &std::collections::HashSet<usize>,
) -> (std::collections::HashSet<usize>, bool) {
    hover_set_in(
        dom,
        dom.composed_descendants(dom.document()).into_iter(),
        listener_hosts,
    )
}

fn hover_set_in(
    dom: &Dom,
    nodes: impl Iterator<Item = usize>,
    listener_hosts: &std::collections::HashSet<usize>,
) -> (std::collections::HashSet<usize>, bool) {
    let mut hosts = listener_hosts.clone();
    hosts.extend(nodes.filter(|&node| dom.has_any_attr(node, HOVER_ATTRS)));
    let complete = !hosts.is_empty();
    let mut marked = if complete {
        hosts
    } else {
        dom.hover_css_candidates().into_iter().collect()
    };
    marked.retain(|&node| dom.tag_name(node).is_some());
    (marked, complete)
}

#[cfg(test)]
mod interaction_set_tests {
    use super::*;

    /// The single listening walk agrees with the definition it replaces:
    /// a node listens when it or a composed ancestor (`parent_composed`,
    /// which reaches shadow roots and then their hosts) has a click listener
    /// or an `onclick` attribute.
    #[test]
    fn the_listening_walk_matches_the_composed_ancestor_definition() {
        let mut dom = Dom::parse_document(
            "<div id=a onclick=''><p id=p>x<b>y</b></p></div><div id=h><span id=s>light</span></div>\
             <section id=q><a id=link href=/x>z</a><i id=i>w</i></section>\
             <iframe id=f srcdoc='<p>inner</p>'></iframe>",
        );
        let host = dom.get_by_id("h").unwrap();
        let root = dom.attach_shadow(host);
        let wrap = dom.create_element("div");
        let slot = dom.create_element("slot");
        let deep = dom.create_element("em");
        dom.append(wrap, deep);
        dom.append(wrap, slot);
        dom.append(root, wrap);
        let i = dom.get_by_id("i").unwrap();
        for listeners in [
            std::collections::HashSet::new(),
            std::collections::HashSet::from([root]),
            std::collections::HashSet::from([host, i]),
            std::collections::HashSet::from([dom.document()]),
        ] {
            let walk = listening_walk(&dom, &listeners);
            let nodes: Vec<usize> = walk.iter().map(|&(node, _)| node).collect();
            assert_eq!(nodes, dom.composed_descendants(dom.document()));
            for (node, listening) in walk {
                let mut expected = false;
                let mut current = Some(node);
                while let Some(candidate) = current {
                    if listeners.contains(&candidate) || dom.attr(candidate, "onclick").is_some() {
                        expected = true;
                    }
                    current = dom.parent_composed(candidate);
                }
                assert_eq!(listening, expected, "node {node} with {listeners:?}");
            }
        }
    }

    /// With pointer boundary listeners every element is a hover target; the
    /// set then holds only the listener hosts and the serializer marks all.
    #[test]
    fn complete_hover_hits_mark_every_element_without_listing_them() {
        let mut dom =
            Dom::parse_document("<div id=a onmouseover=''><p id=p>x</p></div><span id=s>y</span>");
        let (a, s) = (dom.get_by_id("a").unwrap(), dom.get_by_id("s").unwrap());
        let sets = interaction_sets_for_dom(&dom, &Default::default(), &Default::default());
        assert!(sets.complete_hover_hits);
        assert_eq!(sets.hover, std::collections::HashSet::from([a]));
        dom.set_hover_hosts(sets.hover, sets.complete_hover_hits);
        let html = dom.serialize_live(dom.document(), &Default::default());
        assert!(
            html.contains(&format!("data-trust-hover=\"{s}\"")),
            "{html}"
        );
    }
}

#[cfg(test)]
pub(crate) fn confined_boundaries(
    dom: &Dom,
    live_regions: &std::collections::HashSet<usize>,
    live_boundaries: &std::collections::HashSet<usize>,
    targets: Option<&[(crate::dom::NodeId, crate::dom::DirtyKind)]>,
) -> Option<Vec<(crate::dom::NodeId, BoundaryTier)>> {
    let targets = targets.filter(|targets| !targets.is_empty())?;
    let mut boundaries = Vec::new();
    for &(node, kind) in targets {
        let boundary = dom
            .relayout_boundary(node, kind, live_regions)
            .map(|node| (node, BoundaryTier::Size))
            .or_else(|| {
                dom.nearest_cached_boundary(node, kind, live_boundaries)
                    .map(|node| {
                        let tier = if kind == crate::dom::DirtyKind::Paint {
                            BoundaryTier::Paint
                        } else {
                            BoundaryTier::WidthStable
                        };
                        (node, tier)
                    })
            })?;
        if boundary.1 != BoundaryTier::Paint && !patchable_boundary(dom, boundary.0) {
            return None;
        }
        if !boundaries.iter().any(|(node, _)| *node == boundary.0) {
            boundaries.push(boundary);
        }
    }
    Some(boundaries)
}

#[cfg(test)]
fn patchable_boundary(dom: &Dom, boundary: crate::dom::NodeId) -> bool {
    let mut current = dom.parent_composed(boundary);
    while let Some(node) = current {
        if dom.tag_name(node) == Some("a") && dom.attr(node, "href").is_some() {
            return false;
        }
        current = dom.parent_composed(node);
    }
    !dom.composed_descendants(boundary).iter().any(|&node| {
        matches!(dom.tag_name(node), Some("input" | "select" | "textarea"))
            || dom.is_contenteditable_host(node)
    })
}

#[cfg(test)]
pub(crate) fn render_canonical(html: &str) -> String {
    let bytes = html.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b' ' {
            let mut name_end = index + 1;
            while name_end < bytes.len()
                && (bytes[name_end].is_ascii_alphanumeric() || bytes[name_end] == b'-')
            {
                name_end += 1;
            }
            if name_end > index + 1
                && bytes.get(name_end) == Some(&b'=')
                && bytes.get(name_end + 1) == Some(&b'"')
            {
                let name = &html[index + 1..name_end];
                if matches!(name, "class" | "alt" | "title") || name.starts_with("aria-") {
                    let mut value_end = name_end + 2;
                    while value_end < bytes.len() && bytes[value_end] != b'"' {
                        value_end += 1;
                    }
                    index = (value_end + 1).min(bytes.len());
                    continue;
                }
            }
        }
        output.push(bytes[index]);
        index += 1;
    }
    String::from_utf8(output).unwrap_or_else(|_| html.to_string())
}

fn is_classic(type_attr: &Option<String>) -> bool {
    match type_attr {
        None => true,
        Some(value) => matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "" | "text/javascript" | "application/javascript" | "text/ecmascript"
        ),
    }
}

pub fn external_scripts(html: &str) -> Vec<String> {
    let dom = Dom::parse_document(html);
    dom.scripts()
        .into_iter()
        .filter(|(_, _, script_type, node)| {
            is_classic(script_type) && dom.attr(*node, "nomodule").is_none()
        })
        .filter_map(|(source, _, _, _)| source)
        .collect()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExternalResourceKind {
    Script,
    Sheet,
    Preload,
    Sprite,
}

pub(crate) struct ExternalResource {
    pub kind: ExternalResourceKind,
    pub source: String,
    pub cross_origin: Option<String>,
    pub initiator: &'static str,
    pub render_blocking: bool,
    /// The element's `charset` attribute: a style sheet's environment
    /// encoding label (HTML #link-type-stylesheet).
    pub charset: Option<String>,
    /// The state of the element's `referrerpolicy` attribute (HTML
    /// #referrer-policy-attributes); `None` is the empty string, which takes
    /// the Document's policy container's policy.
    pub referrer_policy: Option<crate::referrer_policy::ReferrerPolicy>,
}

impl ExternalResource {
    pub fn destination(&self) -> &'static str {
        match self.kind {
            ExternalResourceKind::Script | ExternalResourceKind::Preload => "script",
            ExternalResourceKind::Sheet => "style",
            ExternalResourceKind::Sprite => "image",
        }
    }

    pub fn cors_credentials(&self) -> Option<crate::http::CredentialsMode> {
        element_cors_credentials(
            self.cross_origin.as_deref(),
            self.kind == ExternalResourceKind::Preload,
        )
    }
}

/// HTML #create-a-potential-cors-request and #fetch-a-single-module-script:
/// absent/anonymous module crossorigin uses same-origin credentials, whereas
/// an ordinary no-CORS element includes credentials. Invalid values select
/// Anonymous, not No CORS. Keep this separate from the Referer policy.
pub(crate) fn element_cors_credentials(
    cross_origin: Option<&str>,
    module: bool,
) -> Option<crate::http::CredentialsMode> {
    match cross_origin {
        Some(value) if value.eq_ignore_ascii_case("use-credentials") => {
            Some(crate::http::CredentialsMode::Include)
        }
        Some(_) => Some(crate::http::CredentialsMode::SameOrigin),
        None if module => Some(crate::http::CredentialsMode::SameOrigin),
        None => None,
    }
}

/// Preserve the element's request context when the preload scanner starts its
/// fetch. The old URL-only lists lost crossorigin/mode. One parse also replaces
/// four independent scans, while retaining the existing per-kind/source order.
pub(crate) fn external_resources(html: &str) -> Vec<ExternalResource> {
    external_resources_at(html, 640.0, 384.0, 1.0)
}

pub(crate) fn external_resources_at(
    html: &str,
    width: f32,
    height: f32,
    density: f32,
) -> Vec<ExternalResource> {
    let mut dom = Dom::parse_document(html);
    dom.set_viewport_px(width, height);
    dom.set_device_pixel_ratio(density);
    // HTML #render-blocking-mechanism / #link-type-stylesheet: capture
    // parser-time eligibility before the complete DOM reaches the page actor.
    // Head children are encountered before the parser creates the body. A
    // stylesheet is implicitly eligible; scripts require blocking=render.
    let blocks = |node, sheet| {
        let mut parent = dom.node(node).parent;
        let mut in_head = false;
        while let Some(id) = parent {
            if dom.tag_name(id) == Some("head") {
                in_head = true;
                break;
            }
            parent = dom.node(id).parent;
        }
        in_head
            && (sheet
                || dom.attr(node, "blocking").is_some_and(|tokens| {
                    tokens
                        .split_ascii_whitespace()
                        .any(|token| token.eq_ignore_ascii_case("render"))
                }))
            && (!sheet || dom.media_matches(dom.attr(node, "media").unwrap_or("all")))
    };
    let mut scripts = Vec::new();
    let mut sheets = Vec::new();
    let mut modules = Vec::new();
    let mut sprites = Vec::new();
    let mut seen_sheets = std::collections::HashSet::new();
    let mut seen_modules = std::collections::HashSet::new();
    let mut seen_sprites = std::collections::HashSet::new();
    for (source, _, script_type, node) in dom.scripts() {
        if let Some(source) = source
            && is_classic(&script_type)
            && dom.attr(node, "nomodule").is_none()
        {
            scripts.push(ExternalResource {
                kind: ExternalResourceKind::Script,
                source,
                cross_origin: dom.attr(node, "crossorigin").map(str::to_string),
                initiator: "script",
                render_blocking: blocks(node, false),
                charset: dom.attr(node, "charset").map(str::to_string),
                referrer_policy: dom
                    .attr(node, "referrerpolicy")
                    .and_then(crate::referrer_policy::ReferrerPolicy::parse_attribute),
            });
        }
    }
    for node in dom.shadow_including_subtree(dom.document()) {
        let tag = dom.tag_name(node);
        let rel_has = |name: &str| {
            dom.attr(node, "rel").is_some_and(|rel| {
                rel.split_ascii_whitespace()
                    .any(|part| part.eq_ignore_ascii_case(name))
            })
        };
        let request = |kind, source: &str| ExternalResource {
            kind,
            source: source.to_string(),
            cross_origin: dom.attr(node, "crossorigin").map(str::to_string),
            // HTML #default-fetch-and-process-the-linked-resource uses css
            // for stylesheets; #fetch-a-single-module-script uses script,
            // including modulepreload. Some current engines instead expose
            // link for both; Resource Timing's informative table is not the
            // request-construction algorithm.
            initiator: match (kind, tag) {
                (ExternalResourceKind::Sheet, _) => "css",
                (ExternalResourceKind::Preload, _) => "script",
                (_, Some("link")) => "link",
                (_, Some("script")) => "script",
                _ => "other",
            },
            render_blocking: kind == ExternalResourceKind::Sheet && blocks(node, true)
                || tag == Some("script") && blocks(node, false),
            charset: dom.attr(node, "charset").map(str::to_string),
            // SVG `use` has no referrerpolicy attribute.
            referrer_policy: matches!(tag, Some("link" | "script"))
                .then(|| dom.attr(node, "referrerpolicy"))
                .flatten()
                .and_then(crate::referrer_policy::ReferrerPolicy::parse_attribute),
        };
        if tag == Some("link")
            && rel_has("stylesheet")
            && !rel_has("alternate")
            && dom.attr(node, "disabled").is_none()
            && let Some(source) = dom.attr(node, "href")
            && seen_sheets.insert(source.to_string())
        {
            sheets.push(request(ExternalResourceKind::Sheet, source));
        }
        let module = if tag == Some("link") && rel_has("modulepreload") {
            dom.attr(node, "href")
        } else if tag == Some("script")
            && dom
                .attr(node, "type")
                .is_some_and(|value| value.trim().eq_ignore_ascii_case("module"))
        {
            dom.attr(node, "src")
        } else {
            None
        };
        if let Some(source) = module
            && seen_modules.insert(source.to_string())
        {
            modules.push(request(ExternalResourceKind::Preload, source));
        }
        if tag == Some("use")
            && let Some((file, fragment)) = dom
                .attr(node, "href")
                .or_else(|| dom.attr(node, "xlink:href"))
                .map(str::trim)
                .and_then(|value| value.split_once('#'))
            && !file.is_empty()
            && !fragment.is_empty()
            && seen_sprites.insert(file.to_string())
        {
            sprites.push(request(ExternalResourceKind::Sprite, file));
        }
    }
    scripts.extend(sheets);
    scripts.extend(modules);
    scripts.extend(sprites);
    scripts
}

pub fn external_stylesheets(html: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    Dom::parse_document(html)
        .stylesheet_links()
        .into_iter()
        .filter(|href| seen.insert(href.clone()))
        .collect()
}

pub fn sprite_use_sheets(html: &str) -> Vec<String> {
    let dom = Dom::parse_document(html);
    let mut seen = std::collections::HashSet::new();
    let mut sheets = Vec::new();
    for node in dom.shadow_including_subtree(dom.document()) {
        if dom.tag_name(node) != Some("use") {
            continue;
        }
        let Some(href) = dom
            .attr(node, "href")
            .or_else(|| dom.attr(node, "xlink:href"))
            .map(str::trim)
            .filter(|href| !href.is_empty())
        else {
            continue;
        };
        let Some((file, fragment)) = href.split_once('#') else {
            continue;
        };
        if !file.is_empty() && !fragment.is_empty() && seen.insert(file.to_string()) {
            sheets.push(file.to_string());
        }
    }
    sheets
}

pub fn module_preloads(html: &str) -> Vec<String> {
    let dom = Dom::parse_document(html);
    let mut seen = std::collections::HashSet::new();
    let mut targets = Vec::new();
    for node in dom.shadow_including_subtree(dom.document()) {
        let target = match dom.tag_name(node) {
            Some("link")
                if dom.attr(node, "rel").is_some_and(|rel| {
                    rel.split_ascii_whitespace()
                        .any(|word| word.eq_ignore_ascii_case("modulepreload"))
                }) =>
            {
                dom.attr(node, "href")
            }
            Some("script")
                if dom
                    .attr(node, "type")
                    .is_some_and(|script_type| script_type.trim() == "module") =>
            {
                dom.attr(node, "src")
            }
            _ => None,
        };
        if let Some(target) = target
            && seen.insert(target.to_string())
        {
            targets.push(target.to_string());
        }
    }
    targets
}
