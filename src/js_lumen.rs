//! Engine-neutral browser contract and Lumen-backed page actor.
//!
//! The default build deliberately does not compile `js.rs`: that file owns the
//! legacy Boa implementation.  This facade keeps the browser-facing contract
//! stable while delegating JavaScript execution to the sibling Lumen engine.

use crate::dom::{DOCUMENT, Dom};
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

/// Session-lifetime, RAM-only, origin-bucketed Web Storage.
pub type WebStorage = std::sync::Arc<
    std::sync::Mutex<std::collections::HashMap<String, std::collections::HashMap<String, String>>>,
>;

#[derive(Debug)]
pub enum WorkerOut {
    Message(String),
    Error(String),
}

/// Page-lifetime mirror of JavaScript-created `blob:` URL bytes.
pub type BlobMap =
    std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, (Vec<u8>, String)>>>;

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
    pub viewport: (u16, u16),
    pub cell_px: (u16, u16),
    pub device_pixel_ratio: f32,
    pub externals: Vec<(String, Option<std::sync::Arc<Vec<u8>>>)>,
    pub sheets: Vec<(String, String)>,
    pub cache: std::sync::Arc<crate::http::PageCache>,
    pub net: Option<tokio::runtime::Handle>,
    pub storage: Option<WebStorage>,
    pub blobs: BlobMap,
}

impl PageEnv {
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn bare(url: &str) -> Self {
        Self {
            url: url.to_string(),
            viewport: (80, 24),
            cell_px: (8, 16),
            device_pixel_ratio: 1.0,
            externals: Vec::new(),
            sheets: Vec::new(),
            cache: Default::default(),
            net: None,
            storage: None,
            blobs: Default::default(),
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
    sheets: &[(String, String)],
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

pub fn css_finish(mut dom: Dom, sheets: &[(String, String)]) -> String {
    if !sheets.is_empty() {
        dom.attach_external_sheets(sheets);
    }
    dom.serialize(DOCUMENT)
}

pub fn transform(html: &str, env: &PageEnv) -> (String, Outcome) {
    crate::lumen_backend::transform(html, env)
}

#[derive(Debug)]
pub enum PageCmd {
    Click(usize),
    Key {
        node: usize,
        input: crate::core::KeyInput,
    },
    SetValue {
        node: usize,
        value: String,
        checked: Option<bool>,
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
}

impl PageCmd {
    pub(crate) fn is_user_interaction(&self) -> bool {
        matches!(
            self,
            Self::Click(_)
                | Self::Key { .. }
                | Self::SetValue { .. }
                | Self::Submit { .. }
                | Self::Scroll { .. }
                | Self::Hover { .. }
                | Self::SetScroll { .. }
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PageHover {
    pub(crate) node: Option<usize>,
    pub(crate) x: f64,
    pub(crate) y: f64,
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

#[derive(Debug, Clone)]
pub struct FormSubmission {
    pub action: String,
    pub method: String,
    pub body: String,
}

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
    Navigate(String),
    Replace(String),
    HistoryUpdate {
        url: String,
        replace: bool,
    },
    ScrollToFragment(String),
    Trouble(Vec<String>),
    Settled,
    KeyDefault {
        prevented: bool,
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
    interaction_running: std::sync::Arc<std::sync::Mutex<bool>>,
    hover: Option<tokio::sync::watch::Sender<PageHover>>,
    cache: std::sync::Arc<crate::http::PageCache>,
    runtime_interrupt: std::sync::Arc<lumen::RuntimeInterrupt>,
}

impl PageHandle {
    pub(crate) fn from_lumen_parts(
        cmds: tokio::sync::mpsc::Sender<PageCmd>,
        interactions: tokio::sync::mpsc::Sender<PageCmd>,
        interaction_running: std::sync::Arc<std::sync::Mutex<bool>>,
        hover: tokio::sync::watch::Sender<PageHover>,
        cache: std::sync::Arc<crate::http::PageCache>,
        runtime_interrupt: std::sync::Arc<lumen::RuntimeInterrupt>,
    ) -> Self {
        Self {
            cmds,
            state: Box::new(PageHandleState {
                interactions: Some(interactions),
                interaction_running,
                hover: Some(hover),
                cache,
                runtime_interrupt,
            }),
        }
    }

    pub fn try_send_user(
        &self,
        command: PageCmd,
    ) -> Result<(), tokio::sync::mpsc::error::TrySendError<PageCmd>> {
        self.try_send_user_with_navigation_preemption(command, false)
    }

    pub fn try_send_navigation_click(
        &self,
        node: usize,
    ) -> Result<(), tokio::sync::mpsc::error::TrySendError<PageCmd>> {
        self.try_send_user_with_navigation_preemption(PageCmd::Click(node), true)
    }

    fn try_send_user_with_navigation_preemption(
        &self,
        command: PageCmd,
        preempt_for_navigation: bool,
    ) -> Result<(), tokio::sync::mpsc::error::TrySendError<PageCmd>> {
        debug_assert!(command.is_user_interaction());
        let Some(sender) = &self.state.interactions else {
            return self.cmds.try_send(command);
        };
        let permit = match sender.try_reserve() {
            Ok(permit) => permit,
            Err(tokio::sync::mpsc::error::TrySendError::Full(())) => {
                return Err(tokio::sync::mpsc::error::TrySendError::Full(command));
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(())) => {
                return Err(tokio::sync::mpsc::error::TrySendError::Closed(command));
            }
        };
        if preempt_for_navigation {
            let running = self
                .state
                .interaction_running
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if !*running {
                self.state.runtime_interrupt.request_user_navigation();
            }
            permit.send(command);
            drop(running);
        } else {
            permit.send(command);
        }
        Ok(())
    }

    pub fn send_hover(&self, node: Option<usize>, x: f64, y: f64) -> bool {
        let hover = PageHover { node, x, y };
        let Some(sender) = self.state.hover.as_ref() else {
            return self.try_send_user(PageCmd::Hover { node, x, y }).is_ok();
        };
        sender.send(hover).is_ok()
    }

    pub fn retire(&self) {
        self.state.runtime_interrupt.cancel();
        self.state.cache.cancel();
    }

    #[cfg(test)]
    pub(crate) fn from_test_sender(cmds: tokio::sync::mpsc::Sender<PageCmd>) -> Self {
        Self {
            cmds,
            state: Box::new(PageHandleState {
                interactions: None,
                interaction_running: Default::default(),
                hover: None,
                cache: Default::default(),
                runtime_interrupt: Default::default(),
            }),
        }
    }
}

impl Drop for PageHandle {
    fn drop(&mut self) {
        self.retire();
    }
}

pub fn spawn_page(
    html: String,
    env: PageEnv,
) -> (PageHandle, tokio::sync::mpsc::Receiver<PageEvt>) {
    crate::lumen_backend::spawn_page(html, env)
}

pub(crate) fn worker_prelude() -> &'static str {
    static PRELUDE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    PRELUDE
        .get_or_init(|| {
            let codec = platform_block("/*__SC_CODEC_BEGIN__*/", "/*__SC_CODEC_END__*/");
            let crypto = wrapped_platform_block("/*__CRYPTO_BEGIN__*/", "/*__CRYPTO_END__*/");
            let streams = wrapped_platform_block("/*__STREAMS_BEGIN__*/", "/*__STREAMS_END__*/");
            let wasm = platform_block("/*__WASM_BEGIN__*/", "/*__WASM_END__*/");
            let urlpattern = platform_block("/*__URLPATTERN_BEGIN__*/", "/*__URLPATTERN_END__*/");
            format!("{codec}\n{WORKER_SCOPE}\n{streams}\n{crypto}\n{urlpattern}\n{wasm}")
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
pub(crate) const PRELUDE: &str = include_str!("js_platform.js");

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

pub(crate) fn clickable_set_for_dom(
    dom: &Dom,
    listeners: &std::collections::HashSet<usize>,
) -> (std::collections::HashSet<usize>, bool) {
    use std::collections::HashSet;

    let everyone = dom.composed_descendants(DOCUMENT);
    let inherent: Vec<usize> = everyone
        .iter()
        .copied()
        .filter(|&node| {
            matches!(dom.tag_name(node), Some("button" | "summary"))
                || dom.attr(node, "onclick").is_some()
                || dom.attr(node, "role") == Some("button")
        })
        .collect();
    let listens = |node: usize| -> bool {
        let mut current = Some(node);
        while let Some(candidate) = current {
            if listeners.contains(&candidate) || dom.attr(candidate, "onclick").is_some() {
                return true;
            }
            current = dom.parent_composed(candidate);
        }
        false
    };
    let anchors: Vec<usize> = everyone
        .iter()
        .copied()
        .filter(|&node| dom.tag_name(node) == Some("a") && listens(node))
        .collect();
    let mut candidates: HashSet<usize> = inherent.into_iter().collect();
    candidates.extend(listeners.iter().copied());
    let cursor_started = Instant::now();
    for &node in &everyone {
        if dom.computed_style(node, "cursor").as_deref() == Some("pointer") && listens(node) {
            candidates.insert(node);
        }
    }
    phase(&format!(
        "extract_live: cursor loop over {} nodes +{}ms",
        everyone.len(),
        cursor_started.elapsed().as_millis()
    ));
    candidates.retain(|&node| !matches!(dom.tag_name(node), None | Some("html" | "body")));
    let mut containers = HashSet::new();
    for node in candidates.iter().chain(&anchors) {
        let mut current = dom.parent_composed(*node);
        while let Some(parent) = current {
            containers.insert(parent);
            current = dom.parent_composed(parent);
        }
    }
    let mut clickable: HashSet<usize> = candidates.difference(&containers).copied().collect();
    clickable.extend(anchors);
    let has_forms = everyone.iter().copied().any(|node| {
        matches!(
            dom.tag_name(node),
            Some("form" | "input" | "button" | "select" | "textarea")
        ) || dom.is_contenteditable_host(node)
    });
    let has_any = !clickable.is_empty() || has_forms;
    (clickable, has_any)
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

pub(crate) fn hover_set_for_dom(
    dom: &Dom,
    listener_hosts: &std::collections::HashSet<usize>,
) -> (std::collections::HashSet<usize>, bool) {
    let mut hosts = listener_hosts.clone();
    for node in dom.composed_descendants(DOCUMENT) {
        if HOVER_ATTRS
            .iter()
            .any(|attribute| dom.attr(node, attribute).is_some())
        {
            hosts.insert(node);
        }
    }
    let complete = !hosts.is_empty();
    let mut marked: std::collections::HashSet<usize> = if complete {
        dom.composed_descendants(DOCUMENT)
            .into_iter()
            .filter(|&node| dom.tag_name(node).is_some())
            .collect()
    } else {
        dom.hover_css_candidates().into_iter().collect()
    };
    marked.retain(|&node| dom.tag_name(node).is_some());
    (marked, complete)
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
    for node in dom.descendants(DOCUMENT) {
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
    for node in dom.descendants(DOCUMENT) {
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
