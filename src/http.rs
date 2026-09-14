//! A deliberately small HTTP/1.1 client for the text web.
//!
//! Persistent connections (a RAM-only keep-alive pool — measured
//! 2026-06-12: serial fresh-TLS-per-request was 85% of a page load),
//! responses delimited precisely (Content-Length / chunked / EOF), no
//! compression (`Accept-Encoding: gzip, deflate`), redirects followed here.
//! HTTPS uses standard WebPKI validation (`tls::webpki_connector`),
//! not TOFU. HTML renders through our own arena DOM (`dom.rs`) laid out
//! into positioned rows by `layout2`; forms are extracted from that same
//! arena. With JS on, `execute_js` first runs the page's scripts through the
//! selected backend and lays out what they built.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use futures::StreamExt as _;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use url::{Host, Url};

use crate::doc::{Doc, DocLine, Field, FieldKind, Form, FormMethod, Kind, Link};
pub use crate::performance::{FetchTiming, NavigationType};
use crate::tls;

// Per-response memory guard, shared by HTTP framing and content decoding.
// Fetch/XHR bodies include binary application assets (archives, disk images,
// and WASM data), not just scripts. 512 MiB accommodates those resources,
// including game archives well over 100 MiB, while keeping buffering bounded.
// RFC 9112 §6.3 defines framing independently of the resource's media type.
const MAX_BODY: usize = 512 * 1024 * 1024;
// A single request's wall cap. Generous because a streamed LLM chat completion
// (Open WebUI → llama.cpp) holds the connection open for the WHOLE generation —
// a reasoning model can think for a minute or more before the body closes. The
// Request I/O retains a network timeout independent of JavaScript task
// execution. A stalled connection fails as a resource operation; it does not
// impose a wall-clock deadline on the event-loop task that initiated it.
const FETCH_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_REDIRECTS: usize = 10;
pub(crate) const USER_AGENT: &str = "TRust/0.1";

/// Global Privacy Control is enabled for this user agent. The W3C GPC
/// specification (§3.1–§3.3) requires the opt-out signal to be expressed on
/// every HTTP request when enabled, while JavaScript observes the same choice
/// through `navigator.globalPrivacyControl`.
pub(crate) const GLOBAL_PRIVACY_CONTROL: bool = true;

/// Fetch's default `Accept` value for requests whose destination is `image`.
/// Keep this separate from the document default used by [`exchange`]: an
/// image request must advertise an image destination so content-negotiating
/// CDNs return image bytes rather than an HTML fallback. (Fetch Standard,
/// "HTTP-network-or-cache fetch", step 5.)
pub const IMAGE_ACCEPT: &str = "image/png,image/svg+xml,image/*;q=0.8,*/*;q=0.5";

/// An HTTP request as the app sees it: method plus optional body.
#[derive(Clone, Debug)]
pub struct Request {
    pub method: String,
    pub url: Url,
    /// (content-type, payload) for POST and friends.
    pub body: Option<(String, Vec<u8>)>,
    /// Extra request headers the page set (XHR `setRequestHeader`, fetch
    /// `init.headers`) — `X-Requested-With`, `Authorization`, a custom
    /// `Accept`, etc. Managed headers (Host/Cookie/Content-Length/…) are NOT
    /// taken from here; see `exchange`. Empty for normal navigations.
    pub headers: Vec<(String, String)>,
    /// User-agent-owned Fetch Metadata context. It is kept apart from the
    /// page header list so JavaScript cannot forge `Sec-Fetch-*` values.
    pub(crate) fetch_metadata: Option<FetchMetadata>,
    /// The native request client for Resource Timing's TAO check. Referer can
    /// be suppressed and is not an origin or authorization credential.
    pub(crate) timing_client: Option<Url>,
    /// The page Fetch/XHR policy, when this request originated in a script
    /// realm. Navigation and browser-owned subresource requests leave this
    /// unset and retain the existing browser defaults.
    pub(crate) fetch_policy: Option<FetchPolicy>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RequestMode {
    Cors,
    NoCors,
    SameOrigin,
}

impl RequestMode {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Cors => "cors",
            Self::NoCors => "no-cors",
            Self::SameOrigin => "same-origin",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CredentialsMode {
    Omit,
    SameOrigin,
    Include,
}

#[derive(Clone, Debug)]
pub(crate) struct FetchPolicy {
    pub(crate) origin: Url,
    pub(crate) mode: RequestMode,
    pub(crate) credentials: CredentialsMode,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FetchSite {
    None,
    SameOrigin,
    SameSite,
    CrossSite,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FetchMetadata {
    pub(crate) destination: &'static str,
    pub(crate) mode: &'static str,
    pub(crate) site: FetchSite,
    pub(crate) user_activation: bool,
}

impl Request {
    pub fn get(url: Url) -> Self {
        Self {
            method: String::from("GET"),
            url,
            body: None,
            headers: Vec::new(),
            fetch_metadata: None,
            timing_client: None,
            fetch_policy: None,
        }
    }

    /// HTML #create-a-potential-cors-request. Browser-owned no-CORS consumers
    /// need the internal response bytes (not script Fetch's opaque response).
    /// A CORS consumer uses the ordinary CORS/credentials checks before it can
    /// read those bytes. No page-authored request headers participate here.
    pub(crate) fn subresource(
        url: Url,
        client: &Url,
        destination: &'static str,
        cors_credentials: Option<CredentialsMode>,
    ) -> Self {
        let mut request = Self::get(url);
        let mode = if cors_credentials.is_some() {
            "cors"
        } else {
            "no-cors"
        };
        set_fetch_metadata(&mut request, client, destination, mode);
        set_referrer(&mut request, client);
        request.fetch_policy = cors_credentials.map(|credentials| FetchPolicy {
            origin: client.clone(),
            mode: RequestMode::Cors,
            credentials,
        });
        request
    }
}

#[derive(Debug)]
pub struct Response {
    /// The URL that finally answered, after redirects.
    pub url: Url,
    pub status: u16,
    pub content_type: String,
    /// Response headers (names lowercased, sorted; the dedup'ing wire map is
    /// unordered), minus `Set-Cookie` — that's a forbidden response-header
    /// name for scripts (Fetch spec), and exposing it would leak HttpOnly
    /// cookies the jar deliberately hides. Pages read real APIs' out-of-band
    /// results from here (Steam's WebAPI puts the EResult in `x-eresult`).
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// Typed canonical-DOM → CSS-pixel output for HTML. Once present, neither
    /// terminal nor desktop may parse `body` into a presentation DOM.
    pub rendered: Option<Box<RenderedPage>>,
    /// What the page's JavaScript did, when `execute_js` ran it.
    pub js: Option<crate::js::Outcome>,
    /// The page's `blob:` URL byte mirror, when JS ran (see `js::BlobMap`).
    /// The app hangs it on the `Doc` so the image pipeline can decode an
    /// `<img src="blob:…">` (a client-generated image — Steam's login QR).
    pub blobs: Option<crate::js::BlobMap>,
    /// The living page behind this response, when its JS left
    /// something to interact with.
    pub live: Option<LivePage>,
    /// The first successfully processed HTTP/HTML declarative refresh.
    /// Frontends arm this only after committing the completely loaded
    /// document, then navigate with replacement history handling.
    pub declarative_refresh: Option<DeclarativeRefresh>,
    /// Set when response headers identify a bot-mitigation challenge (AWS WAF,
    /// Cloudflare, …), with a short human-readable label such as
    /// `"AWS WAF (challenge)"`. This is metadata for the status line only: the
    /// body still enters the HTML/JS pipeline so a browser-capable challenge
    /// can execute or present its user interaction. See `detect_challenge`.
    pub challenge: Option<String>,
    /// The FINAL hop of this exchange was a POST (a 2xx straight off the
    /// POST, with no redirect). A Post/Redirect/Get flow ends on a GET, so
    /// it stays false. The browser history uses this: a true POST result
    /// can't be refetched honestly (a re-POST double-submits), so its doc
    /// is never evicted from the trail.
    pub from_post: bool,
    /// Native Fetch timestamps, never synthesized from page JavaScript. The
    /// record follows the response into the document's own timing entry.
    pub timing: Option<Box<FetchTiming>>,
}

/// A parsed HTML/HTTP declarative refresh. WHATWG HTML's shared declarative
/// refresh steps use whole seconds and replacement history handling.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeclarativeRefresh {
    pub delay: Duration,
    pub url: Url,
}
#[derive(Debug)]
pub struct RenderedPage {
    pub layout: std::sync::Arc<crate::layout2::PixelLayout>,
    pub viewport: crate::layout2::Viewport,
    pub device_pixel_ratio: f32,
    pub forms: Vec<Form>,
    pub controls: crate::layout2::ControlMap,
    pub rich_editors:
        std::collections::HashMap<crate::dom::NodeId, crate::layout2::RichEditorPresentation>,
    pub image_urls: Vec<String>,
    pub eager_image_urls: Vec<String>,
    pub lazy_image_handles: std::collections::HashSet<crate::render::ImageHandle>,
    pub deferred_images: Vec<crate::doc::DeferredImage>,
    /// Composed-tree ancestry and named-fragment geometry needed for native
    /// interaction. These are resolved facts, not a second mutable DOM.
    pub parents: std::collections::HashMap<crate::dom::NodeId, crate::dom::NodeId>,
    pub fragment_y: std::collections::HashMap<String, f32>,
    pub semantics: crate::accessibility::SemanticTree,
    pub focus_order: Vec<crate::dom::NodeId>,
    /// Layout node ids are the resident actor's canonical arena ids. Static
    /// snapshots leave this false because there is no actor to receive input.
    pub direct_actor_nodes: bool,
}

impl Clone for RenderedPage {
    fn clone(&self) -> Self {
        Self {
            layout: self.layout.clone(),
            viewport: self.viewport,
            device_pixel_ratio: self.device_pixel_ratio,
            forms: self.forms.clone(),
            controls: self.controls.clone(),
            rich_editors: self.rich_editors.clone(),
            image_urls: self.image_urls.clone(),
            eager_image_urls: self.eager_image_urls.clone(),
            lazy_image_handles: self.lazy_image_handles.clone(),
            deferred_images: self.deferred_images.clone(),
            parents: self.parents.clone(),
            fragment_y: self.fragment_y.clone(),
            semantics: self.semantics.clone(),
            focus_order: self.focus_order.clone(),
            direct_actor_nodes: self.direct_actor_nodes,
        }
    }
}

impl RenderedPage {
    /// Render dedup compares the frontend-neutral product rather than an HTML
    /// serialization. The retained fragment cache is an adapter detail; the
    /// display list and resource/form metadata are the observable result.
    pub fn visually_eq(&self, other: &Self) -> bool {
        self.layout.presentation_eq(&other.layout)
            && self.viewport == other.viewport
            && self.device_pixel_ratio == other.device_pixel_ratio
            && self.forms == other.forms
            && self.controls == other.controls
            && self.rich_editors == other.rich_editors
            && self.image_urls == other.image_urls
            && self.eager_image_urls == other.eager_image_urls
            && self.lazy_image_handles == other.lazy_image_handles
            && self.deferred_images == other.deferred_images
            && self.parents == other.parents
            && self.fragment_y == other.fragment_y
            && self.semantics.content_eq(&other.semantics)
            && self.focus_order == other.focus_order
            && self.direct_actor_nodes == other.direct_actor_nodes
    }
}

/// CSS Display 3's document/flat-tree → box-tree → canvas pipeline, performed
/// once against the canonical DOM. Frontends consume the returned pixel layout
/// directly or quantize it through `layout2::adapt_terminal`.
pub fn render_arena(
    dom: &crate::dom::Dom,
    base: &Url,
    viewport: crate::layout2::Viewport,
    device_pixel_ratio: f32,
    seed: Option<&[Form]>,
    images: &crate::layout2::ImageSizes,
) -> RenderedPage {
    render_arena_with_layout(
        dom,
        base,
        viewport,
        device_pixel_ratio,
        seed,
        |forms, controls| {
            crate::layout2::lay_out_graphical(dom, base, viewport, forms, controls, images)
        },
    )
}

/// Package one current layout with live resource, interaction and accessibility
/// metadata. A resident actor supplies its retained geometry transaction; static
/// documents use `render_arena` above. No frontend-owned geometry is fed back
/// into synchronous script reads.
pub(crate) fn render_arena_with_layout(
    dom: &crate::dom::Dom,
    base: &Url,
    viewport: crate::layout2::Viewport,
    device_pixel_ratio: f32,
    seed: Option<&[Form]>,
    layout: impl FnOnce(&[Form], &crate::layout2::ControlMap) -> crate::layout2::GraphicalLayout,
) -> RenderedPage {
    let diagnostic = std::env::var_os("TRUST_DIAG_FRAME").is_some();
    let started = diagnostic.then(std::time::Instant::now);
    let (forms, controls) = extract_forms_arena(dom, base, seed);
    let forms_elapsed = started.map(|started| started.elapsed());
    let layout = layout(&forms, &controls);
    let layout_elapsed = started.map(|started| started.elapsed());
    let resources =
        collect_image_urls_for_boxes(dom, base, viewport, device_pixel_ratio, Some(&layout.boxes));
    let resources_elapsed = started.map(|started| started.elapsed());
    let deferred_images = resources
        .lazy_nodes
        .iter()
        .filter(|(_, source)| {
            resources
                .lazy_handles
                .contains(&crate::render::ImageHandle::for_source(source))
        })
        .filter_map(|(node, source)| {
            let rect = layout.boxes.get(node)?;
            // A lazy image in a fixed-position subtree is viewport-relative.
            // Treating a transformed fixed containing block as fixed here can
            // only prefetch it early; it cannot strand a visible resource.
            let mut current = Some(*node);
            let mut fixed = false;
            while let Some(candidate) = current {
                if dom
                    .computed_value_resolved(candidate, "position")
                    .is_some_and(|position| position.trim() == "fixed")
                {
                    fixed = true;
                    break;
                }
                current = dom.parent_flat(candidate);
            }
            Some(crate::doc::DeferredImage {
                source: source.clone(),
                rect: crate::render::CssRect::new(
                    rect.left as f32,
                    rect.top as f32,
                    rect.width as f32,
                    rect.height as f32,
                ),
                fixed,
            })
        })
        .collect();
    // The desktop adapter needs ancestry only for nodes participating in the
    // measured presentation. Retaining every DOM edge made mutations inside a
    // display:none subtree look like frontend changes even though CSS Display
    // generates no boxes for them. Include each measured node's complete
    // flat-tree ancestor chain so display:contents, slots, and shadow hosts
    // agree with the ancestry of the boxes the frontend actually paints.
    let mut parents = std::collections::HashMap::new();
    for &node in layout.boxes.keys() {
        let mut child = node;
        while let Some(parent) = dom.parent_flat(child) {
            if parents.insert(child, parent).is_some() {
                break;
            }
            child = parent;
        }
    }
    let mut fragment_y = std::collections::HashMap::new();
    for node in dom.flat_descendants(crate::dom::DOCUMENT) {
        let Some(rect) = layout.boxes.get(&node) else {
            continue;
        };
        if let Some(id) = dom.attr(node, "id").filter(|id| !id.is_empty()) {
            fragment_y.entry(id.to_string()).or_insert(rect.top as f32);
        }
        if dom.tag_name(node) == Some("a")
            && let Some(name) = dom.attr(node, "name").filter(|name| !name.is_empty())
        {
            fragment_y
                .entry(name.to_string())
                .or_insert(rect.top as f32);
        }
    }
    let semantics = crate::accessibility::SemanticTree::for_document(
        dom,
        &layout.boxes,
        &forms,
        &controls,
        None,
    );
    if let (Some(started), Some(forms_elapsed), Some(resources_elapsed), Some(layout_elapsed)) =
        (started, forms_elapsed, resources_elapsed, layout_elapsed)
    {
        eprintln!(
            "DIAGRENDER nodes={} total={}ms forms={}ms resources={}ms layout={}ms metadata={}ms boxes={}",
            dom.node_count(),
            started.elapsed().as_millis(),
            forms_elapsed.as_millis(),
            (resources_elapsed - layout_elapsed).as_millis(),
            (layout_elapsed - forms_elapsed).as_millis(),
            (started.elapsed() - resources_elapsed).as_millis(),
            layout.boxes.len(),
        );
    }
    let rich_editors = controls
        .keys()
        .filter_map(|&node| {
            crate::layout2::rich_editor_presentation(dom, base, &layout, node)
                .map(|style| (node, style))
        })
        .collect();
    let focus_order = dom.sequential_focus_order(&layout.boxes);
    RenderedPage {
        layout: std::sync::Arc::new(layout),
        viewport,
        device_pixel_ratio,
        forms,
        controls,
        rich_editors,
        image_urls: resources.all,
        eager_image_urls: resources.eager,
        lazy_image_handles: resources.lazy_handles,
        deferred_images,
        parents,
        fragment_y,
        semantics,
        focus_order,
        direct_actor_nodes: dom.render_live(),
    }
}

/// Rebuild a static HTML snapshot for a changed native environment. The DOM is
/// transient and remains inside the shared browser engine; callers receive the
/// same typed pixel contract as a resident actor and never retain or inspect a
/// presentation tree.
pub fn render_html_for_environment(
    response: &Response,
    viewport: crate::layout2::Viewport,
    device_pixel_ratio: f32,
    seed: Option<&[Form]>,
    images: &crate::layout2::ImageSizes,
) -> Option<RenderedPage> {
    let media = response
        .content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if !matches!(media.as_str(), "text/html" | "application/xhtml+xml") {
        return None;
    }
    let html = decode_body(&response.content_type, &response.body);
    let base = base_with_doc_base(&html, &response.url);
    let mut dom = crate::dom::Dom::parse_document(&html);
    dom.set_doc_url(Some(base.clone()));
    dom.set_viewport_px(viewport.width, viewport.height);
    dom.set_device_pixel_ratio(device_pixel_ratio);
    dom.rewrite_inline_svgs(Some(&base));
    Some(render_arena(
        &dom,
        &base,
        viewport,
        device_pixel_ratio,
        seed,
        images,
    ))
}

/// Turn a supported top-level image response into the same kind of media
/// document that HTML navigation creates for a supported image type. Desktop
/// has no separate image-viewer surface, so the image is embedded in a tiny
/// synthetic HTML document and then goes through the canonical DOM/layout and
/// image scheduler. The original response URL remains the document URL.
pub fn image_navigation_response(mut response: Response, mime: &str) -> Response {
    let essence = mime.split(';').next().unwrap_or("image/*").trim();
    let data_url = format!(
        "data:{essence};base64,{}",
        crate::img::base64_encode(&response.body)
    );
    response.content_type = String::from("text/html; charset=utf-8");
    response.body = format!(
        "<!doctype html><meta charset=utf-8><style>html,body{{margin:0}}img{{display:block;max-width:100%;height:auto}}</style><img src=\"{data_url}\">"
    )
    .into_bytes();
    response.rendered = None;
    response.js = None;
    response.blobs = None;
    response.live = None;
    response.declarative_refresh = None;
    response
}

/// The terminal frontend's deliberately small adapter over the shared pixel
/// layout. No HTML is parsed here: layout has already consumed the canonical
/// DOM, and this function performs only the CSS-pixel-to-cell quantization plus
/// packaging in the protocol-neutral [`Doc`] presentation contract.
pub fn adapt_rendered_terminal(
    url: &Url,
    content_type: &str,
    raw: Vec<u8>,
    mut rendered: RenderedPage,
    viewport: crate::layout2::TerminalViewport,
    alpha: &std::collections::HashMap<String, bool>,
) -> Doc {
    let deferred_images = rendered.deferred_images.clone();
    let output = crate::layout2::adapt_terminal(&rendered.layout, viewport, alpha);
    for (_, canvas) in &rendered.layout.paint.canvas_images {
        if let Some(url) = canvas.data_url() {
            if !rendered.image_urls.contains(&url) {
                rendered.image_urls.push(url.clone());
            }
            if !rendered.eager_image_urls.contains(&url) {
                rendered.eager_image_urls.push(url);
            }
        }
    }
    let hover_ids = if rendered.direct_actor_nodes {
        rendered
            .layout
            .boxes
            .keys()
            .copied()
            .map(|node| (node, node))
            .collect()
    } else {
        std::collections::HashMap::new()
    };
    Doc {
        url: Link::Http(url.clone()),
        lines: Vec::new(),
        finger: None,
        gopher: None,
        whois: None,
        rdap: None,
        dict: None,
        raw,
        wrapped_to: viewport.columns,
        cp437: false,
        meta: Some(content_type.to_string()),
        forms: rendered.forms,
        rows: output.rows,
        image_urls: rendered.image_urls,
        eager_image_urls: rendered.eager_image_urls,
        deferred_images,
        blobs: None,
        carousels: output.carousels,
        fixed: output.fixed,
        regions: output.regions,
        scroll_clips: output.scroll_clips,
        boundaries: output.boundaries,
        hover_ids,
        anchor_rows: output.anchor_rows,
        composites: output.composites,
    }
}

pub async fn fetch_graphical_image(
    page: &Url,
    source: &str,
    blobs: Option<&crate::js::BlobMap>,
) -> Result<Vec<u8>, String> {
    if source.starts_with("data:") {
        return crate::img::decode_data_url(source)
            .ok_or_else(|| String::from("invalid data image URL"));
    }
    if source.starts_with("blob:") {
        let key = source.split('#').next().unwrap_or(source);
        return blobs
            .and_then(|mirror| mirror.lock().ok())
            .and_then(|mirror| mirror.get(key).map(|(bytes, _)| bytes.clone()))
            .ok_or_else(|| String::from("blob image is not present in the page mirror"));
    }
    let url = Url::parse(source).map_err(|error| format!("invalid image URL: {error}"))?;
    if !subresource_allowed(page, &url) {
        return Err(String::from(
            "image subresource blocked by page-origin policy",
        ));
    }
    let mut request = Request::subresource(url, page, "image", None);
    set_image_accept(&mut request);
    set_referrer(&mut request, page);
    fetch(&request).await.map(|response| response.body)
}

/// A page kept alive for interaction: commands in, renders out.
#[derive(Debug)]
pub struct LivePage {
    pub handle: crate::js::PageHandle,
    pub events: tokio::sync::mpsc::Receiver<crate::js::PageEvt>,
}

/// A successful GET response, cached for one page load.
#[derive(Debug)]
pub struct CachedResp {
    pub status: u16,
    pub content_type: String,
    /// Same filtered/sorted pairs as `Response.headers` (a `seed`ed entry
    /// synthesizes just its content-type).
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// Fetch URL list, including every redirect hop. Empty means unavailable
    /// provenance (e.g. a legacy seeded entry), never proof of same-origin.
    pub(crate) url_list: Vec<Url>,
    pub(crate) timing: Option<Box<FetchTiming>>,
}

pub type FetchOutcome = Result<std::sync::Arc<CachedResp>, ()>;
pub type SharedFetch = futures::future::Shared<futures::future::BoxFuture<'static, FetchOutcome>>;

/// A per-page subresource cache with in-flight dedup. Shared across the
/// runtime (the initial `execute_js` prefetch) and the page thread (the
/// module loader, speculative import prefetch, the page's own `fetch()`).
/// One GET per URL for the whole load: a `Shared` future means the second
/// asker — the module loader, a speculative prefetch, or the bundler's
/// own `fetch()` warm-up of a chunk we already have — joins the single
/// in-flight request instead of re-downloading it. Browsers do exactly
/// this within a navigation (memory cache + preload cache). POSTs and
/// uncached API GETs bypass it (`peek` never inserts), so polling reads
/// stay fresh.
#[derive(Default)]
pub struct PageTaskScope {
    cancelled: std::sync::atomic::AtomicBool,
    tasks: std::sync::Mutex<Vec<tokio::task::AbortHandle>>,
    fetches: std::sync::Mutex<Vec<futures::future::AbortHandle>>,
}

impl PageTaskScope {
    /// Spawn work owned by one document. HTML §7.5.11 requires aborting every
    /// fetch in a document's context and discarding its pending work when the
    /// document load is stopped; keeping the abort handles in one scope makes
    /// that lifecycle boundary explicit instead of relying on result filtering.
    pub fn spawn<F>(&self, handle: &tokio::runtime::Handle, future: F)
    where
        F: std::future::Future + Send + 'static,
        F::Output: Send + 'static,
    {
        use std::sync::atomic::Ordering;

        let mut tasks = self.tasks.lock().unwrap();
        tasks.retain(|task| !task.is_finished());
        if self.cancelled.load(Ordering::Acquire) {
            return;
        }
        tasks.push(handle.spawn(future).abort_handle());
    }

    pub fn track<T>(&self, task: tokio::task::JoinHandle<T>)
    where
        T: Send + 'static,
    {
        use std::sync::atomic::Ordering;

        let mut tasks = self.tasks.lock().unwrap();
        tasks.retain(|task| !task.is_finished());
        if self.cancelled.load(Ordering::Acquire) {
            task.abort();
            return;
        }
        tasks.push(task.abort_handle());
    }

    fn track_fetch(&self, fetch: futures::future::AbortHandle) {
        use std::sync::atomic::Ordering;

        let mut fetches = self.fetches.lock().unwrap();
        if self.cancelled.load(Ordering::Acquire) {
            fetch.abort();
            return;
        }
        fetches.push(fetch);
    }

    pub fn cancel(&self) {
        use std::sync::atomic::Ordering;

        self.cancelled.store(true, Ordering::Release);
        for task in self.tasks.lock().unwrap().drain(..) {
            task.abort();
        }
        for fetch in self.fetches.lock().unwrap().drain(..) {
            fetch.abort();
        }
    }
}

#[derive(Default)]
pub struct PageCache {
    map: std::sync::Mutex<HashMap<String, CachedFetch>>,
    tasks: std::sync::Arc<PageTaskScope>,
}

struct CachedFetch {
    future: SharedFetch,
    module_imports_scanned: bool,
    resource_context: Option<ResourceCacheContext>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ResourceCacheContext {
    Unscoped,
    Element {
        origin: url::Origin,
        destination: &'static str,
        cors_credentials: Option<CredentialsMode>,
    },
}

impl ResourceCacheContext {
    fn new(
        client: &Url,
        destination: &'static str,
        cors_credentials: Option<CredentialsMode>,
    ) -> Self {
        Self::Element {
            origin: client.origin(),
            destination,
            cors_credentials,
        }
    }
}

impl CachedFetch {
    fn new(future: SharedFetch) -> Self {
        Self {
            future,
            module_imports_scanned: false,
            resource_context: None,
        }
    }
}

impl std::fmt::Debug for PageCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let n = self.map.lock().map(|m| m.len()).unwrap_or(0);
        write!(f, "PageCache({n} entries)")
    }
}

impl PageCache {
    pub fn with_task_scope(tasks: std::sync::Arc<PageTaskScope>) -> Self {
        Self {
            map: Default::default(),
            tasks,
        }
    }

    pub fn task_scope(&self) -> std::sync::Arc<PageTaskScope> {
        self.tasks.clone()
    }

    pub fn spawn<F>(&self, handle: &tokio::runtime::Handle, future: F)
    where
        F: std::future::Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.tasks.spawn(handle, future);
    }

    /// Retire this page's subresource work. Completed response bodies may
    /// remain elsewhere as immutable presentation data, but no task in this
    /// document scope is allowed to continue or schedule new network work.
    pub fn cancel(&self) {
        self.tasks.cancel();
        self.map.lock().unwrap().clear();
    }

    /// Get-or-start the shared fetch for `url`. The first caller spawns it
    /// (driven concurrently on the runtime, so speculative prefetch makes
    /// progress before anyone awaits); every later caller shares that one
    /// request. The caller is responsible for cap/`subresource_allowed`
    /// gating BEFORE starting a brand-new fetch (see `page_net_prepare`).
    pub fn fetch(&self, handle: &tokio::runtime::Handle, url: Url) -> SharedFetch {
        self.fetch_request(handle, Request::get(url), ResourceCacheContext::Unscoped)
    }

    pub(crate) fn fetch_resource(
        &self,
        handle: &tokio::runtime::Handle,
        url: Url,
        client: &Url,
        destination: &'static str,
        cors_credentials: Option<CredentialsMode>,
    ) -> SharedFetch {
        self.fetch_request(
            handle,
            Request::subresource(url, client, destination, cors_credentials),
            ResourceCacheContext::new(client, destination, cors_credentials),
        )
    }

    fn fetch_request(
        &self,
        handle: &tokio::runtime::Handle,
        request: Request,
        context: ResourceCacheContext,
    ) -> SharedFetch {
        use futures::future::FutureExt as _;
        let key = request.url.to_string();
        let mut map = self.map.lock().unwrap();
        if let Some(f) = map.get(&key)
            && (f.resource_context.as_ref() == Some(&context) || f.resource_context.is_none())
        {
            return f.future.clone();
        }
        let (abort, registration) = futures::future::AbortHandle::new_pair();
        self.tasks.track_fetch(abort);
        let fut = futures::future::Abortable::new(
            async move {
                match fetch_with_metadata(&request, Default::default()).await {
                    Ok(details) => {
                        let r = details.response;
                        Ok(std::sync::Arc::new(CachedResp {
                            status: r.status,
                            content_type: r.content_type,
                            headers: r.headers,
                            body: r.body,
                            url_list: details.url_list,
                            timing: r.timing,
                        }))
                    }
                    Err(_) => Err(()),
                }
            },
            registration,
        )
        .map(|result| result.unwrap_or(Err(())))
        .boxed()
        .shared();
        let mut entry = CachedFetch::new(fut.clone());
        entry.resource_context = Some(context);
        map.insert(key, entry);
        // Drive it now (dropping the JoinHandle doesn't cancel the task):
        // speculation overlaps with everything, even with no awaiter yet.
        self.spawn(handle, fut.clone());
        fut
    }

    /// Start (or join) the fetch for `url` and discard the handle — a
    /// fire-and-forget warm-up for speculative import prefetch. The driver
    /// task `fetch` spawned keeps it running; a later `fetch`/`peek` for
    /// the same URL joins it.
    pub fn prefetch(&self, handle: &tokio::runtime::Handle, url: Url) {
        // Discard the returned handle; the driver task `fetch` spawned
        // keeps the request running, so dropping our clone is fine.
        drop(self.fetch(handle, url));
    }

    /// Test helper for seeding a body when response headers are immaterial.
    #[cfg(test)]
    pub fn seed(&self, url: String, status: u16, content_type: String, body: Vec<u8>) {
        self.seed_with_headers(
            url,
            status,
            content_type.clone(),
            vec![(String::from("content-type"), content_type)],
            body,
        );
    }

    #[cfg(test)]
    pub fn seed_pending(&self, url: String, fetch: SharedFetch) {
        self.map
            .lock()
            .unwrap()
            .insert(url, CachedFetch::new(fetch));
    }

    /// An existing entry (in-flight or done) for `url`, or None. The
    /// page's own `fetch()` uses this to join a known subresource request
    /// WITHOUT caching arbitrary API GETs — a miss falls through to a
    /// normal, uncached fetch so polling stays honest.
    pub fn peek(&self, url: &Url) -> Option<SharedFetch> {
        self.map
            .lock()
            .unwrap()
            .get(url.as_str())
            .map(|entry| entry.future.clone())
    }

    /// A no-CORS preload must not satisfy an element's CORS load, nor may an
    /// authorization for one document origin or credentials mode authorize a
    /// different one. Retain in-flight sharing only within the same context.
    pub(crate) fn peek_resource(
        &self,
        url: &Url,
        client: &Url,
        destination: &'static str,
        cors_credentials: Option<CredentialsMode>,
    ) -> Option<SharedFetch> {
        let context = ResourceCacheContext::new(client, destination, cors_credentials);
        self.map
            .lock()
            .unwrap()
            .get(url.as_str())
            .filter(|entry| {
                entry.resource_context.as_ref() == Some(&context)
                    || entry.resource_context.is_none()
            })
            .map(|entry| entry.future.clone())
    }

    fn seed_resource(
        &self,
        url: String,
        client: &Url,
        destination: &'static str,
        cors_credentials: Option<CredentialsMode>,
        response: CachedResp,
    ) {
        use futures::future::FutureExt as _;
        let response = std::sync::Arc::new(response);
        let mut entry = CachedFetch::new(async move { Ok(response) }.boxed().shared());
        entry.resource_context = Some(ResourceCacheContext::new(
            client,
            destination,
            cors_credentials,
        ));
        self.map.lock().unwrap().insert(url, entry);
    }

    /// Speculative descendant discovery is optional (HTML's fetch-a-
    /// modulepreload-module-script-graph). Once a cached external module has
    /// been scanned, revisiting it through another importing parent must not
    /// rescan megabytes of identical source. This bit belongs to the response
    /// entry, so replacement/cancellation resets it without another URL cache.
    /// Inline modules intentionally do not use this: they can share a base URL
    /// while containing different source. Required graph loading is unchanged.
    pub(crate) fn claim_module_import_scan(&self, url: &Url) -> bool {
        let mut map = self.map.lock().unwrap();
        let Some(entry) = map.get_mut(url.as_str()) else {
            return false;
        };
        !std::mem::replace(&mut entry.module_imports_scanned, true)
    }

    /// Block the calling (page) thread on a shared fetch. The module
    /// module loader needs this: it must NOT `.await` (yielding would let the
    /// loader interleave another module load and cross up per-referrer module
    /// state), and it can't `block_on` (it already runs inside one). With a
    /// runtime `handle` the fetch is driven on the runtime
    /// and waited on via a plain channel — no yield to the JS job loop.
    /// Without one (a no-net page whose cache holds only pre-seeded, ready
    /// futures) a bare executor resolves it. Speculative prefetch usually
    /// has the body ready by now, so this rarely waits long.
    pub fn block_on_fetch(
        handle: Option<&tokio::runtime::Handle>,
        fut: SharedFetch,
    ) -> Option<std::sync::Arc<CachedResp>> {
        match handle {
            Some(handle) => {
                let (tx, rx) = std::sync::mpsc::channel();
                handle.spawn(async move {
                    let _ = tx.send(fut.await);
                });
                rx.recv().ok().and_then(|r| r.ok())
            }
            // No runtime: the only entries are pre-seeded ready futures,
            // and we're already inside the job loop's executor (nesting a
            // `block_on` there panics) — poll once, which resolves them.
            None => {
                use futures::future::FutureExt as _;
                fut.now_or_never().and_then(|r| r.ok())
            }
        }
    }

    /// Seed host-provided, preauthorized bytes (e.g. a no-network embedding
    /// fixture). Network preload consumers must use `seed_resource` to retain
    /// their authorization context and complete redirect provenance.
    pub fn seed_with_headers(
        &self,
        url: String,
        status: u16,
        content_type: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    ) {
        use futures::future::FutureExt as _;
        let resp = std::sync::Arc::new(CachedResp {
            status,
            headers,
            content_type,
            body,
            url_list: Vec::new(),
            timing: None,
        });
        let fut = async move { Ok(resp) }.boxed().shared();
        self.map.lock().unwrap().insert(url, CachedFetch::new(fut));
    }
}

/// Parse an absolute http(s) URL.
pub fn parse_url(s: &str) -> Option<Url> {
    if !(s.starts_with("http://") || s.starts_with("https://")) {
        return None;
    }
    Url::parse(s).ok()
}

/// Fetch a web URL, https-first with an http fallback. If the target is
/// https and the attempt fails at the connection level (DNS/TCP/TLS), the
/// SAME host+path is retried over plain http. Used for a bare hostname
/// typed without a scheme — explicit-scheme URLs never set this fallback.
/// An http *status* error is returned as-is (4xx/5xx is `Ok` here); only a
/// connection-level `Err` triggers the retry, and if the retry also fails
/// the ORIGINAL https error is reported.
pub async fn fetch_web_default(url: &Url) -> Result<Response, String> {
    fetch_web_default_with_referrer(url, None).await
}

/// Variant of [`fetch_web_default`] for a link activated from an existing
/// document. Keeping the referrer in this adapter means both the HTTPS
/// attempt and its HTTP fallback carry the same navigation context.
pub async fn fetch_web_default_with_referrer(
    url: &Url,
    referrer: Option<&Url>,
) -> Result<Response, String> {
    let mut first_request = Request::get(url.clone());
    set_navigation_metadata(&mut first_request, referrer);
    if let Some(source) = referrer {
        set_referrer(&mut first_request, source);
    }
    let first = fetch(&first_request).await;
    if first.is_ok() || url.scheme() != "https" {
        return first;
    }
    let http_str = format!("http://{}", &url.as_str()["https://".len()..]);
    let Some(http_url) = parse_url(&http_str) else {
        return first;
    };
    let mut fallback_request = Request::get(http_url);
    set_navigation_metadata(&mut fallback_request, referrer);
    if let Some(source) = referrer {
        set_referrer(&mut fallback_request, source);
    }
    match fetch(&fallback_request).await {
        Ok(response) => Ok(response),
        Err(_) => first,
    }
}

/// A process-global monotonic origin shared by every trace line (network
/// requests and JavaScript phase markers) so a single load's
/// timeline reads against one clock. Only consulted when tracing is on.
pub fn trace_origin() -> Instant {
    static ORIGIN: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    *ORIGIN.get_or_init(Instant::now)
}

/// Milliseconds since `trace_origin`, for trace lines.
pub fn trace_ms() -> u128 {
    trace_origin().elapsed().as_millis()
}

/// Fetch a request, following up to `MAX_REDIRECTS` redirects.
/// 301/302 rewrite POST to GET, 303 rewrites non-GET/HEAD to GET, and
/// 307/308 preserve the method and body.
/// `TRUST_NET_TRACE=1` prints one timing line per request to stderr —
/// the diagnostic for "where did the page-load time go". Each line shows
/// `@<start>ms +<duration>ms` against the shared `trace_origin`, so the
/// timeline (which requests overlap, where the gaps are) is reconstructable.
pub async fn fetch(request: &Request) -> Result<Response, String> {
    fetch_with_metadata(request, Default::default())
        .await
        .map(|details| details.response)
}

/// Navigation exposes the final request referrer to Document; image consumers
/// need the entire URL list to retain no-CORS taint across redirects.
pub(crate) struct FetchResponseDetails {
    pub response: Response,
    pub url_list: Vec<Url>,
    pub referrer: String,
}

pub(crate) async fn fetch_with_metadata(
    request: &Request,
    policy: crate::referrer_policy::ReferrerPolicy,
) -> Result<FetchResponseDetails, String> {
    fetch_with_timing(request, policy)
        .await
        .map_err(|error| error.message)
}

/// Keep transport failures distinct from rejected Fetch preconditions. Only
/// actual network attempts may produce an opaque failure timing entry.
#[derive(Debug)]
pub(crate) struct TimedFetchError {
    pub message: String,
    pub timing: Option<Box<FetchTiming>>,
}

impl From<String> for TimedFetchError {
    fn from(message: String) -> Self {
        Self {
            message,
            timing: None,
        }
    }
}

impl TimedFetchError {
    fn network(message: String, start: f64) -> Self {
        Self {
            message,
            timing: Some(Box::new(FetchTiming {
                start_time: start,
                fetch_start: start,
                response_end: crate::performance::now_ms(),
                ..Default::default()
            })),
        }
    }
}

pub(crate) async fn fetch_with_timing(
    request: &Request,
    policy: crate::referrer_policy::ReferrerPolicy,
) -> Result<FetchResponseDetails, TimedFetchError> {
    if std::env::var_os("TRUST_NET_TRACE").is_none() {
        return fetch_redirecting(request, policy).await;
    }
    let at = trace_ms();
    let started = std::time::Instant::now();
    let result = fetch_redirecting(request, policy).await;
    let ms = started.elapsed().as_millis();
    match &result {
        Ok(r) => eprintln!(
            "net: @{at:>6}ms +{ms:>5}ms {} {}B {}",
            r.response.status,
            r.response.body.len(),
            request.url
        ),
        Err(e) => eprintln!(
            "net: @{at:>6}ms +{ms:>5}ms ERR {} ({})",
            request.url, e.message
        ),
    }
    result
}

/// Fetch a request created by script. CORS requests with a non-safelisted
/// method, header, or media type perform the Fetch Standard's preflight before
/// the actual request; no-cors rejects those same requests rather than putting
/// forbidden author data on the wire. Browser-owned navigations and
/// subresources call [`fetch`] directly because they do not have a script
/// policy.
#[cfg(test)]
pub(crate) async fn fetch_script(request: &Request) -> Result<Response, String> {
    fetch_script_with_timing(request)
        .await
        .map_err(|error| error.message)
}

pub(crate) async fn fetch_script_with_timing(
    request: &Request,
) -> Result<Response, TimedFetchError> {
    let start = crate::performance::now_ms();
    let result = fetch_script_timing_inner(request).await;
    result
        .map(|mut response| {
            if let Some(timing) = response.timing.as_mut() {
                timing.start_time = start;
                if timing.redirect_start == 0.0 {
                    timing.fetch_start = start;
                }
            }
            response
        })
        .map_err(|mut error| {
            if let Some(timing) = error.timing.as_mut() {
                timing.start_time = start;
                timing.fetch_start = start;
            }
            error
        })
}

async fn fetch_timed(request: &Request) -> Result<Response, TimedFetchError> {
    fetch_with_timing(request, Default::default())
        .await
        .map(|details| details.response)
}

async fn fetch_script_timing_inner(request: &Request) -> Result<Response, TimedFetchError> {
    let Some(policy) = &request.fetch_policy else {
        return fetch_timed(request).await;
    };
    let cross_origin = !same_origin(&policy.origin, &request.url);
    if !cross_origin {
        return fetch_timed(request).await;
    }
    let needs_preflight = cors_preflight_required(request);
    if policy.mode == RequestMode::SameOrigin {
        return Err(String::from("same-origin fetch crossed origins").into());
    }
    if policy.mode == RequestMode::NoCors {
        if needs_preflight {
            return Err(String::from(
                "no-cors request uses a non-safelisted method, header, or content type",
            )
            .into());
        }
        return fetch_timed(request).await;
    }
    if needs_preflight {
        let names = cors_non_safelisted_header_names(request);
        let mut headers = vec![(
            String::from("Access-Control-Request-Method"),
            request.method.clone(),
        )];
        if !names.is_empty() {
            headers.push((
                String::from("Access-Control-Request-Headers"),
                names.join(", "),
            ));
        }
        // A preflight never carries the target origin's cookies. Use
        // `same-origin` credentials for the OPTIONS request while retaining
        // the original mode/credentials for the actual request's CORS check.
        let preflight = Request {
            method: String::from("OPTIONS"),
            url: request.url.clone(),
            body: None,
            headers,
            fetch_metadata: request.fetch_metadata.map(|mut metadata| {
                metadata.mode = "cors";
                metadata.user_activation = false;
                metadata
            }),
            timing_client: request.timing_client.clone(),
            fetch_policy: Some(FetchPolicy {
                origin: policy.origin.clone(),
                mode: RequestMode::Cors,
                credentials: CredentialsMode::SameOrigin,
            }),
        };
        // A preflight response is an internal CORS result. Do not run the
        // ordinary response tainting/filtering step over it: Fetch requires
        // reading `Access-Control-Allow-*` directly, without requiring those
        // fields to appear in `Access-Control-Expose-Headers`.
        let response = fetch_preflight(&preflight).await?;
        validate_cors_preflight(&response, request, &names)?;
    }
    fetch_timed(request).await
}

fn cors_preflight_required(request: &Request) -> bool {
    let method = request.method.to_ascii_uppercase();
    if !matches!(method.as_str(), "GET" | "HEAD" | "POST") {
        return true;
    }
    !cors_non_safelisted_header_names(request).is_empty()
        || request
            .body
            .as_ref()
            .is_some_and(|(content_type, _)| !cors_safelisted_content_type(content_type))
}

fn cors_non_safelisted_header_names(request: &Request) -> Vec<String> {
    let mut names = Vec::new();
    for (name, value) in &request.headers {
        let lower = name.to_ascii_lowercase();
        // These are generated or filtered by `exchange`; they are not part of
        // the author-controlled CORS header list.
        // `set_referrer` also stores the browser's Referer in this wire list.
        // Fetch §4.6 appends it after §4.4's preflight decision: it must not
        // trigger OPTIONS or appear in Access-Control-Request-Headers (§4.8).
        // https://fetch.spec.whatwg.org/#http-network-or-cache-fetch
        let generated_content_type = lower == "content-type" && request.body.is_some();
        if lower.starts_with("access-control-")
            || generated_content_type
            || matches!(
                lower.as_str(),
                "host"
                    | "user-agent"
                    | "content-length"
                    | "connection"
                    | "cookie"
                    | "accept-encoding"
                    | "accept-language"
                    | "sec-gpc"
                    | "sec-fetch-dest"
                    | "sec-fetch-mode"
                    | "sec-fetch-site"
                    | "sec-fetch-user"
                    | "upgrade-insecure-requests"
                    | "origin"
                    | "referer"
            )
        {
            continue;
        }
        if !cors_safelisted_header(&lower, value) {
            names.push(lower);
        }
    }
    if request
        .body
        .as_ref()
        .is_some_and(|(content_type, _)| !cors_safelisted_content_type(content_type))
    {
        names.push(String::from("content-type"));
    }
    names.sort();
    names.dedup();
    names
}

fn cors_safelisted_header(name: &str, value: &str) -> bool {
    let raw = value;
    let value = raw.as_bytes();
    if value.len() > 128 {
        return false;
    }
    match name {
        "accept" => value.iter().all(|byte| {
            !matches!(
                *byte,
                0x00..=0x08
                    | 0x0a..=0x1f
                    | 0x7f
                    | b'('
                    | b')'
                    | b'<'
                    | b'>'
                    | b'?'
                    | b'@'
                    | b'['
                    | b'\\'
                    | b']'
                    | b'{'
                    | b'}'
                    | b'"'
            )
        }),
        "accept-language" | "content-language" => value.iter().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(*byte, b' ' | b'*' | b',' | b'-' | b'.' | b';' | b'=')
        }),
        "content-type" => cors_safelisted_content_type(raw),
        _ => false,
    }
}

fn cors_safelisted_content_type(value: &str) -> bool {
    if value.len() > 128
        || value.bytes().any(|byte| {
            matches!(
                byte,
                0x00..=0x08
                    | 0x0a..=0x1f
                    | 0x7f
                    | b'(' | b')' | b'<' | b'>' | b'?' | b'@' | b'[' | b'\\' | b']'
                    | b'{' | b'}' | b'"'
            )
        })
    {
        return false;
    }
    let essence = value
        .split(';')
        .next()
        .map(str::trim)
        .unwrap_or_default()
        .to_ascii_lowercase();
    matches!(
        essence.as_str(),
        "application/x-www-form-urlencoded" | "multipart/form-data" | "text/plain"
    )
}

fn validate_cors_preflight(
    response: &Response,
    request: &Request,
    requested_headers: &[String],
) -> Result<(), String> {
    if !(200..300).contains(&response.status) {
        return Err(String::from("CORS preflight failed"));
    }
    let header = |name: &str| {
        response
            .headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    };
    let credentials = request
        .fetch_policy
        .as_ref()
        .is_some_and(|policy| policy.credentials == CredentialsMode::Include);
    let origin = request
        .fetch_policy
        .as_ref()
        .map(|policy| policy.origin.origin().ascii_serialization())
        .unwrap_or_default();
    let allowed_origin = header("access-control-allow-origin")
        .unwrap_or_default()
        .trim();
    if allowed_origin != origin && !(allowed_origin == "*" && !credentials) {
        return Err(String::from("CORS preflight origin not allowed"));
    }
    let methods = header("access-control-allow-methods")
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let method_allowed = methods
        .into_iter()
        .any(|value| value.eq_ignore_ascii_case(&request.method) || (!credentials && value == "*"));
    let method_is_safelisted = matches!(
        request.method.to_ascii_uppercase().as_str(),
        "GET" | "HEAD" | "POST"
    );
    if !method_allowed && !method_is_safelisted {
        return Err(String::from("CORS preflight method not allowed"));
    }
    if !requested_headers.is_empty() {
        let allowed = header("access-control-allow-headers")
            .unwrap_or_default()
            .split(',')
            .map(|value| value.trim().to_ascii_lowercase())
            .collect::<Vec<_>>();
        let wildcard = allowed.iter().any(|value| value == "*");
        if (!wildcard || credentials)
            && requested_headers
                .iter()
                .any(|name| !allowed.iter().any(|value| value == name))
        {
            return Err(String::from("CORS preflight header not allowed"));
        }
    }
    if credentials
        && !header("access-control-allow-credentials")
            .is_some_and(|value| value.trim().eq_ignore_ascii_case("true"))
    {
        return Err(String::from("credentialed CORS preflight failed"));
    }
    Ok(())
}

async fn fetch_preflight(request: &Request) -> Result<Response, TimedFetchError> {
    let start = crate::performance::now_ms();
    let response = tokio::time::timeout(FETCH_TIMEOUT, fetch_once(request))
        .await
        .map_err(|_| TimedFetchError::network(String::from("timed out"), start))?
        .map_err(|error| TimedFetchError::network(error, start))?;
    if matches!(response.status, 301 | 302 | 303 | 307 | 308) {
        return Err(String::from("CORS preflight redirect is not allowed").into());
    }
    Ok(response)
}

async fn fetch_redirecting(
    request: &Request,
    mut policy: crate::referrer_policy::ReferrerPolicy,
) -> Result<FetchResponseDetails, TimedFetchError> {
    let navigation_start = crate::performance::now_ms();
    let mut redirect_end = 0.0;
    // Fetch's navigation TAO list contains only the timing allow values, not
    // arbitrary response headers. It is discarded after the final decision.
    let mut navigation_tao_values: Vec<Vec<String>> = Vec::new();
    let can_use_navigation_tao = request
        .fetch_metadata
        .is_some_and(|metadata| metadata.site == FetchSite::None)
        || request
            .headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("referer"));
    let mut request = request.clone();
    let mut url_list = vec![request.url.clone()];
    let client = request.timing_client.clone().or_else(|| {
        request
            .fetch_policy
            .as_ref()
            .map(|policy| policy.origin.clone())
    });
    let mode = request.fetch_metadata.map_or_else(
        || {
            request
                .fetch_policy
                .as_ref()
                .map_or("no-cors", |policy| policy.mode.as_str())
        },
        |metadata| metadata.mode,
    );
    let mut timing_allowed = client.is_some();
    let mut cross_origin_response = false;
    for _ in 0..=MAX_REDIRECTS {
        if let Some(policy) = &request.fetch_policy
            && policy.mode == RequestMode::SameOrigin
            && !same_origin(&policy.origin, &request.url)
        {
            return Err(String::from("same-origin fetch crossed origins").into());
        }
        let response = tokio::time::timeout(FETCH_TIMEOUT, fetch_once(&request))
            .await
            .map_err(|_| TimedFetchError::network(String::from("timed out"), navigation_start))?
            .map_err(|error| TimedFetchError::network(error, navigation_start))?;
        if let Some(client) = &client {
            cross_origin_response |= !same_origin(client, &request.url);
            let opaque_origin = url_list
                .windows(2)
                .any(|hop| !same_origin(&hop[0], &hop[1]) && !same_origin(client, &hop[0]));
            let origin = if opaque_origin {
                String::from("null")
            } else {
                client.origin().ascii_serialization()
            };
            let explicit = response
                .headers
                .iter()
                .filter(|(name, _)| name.eq_ignore_ascii_case("timing-allow-origin"))
                .flat_map(|(_, value)| value.split(','))
                .any(|value| value.trim() == "*" || value.trim() == origin);
            // Fetch #concept-tao-check is independent of the response CORS
            // check, and a failed hop cannot be cleared by a later redirect.
            timing_allowed &= explicit || !cross_origin_response;
        }
        navigation_tao_values.push(
            response
                .headers
                .iter()
                .filter(|(name, _)| name.eq_ignore_ascii_case("timing-allow-origin"))
                .flat_map(|(_, value)| value.split(','))
                .map(|value| value.trim().to_string())
                .collect(),
        );
        match response.status {
            301 | 302 | 303 | 307 | 308 => {}
            _ => {
                let mut response = response;
                if let Some(timing) = response.timing.as_mut() {
                    timing.resource_timing_allowed = timing_allowed;
                    let opaque_origin = client.as_ref().is_none_or(|client| {
                        url_list.windows(2).any(|hop| {
                            !same_origin(&hop[0], &hop[1]) && !same_origin(client, &hop[0])
                        })
                    });
                    timing.resource_body_exposed = if mode == "navigate" {
                        !opaque_origin && !cross_origin_response
                    } else {
                        mode != "no-cors" || !cross_origin_response
                    };
                    timing.resource_response_status = if mode == "navigate" {
                        if opaque_origin { 0 } else { response.status }
                    } else if mode == "no-cors" && cross_origin_response {
                        0
                    } else {
                        response.status
                    };
                    timing.start_time = navigation_start;
                    timing.fetch_start = if redirect_end == 0.0 {
                        navigation_start
                    } else {
                        redirect_end
                    };
                    timing.redirect_start = if url_list.len() > 1 {
                        navigation_start
                    } else {
                        0.0
                    };
                    timing.redirect_end = redirect_end;
                    let origin = response.url.origin().ascii_serialization();
                    let all_same_origin =
                        url_list.iter().all(|url| same_origin(url, &response.url));
                    timing.navigation_cross_origin_redirect =
                        url_list.len() > 1 && !all_same_origin;
                    let navigation_tao = can_use_navigation_tao
                        && navigation_tao_values.iter().all(|values| {
                            values.iter().any(|value| value == "*" || value == &origin)
                        });
                    // HTML #initialise-the-document-object / Fetch #navigation-tao-check.
                    timing.navigation_redirect_count = if all_same_origin || navigation_tao {
                        (url_list.len() - 1) as u16
                    } else {
                        0
                    };
                }
                // Recorded off the FINAL hop: 301-303 rewrite the method to
                // GET below, so a Post/Redirect/Get flow lands here as a GET.
                response.from_post = request.method.eq_ignore_ascii_case("POST");
                response = enforce_fetch_policy(&request, response)?;
                let referrer = request
                    .headers
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case("referer"))
                    .map_or_else(String::new, |(_, value)| value.clone());
                return Ok(FetchResponseDetails {
                    response,
                    url_list,
                    referrer,
                });
            }
        }
        // Redirect: fetch_once stashes the Location header in
        // content_type for 3xx (their bodies are never rendered).
        let target = request
            .url
            .join(response.content_type.trim())
            .map_err(|e| {
                TimedFetchError::network(format!("bad redirect location: {e}"), navigation_start)
            })?;
        match target.scheme() {
            "http" | "https" => {}
            other => {
                return Err(TimedFetchError::network(
                    format!("redirect leaves the web: {other}://"),
                    navigation_start,
                ));
            }
        }
        if !same_origin(&request.url, &target) {
            // Fetch §4.5 step 13 removes CORS non-wildcard request headers,
            // including Authorization, as soon as a redirect crosses origin.
            request
                .headers
                .retain(|(name, _)| !name.eq_ignore_ascii_case("authorization"));
        }
        update_navigation_metadata_for_redirect(&mut request, &target);
        if let Some(updated) =
            crate::referrer_policy::ReferrerPolicy::from_headers(&response.headers)
        {
            policy = updated;
        }
        // Referrer policy is per hop: re-evaluate the carried `Referer` against
        // the new target the way a browser does — never leak an https URL onto
        // an http (downgraded) hop, reduce a cross-origin hop to an origin.
        // The Referer's own value is the source document.
        if let Some(pos) = request
            .headers
            .iter()
            .position(|(k, _)| k.eq_ignore_ascii_case("referer"))
        {
            match Url::parse(&request.headers[pos].1)
                .ok()
                .and_then(|src| policy.determine(&src, &target))
            {
                Some(v) => request.headers[pos].1 = v.to_string(),
                None => {
                    request.headers.remove(pos);
                }
            }
        }
        if (response.status == 303
            && !request.method.eq_ignore_ascii_case("GET")
            && !request.method.eq_ignore_ascii_case("HEAD"))
            || (matches!(response.status, 301 | 302) && request.method.eq_ignore_ascii_case("POST"))
        {
            request.method = String::from("GET");
            request.body = None;
            request.headers.retain(|(name, _)| {
                !matches!(
                    name.to_ascii_lowercase().as_str(),
                    "content-type" | "content-length" | "content-encoding" | "transfer-encoding"
                )
            });
        }
        redirect_end = crate::performance::now_ms();
        url_list.push(target.clone());
        request.url = target;
    }
    Err(TimedFetchError::network(
        format!("too many redirects (>{MAX_REDIRECTS})"),
        navigation_start,
    ))
}

/// Apply the script-facing part of Fetch after the network/redirect steps.
/// Browser-owned navigations and element subresources have no policy here and
/// therefore retain their existing presentation behavior.
fn enforce_fetch_policy(request: &Request, mut response: Response) -> Result<Response, String> {
    let Some(policy) = &request.fetch_policy else {
        return Ok(response);
    };
    let cross_origin = !same_origin(&policy.origin, &response.url);
    match policy.mode {
        RequestMode::SameOrigin if cross_origin => {
            Err(String::from("same-origin fetch crossed origins"))
        }
        RequestMode::NoCors if cross_origin => {
            // Fetch exposes a filtered opaque response for no-cors requests:
            // status, headers, and body are intentionally unreadable.
            response.status = 0;
            response.content_type.clear();
            response.headers.clear();
            response.body.clear();
            response.challenge = None;
            Ok(response)
        }
        RequestMode::Cors if cross_origin => {
            let allowed_origin = response
                .headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("access-control-allow-origin"))
                .map(|(_, value)| value.trim());
            let origin = policy.origin.origin().ascii_serialization();
            let origin_ok = allowed_origin.is_some_and(|value| {
                value == origin || (value == "*" && policy.credentials != CredentialsMode::Include)
            });
            let credentials_ok = policy.credentials != CredentialsMode::Include
                || response.headers.iter().any(|(name, value)| {
                    name.eq_ignore_ascii_case("access-control-allow-credentials")
                        && value.trim().eq_ignore_ascii_case("true")
                });
            if origin_ok && credentials_ok {
                // HTML's script/link consumers inspect the unsafe (internal)
                // response after CORS succeeds. Filtering `nosniff` here would
                // incorrectly turn a blocked script MIME type into an allowed
                // one. Only the empty-destination Fetch/XHR API exposes the
                // CORS-filtered header list to author JavaScript.
                if request
                    .fetch_metadata
                    .is_some_and(|metadata| metadata.destination != "empty")
                {
                    return Ok(response);
                }
                let exposed = response
                    .headers
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case("access-control-expose-headers"))
                    .map(|(_, value)| {
                        value
                            .split(',')
                            .map(|name| name.trim().to_ascii_lowercase())
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                let wildcard = exposed.iter().any(|name| name == "*")
                    && policy.credentials != CredentialsMode::Include;
                response.headers.retain(|(name, _)| {
                    let lower = name.to_ascii_lowercase();
                    cors_safelisted_response_header(&lower)
                        || wildcard
                        || exposed.iter().any(|value| value == &lower)
                });
                Ok(response)
            } else {
                Err(String::from("CORS check failed"))
            }
        }
        _ => Ok(response),
    }
}

fn cors_safelisted_response_header(name: &str) -> bool {
    matches!(
        name,
        "cache-control"
            | "content-language"
            | "content-length"
            | "content-type"
            | "expires"
            | "last-modified"
            | "pragma"
    )
}

fn credentials_included(request: &Request) -> bool {
    let Some(policy) = &request.fetch_policy else {
        return true;
    };
    match policy.credentials {
        CredentialsMode::Include => true,
        CredentialsMode::SameOrigin => same_origin(&policy.origin, &request.url),
        CredentialsMode::Omit => false,
    }
}

// ---- the connection pool ---------------------------------------------

/// One transport, plain or TLS, behind a single read/write face so the
/// pool can hold either.
pub(crate) enum Conn {
    Plain(TcpStream),
    Tls(Box<tokio_rustls::client::TlsStream<TcpStream>>),
}

/// The live transport a WebSocket runs over (see `ws.rs`): the same plain/TLS
/// `Conn` the HTTP path uses, buffered so the upgrade handshake can read the
/// response head without consuming into the first frame.
pub(crate) type WsTransport = BufReader<Conn>;

/// Dial + (for `wss`) TLS-connect for a WebSocket upgrade, reusing the HTTP
/// dial. WebPKI validation, exactly like `https` (a `wss` is an `https` that
/// upgrades).
pub(crate) async fn ws_dial(secure: bool, host: &str, port: u16) -> Result<WsTransport, String> {
    dial(if secure { "https" } else { "http" }, host, port).await
}

impl AsyncRead for Conn {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            Conn::Plain(s) => std::pin::Pin::new(s).poll_read(cx, buf),
            Conn::Tls(s) => std::pin::Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Conn {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        match self.get_mut() {
            Conn::Plain(s) => std::pin::Pin::new(s).poll_write(cx, buf),
            Conn::Tls(s) => std::pin::Pin::new(s).poll_write(cx, buf),
        }
    }
    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            Conn::Plain(s) => std::pin::Pin::new(s).poll_flush(cx),
            Conn::Tls(s) => std::pin::Pin::new(s).poll_flush(cx),
        }
    }
    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            Conn::Plain(s) => std::pin::Pin::new(s).poll_shutdown(cx),
            Conn::Tls(s) => std::pin::Pin::new(s).poll_shutdown(cx),
        }
    }
}

type PoolKey = (String, String, u16); // (scheme, host, port)

struct IdleConn {
    io: Option<BufReader<Conn>>,
    since: Instant,
    runtime: tokio::runtime::Handle,
}

impl Drop for IdleConn {
    fn drop(&mut self) {
        let Some(mut io) = self.io.take() else { return };
        // RFC 9112 §9.5 / RFC 8446 §6.1: close gracefully, including TLS
        // close_notify. An idle stream normally finishes this in one poll.
        // The native frontend may evict it outside any entered Tokio runtime.
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        if std::pin::Pin::new(&mut io)
            .poll_shutdown(&mut cx)
            .is_pending()
            && let Ok(permit) = POOL_CLOSE_SLOTS.clone().try_acquire_owned()
        {
            self.runtime.spawn(async move {
                let _permit = permit;
                // A blocked/broken peer must not retain an evicted socket or
                // TLS buffers indefinitely, nor make navigation wait for I/O.
                let _ = tokio::time::timeout(Duration::from_secs(1), io.shutdown()).await;
            });
        }
    }
}

/// The keep-alive pool: idle connections per (scheme, host, port),
/// RAM-only, newest-first reuse. A page load that used to pay a fresh
/// DNS+TCP+TLS per subresource (~500ms each on the wide net) now pays
/// it once per host.
static POOL: std::sync::LazyLock<std::sync::Mutex<ConnectionPool>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(ConnectionPool::default()));

/// Servers commonly drop idle connections after ~60s; don't bother
/// trying one older than this.
const POOL_IDLE_TTL: Duration = Duration::from_secs(30);
const POOL_MAX_IDLE_PER_KEY: usize = 8;
const POOL_MAX_IDLE: usize = 32;
static POOL_CLOSE_SLOTS: std::sync::LazyLock<std::sync::Arc<tokio::sync::Semaphore>> =
    std::sync::LazyLock::new(|| std::sync::Arc::new(tokio::sync::Semaphore::new(POOL_MAX_IDLE)));

#[derive(Default)]
struct ConnectionPool {
    idle: HashMap<PoolKey, Vec<IdleConn>>,
}

impl ConnectionPool {
    fn prune(&mut self, now: Instant) {
        // RFC 9112 §§9.3, 9.5: persistence permits reuse, but either peer may
        // close an idle transport. Visit every origin, including origins that
        // will never be requested again. Empty buckets must not retain host names.
        self.idle.retain(|_, connections| {
            connections.retain(|connection| {
                now.saturating_duration_since(connection.since) < POOL_IDLE_TTL
            });
            !connections.is_empty()
        });
        let mut count: usize = self.idle.values().map(Vec::len).sum();
        while count > POOL_MAX_IDLE {
            let oldest = self
                .idle
                .iter()
                .flat_map(|(key, connections)| {
                    connections
                        .iter()
                        .enumerate()
                        .map(move |(index, connection)| (key, index, connection.since))
                })
                .min_by_key(|(_, _, since)| *since)
                .map(|(key, index, _)| (key.clone(), index));
            let Some((key, index)) = oldest else { break };
            let connections = self.idle.get_mut(&key).expect("oldest entry exists");
            connections.remove(index);
            if connections.is_empty() {
                self.idle.remove(&key);
            }
            count -= 1;
        }
    }

    fn get(&mut self, key: &PoolKey, now: Instant) -> Option<BufReader<Conn>> {
        self.prune(now);
        let connections = self.idle.get_mut(key)?;
        let connection = connections
            .pop()
            .and_then(|mut connection| connection.io.take());
        if connections.is_empty() {
            self.idle.remove(key);
        }
        connection
    }

    fn put(&mut self, key: PoolKey, io: BufReader<Conn>, now: Instant) {
        let connections = self.idle.entry(key).or_default();
        if connections.len() == POOL_MAX_IDLE_PER_KEY {
            connections.remove(0);
        }
        connections.push(IdleConn {
            io: Some(io),
            since: now,
            runtime: tokio::runtime::Handle::current(),
        });
        self.prune(now);
    }
}

/// Navigation is an idle-pool maintenance boundary even when the destination
/// makes no HTTP requests. Recent connections remain reusable across pages;
/// expired connections and empty origin buckets are released without a timer.
pub(crate) fn prune_idle_connections() {
    if let Ok(mut pool) = POOL.lock() {
        pool.prune(Instant::now());
    }
}

fn pool_get(key: &PoolKey) -> Option<BufReader<Conn>> {
    POOL.lock().ok()?.get(key, Instant::now())
}

fn pool_put(key: PoolKey, io: BufReader<Conn>) {
    if let Ok(mut pool) = POOL.lock() {
        pool.put(key, io, Instant::now());
    }
}

// ---- RAM-only cookie jar ----------------------------------------------
//
// Cookies are ON by default, RAM-only, and never persisted. We CAPTURE
// `Set-Cookie`, expose non-HttpOnly matches to page JS via
// `document.cookie`, and send matching cookies back on requests. Cookie scope
// follows RFC 6265: a cookie without `Domain=` is host-only, while an explicit
// domain can cover matching subdomains. `set cookies off` disables capture,
// sends, and document.cookie exposure without deleting the in-memory jar.
// This bounded session jar implements name/value, Domain, Path, Secure,
// HttpOnly, Expires, and Max-Age; it intentionally remains non-persistent.

#[derive(Clone)]
struct Cookie {
    name: String,
    value: String,
    domain: String, // lowercased host-only host or explicit domain scope
    host_only: bool,
    path: String,
    secure: bool,
    http_only: bool,
    expires_at: Option<i64>, // UTC milliseconds; None means the process session
}

static COOKIE_JAR: std::sync::LazyLock<std::sync::Mutex<Vec<Cookie>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(Vec::new()));
static COOKIES_ENABLED: AtomicBool = AtomicBool::new(true);

const COOKIE_JAR_MAX: usize = 1000;
// RFC6265bis #cookie-lifetime-limits (local http-extensions snapshot
// 1057fe0f1570aa539eb90502411995e28ba304c7): cap explicit lifetimes at 400 days.
const COOKIE_MAX_AGE_SECS: i64 = 400 * 24 * 60 * 60;

fn cookie_now_ms() -> i64 {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(duration) => i64::try_from(duration.as_millis()).unwrap_or(i64::MAX),
        Err(error) => -i64::try_from(error.duration().as_millis()).unwrap_or(i64::MAX),
    }
}

/// RFC 6265 §5.1.1, including verified erratum 4148: cookie dates use an
/// ordered token scan, not HTTP-date's fixed grammar. Ignore weekday/timezone
/// labels and interpret the extracted Gregorian date and time as UTC.
/// https://www.rfc-editor.org/errata/eid4148
fn parse_cookie_date(value: &str) -> Option<i64> {
    fn number(token: &[u8], minimum: usize, maximum: usize) -> Option<(i64, &[u8])> {
        let digits = token.iter().take_while(|b| b.is_ascii_digit()).count();
        if !(minimum..=maximum).contains(&digits) {
            return None;
        }
        Some((
            token[..digits]
                .iter()
                .fold(0, |n, b| n * 10 + i64::from(b - b'0')),
            &token[digits..],
        ))
    }
    fn time(token: &[u8]) -> Option<(i64, i64, i64)> {
        let (hour, rest) = number(token, 1, 2)?;
        let (minute, rest) = number(rest.strip_prefix(b":")?, 1, 2)?;
        let (second, _) = number(rest.strip_prefix(b":")?, 1, 2)?;
        Some((hour, minute, second))
    }
    let (mut found_time, mut day, mut month, mut year) = (None, None, None, None);
    for token in value
        .as_bytes()
        .split(|b| matches!(b, 0x09 | 0x20..=0x2f | 0x3b..=0x40 | 0x5b..=0x60 | 0x7b..=0x7e))
    {
        if found_time.is_none()
            && let Some(parsed) = time(token)
        {
            found_time = Some(parsed);
            continue;
        }
        if day.is_none()
            && let Some((parsed, _)) = number(token, 1, 2)
        {
            day = Some(parsed);
            continue;
        }
        if month.is_none()
            && let Some(prefix) = token.get(..3)
            && let Some(index) = [
                b"jan", b"feb", b"mar", b"apr", b"may", b"jun", b"jul", b"aug", b"sep", b"oct",
                b"nov", b"dec",
            ]
            .iter()
            .position(|name| prefix.eq_ignore_ascii_case(*name))
        {
            month = Some(index);
            continue;
        }
        if year.is_none()
            && let Some((parsed, _)) = number(token, 2, 4)
        {
            year = Some(parsed);
        }
    }
    let (hour, minute, second) = found_time?;
    let (day, month, mut year) = (day?, month?, year?);
    year += match year {
        70..=99 => 1900,
        0..=69 => 2000,
        _ => 0,
    };
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let mut month_days = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    month_days[1] += i64::from(leap);
    if year < 1601
        || !(1..=month_days[month]).contains(&day)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }
    let days_before_year = |year: i64| {
        let y = year - 1;
        365 * y + y / 4 - y / 100 + y / 400
    };
    let days = days_before_year(year) - days_before_year(1970)
        + month_days[..month].iter().sum::<i64>()
        + day
        - 1;
    Some((days * 86_400 + hour * 3_600 + minute * 60 + second) * 1_000)
}

/// RFC 6265 §5.2.2 / RFC6265bis #ua-attribute-max-age. Invalid attributes
/// are ignored; oversized integers saturate at the supported lifetime limit.
fn cookie_max_age(value: &str, now: i64) -> Option<i64> {
    let digits = value.strip_prefix('-').unwrap_or(value).as_bytes();
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let seconds = digits.iter().fold(0i64, |n, b| {
        (n * 10 + i64::from(b - b'0')).min(COOKIE_MAX_AGE_SECS)
    });
    Some(if value.starts_with('-') || seconds == 0 {
        i64::MIN
    } else {
        now.saturating_add(seconds * 1000)
    })
}

fn evict_expired_cookies(jar: &mut Vec<Cookie>, now: i64) {
    // RFC 6265 §5.3: evict expired cookies before storage or retrieval.
    // Lazy eviction avoids a timer or background work for this bounded jar.
    jar.retain(|cookie| cookie.expires_at.is_none_or(|expiry| expiry > now));
}

#[cfg(test)]
pub(crate) static COOKIE_TEST_LOCK: std::sync::LazyLock<std::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(()));

pub(crate) fn set_cookies_enabled(enabled: bool) {
    COOKIES_ENABLED.store(enabled, Ordering::Relaxed);
}

pub(crate) fn cookies_enabled() -> bool {
    COOKIES_ENABLED.load(Ordering::Relaxed)
}

fn cookie_trace_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("TRUST_COOKIE_TRACE").is_some())
}

fn cookie_trace_counts<'a>(lines: impl Iterator<Item = &'a str>) -> (usize, usize) {
    let mut total = 0;
    let mut clearance = 0;
    for line in lines.filter(|line| !line.trim().is_empty()) {
        total += 1;
        if line
            .split_once('=')
            .is_some_and(|(name, _)| name.trim() == "cf_clearance")
        {
            clearance += 1;
        }
    }
    (total, clearance)
}

// Opt-in transport diagnostics: fixed event categories, counts and an opaque
// origin identifier only. Never log cookie values, arbitrary cookie names,
// request paths/queries, response payloads, or a clearance token's contents.
fn trace_cookie_counts(event: &'static str, url: &Url, status: u16, counts: (usize, usize)) {
    if !cookie_trace_enabled() {
        return;
    }
    use std::hash::{Hash as _, Hasher as _};
    let mut scope = std::collections::hash_map::DefaultHasher::new();
    url.origin().ascii_serialization().hash(&mut scope);
    eprintln!(
        "[cookie-trace] event={event} at_ms={} scope={:016x} root={} status={status} count={} clearance={} enabled={}",
        trace_ms(),
        scope.finish(),
        u8::from(url.path() == "/"),
        counts.0,
        counts.1,
        u8::from(cookies_enabled()),
    );
}

fn trace_cookie_line(event: &'static str, url: &Url, line: &str) {
    if cookie_trace_enabled() {
        trace_cookie_counts(event, url, 0, cookie_trace_counts(std::iter::once(line)));
    }
}

// Diagnostic-only receipts compare the actual outgoing value byte-for-byte
// with the original received cookie-value (RFC 6265 §5.2 whitespace parsing).
// Values stay in bounded process memory, never in the diagnostic output.
// Keep this separate from Cookie so the normal jar's representation is unchanged.
static COOKIE_TRACE_RECEIPTS: std::sync::LazyLock<std::sync::Mutex<Vec<Cookie>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(Vec::new()));

fn trace_cookie_receipt(cookie: &Cookie, line: &str) {
    if !cookie_trace_enabled() || cookie.name != "cf_clearance" {
        return;
    }
    let pair = line.split_once(';').map_or(line, |(pair, _)| pair);
    let Some((_, original)) = pair.split_once('=') else {
        return;
    };
    let original = original.trim();
    let mut receipts = COOKIE_TRACE_RECEIPTS.lock().unwrap();
    receipts.retain(|old| !(old.domain == cookie.domain && old.path == cookie.path));
    // Oversized or evicted receipts yield 'unknown', never a false exact match.
    if original.len() > 8192 {
        return;
    }
    let mut receipt = cookie.clone();
    receipt.value = original.to_string();
    receipts.push(receipt);
    if receipts.len() > 64 {
        receipts.remove(0);
    }
}

fn cookie_wire_integrity(url: &Url, line: &str, receipts: &[Cookie]) -> (usize, usize, usize) {
    let host = url.host_str().unwrap_or_default();
    let mut total = 0;
    let mut exact = 0;
    let mut empty = 0;
    for pair in line.split(';') {
        let Some((name, value)) = pair.trim_start().split_once('=') else {
            continue;
        };
        if name != "cf_clearance" {
            continue;
        }
        total += 1;
        empty += usize::from(value.is_empty());
        exact += usize::from(receipts.iter().any(|receipt| {
            receipt.name == name
                && receipt.value.as_bytes() == value.as_bytes()
                && cookie_domain_match(host, receipt)
                && cookie_path_matches(url.path(), &receipt.path)
                && (!receipt.secure || url.scheme() == "https")
        }));
    }
    (total, exact, empty)
}

fn trace_cookie_wire(url: &Url, line: &str) {
    if !cookie_trace_enabled() {
        return;
    }
    let receipts = COOKIE_TRACE_RECEIPTS.lock().unwrap();
    let (total, exact, empty) = cookie_wire_integrity(url, line, &receipts);
    if total > 0 {
        eprintln!(
            "[cookie-integrity] at_ms={} total={total} exact={exact} unknown={} empty={empty}",
            trace_ms(),
            total - exact,
        );
    }
}

/// Store a `Set-Cookie` header value against the response URL. `from_js`
/// (a `document.cookie` write) cannot create or overwrite HttpOnly cookies.
fn store_cookie(url: &Url, line: &str, from_js: bool) {
    store_cookie_at(
        &mut COOKIE_JAR.lock().unwrap(),
        url,
        line,
        from_js,
        cookie_now_ms(),
    );
}

fn store_cookie_at(jar: &mut Vec<Cookie>, url: &Url, line: &str, from_js: bool, now: i64) {
    trace_cookie_line(
        if from_js {
            "script-write"
        } else {
            "http-store-attempt"
        },
        url,
        line,
    );
    if !cookies_enabled() {
        trace_cookie_line("store-disabled", url, line);
        return;
    }
    let (nv, rest) = line.split_once(';').unwrap_or((line, ""));
    let Some((name, value)) = nv.split_once('=') else {
        trace_cookie_line("store-invalid-pair", url, line);
        return;
    };
    let (name, value) = (name.trim().to_string(), value.trim().to_string());
    if name.is_empty() {
        trace_cookie_line("store-empty-name", url, line);
        return;
    }
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    let (mut domain, mut path) = (host.clone(), default_cookie_path(url));
    let mut host_only = true;
    let mut invalid_domain = false;
    let (mut secure, mut http_only, mut max_age) = (false, false, None::<i64>);
    let mut expires = None;
    for attr in rest.split(';') {
        let attr = attr.trim();
        let (k, v) = attr
            .split_once('=')
            .map_or((attr.to_ascii_lowercase(), String::new()), |(k, v)| {
                (k.trim().to_ascii_lowercase(), v.trim().to_string())
            });
        match k.as_str() {
            "domain" => {
                let candidate = v.strip_prefix('.').unwrap_or(&v).to_ascii_lowercase();
                if candidate.is_empty() || !domain_matches(&host, &candidate) {
                    invalid_domain = true;
                } else {
                    domain = candidate;
                    host_only = false;
                }
            }
            "path" if v.starts_with('/') => path = v,
            "secure" => secure = true,
            "httponly" => http_only = true,
            "max-age" => {
                if let Some(expiry) = cookie_max_age(&v, now) {
                    max_age = Some(expiry);
                }
            }
            "expires" => {
                if let Some(expiry) = parse_cookie_date(&v) {
                    expires = Some(expiry.min(now.saturating_add(COOKIE_MAX_AGE_SECS * 1000)));
                }
            }
            _ => {}
        }
    }
    if from_js && http_only {
        // RFC 6265 §5.3 step 10: a non-HTTP API cannot create an
        // HttpOnly cookie, even if the attribute appears in the string.
        trace_cookie_line("store-script-httponly", url, line);
        return;
    }
    if invalid_domain {
        trace_cookie_line("store-domain-mismatch", url, line);
        return;
    }
    evict_expired_cookies(jar, now);
    if from_js
        && jar
            .iter()
            .any(|c| c.name == name && c.domain == domain && c.path == path && c.http_only)
    {
        // RFC 6265 §5.3 step 11.2: document.cookie cannot overwrite or
        // delete an existing HttpOnly cookie with the same name/domain/path.
        trace_cookie_line("store-protected-httponly", url, line);
        return;
    }
    jar.retain(|c| !(c.name == name && c.domain == domain && c.path == path));
    // §5.3 step 3: the last valid Max-Age overrides every Expires attribute.
    let expires_at = max_age.or(expires);
    if expires_at.is_some_and(|expiry| expiry <= now) {
        trace_cookie_line("store-deleted", url, line);
        return; // deletion (the retain above removed it)
    }
    let cookie = Cookie {
        name,
        value,
        domain,
        host_only,
        path,
        secure,
        http_only,
        expires_at,
    };
    trace_cookie_receipt(&cookie, line);
    jar.push(cookie);
    if jar.len() > COOKIE_JAR_MAX {
        jar.remove(0);
    }
    trace_cookie_line("stored", url, line);
}

/// RFC 6265 §5.1.3 domain-match. The suffix form is only valid for DNS host
/// names; an IP address must not inherit a cookie from a dotted suffix.
fn domain_matches(host: &str, domain: &str) -> bool {
    if host == domain {
        return true;
    }
    if host.parse::<std::net::IpAddr>().is_ok() {
        return false;
    }
    host.strip_suffix(domain)
        .is_some_and(|prefix| prefix.ends_with('.'))
}

/// RFC 6265 §5.1.4 default-path for a Set-Cookie received on `url`.
fn default_cookie_path(url: &Url) -> String {
    let path = url.path();
    if !path.starts_with('/') {
        return String::from("/");
    }
    let Some(last) = path.rfind('/') else {
        return String::from("/");
    };
    if last == 0 {
        String::from("/")
    } else {
        path[..last].to_string()
    }
}

/// RFC 6265 §5.1.4 path-match. A prefix ending in `/` matches descendants,
/// but `/foo` does not match `/foobar`.
fn cookie_path_matches(request_path: &str, cookie_path: &str) -> bool {
    request_path == cookie_path
        || request_path
            .strip_prefix(cookie_path)
            .is_some_and(|rest| cookie_path.ends_with('/') || rest.starts_with('/'))
}

fn cookie_domain_match(host: &str, c: &Cookie) -> bool {
    if c.host_only {
        host == c.domain
    } else {
        domain_matches(host, &c.domain)
    }
}

/// The `document.cookie` string for a page: name=value pairs for every
/// jar cookie that domain/path/secure-matches, excluding HttpOnly (which
/// JS can never read).
pub(crate) fn cookies_for_js(page: &Url) -> String {
    cookies_for_js_at(&mut COOKIE_JAR.lock().unwrap(), page, cookie_now_ms())
}

fn cookies_for_js_at(jar: &mut Vec<Cookie>, page: &Url, now: i64) -> String {
    if !cookies_enabled() {
        trace_cookie_counts("script-read-disabled", page, 0, (0, 0));
        return String::new();
    }
    let host = page.host_str().unwrap_or_default().to_ascii_lowercase();
    let path = page.path();
    let https = page.scheme() == "https";
    evict_expired_cookies(jar, now);
    let result = jar
        .iter()
        .filter(|c| !c.http_only)
        .filter(|c| !c.secure || https)
        .filter(|c| cookie_domain_match(&host, c))
        .filter(|c| cookie_path_matches(path, &c.path))
        .map(|c| format!("{}={}", c.name, c.value))
        .collect::<Vec<_>>()
        .join("; ");
    if cookie_trace_enabled() {
        trace_cookie_counts(
            "script-read",
            page,
            0,
            cookie_trace_counts(result.split(';')),
        );
    }
    result
}

/// A `document.cookie = "..."` write from page JS. Stored in the same
/// RAM-only cookie jar used for requests; Domain is validated against the
/// current document host.
pub(crate) fn set_cookie_from_js(page: &Url, line: &str) {
    store_cookie(page, line, true);
}

pub(crate) fn cookies_for_request(url: &Url) -> String {
    cookies_for_request_at(&mut COOKIE_JAR.lock().unwrap(), url, cookie_now_ms())
}

fn cookies_for_request_at(jar: &mut Vec<Cookie>, url: &Url, now: i64) -> String {
    if !cookies_enabled() {
        return String::new();
    }
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    let path = url.path();
    let https = url.scheme() == "https";
    evict_expired_cookies(jar, now);
    jar.iter()
        .filter(|c| !c.secure || https)
        .filter(|c| cookie_domain_match(&host, c))
        .filter(|c| cookie_path_matches(path, &c.path))
        .map(|c| format!("{}={}", c.name, c.value))
        .collect::<Vec<_>>()
        .join("; ")
}

async fn dial(scheme: &str, host: &str, port: u16) -> Result<BufReader<Conn>, String> {
    dial_with_timing(scheme, host, port, None).await
}

async fn dial_with_timing(
    scheme: &str,
    host: &str,
    port: u16,
    mut timing: Option<&mut crate::performance::FetchTiming>,
) -> Result<BufReader<Conn>, String> {
    // Same resolver and address-order fallback as TcpStream::connect(host),
    // with the actual DNS and connection boundaries made explicit (Fetch
    // #create-a-connection / #record-connection-timing-info).
    if let Some(timing) = timing.as_deref_mut() {
        timing.connection_reused = false;
        timing.domain_lookup_start = crate::performance::now_ms();
    }
    let addresses: Vec<_> = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| e.to_string())?
        .collect();
    if let Some(timing) = timing.as_deref_mut() {
        timing.domain_lookup_end = crate::performance::now_ms();
        timing.connect_start = timing.domain_lookup_end;
    }
    let stream = TcpStream::connect(addresses.as_slice())
        .await
        .map_err(|e| e.to_string())?;
    let _ = stream.set_nodelay(true);
    let conn = if scheme == "https" {
        let name = tls::server_name(host)?;
        if let Some(timing) = timing.as_deref_mut() {
            timing.secure_connection_start = crate::performance::now_ms();
        }
        let stream = tls::webpki_connector()
            .connect(name, stream)
            .await
            .map_err(|e| format!("TLS: {e}"))?;
        Conn::Tls(Box::new(stream))
    } else {
        Conn::Plain(stream)
    };
    if let Some(timing) = timing {
        timing.connect_end = crate::performance::now_ms();
        // This transport implements HTTP/1.1 even when no ALPN negotiation
        // occurs; Fetch permits the protocol's descriptive registered ID.
        timing.next_hop_protocol = "http/1.1";
    }
    Ok(BufReader::new(conn))
}

/// Fresh connection for the bounded streaming download path. Downloads never
/// enter the keep-alive pool because their body is written directly to disk.
pub(crate) async fn download_connection(url: &Url) -> Result<BufReader<Conn>, String> {
    let host = url.host_str().ok_or("download URL has no host")?;
    let port = url.port_or_known_default().unwrap_or(80);
    dial(url.scheme(), host, port).await
}

pub(crate) fn download_cookies(url: &Url) -> String {
    cookies_for_request(url)
}

pub(crate) fn download_store_cookie(url: &Url, line: &str) {
    store_cookie(url, line, false);
}

pub(crate) fn download_referrer(source: &Url, target: &Url) -> Option<String> {
    referrer_for(source, target)
}

async fn fetch_once(request: &Request) -> Result<Response, String> {
    let url = &request.url;
    if url.scheme() == "file" {
        // Fetch includes `file` in its fetch-scheme set but leaves the
        // dereference algorithm to the user agent. WHATWG URL's file state
        // plus RFC 8089 §§2–3 are implemented by the local adapter; never
        // send a file URL through the HTTP authority/dial path.
        return crate::file::fetch(request).await;
    }
    let mut timing = crate::performance::FetchTiming::new();
    let host = url.host_str().ok_or("URL has no host")?.to_string();
    let port = url.port_or_known_default().unwrap_or(80);
    let key: PoolKey = (url.scheme().to_string(), host.clone(), port);

    // Reuse an idle connection for GETs only: a pooled connection can
    // be stale (server closed it while idle), and the silent re-send
    // that recovers from that must never double-submit a POST.
    if request.method == "GET" {
        let mut tried = 0;
        while tried < 2
            && let Some(mut io) = pool_get(&key)
        {
            tried += 1;
            timing.reused_connection(url.scheme() == "https");
            timing.first_interim_response_start = 0.0;
            timing.final_response_start = 0.0;
            if let Ok(parts) = exchange(&mut io, request, &host, port, &mut timing).await {
                return finish_response(request, parts, io, key, timing);
            }
        }
    }
    // A failed stale-connection attempt does not contribute response metadata
    // to the successful retry. Preserve this hop's fetch start, not its errors.
    timing.first_interim_response_start = 0.0;
    timing.final_response_start = 0.0;
    let mut io = dial_with_timing(url.scheme(), &host, port, Some(&mut timing)).await?;
    let parts = exchange(&mut io, request, &host, port, &mut timing).await?;
    finish_response(request, parts, io, key, timing)
}

/// Build the Response and return a still-healthy connection to the
/// pool.
/// Recognise a bot-mitigation challenge from response headers. AWS WAF and
/// Cloudflare use these headers to describe a challenge response; detecting
/// them is host-agnostic metadata, not a decision that the browser cannot
/// continue. The body is still rendered and its scripts are still attempted.
/// Returns a short label, e.g. `"AWS WAF (challenge)"`.
fn detect_challenge(headers: &Headers) -> Option<String> {
    // AWS WAF: `x-amzn-waf-action: challenge|captcha|block` (HTTP 202/405).
    // `allow` is the pass-through value — not a wall.
    if let Some(action) = headers.get("x-amzn-waf-action") {
        let action = action.trim().to_ascii_lowercase();
        if action != "allow" && !action.is_empty() {
            return Some(format!("AWS WAF ({action})"));
        }
    }
    // Cloudflare managed challenge: `cf-mitigated: challenge` (HTTP 403/503).
    if let Some(m) = headers.get("cf-mitigated") {
        let m = m.trim().to_ascii_lowercase();
        if m != "allow" && !m.is_empty() {
            return Some(format!("Cloudflare ({m})"));
        }
    }
    None
}

fn finish_response(
    request: &Request,
    (status, headers, body, reusable, set_cookies): (u16, Headers, Vec<u8>, bool, Vec<String>),
    io: BufReader<Conn>,
    key: PoolKey,
    mut timing: crate::performance::FetchTiming,
) -> Result<Response, String> {
    timing.response_status = status;
    timing.content_type = headers.get("content-type").cloned().unwrap_or_default();
    timing.content_encoding = headers.get("content-encoding").cloned().unwrap_or_default();
    if reusable {
        pool_put(key, io);
    }
    let url = &request.url;
    if cookie_trace_enabled() {
        trace_cookie_counts(
            if credentials_included(request) {
                "response"
            } else {
                "response-credentials-excluded"
            },
            url,
            status,
            cookie_trace_counts(set_cookies.iter().map(String::as_str)),
        );
    }
    if credentials_included(request) {
        for line in &set_cookies {
            store_cookie(url, line, false);
        }
    }
    // Redirect processing also consumes response headers (notably
    // Referrer-Policy). Preserve those while still hiding Set-Cookie.
    let mut header_pairs: Vec<(String, String)> = headers
        .iter()
        .filter(|(k, _)| *k != "set-cookie")
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    header_pairs.sort();
    if matches!(status, 301 | 302 | 303 | 307 | 308) {
        let location = headers
            .get("location")
            .cloned()
            .ok_or_else(|| format!("{status} redirect without a Location header"))?;
        // Smuggle the location to `fetch` via content_type; 3xx bodies
        // are not rendered.
        return Ok(Response {
            url: url.clone(),
            status,
            content_type: location,
            headers: header_pairs,
            body: Vec::new(),
            rendered: None,
            js: None,
            blobs: None,
            live: None,
            declarative_refresh: None,
            challenge: None,
            from_post: false,
            timing: Some(Box::new(timing)),
        });
    }
    // RFC 9110 §8.3 and MIME Sniffing §5: an absent Content-Type is unknown,
    // not HTML. Top-level navigation computes its type from the bounded
    // resource header before deciding whether to render or download.
    let content_type = headers.get("content-type").cloned().unwrap_or_default();
    Ok(Response {
        url: url.clone(),
        status,
        content_type,
        headers: header_pairs,
        body,
        rendered: None,
        js: None,
        blobs: None,
        live: None,
        declarative_refresh: None,
        challenge: detect_challenge(&headers),
        from_post: false,
        timing: Some(Box::new(timing)),
    })
}

type Headers = HashMap<String, String>;

/// Write the request, read exactly one response. The bool says the
/// connection is positioned at the next message boundary — safe to
/// pool. A truncated length-delimited response is a network error and is
/// never exposed as a partial body or returned to the pool.
async fn exchange(
    io: &mut BufReader<Conn>,
    request: &Request,
    host: &str,
    port: u16,
    timing: &mut crate::performance::FetchTiming,
) -> Result<(u16, Headers, Vec<u8>, bool, Vec<String>), String> {
    let url = &request.url;
    let mut path = url.path().to_string();
    if let Some(query) = url.query() {
        path.push('?');
        path.push_str(query);
    }
    let host_header = match (url.scheme(), port) {
        ("http", 80) | ("https", 443) => host.to_string(),
        _ => format!("{host}:{port}"),
    };
    // Headers we manage ourselves — a page-supplied copy is ignored so we
    // never emit a duplicate or let a page spoof transport/identity headers.
    // `accept` and `accept-language` are the exceptions: Fetch supplies the
    // UA defaults only when the request's header list does not already contain
    // those names, so a page-authored value overrides our defaults.
    const MANAGED: &[&str] = &[
        "host",
        "user-agent",
        "content-length",
        "connection",
        "cookie",
        "accept-encoding",
        "accept-language",
        "sec-gpc",
        "sec-fetch-dest",
        "sec-fetch-mode",
        "sec-fetch-site",
        "sec-fetch-user",
        "upgrade-insecure-requests",
    ];
    let page_accept = request
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("accept"))
        .filter(|(_, v)| !v.contains(['\r', '\n']))
        .map(|(_, v)| v.as_str());
    let page_accept_language = request
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("accept-language"))
        .filter(|(_, v)| !v.contains(['\r', '\n']))
        .map(|(_, v)| v.as_str());
    let mut head = format!(
        "{} {} HTTP/1.1\r\n\
         Host: {}\r\n\
         User-Agent: {}\r\n\
         Accept: {}\r\n\
         Accept-Language: {}\r\n\
         Accept-Encoding: gzip, deflate\r\n\
         Connection: keep-alive\r\n",
        request.method,
        path,
        host_header,
        USER_AGENT,
        page_accept.unwrap_or("text/html, text/*;q=0.8, */*;q=0.1"),
        page_accept_language.unwrap_or(crate::locale::ACCEPT_LANGUAGE),
    );
    if GLOBAL_PRIVACY_CONTROL {
        // GPC §3.3: the field value is exactly the single character `1`.
        // It is user-agent-owned, so a page-supplied Sec-GPC is filtered by
        // MANAGED below and can neither override nor duplicate this field.
        head.push_str("Sec-GPC: 1\r\n");
    }
    // Fetch Metadata request headers are user-agent-owned. They describe a
    // real navigation context and are therefore not accepted from the page's
    // mutable header list (Fetch Metadata Request Headers, §3).
    if let Some(metadata) = request
        .fetch_metadata
        .filter(|_| potentially_trustworthy(url))
    {
        let site = match metadata.site {
            FetchSite::None => "none",
            FetchSite::SameOrigin => "same-origin",
            FetchSite::SameSite => "same-site",
            FetchSite::CrossSite => "cross-site",
        };
        head.push_str(&format!(
            "Sec-Fetch-Dest: {}\r\nSec-Fetch-Mode: {}\r\nSec-Fetch-Site: {site}\r\n",
            metadata.destination, metadata.mode,
        ));
        if metadata.mode == "navigate" && metadata.user_activation {
            head.push_str("Sec-Fetch-User: ?1\r\n");
        }
        // UIR §3.2.1: advertise support for secure navigations. TRust does
        // not yet maintain an HSTS preload list, so this applies to every
        // trustworthy top-level navigation.
        if metadata.mode == "navigate" {
            head.push_str("Upgrade-Insecure-Requests: 1\r\n");
        }
    }
    let cookie = if credentials_included(request) {
        cookies_for_request(url)
    } else {
        String::new()
    };
    if cookie_trace_enabled() {
        trace_cookie_counts(
            if credentials_included(request) {
                "request"
            } else {
                "request-credentials-excluded"
            },
            url,
            0,
            cookie_trace_counts(cookie.split(';')),
        );
    }
    if !cookie.is_empty() {
        trace_cookie_wire(url, &cookie);
        head.push_str(&format!("Cookie: {cookie}\r\n"));
    }
    if let Some(policy) = &request.fetch_policy
        && policy.mode == RequestMode::Cors
        && !same_origin(&policy.origin, url)
    {
        head.push_str(&format!(
            "Origin: {}\r\n",
            policy.origin.origin().ascii_serialization()
        ));
    }
    if let Some((content_type, payload)) = &request.body {
        head.push_str(&format!(
            "Content-Type: {}\r\nContent-Length: {}\r\n",
            content_type,
            payload.len()
        ));
    } else if request.method.eq_ignore_ascii_case("POST")
        || request.method.eq_ignore_ascii_case("PUT")
    {
        // Fetch, "HTTP-network-or-cache fetch": a null-body POST or PUT has a
        // Content-Length value of 0. This is semantically distinct from
        // omitting framing for a GET/HEAD (RFC 9110 §8.6), and some HTTP/1.1
        // gateways correctly answer an unframed POST with 411 Length Required.
        head.push_str("Content-Length: 0\r\n");
    }
    // Page-supplied headers (X-Requested-With — which servers read as
    // `$request->ajax()` — Authorization, X-CSRF-TOKEN, …), minus the managed
    // set and `accept` (already folded in above). A header with no value or a
    // CR/LF (injection) is dropped.
    for (k, v) in &request.headers {
        let lk = k.to_ascii_lowercase();
        if MANAGED.contains(&lk.as_str())
            || (lk == "content-type" && request.body.is_some())
            || lk == "accept"
            || lk == "origin"
            || (lk.starts_with("access-control-")
                && !request.method.eq_ignore_ascii_case("OPTIONS"))
            || k.is_empty()
            || k.contains(['\r', '\n', ':'])
            || v.contains(['\r', '\n'])
        {
            continue;
        }
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");

    timing.request_start = crate::performance::now_ms();
    io.write_all(head.as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    if let Some((_, payload)) = &request.body {
        io.write_all(payload).await.map_err(|e| e.to_string())?;
    }
    io.flush().await.map_err(|e| e.to_string())?;

    let navigation_get = request.method.eq_ignore_ascii_case("GET")
        && request
            .fetch_metadata
            .is_some_and(|metadata| metadata.destination == "document");
    read_response_with_timing(
        io,
        request.method.eq_ignore_ascii_case("HEAD"),
        navigation_get,
        Some(timing),
    )
    .await
}

/// One CRLF-terminated line, sans terminator. Err on EOF-before-line.
async fn read_line<R: tokio::io::AsyncBufRead + Unpin>(io: &mut R) -> Result<String, String> {
    const MAX_LINE: usize = 64 * 1024;
    let mut buf = Vec::new();
    loop {
        let available = io.fill_buf().await.map_err(|e| e.to_string())?;
        if available.is_empty() {
            return Err(if buf.is_empty() {
                String::from("connection closed")
            } else {
                String::from("connection closed before line terminator")
            });
        }
        let take = available
            .iter()
            .position(|&byte| byte == b'\n')
            .map_or(available.len(), |position| position + 1);
        if buf.len().saturating_add(take) > MAX_LINE {
            return Err(String::from("header line exceeds 64 KB"));
        }
        let terminated = take < available.len() || available.last() == Some(&b'\n');
        buf.extend_from_slice(&available[..take]);
        io.consume(take);
        if terminated {
            if !buf.ends_with(b"\r\n") {
                return Err(String::from("line is not CRLF terminated"));
            }
            if buf[..buf.len() - 2].contains(&b'\r') {
                return Err(String::from("bare carriage return in line"));
            }
            buf.truncate(buf.len() - 2);
            return Ok(String::from_utf8_lossy(&buf).into_owned());
        }
    }
}

/// Read one response off the stream: status line, headers, then a body
/// delimited by Content-Length, chunked encoding, or (last resort)
/// EOF. Returns (status, headers, body, reusable).
#[cfg(test)]
async fn read_response<R: AsyncRead + Unpin>(
    io: &mut BufReader<R>,
    is_head: bool,
) -> Result<(u16, Headers, Vec<u8>, bool, Vec<String>), String> {
    read_response_with_policy(io, is_head, false).await
}

#[cfg(test)]
async fn read_response_with_policy<R: AsyncRead + Unpin>(
    io: &mut BufReader<R>,
    is_head: bool,
    navigation_get: bool,
) -> Result<(u16, Headers, Vec<u8>, bool, Vec<String>), String> {
    read_response_with_timing(io, is_head, navigation_get, None).await
}

async fn read_response_with_timing<R: AsyncRead + Unpin>(
    io: &mut BufReader<R>,
    is_head: bool,
    navigation_get: bool,
    mut timing: Option<&mut crate::performance::FetchTiming>,
) -> Result<(u16, Headers, Vec<u8>, bool, Vec<String>), String> {
    const MAX_HEADER_LINE: usize = 64 * 1024;
    const MAX_HEADER_BYTES: usize = 256 * 1024;
    const MAX_HEADER_FIELDS: usize = 256;
    let mut interim_framing_violation = false;
    let (status, headers, set_cookies, http11) = loop {
        // Wait for the first available response bytes before sampling. A
        // timestamp taken before this await would measure request completion.
        let _ = io.fill_buf().await.map_err(|e| e.to_string())?;
        let response_start = crate::performance::now_ms();
        let status_line = read_line(io).await?;
        let Some((version, rest)) = status_line.split_once(' ') else {
            return Err(format!("malformed status line: {status_line:?}"));
        };
        let http11 = version == "HTTP/1.1";
        if version != "HTTP/1.0" && !http11 {
            return Err(format!("unsupported HTTP version: {version:?}"));
        }
        let status_code = rest
            .split(' ')
            .next()
            .filter(|code| code.len() == 3 && code.bytes().all(|byte| byte.is_ascii_digit()))
            .ok_or_else(|| format!("malformed status line: {status_line:?}"))?;
        let status: u16 = status_code
            .parse()
            .ok()
            .filter(|status| (100..=599).contains(status))
            .ok_or_else(|| format!("invalid status code: {status_line:?}"))?;
        let mut headers = HashMap::new();
        let mut set_cookies = Vec::new();
        let mut header_bytes = status_line.len();
        let mut header_fields = 0usize;
        loop {
            let line = read_line(io).await?;
            header_bytes = header_bytes.saturating_add(line.len().saturating_add(2));
            if line.len() > MAX_HEADER_LINE || header_bytes > MAX_HEADER_BYTES {
                return Err(String::from("response headers exceed size limit"));
            }
            if line.is_empty() {
                break;
            }
            let Some((raw_name, value)) = line.split_once(':') else {
                return Err(String::from("malformed response header"));
            };
            if raw_name.is_empty()
                || raw_name.trim() != raw_name
                || !raw_name.bytes().all(is_http_token)
            {
                return Err(String::from("malformed response header name"));
            }
            if value
                .bytes()
                .any(|byte| (byte < 0x20 && byte != b'\t') || byte == 0x7f)
            {
                return Err(String::from("malformed response header value"));
            }
            let name = raw_name.to_ascii_lowercase();
            if name.is_empty() {
                return Err(String::from("response header has an empty name"));
            }
            header_fields += 1;
            if header_fields > MAX_HEADER_FIELDS {
                return Err(String::from("too many response headers"));
            }
            if name == "set-cookie" {
                set_cookies.push(value.trim().to_string());
            }
            headers
                .entry(name)
                .and_modify(|existing: &mut String| {
                    existing.push_str(", ");
                    existing.push_str(value.trim());
                })
                .or_insert_with(|| value.trim().to_string());
        }
        // RFC 9110 permits one or more informational responses before the
        // final response. They have no body and must never be returned as the
        // result of the request or become the next pooled response. RFC 9112
        // §6.3 item 1 ends them at the header boundary even if the sender
        // included forbidden framing fields. Retire that connection after
        // the final response; never consume an invented informational body.
        if (100..200).contains(&status) && status != 101 {
            if let Some(timing) = timing.as_deref_mut()
                && timing.first_interim_response_start == 0.0
            {
                timing.first_interim_response_start = response_start;
            }
            interim_framing_violation |=
                headers.contains_key("transfer-encoding") || headers.contains_key("content-length");
            continue;
        }
        if let Some(timing) = timing.as_deref_mut() {
            timing.final_response_start = response_start;
            timing.next_hop_protocol = if http11 { "http/1.1" } else { "http/1.0" };
        }
        break (status, headers, set_cookies, http11);
    };

    let mut reusable = http11
        && !interim_framing_violation
        && !headers
            .get("connection")
            .is_some_and(|c| c.to_ascii_lowercase().contains("close"));

    // RFC 9112 §6.3: any response to a HEAD request is terminated by the
    // end of the header section and NEVER carries a message body, whatever
    // `Content-Length`/`Transfer-Encoding` it advertises — those describe
    // the body a GET would return. Reading one here would block forever on
    // bytes that never arrive; a single HEAD to any keep-alive server (an
    // ad framework's latency probe, a preflight resource check) hung the
    // whole page load until the former page wall deadline killed it. The socket is at a
    // clean message boundary, so it stays poolable.
    // The same first-precedence rule covers 1xx, 204 and 304, before
    // validating framing for messages that actually have bodies. In
    // particular, 204 + Content-Length: 0 is still a completed response,
    // not a failed POST. Preserve its status/headers (including cookies).
    // RFC 9110 §2.4 permits recovery from a sender's invalid construct;
    // close contradictory connections so stray bytes cannot poison reuse.
    // https://www.rfc-editor.org/rfc/rfc9112.html#section-6.3
    if is_head || matches!(status, 101 | 204 | 304) {
        let length = headers.get("content-length");
        let transfer = headers.contains_key("transfer-encoding");
        let contradictory = (transfer && length.is_some())
            || length.is_some_and(|value| parse_content_length(value).is_err())
            || (status == 204 && (transfer || length.is_some()));
        reusable &= status != 101 && !contradictory;
        if let Some(timing) = timing.as_deref_mut() {
            timing.response_end = crate::performance::now_ms();
        }
        return Ok((status, headers, Vec::new(), reusable, set_cookies));
    }

    let transfer_encoding = headers.get("transfer-encoding").map(|value| {
        value
            .split(',')
            .map(|coding| coding.trim().to_ascii_lowercase())
            .collect::<Vec<_>>()
    });
    let content_length = headers
        .get("content-length")
        .map(|value| parse_content_length(value))
        .transpose()?
        .flatten();
    if transfer_encoding.is_some() && content_length.is_some() {
        // RFC 9112 §6.3 treats simultaneous Transfer-Encoding and
        // Content-Length as an invalid framing signal. We never need the
        // latter for a response whose body is transfer-coded, so discard the
        // message instead of risking an ambiguous connection boundary.
        return Err(String::from(
            "response contains both Transfer-Encoding and Content-Length",
        ));
    }
    if transfer_encoding
        .as_ref()
        .is_some_and(|codings| codings.len() != 1 || codings[0] != "chunked")
    {
        return Err(String::from("unsupported transfer encoding"));
    }
    // WHATWG HTML "loading a document": an attachment or a type for which
    // TRust has no internal presentation must not be consumed as a document.
    // Stop after headers for GET navigations; Save/Open owns a separate bounded
    // streaming transfer, so large PDFs/media never enter the 16 MiB page heap.
    if navigation_get && navigation_headers_need_download(&headers) {
        if let Some(timing) = timing.as_deref_mut() {
            timing.response_end = crate::performance::now_ms();
        }
        return Ok((status, headers, Vec::new(), false, set_cookies));
    }

    // Media subresources are not presentation documents. Top-level media
    // navigations have already returned above for the Save/Open prompt;
    // downloading background media bodies here would only consume the page
    // memory budget (sites commonly prefetch video previews). Skip the body so
    // the page falls back to its poster. The unread socket cannot be pooled.
    if headers.get("content-type").is_some_and(|c| {
        let c = c.trim_start().to_ascii_lowercase();
        c.starts_with("video/") || c.starts_with("audio/")
    }) {
        if let Some(timing) = timing.as_deref_mut() {
            timing.response_end = crate::performance::now_ms();
        }
        return Ok((status, headers, Vec::new(), false, set_cookies));
    }

    let body = if transfer_encoding.is_some() {
        let (body, complete) = read_chunked(io).await?;
        if !complete {
            return Err(String::from("incomplete chunked response"));
        }
        reusable &= complete;
        body
    } else if let Some(len) = content_length {
        if len > MAX_BODY {
            return Err(format!(
                "response exceeds {} MB cap",
                MAX_BODY / (1024 * 1024)
            ));
        }
        let (body, complete) = read_exactly(io, len).await?;
        if !complete {
            return Err(String::from("incomplete response body"));
        }
        reusable &= complete;
        body
    } else {
        // No delimiter: the old read-to-EOF world. Never reusable.
        reusable = false;
        read_to_eof(io).await?
    };
    if let Some(timing) = timing.as_deref_mut() {
        timing.response_end = crate::performance::now_ms();
        timing.encoded_body_size = body.len();
    }
    // Undo any (unsolicited) Content-Encoding now the body is fully framed.
    let body = decode_content_encoding(&headers, body);
    if let Some(timing) = timing {
        timing.decoded_body_size = body.len();
    }
    Ok((status, headers, body, reusable, set_cookies))
}

fn parse_content_length(value: &str) -> Result<Option<usize>, String> {
    let mut lengths = value
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty());
    let Some(first) = lengths.next() else {
        return Err(String::from("invalid Content-Length"));
    };
    let parse_one = |part: &str| {
        if !part.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(String::from("invalid Content-Length"));
        }
        part.parse::<usize>()
            .map_err(|_| String::from("invalid Content-Length"))
    };
    let length = parse_one(first)?;
    for part in lengths {
        if parse_one(part)? != length {
            return Err(String::from("conflicting Content-Length values"));
        }
    }
    Ok(Some(length))
}

fn navigation_headers_need_download(headers: &Headers) -> bool {
    if let Some(value) = headers.get("content-disposition") {
        let disposition = value.split(';').next().unwrap_or("").trim();
        if !disposition.is_empty() && !disposition.eq_ignore_ascii_case("inline") {
            return true;
        }
    }
    let Some(content_type) = headers.get("content-type") else {
        return false;
    };
    !crate::download::mime_is_renderable(content_type, true)
}

/// Decode a chunked body (RFC 9112 §7.1) incrementally. The bool says
/// the terminating chunk (and its trailers) arrived intact.
async fn read_chunked<R: AsyncRead + Unpin>(
    io: &mut BufReader<R>,
) -> Result<(Vec<u8>, bool), String> {
    let mut out = Vec::new();
    loop {
        let Ok(line) = read_line(io).await else {
            return Ok((out, false));
        };
        let size_text = line.split(';').next().unwrap_or("").trim();
        if size_text.is_empty() || !size_text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Ok((out, false));
        }
        let Ok(size) = usize::from_str_radix(size_text, 16) else {
            return Ok((out, false));
        };
        if size == 0 {
            // Trailers, if any, end at an empty line.
            let mut trailer_bytes = 0usize;
            let mut trailer_fields = 0usize;
            loop {
                match read_line(io).await {
                    Ok(l) if l.is_empty() => return Ok((out, true)),
                    Ok(l) if valid_chunk_trailer(&l) => {
                        trailer_bytes = trailer_bytes.saturating_add(l.len().saturating_add(2));
                        trailer_fields += 1;
                        if trailer_bytes > 256 * 1024 || trailer_fields > 256 {
                            return Ok((out, false));
                        }
                    }
                    Ok(_) => return Ok((out, false)),
                    Err(_) => return Ok((out, false)),
                }
            }
        }
        let end = out
            .len()
            .checked_add(size)
            .ok_or_else(|| String::from("chunk size overflows body limit"))?;
        if end > MAX_BODY {
            return Err(format!(
                "response exceeds {} MB cap",
                MAX_BODY / (1024 * 1024)
            ));
        }
        let start = out.len();
        out.resize(end, 0);
        if !fill(io, &mut out[start..]).await {
            out.truncate(start);
            return Ok((out, false));
        }
        // The CRLF after the chunk data.
        let mut crlf = [0u8; 2];
        if !fill(io, &mut crlf).await {
            return Ok((out, false));
        }
        if crlf != *b"\r\n" {
            return Ok((out, false));
        }
    }
}

fn is_http_token(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

fn valid_chunk_trailer(line: &str) -> bool {
    let Some((name, _)) = line.split_once(':') else {
        return false;
    };
    !name.is_empty() && name.trim() == name && name.bytes().all(is_http_token)
}

async fn read_exactly<R: AsyncRead + Unpin>(
    io: &mut BufReader<R>,
    len: usize,
) -> Result<(Vec<u8>, bool), String> {
    let mut body = vec![0u8; len];
    let mut filled = 0;
    while filled < len {
        match io.read(&mut body[filled..]).await {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e.to_string()),
        }
    }
    let complete = filled == len;
    body.truncate(filled);
    Ok((body, complete))
}

/// Fill `buf` completely; false means EOF/error got there first.
async fn fill<R: AsyncRead + Unpin>(io: &mut BufReader<R>, buf: &mut [u8]) -> bool {
    let mut filled = 0;
    while filled < buf.len() {
        match io.read(&mut buf[filled..]).await {
            Ok(0) | Err(_) => return false,
            Ok(n) => filled += n,
        }
    }
    true
}

async fn read_to_eof<R: AsyncRead + Unpin>(io: &mut BufReader<R>) -> Result<Vec<u8>, String> {
    let mut raw = Vec::new();
    let mut buf = [0u8; 16384];
    loop {
        // Tolerate a missing TLS close_notify, as on the small net.
        let n = match io.read(&mut buf).await {
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => 0,
            Err(e) => return Err(e.to_string()),
        };
        if n == 0 {
            return Ok(raw);
        }
        raw.extend_from_slice(&buf[..n]);
        if raw.len() > MAX_BODY {
            return Err(format!(
                "response exceeds {} MB cap",
                MAX_BODY / (1024 * 1024)
            ));
        }
    }
}

/// Undo `Content-Encoding` on a fully-read body. We advertise the codings we
/// can decode (`Accept-Encoding: gzip, deflate`) and tolerate servers that
/// compress regardless; a browser decodes it, so we do too. Layering: this
/// runs AFTER framing
/// (Content-Length / dechunking) — `Content-Encoding` is the payload, not
/// the message framing, so it never affects connection reuse.
///
/// `gzip`/`deflate` only (pure-Rust miniz_oxide via flate2). Brotli/zstd are
/// left as-is — we don't advertise them, so a compliant server won't send
/// them; if a misbehaving one does, the parser sees the raw bytes (the best
/// we can do without those decoders). Truncated streams are tolerated like a
/// missing TLS close_notify: keep whatever decoded.
fn decode_content_encoding(headers: &Headers, body: Vec<u8>) -> Vec<u8> {
    let Some(enc) = headers.get("content-encoding") else {
        return body;
    };
    // A response MAY stack codings (applied left-to-right); undo them
    // right-to-left. In practice it's a single coding.
    let codings: Vec<String> = enc
        .split(',')
        .map(|c| c.trim().to_ascii_lowercase())
        .filter(|c| !c.is_empty())
        .collect();
    let mut body = body;
    for coding in codings.iter().rev() {
        body = match coding.as_str() {
            "identity" => body,
            "gzip" | "x-gzip" => inflate_tolerant(flate2::read::GzDecoder::new(body.as_slice())),
            "deflate" => decode_deflate(&body),
            // Brotli/zstd/unknown: can't undo it, so stop and hand back what
            // we have (any remaining leftward codings stay applied).
            _ => break,
        };
    }
    body
}

/// `Content-Encoding: deflate` is ambiguous in the wild: the spec means a
/// zlib stream (RFC 1950), but many servers send a bare DEFLATE stream.
/// Browsers cope by trying zlib first, then raw — we do the same.
fn decode_deflate(body: &[u8]) -> Vec<u8> {
    let zlib = inflate_tolerant(flate2::read::ZlibDecoder::new(body));
    if !zlib.is_empty() || body.is_empty() {
        return zlib;
    }
    // Zlib decode produced nothing from non-empty input: it's likely a raw
    // DEFLATE stream mislabelled `deflate`. Retry without the zlib wrapper.
    inflate_tolerant(flate2::read::DeflateDecoder::new(body))
}

/// Read a decoder to its end, keeping whatever decoded before an error.
/// Mirrors our read-to-EOF tolerance — a server that cuts the stream short
/// (or omits a clean trailer/CRC) still yields the bytes we got. Caps output
/// at `MAX_BODY` as a decompression-bomb guard (the compressed body is
/// already capped, but it can inflate far past that).
fn inflate_tolerant<R: std::io::Read>(mut dec: R) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = [0u8; 16384];
    loop {
        match dec.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                out.extend_from_slice(&buf[..n]);
                if out.len() >= MAX_BODY {
                    out.truncate(MAX_BODY);
                    break;
                }
            }
            // Truncated stream / bad trailer / corrupt data: keep what we have.
            Err(_) => break,
        }
    }
    out
}

/// Decode the body per the content-type charset: UTF-8 by default,
/// Latin-1 (and its windows-1252 sibling, near enough) by byte map.
pub(crate) fn decode_body(content_type: &str, body: &[u8]) -> String {
    let charset = content_type
        .split(';')
        .find_map(|p| p.trim().strip_prefix("charset="))
        .map(|c| c.trim_matches('"').to_ascii_lowercase());
    match charset.as_deref() {
        Some("iso-8859-1" | "latin1" | "windows-1252") => body.iter().map(|&b| b as char).collect(),
        _ => String::from_utf8_lossy(body).into_owned(),
    }
}

/// Whether a MIME type is a JavaScript MIME type (MIME Sniffing §4.2).
/// Parameters do not participate in the essence match.
fn is_javascript_mime_type(content_type: &str) -> bool {
    let essence = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    matches!(
        essence.as_str(),
        "application/ecmascript"
            | "application/javascript"
            | "application/x-ecmascript"
            | "application/x-javascript"
            | "text/ecmascript"
            | "text/javascript"
            | "text/javascript1.0"
            | "text/javascript1.1"
            | "text/javascript1.2"
            | "text/javascript1.3"
            | "text/javascript1.4"
            | "text/javascript1.5"
            | "text/jscript"
            | "text/livescript"
            | "text/x-ecmascript"
            | "text/x-javascript"
    )
}

/// Fetch §3.6: only the first comma-separated value controls `nosniff`.
fn has_nosniff(headers: &[(String, String)]) -> bool {
    headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("x-content-type-options"))
        .and_then(|(_, value)| value.split(',').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("nosniff"))
}

/// HTML "fetch a classic script" plus Fetch's `nosniff` response check.
/// Classic scripts retain the web-compatible historical behavior of accepting
/// a non-JavaScript MIME type when `nosniff` is absent.
pub(crate) fn classic_script_response_allowed(
    status: u16,
    content_type: &str,
    headers: &[(String, String)],
) -> bool {
    (200..300).contains(&status) && (!has_nosniff(headers) || is_javascript_mime_type(content_type))
}

/// HTML "fetch a single module script": HTTP(S) modules require both an OK
/// response and a JavaScript MIME type, independently of `nosniff`.
pub(crate) fn module_script_response_allowed(status: u16, content_type: &str) -> bool {
    (200..300).contains(&status) && is_javascript_mime_type(content_type)
}

/// Fetch's style-destination `nosniff` check, plus the ordinary OK-status
/// requirement for obtaining an external stylesheet.
pub(crate) fn stylesheet_response_allowed(
    status: u16,
    content_type: &str,
    headers: &[(String, String)],
) -> bool {
    if !(200..300).contains(&status) {
        return false;
    }
    let essence = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    !has_nosniff(headers) || essence == "text/css"
}

/// External classic scripts prefetched in parallel for one page. A browser has
/// no such cap; it's only a parallelism lid (politeness toward one host) and a
/// hostile-page lid. It must clear a real code-split SPA's chunk count — a
/// webpack `cr-acquisition`-style app ships ~24 `<script src>` chunks, so at the
/// old 16 the app's own bundle was truncated (the trailing chunks errored "not
/// fetched") and the page never mounted. Matches `MAX_PAGE_PRELOADS` in spirit
/// (both are "app code"). It is NOT a correctness cliff anymore: a classic
/// script the execution loop reaches that wasn't prefetched is fetched on
/// demand (see the selected JavaScript backend). The preload count is an
/// optimization bound, not a correctness cliff for required scripts.
const MAX_PAGE_SCRIPTS: usize = 96;

// External stylesheets fetched for the cascade have no count cap. CSS Cascade
// requires every applicable stylesheet to participate in source order; unlike
// optional prefetches, dropping a later sheet changes computed style and is
// observable. The request pool still bounds concurrency, while the normal
// body/cache limits bound each resource's storage.

/// Module-graph prefetches (`<link rel=modulepreload>` + module entry
/// srcs); archive.org announces ~32. This only bounds optional early work;
/// required graph dependencies remain fetchable on demand.
const MAX_PAGE_PRELOADS: usize = 96;

/// Concurrent subresource fetches per page load — browser-ish
/// politeness toward one host, and the pool holds about this many
/// idle connections anyway.
const PREFETCH_CONCURRENCY: usize = 8;

/// Run an HTML page's JavaScript through the selected backend and swap the
/// body for the post-JS document. External scripts are fetched here, with the same
/// caps and timeouts as pages — the page's own JS has no I/O at all.
/// Never fails: trouble lands in `response.js` and the original body
/// survives.
pub async fn execute_js(
    response: Response,
    viewport: (u16, u16),
    cell_px: (u16, u16),
    storage: crate::js::WebStorage,
) -> Response {
    execute_js_for_device(response, viewport, cell_px, 1.0, storage).await
}

/// Desktop/native entry carrying the real output-device density into the
/// page's responsive-image environment and `window.devicePixelRatio`.
pub async fn execute_js_for_device(
    response: Response,
    viewport: (u16, u16),
    cell_px: (u16, u16),
    device_pixel_ratio: f32,
    storage: crate::js::WebStorage,
) -> Response {
    execute_js_for_window(
        response,
        viewport,
        cell_px,
        device_pixel_ratio,
        (0, 0),
        storage,
    )
    .await
}

pub(crate) async fn execute_js_for_window(
    mut response: Response,
    viewport: (u16, u16),
    cell_px: (u16, u16),
    device_pixel_ratio: f32,
    screen_position: (i32, i32),
    storage: crate::js::WebStorage,
) -> Response {
    let device_pixel_ratio = if device_pixel_ratio.is_finite() && device_pixel_ratio > 0.0 {
        device_pixel_ratio
    } else {
        1.0
    };
    let media = response
        .content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if !(media == "text/html" || media == "application/xhtml+xml") {
        return response;
    }
    let html = decode_body(&response.content_type, &response.body);
    if response.declarative_refresh.is_none() {
        response.declarative_refresh = detect_declarative_refresh(&response, &html);
    }
    // Keep the cheap CSS-only path for inert documents, but do not classify a
    // script-less document that uses `:hover` as inert. Selectors 4 §9.1
    // requires that user-action pseudo-class to track the pointing device even
    // when there is no author script. The resident page actor already owns the
    // canonical DOM and its incremental hover invalidation; the old shortcut
    // dropped that actor before desktop/terminal hit testing could dispatch a
    // hover, leaving links permanently at their rest color.
    let mut prefetched_sheets = None;
    if !html.to_ascii_lowercase().contains("<script") {
        let sheets = fetch_page_sheets(&html, &response.url).await;
        let has_rendering_hover = {
            let mut probe = crate::js::css_prepare(&html, viewport, cell_px);
            probe.attach_external_sheets(&sheets);
            probe.hover_css_affects_rendering()
        };
        if !has_rendering_hover {
            return css_only_with_sheets(response, viewport, cell_px, device_pixel_ratio, sheets)
                .await;
        }
        // The actor needs the fetched sheets, and re-fetching them here would
        // add latency precisely on a first-paint interaction path. Leave the
        // original HTML untouched (the actor's DOM/frame lifecycle remains the
        // source of truth); script-less iframe loading retains its established
        // CSS-only behavior when no hover state is present.
        prefetched_sheets = Some(sheets);
    }
    // All subresources — classic scripts, stylesheets, and the module
    // graph the page announces up front (modulepreload + module entry
    // srcs) — fetch CONCURRENTLY. With the keep-alive pool this turns
    // a page load from sum-of-latencies into max-of-latencies.
    use crate::js::ExternalResourceKind as Kind;
    let mut scripts = 0;
    let mut preloads = 0;
    let jobs: Vec<_> = crate::js::external_resources_at(
        &html,
        f32::from(viewport.0) * f32::from(cell_px.0.max(1)),
        f32::from(viewport.1) * f32::from(cell_px.1.max(1)),
        device_pixel_ratio,
    )
    .into_iter()
    .filter(|job| match job.kind {
        Kind::Script => {
            scripts += 1;
            scripts <= MAX_PAGE_SCRIPTS
        }
        Kind::Preload => {
            preloads += 1;
            preloads <= MAX_PAGE_PRELOADS
        }
        Kind::Sheet => prefetched_sheets.is_none(),
        Kind::Sprite => true,
    })
    .collect();
    if std::env::var_os("TRUST_NET_TRACE").is_some() {
        eprintln!(
            "js : @{:>6}ms prefetch start ({} subresources)",
            trace_ms(),
            jobs.len()
        );
    }
    // Relative subresource URLs resolve against the document BASE URL — the
    // first `<base href>` (HTML §4.2.3) — not the document URL. peer.tube
    // serves `<base href="/client/en-US/">` and references its scripts/sheets
    // relatively; resolving against the document URL hits the Angular SPA
    // catch-all (every unknown path returns index.html → the scripts parse as
    // "unexpected token '<'" and the app never boots). The SSRF gate still
    // keys off the real page URL.
    let doc_base = base_with_doc_base(&html, &response.url);
    let results = futures::stream::iter(jobs.into_iter().map(|job| {
        let doc_base = doc_base.clone();
        let page_url = response.url.clone();
        async move {
            let resolved = doc_base
                .join(&job.source)
                .ok()
                .filter(|u| matches!(u.scheme(), "http" | "https" | "file"))
                // File symbols must not enter the origin-unpartitioned SVG
                // sprite cache (see dom::prime_sprite_sheet).
                .filter(|u| !matches!(job.kind, Kind::Sprite) || u.scheme() != "file")
                .filter(|u| subresource_allowed(&page_url, u));
            let resp = match &resolved {
                Some(u) => {
                    if std::env::var_os("TRUST_NET_TRACE").is_some() {
                        eprintln!("src: @{:>6}ms PREFETCH {u}", trace_ms());
                    }
                    let request = Request::subresource(
                        u.clone(),
                        &page_url,
                        job.destination(),
                        job.cors_credentials(),
                    );
                    Some(fetch_with_timing(&request, Default::default()).await)
                }
                None => None,
            };
            let credentials = job.cors_credentials();
            (job, resolved, resp, credentials)
        }
    }))
    // `buffered` keeps list order: scripts execute and sheets cascade
    // in document order regardless of arrival order.
    .buffered(PREFETCH_CONCURRENCY)
    .collect::<Vec<_>>()
    .await;

    // The shared subresource cache. Module preloads seed it; the module
    // loader, speculative import prefetch, and the page's own fetch() all
    // share it from here on (no chunk is downloaded twice).
    let cache = std::sync::Arc::new(PageCache::default());
    let mut externals = Vec::new();
    let mut sheets = prefetched_sheets.unwrap_or_default();
    let mut resource_timings = Vec::new();
    for (job, resolved, details, credentials) in results {
        let kind = job.kind;
        let raw = job.source;
        let (resp, url_list) = match details {
            Some(Ok(details)) => (Some(details.response), details.url_list),
            failure => {
                if let (Some(Err(error)), Some(url)) = (failure, resolved.as_ref())
                    && let Some(mut timing) = error.timing
                {
                    timing.render_blocking = job.render_blocking;
                    resource_timings.push(crate::performance::ResourceTiming {
                        name: url.to_string(),
                        initiator: job.initiator,
                        timing,
                        cached: false,
                    });
                }
                (None, Vec::new())
            }
        };
        if let (Some(response), Some(url)) = (resp.as_ref(), resolved.as_ref())
            && let Some(mut timing) = response.timing.clone()
        {
            timing.render_blocking = job.render_blocking;
            resource_timings.push(crate::performance::ResourceTiming {
                name: url.to_string(),
                initiator: job.initiator,
                timing,
                cached: false,
            });
        }
        match kind {
            Kind::Script => {
                // HTML "fetch a classic script" rejects non-OK responses;
                // Fetch additionally rejects a non-JavaScript MIME type when
                // the response opts into `X-Content-Type-Options: nosniff`.
                // Preserve the complete metadata in the shared HTTP cache, but
                // only expose an allowed body to the parser/executor.
                if let (Some(u), Some(r)) = (resolved.as_ref(), resp.as_ref()) {
                    cache.seed_resource(
                        u.to_string(),
                        &response.url,
                        "script",
                        credentials,
                        CachedResp {
                            status: r.status,
                            content_type: r.content_type.clone(),
                            headers: r.headers.clone(),
                            body: r.body.clone(),
                            url_list: url_list.clone(),
                            timing: r.timing.clone(),
                        },
                    );
                }
                let allowed = resp.as_ref().is_some_and(|r| {
                    classic_script_response_allowed(r.status, &r.content_type, &r.headers)
                });
                // Arc so the parallel-parse pool shares the body with its
                // worker threads instead of cloning a multi-MB bundle.
                externals.push((
                    raw,
                    resp.filter(|_| allowed)
                        .map(|r| std::sync::Arc::new(r.body)),
                ));
            }
            Kind::Sheet => {
                // A failed sheet is simply absent: fail-open, nothing
                // gets hidden.
                if let (Some(url), Some(r)) = (
                    resolved,
                    resp.filter(|r| {
                        stylesheet_response_allowed(r.status, &r.content_type, &r.headers)
                    }),
                ) {
                    let css = expand_stylesheet_imports(
                        decode_body(&r.content_type, &r.body),
                        url,
                        response.url.clone(),
                        Vec::new(),
                    )
                    .await;
                    sheets.push((raw, css));
                }
            }
            Kind::Preload => {
                // Every entry here is a modulepreload or a module-script entry.
                // Keep the full response in the shared HTTP cache. The module
                // consumer performs HTML's mandatory OK-status + JavaScript
                // MIME check before parsing, including for rejected responses;
                // caching those responses avoids an incorrect second request.
                if let (Some(u), Some(r)) = (resolved, resp) {
                    cache.seed_resource(
                        u.to_string(),
                        &response.url,
                        "script",
                        credentials,
                        CachedResp {
                            status: r.status,
                            content_type: r.content_type,
                            headers: r.headers,
                            body: r.body,
                            url_list,
                            timing: r.timing,
                        },
                    );
                }
            }
            Kind::Sprite => {
                if let (Some(url), Some(r)) = (resolved, resp)
                    && (200..300).contains(&r.status)
                {
                    let text = decode_body(&r.content_type, &r.body);
                    crate::dom::prime_sprite_sheet(url.as_str(), &text);
                }
            }
        }
    }
    install_stylesheet_fonts(&html, &sheets, &response.url).await;
    // Created HERE so it outlives the engine: the app hangs it on the Doc
    // and decodes blob: image srcs from it even after the page froze.
    let blobs = crate::js::BlobMap::default();
    response.blobs = Some(blobs.clone());
    let env = crate::js::PageEnv {
        url: response.url.to_string(),
        navigation_timing: response.timing.clone(),
        resource_timings,
        viewport,
        cell_px,
        device_pixel_ratio,
        screen_position,
        externals,
        sheets,
        cache,
        net: Some(tokio::runtime::Handle::current()),
        storage: Some(storage),
        blobs,
    };
    // The page actor owns the engine on its own dedicated stack. Its first
    // event is `Static`
    // (nothing to interact with: actor already gone, free efficiency)
    // or `Updated` (alive: hand the channels to the app).
    if std::env::var_os("TRUST_NET_TRACE").is_some() {
        eprintln!("js : @{:>6}ms prefetch done; spawning page", trace_ms());
    }
    // Inner-scroll GATE diagnostic: run the ONE-SHOT `transform` (which prints
    // the scroll-box report off the live, full-cascade arena — see
    // `layout2::scroll_box_report`) instead of the resident actor. Only when
    // `TRUST_DIAG_SCROLL_BOXES` is set, so production is unaffected.
    if std::env::var_os("TRUST_DIAG_SCROLL_BOXES").is_some() {
        let (out, outcome) = tokio::task::spawn_blocking(move || crate::js::transform(&html, &env))
            .await
            .unwrap();
        response.body = out.into_bytes();
        response.content_type = String::from("text/html; charset=utf-8");
        response.js = Some(outcome);
        response.live = None;
        return response;
    }
    let (handle, mut events) = crate::js::spawn_page(html, env);
    let first = tokio::time::timeout(Duration::from_secs(60), events.recv()).await;
    if std::env::var_os("TRUST_NET_TRACE").is_some() {
        eprintln!(
            "js : @{:>6}ms first PageEvt received (page rendered)",
            trace_ms()
        );
    }
    let (out, rendered, outcome, live) = match first {
        Ok(Some(crate::js::PageEvt::Static { html, mut outcome })) => {
            let Some(rendered) = outcome.rendered.take() else {
                return css_only_for_device(response, viewport, cell_px, device_pixel_ratio).await;
            };
            (html, rendered, outcome, None)
        }
        Ok(Some(crate::js::PageEvt::Updated { html, mut outcome })) => {
            let Some(rendered) = outcome.rendered.take() else {
                return css_only_for_device(response, viewport, cell_px, device_pixel_ratio).await;
            };
            (html, rendered, outcome, Some(LivePage { handle, events }))
        }
        // Died, hung, or spoke out of turn (most often: the page's JS is too
        // slow to first-paint within the timeout — a big GitHub code file).
        // Fall back to a CSS-only render so it still lays out per its own
        // stylesheets (flex gutter, collapsed menus) instead of UA defaults.
        _ => return css_only_for_device(response, viewport, cell_px, device_pixel_ratio).await,
    };
    // Keep source bytes for diagnostics/history. Presentation uses `rendered`
    // directly, so neither frontend reparses this body. Tests and an explicit
    // TRUST_DUMP_RAW run retain the actor serialization for inspection.
    if !out.is_empty() {
        response.body = out.into_bytes();
        response.content_type = String::from("text/html; charset=utf-8");
    }
    response.rendered = Some(rendered);
    response.js = Some(outcome);
    response.live = live;
    response
}

/// External SVG sprite sheets: a `<use href="file.svg#id">` icon (ChatGPT,
/// GitHub, most icon systems) keeps its geometry in one shared file resvg won't
/// fetch itself. Fetch each referenced sheet ONCE — same subresource gate as
/// scripts/sheets — and prime the process-global cache; the layout's
/// `rewrite_inline_svgs` then inlines the used symbol so it rasterizes like any
/// inline vector. Cheap-guarded: a page with no `<use>` pays one substring
/// check. SVG 2 URL reference processing uses the document base URL; the SSRF
/// policy remains anchored to the actual page URL.
async fn fetch_svg_sprite_sheets(html: &str, document_base: &Url, page_url: &Url) {
    if !html.contains("<use") {
        return;
    }
    futures::stream::iter(crate::js::sprite_use_sheets(html).into_iter().map(|raw| {
        let document_base = document_base.clone();
        let page_url = page_url.clone();
        async move {
            let Some(abs) = document_base
                .join(&raw)
                .ok()
                .filter(|u| matches!(u.scheme(), "http" | "https"))
                .filter(|u| subresource_allowed(&page_url, u))
            else {
                return;
            };
            if crate::dom::sprite_sheet_cached(abs.as_str()) {
                return;
            }
            if let Ok(r) = fetch(&Request::subresource(abs.clone(), &page_url, "image", None)).await
                && (200..300).contains(&r.status)
            {
                let text = decode_body(&r.content_type, &r.body);
                crate::dom::prime_sprite_sheet(abs.as_str(), &text);
            }
        }
    }))
    .buffered(PREFETCH_CONCURRENCY)
    .collect::<Vec<_>>()
    .await;
}

/// Fetch every external stylesheet a page declares concurrently, returning
/// `(href, css)` in document order. The request pool bounds simultaneous I/O;
/// no arbitrary declaration-count cutoff is applied to the cascade.
async fn fetch_page_sheets(html: &str, page_url: &Url) -> Vec<(String, String)> {
    let base = base_with_doc_base(html, page_url);
    let jobs = crate::js::external_resources(html)
        .into_iter()
        .filter(|job| job.kind == crate::js::ExternalResourceKind::Sheet);
    let fetched = futures::stream::iter(jobs.map(|job| {
        let base = base.clone();
        let page_url = page_url.clone();
        async move {
            let resolved = base
                .join(&job.source)
                .ok()
                .filter(|u| matches!(u.scheme(), "http" | "https" | "file"))
                .filter(|u| subresource_allowed(&page_url, u));
            let resp = match &resolved {
                Some(u) => fetch(&Request::subresource(
                    u.clone(),
                    &page_url,
                    "style",
                    job.cors_credentials(),
                ))
                .await
                .ok(),
                None => None,
            };
            (job.source, resolved, resp)
        }
    }))
    .buffered(PREFETCH_CONCURRENCY)
    .collect::<Vec<_>>()
    .await
    .into_iter()
    .filter_map(|(raw, resolved, resp)| {
        let url = resolved?;
        let response = resp?.filter_stylesheet()?;
        Some((
            raw,
            url,
            decode_body(&response.content_type, &response.body),
        ))
    })
    .collect::<Vec<_>>();
    futures::stream::iter(fetched.into_iter().map(|(raw, url, css)| {
        let page_url = page_url.clone();
        async move {
            let expanded = expand_stylesheet_imports(css, url, page_url, Vec::new()).await;
            (raw, expanded)
        }
    }))
    .buffered(PREFETCH_CONCURRENCY)
    .collect()
    .await
}

trait StylesheetResponseExt {
    fn filter_stylesheet(self) -> Option<Self>
    where
        Self: Sized;
}

impl StylesheetResponseExt for Response {
    fn filter_stylesheet(self) -> Option<Self> {
        stylesheet_response_allowed(self.status, &self.content_type, &self.headers).then_some(self)
    }
}

#[derive(Clone, Debug)]
struct CssImport {
    start: usize,
    end: usize,
    url: String,
    condition: String,
}

/// CSS Cascade 5 §2.2: replace each applicable `@import` in source order by
/// the imported rules. Imports are cycle-checked; a failed resource contributes
/// no rules. URL tokens are then made absolute
/// against the stylesheet that contained them, as CSSOM URL resolution
/// requires, rather than against the HTML document.
fn expand_stylesheet_imports(
    css: String,
    sheet_url: Url,
    page_url: Url,
    mut ancestry: Vec<String>,
) -> futures::future::BoxFuture<'static, String> {
    use futures::FutureExt as _;
    async move {
        if ancestry.iter().any(|seen| seen == sheet_url.as_str()) {
            return String::new();
        }
        ancestry.push(sheet_url.to_string());
        let imports = stylesheet_imports(&css);
        if imports.is_empty() {
            return absolutize_css_urls(&css, &sheet_url);
        }
        let mut out = String::with_capacity(css.len());
        let mut cursor = 0usize;
        for import in imports {
            out.push_str(&css[cursor..import.start]);
            cursor = import.end;
            let Some(url) = sheet_url
                .join(&import.url)
                .ok()
                .filter(|url| matches!(url.scheme(), "http" | "https" | "file"))
                .filter(|url| subresource_allowed(&page_url, url))
            else {
                continue;
            };
            if ancestry.iter().any(|seen| seen == url.as_str()) {
                continue;
            }
            let Some(response) =
                fetch(&Request::subresource(url.clone(), &page_url, "style", None))
                    .await
                    .ok()
                    .and_then(StylesheetResponseExt::filter_stylesheet)
            else {
                continue;
            };
            let child = expand_stylesheet_imports(
                decode_body(&response.content_type, &response.body),
                url,
                page_url.clone(),
                ancestry.clone(),
            )
            .await;
            out.push_str(&wrap_import_condition(child, &import.condition));
        }
        out.push_str(&css[cursor..]);
        absolutize_css_urls(&out, &sheet_url)
    }
    .boxed()
}

fn wrap_import_condition(css: String, condition: &str) -> String {
    let condition = condition.trim();
    if condition.is_empty() {
        return css;
    }
    // CSS Cascade 5 §2.1 defines layer/supports/media modifiers in that
    // order. Preserve the common media-only form exactly; the parser already
    // understands nested @media blocks. Layer/supports modifiers remain on a
    // generated grouping rule so their cascade/condition semantics survive.
    if let Some(layer) = condition.strip_prefix("layer(")
        && let Some((name, rest)) = layer.split_once(')')
    {
        return wrap_import_condition(format!("@layer {name}{{{css}}}"), rest);
    }
    if let Some(rest) = condition.strip_prefix("layer") {
        return wrap_import_condition(format!("@layer{{{css}}}"), rest);
    }
    if let Some(supports) = condition.strip_prefix("supports(")
        && let Some((query, rest)) = supports.rsplit_once(')')
    {
        return wrap_import_condition(format!("@supports ({query}){{{css}}}"), rest);
    }
    format!("@media {condition}{{{css}}}")
}

fn stylesheet_imports(css: &str) -> Vec<CssImport> {
    let bytes = css.as_bytes();
    let mut imports = Vec::new();
    let mut i = 0usize;
    let mut brace_depth = 0usize;
    let mut quote = None;
    let mut comment = false;
    while i < bytes.len() {
        if comment {
            if bytes.get(i..i + 2) == Some(b"*/") {
                comment = false;
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        if let Some(q) = quote {
            if bytes[i] == b'\\' {
                i = (i + 2).min(bytes.len());
            } else {
                if bytes[i] == q {
                    quote = None;
                }
                i += 1;
            }
            continue;
        }
        if bytes.get(i..i + 2) == Some(b"/*") {
            comment = true;
            i += 2;
            continue;
        }
        if matches!(bytes[i], b'\'' | b'"') {
            quote = Some(bytes[i]);
            i += 1;
            continue;
        }
        match bytes[i] {
            b'{' => brace_depth += 1,
            b'}' => brace_depth = brace_depth.saturating_sub(1),
            b'@' if brace_depth == 0
                && css[i..]
                    .get(..7)
                    .is_some_and(|word| word.eq_ignore_ascii_case("@import")) =>
            {
                let start = i;
                let Some(end) = css_statement_end(css, i + 7) else {
                    break;
                };
                if let Some((url, condition)) = parse_import_prelude(&css[i + 7..end - 1]) {
                    imports.push(CssImport {
                        start,
                        end,
                        url,
                        condition,
                    });
                }
                i = end;
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    imports
}

fn css_statement_end(css: &str, mut i: usize) -> Option<usize> {
    let bytes = css.as_bytes();
    let mut parens = 0usize;
    let mut quote = None;
    while i < bytes.len() {
        if let Some(q) = quote {
            if bytes[i] == b'\\' {
                i = (i + 2).min(bytes.len());
                continue;
            }
            if bytes[i] == q {
                quote = None;
            }
        } else {
            match bytes[i] {
                b'\'' | b'"' => quote = Some(bytes[i]),
                b'(' => parens += 1,
                b')' => parens = parens.saturating_sub(1),
                b';' if parens == 0 => return Some(i + 1),
                _ => {}
            }
        }
        i += 1;
    }
    None
}

fn parse_import_prelude(prelude: &str) -> Option<(String, String)> {
    let prelude = prelude.trim();
    if prelude
        .get(..4)
        .is_some_and(|word| word.eq_ignore_ascii_case("url("))
    {
        let end = prelude.find(')')?;
        let url = unquote_css_url(&prelude[4..end]);
        return Some((url, prelude[end + 1..].trim().to_string()));
    }
    let quote = prelude.as_bytes().first().copied()?;
    if !matches!(quote, b'\'' | b'"') {
        return None;
    }
    let end = quoted_css_end(prelude, 1, quote)?;
    Some((
        prelude[1..end].to_string(),
        prelude[end + 1..].trim().to_string(),
    ))
}

fn quoted_css_end(value: &str, mut i: usize, quote: u8) -> Option<usize> {
    let bytes = value.as_bytes();
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            i += 2;
        } else if bytes[i] == quote {
            return Some(i);
        } else {
            i += 1;
        }
    }
    None
}

fn unquote_css_url(value: &str) -> String {
    let value = value.trim();
    if value.len() >= 2
        && matches!(value.as_bytes()[0], b'\'' | b'"')
        && value.as_bytes()[value.len() - 1] == value.as_bytes()[0]
    {
        value[1..value.len() - 1].to_string()
    } else {
        value.to_string()
    }
}

fn absolutize_css_urls(css: &str, base: &Url) -> String {
    let mut out = String::with_capacity(css.len());
    let mut cursor = 0usize;
    let mut i = 0usize;
    while i < css.len() {
        if css[i..].starts_with("/*") {
            i = css[i + 2..]
                .find("*/")
                .map_or(css.len(), |end| i + 2 + end + 2);
            continue;
        }
        let ch = css[i..].chars().next().unwrap();
        if matches!(ch, '\'' | '"') {
            i = css_string_end(css, i, ch);
            continue;
        }
        if css_url_function_starts(css, i)
            && let Some(span) = css_url_span(css, i)
        {
            let raw = css_unescape_url(&css[span.value_start..span.value_end]);
            let lower = raw.to_ascii_lowercase();
            let resolved = if raw.is_empty()
                || raw.starts_with('#')
                || lower.starts_with("data:")
                || lower.starts_with("blob:")
            {
                None
            } else {
                base.join(&raw).ok()
            };
            if let Some(url) = resolved {
                out.push_str(&css[cursor..span.value_start]);
                out.push_str(url.as_str());
                cursor = span.value_end;
            }
            i = span.function_end;
            continue;
        }
        i += ch.len_utf8();
    }
    out.push_str(&css[cursor..]);
    out
}

#[derive(Clone, Copy)]
struct CssUrlSpan {
    value_start: usize,
    value_end: usize,
    function_end: usize,
}

fn css_url_function_starts(css: &str, start: usize) -> bool {
    let Some(candidate) = css.get(start..start + 4) else {
        return false;
    };
    if !candidate.eq_ignore_ascii_case("url(") {
        return false;
    }
    css[..start]
        .chars()
        .next_back()
        .is_none_or(|ch| !css_ident_continue(ch))
}

fn css_ident_continue(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || !ch.is_ascii()
}

/// Return the URL value and complete function boundaries for one real CSS
/// `url()` token/function. CSS Syntax 3 §§4.3.4–4.3.6 distinguish this from
/// the characters `url(` occurring inside a string or comment; quoted values
/// may themselves contain `)` and arbitrary text such as an SVG `url(#id)`.
fn css_url_span(css: &str, start: usize) -> Option<CssUrlSpan> {
    let mut i = start + 4;
    while css[i..].chars().next().is_some_and(char::is_whitespace) {
        i += css[i..].chars().next().unwrap().len_utf8();
    }
    let first = css[i..].chars().next()?;
    if matches!(first, '\'' | '"') {
        let value_start = i + first.len_utf8();
        let quote_end = css_string_end(css, i, first);
        if quote_end <= value_start || !css[..quote_end].ends_with(first) {
            return None;
        }
        let value_end = quote_end - first.len_utf8();
        let function_end = css_function_end(css, quote_end)?;
        return Some(CssUrlSpan {
            value_start,
            value_end,
            function_end,
        });
    }

    let value_start = i;
    let mut escaped = false;
    while i < css.len() {
        let ch = css[i..].chars().next().unwrap();
        if escaped {
            escaped = false;
            i += ch.len_utf8();
            continue;
        }
        match ch {
            '\\' => {
                escaped = true;
                i += 1;
            }
            ')' => {
                let value_end = css[value_start..i].trim_end().len() + value_start;
                return Some(CssUrlSpan {
                    value_start,
                    value_end,
                    function_end: i + 1,
                });
            }
            '\'' | '"' | '(' => return None,
            _ => i += ch.len_utf8(),
        }
    }
    None
}

fn css_string_end(css: &str, start: usize, quote: char) -> usize {
    let mut i = start + quote.len_utf8();
    let mut escaped = false;
    while i < css.len() {
        let ch = css[i..].chars().next().unwrap();
        i += ch.len_utf8();
        if escaped {
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == quote {
            return i;
        }
    }
    css.len()
}

fn css_function_end(css: &str, mut i: usize) -> Option<usize> {
    let mut depth = 1usize;
    while i < css.len() {
        if css[i..].starts_with("/*") {
            i = css[i + 2..]
                .find("*/")
                .map_or(css.len(), |end| i + 2 + end + 2);
            continue;
        }
        let ch = css[i..].chars().next().unwrap();
        if matches!(ch, '\'' | '"') {
            i = css_string_end(css, i, ch);
            continue;
        }
        i += ch.len_utf8();
        match ch {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

fn css_unescape_url(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        let mut hex = String::new();
        while hex.len() < 6 && chars.peek().is_some_and(char::is_ascii_hexdigit) {
            hex.push(chars.next().unwrap());
        }
        if !hex.is_empty() {
            if chars.peek().is_some_and(|ch| ch.is_whitespace()) {
                chars.next();
            }
            let value = u32::from_str_radix(&hex, 16).unwrap_or(0xFFFD);
            out.push(char::from_u32(value).unwrap_or('\u{FFFD}'));
        } else if let Some(escaped) = chars.next()
            && !matches!(escaped, '\n' | '\r' | '\u{c}')
        {
            out.push(escaped);
        }
    }
    out
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CssFontFace {
    family: String,
    sources: Vec<String>,
}

fn stylesheet_font_faces(css: &str) -> Vec<CssFontFace> {
    let lower = css.to_ascii_lowercase();
    let mut faces = Vec::new();
    let mut cursor = 0usize;
    while let Some(relative) = lower[cursor..].find("@font-face") {
        let rule = cursor + relative;
        let Some(open_relative) = css[rule..].find('{') else {
            break;
        };
        let open = rule + open_relative;
        let Some(close) = css_block_end(css, open) else {
            break;
        };
        let block = &css[open + 1..close];
        let mut family = None;
        let mut sources = Vec::new();
        for declaration in block.split(';') {
            let Some((name, value)) = declaration.split_once(':') else {
                continue;
            };
            if name.trim().eq_ignore_ascii_case("font-family") {
                family = Some(unquote_css_url(value));
            } else if name.trim().eq_ignore_ascii_case("src") {
                let value_lower = value.to_ascii_lowercase();
                let mut source_cursor = 0usize;
                while let Some(relative) = value_lower[source_cursor..].find("url(") {
                    let args = source_cursor + relative + 4;
                    let Some(end) = value[args..].find(')') else {
                        break;
                    };
                    sources.push(unquote_css_url(&value[args..args + end]));
                    source_cursor = args + end + 1;
                }
            }
        }
        if let Some(family) = family
            && !family.trim().is_empty()
            && !sources.is_empty()
        {
            sources.retain(|url| !url.trim().is_empty());
            if !sources.is_empty() {
                faces.push(CssFontFace { family, sources });
            }
        }
        cursor = close + 1;
    }
    faces
}

fn css_block_end(css: &str, open: usize) -> Option<usize> {
    let bytes = css.as_bytes();
    let mut depth = 0usize;
    let mut quote = None;
    let mut i = open;
    while i < bytes.len() {
        if let Some(q) = quote {
            if bytes[i] == b'\\' {
                i += 2;
                continue;
            }
            if bytes[i] == q {
                quote = None;
            }
        } else {
            match bytes[i] {
                b'\'' | b'"' => quote = Some(bytes[i]),
                b'{' => depth += 1,
                b'}' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return Some(i);
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }
    None
}

fn document_font_faces(
    html: &str,
    sheets: &[(String, String)],
    page_url: &Url,
) -> Vec<(String, Vec<Url>)> {
    // CSS Fonts 4 §4.1 defines downloadable faces from the complete set of
    // sheets belonging to the document. Inline style URLs use the document
    // base URL; external-sheet URLs were made absolute against their own sheet
    // by `expand_stylesheet_imports` before reaching this point.
    let document_base = base_with_doc_base(html, page_url);
    let inline = crate::dom::Dom::parse_document(html).inline_stylesheets();
    inline
        .iter()
        .chain(sheets.iter().map(|(_, css)| css))
        .flat_map(|css| stylesheet_font_faces(css))
        .filter_map(|face| {
            let sources = face
                .sources
                .into_iter()
                .filter_map(|source| document_base.join(&source).ok())
                .filter(|url| {
                    matches!(url.scheme(), "http" | "https" | "file")
                        && subresource_allowed(page_url, url)
                })
                .collect::<Vec<_>>();
            (!sources.is_empty()).then_some((face.family, sources))
        })
        .collect()
}

async fn install_stylesheet_fonts(html: &str, sheets: &[(String, String)], page_url: &Url) {
    let faces = document_font_faces(html, sheets, page_url);
    let fonts = futures::stream::iter(faces.into_iter().map(|(family, sources)| async move {
        // CSS Fonts 4 §4.3.3: try external references in specified order and
        // proceed to the next item when loading or format decoding fails.
        for url in sources {
            let Ok(response) = fetch(&Request::subresource(
                url,
                page_url,
                "font",
                Some(CredentialsMode::SameOrigin),
            ))
            .await
            else {
                continue;
            };
            if !(200..300).contains(&response.status) {
                continue;
            }
            if let Some(font) =
                crate::font_system::PageFont::from_web_resource(family.clone(), response.body)
            {
                return Some(font);
            }
        }
        None
    }))
    .buffered(PREFETCH_CONCURRENCY)
    .filter_map(|font| async move { font })
    .collect()
    .await;
    crate::font_system::install_page_fonts(fonts);
}

/// Render an HTML page with ONLY its CSS cascade applied (no JS): fetch its
/// stylesheets and bake the cascade into the serialized DOM. The path for
/// every page JS won't transform — no `<script>`, `set js off`, and the
/// `execute_js` load-timeout/early-exit fallback — so the page still lays out
/// per its own CSS instead of UA defaults (see `crate::js::css_bake`).
pub async fn css_only(response: Response, viewport: (u16, u16), cell_px: (u16, u16)) -> Response {
    css_only_for_device(response, viewport, cell_px, 1.0).await
}

async fn css_only_for_device(
    mut response: Response,
    viewport: (u16, u16),
    cell_px: (u16, u16),
    device_pixel_ratio: f32,
) -> Response {
    let media = response
        .content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if !(media == "text/html" || media == "application/xhtml+xml") {
        return response;
    }
    let html = decode_body(&response.content_type, &response.body);
    if response.declarative_refresh.is_none() {
        response.declarative_refresh = detect_declarative_refresh(&response, &html);
    }
    let sheets = fetch_page_sheets(&html, &response.url).await;
    css_only_with_sheets(response, viewport, cell_px, device_pixel_ratio, sheets).await
}

/// Complete the script-less CSS path after its stylesheet fetches are already
/// available. Keeping this separate lets a CSS-only page that contains a
/// render-affecting `:hover` rule promote to the resident actor without
/// downloading every stylesheet a second time.
async fn css_only_with_sheets(
    mut response: Response,
    viewport: (u16, u16),
    cell_px: (u16, u16),
    device_pixel_ratio: f32,
    sheets: Vec<(String, String)>,
) -> Response {
    let html = decode_body(&response.content_type, &response.body);
    install_stylesheet_fonts(&html, &sheets, &response.url).await;
    let base = base_with_doc_base(&html, &response.url);
    fetch_svg_sprite_sheets(&html, &base, &response.url).await;
    // The frame documents are fetched up front into a url→content map (Dom is
    // not `Send`, so it must never cross an `.await`), then installed into the
    // real arena synchronously.
    let frames = prefetch_frame_documents(&html, &base, &response.url).await;
    let mut dom = crate::js::css_prepare(&html, viewport, cell_px);
    if !frames.is_empty() {
        install_page_frames(&mut dom, &response.url, &frames);
    }
    if !sheets.is_empty() {
        dom.attach_external_sheets(&sheets);
    }
    dom.set_doc_url(Some(base.clone()));
    dom.set_device_pixel_ratio(device_pixel_ratio);
    dom.rewrite_inline_svgs(Some(&base));
    let css_viewport = crate::layout2::TerminalViewport::from_font_pixels(
        usize::from(viewport.0),
        usize::from(viewport.1),
        cell_px,
    )
    .css_viewport();
    response.rendered = Some(Box::new(render_arena(
        &dom,
        &base,
        css_viewport,
        device_pixel_ratio,
        None,
        &crate::layout2::ImageSizes::new(),
    )));
    // Retain a baked source snapshot for history/diagnostics only. Frontends
    // consume `response.rendered` and never parse this presentation copy.
    response.body = dom.serialize(crate::dom::DOCUMENT).into_bytes();
    response.content_type = String::from("text/html; charset=utf-8");
    response
}

fn strip_fragment(u: &str) -> &str {
    u.split('#').next().unwrap_or(u)
}

/// Parse WHATWG HTML's shared declarative-refresh `input` grammar. The URL is
/// left unresolved because an HTTP `Refresh` header uses the response URL as
/// its base, while a `<meta http-equiv=refresh>` uses the document base URL.
fn parse_declarative_refresh_input(input: &str) -> Option<(Duration, Option<String>)> {
    let bytes = input.as_bytes();
    let mut position = 0usize;
    while bytes.get(position).is_some_and(u8::is_ascii_whitespace) {
        position += 1;
    }

    let digits_start = position;
    let mut seconds = 0u64;
    while let Some(digit) = bytes.get(position).filter(|byte| byte.is_ascii_digit()) {
        seconds = seconds
            .saturating_mul(10)
            .saturating_add(u64::from(*digit - b'0'));
        position += 1;
    }
    if position == digits_start && bytes.get(position) != Some(&b'.') {
        return None;
    }
    while bytes
        .get(position)
        .is_some_and(|byte| byte.is_ascii_digit() || *byte == b'.')
    {
        position += 1;
    }

    if position == bytes.len() {
        return Some((Duration::from_secs(seconds), None));
    }
    if !bytes
        .get(position)
        .is_some_and(|byte| byte.is_ascii_whitespace() || matches!(*byte, b';' | b','))
    {
        return None;
    }
    while bytes.get(position).is_some_and(u8::is_ascii_whitespace) {
        position += 1;
    }
    if bytes
        .get(position)
        .is_some_and(|byte| matches!(*byte, b';' | b','))
    {
        position += 1;
    }
    while bytes.get(position).is_some_and(u8::is_ascii_whitespace) {
        position += 1;
    }
    if position == bytes.len() {
        return Some((Duration::from_secs(seconds), None));
    }

    // `URL` is stripped only when the complete ASCII-case-insensitive token,
    // optional whitespace, and `=` are present. A partial token is parsed as
    // part of the URL, matching the specification's labeled jumps.
    let original = position;
    let mut after_url = position;
    let has_url_token = bytes
        .get(after_url..after_url.saturating_add(3))
        .is_some_and(|token| token.eq_ignore_ascii_case(b"url"));
    if has_url_token {
        after_url += 3;
        while bytes.get(after_url).is_some_and(u8::is_ascii_whitespace) {
            after_url += 1;
        }
        if bytes.get(after_url) == Some(&b'=') {
            position = after_url + 1;
            while bytes.get(position).is_some_and(u8::is_ascii_whitespace) {
                position += 1;
            }
        } else {
            position = original;
        }
    }

    let quote = bytes
        .get(position)
        .copied()
        .filter(|byte| matches!(*byte, b'\'' | b'"'));
    if quote.is_some() {
        position += 1;
    }
    let end = quote
        .and_then(|quote| {
            bytes[position..]
                .iter()
                .position(|byte| *byte == quote)
                .map(|offset| position + offset)
        })
        .unwrap_or(bytes.len());
    Some((
        Duration::from_secs(seconds),
        Some(input[position..end].to_string()),
    ))
}

/// Process the first successful `Refresh` header or `<meta http-equiv>`
/// directive using WHATWG HTML §4.2.5's shared declarative refresh steps.
/// Header processing precedes parser-inserted metadata; once one succeeds the
/// document's `will declaratively refresh` flag prevents later directives.
fn detect_declarative_refresh(response: &Response, html: &str) -> Option<DeclarativeRefresh> {
    let resolve = |delay: Duration, target: Option<String>, base: &Url| {
        let url = match target {
            Some(target) => base.join(&target).ok()?,
            None => response.url.clone(),
        };
        (url.scheme() != "javascript").then_some(DeclarativeRefresh { delay, url })
    };

    for (name, value) in &response.headers {
        if name.eq_ignore_ascii_case("refresh")
            && let Some((delay, target)) = parse_declarative_refresh_input(value)
            && let Some(refresh) = resolve(delay, target, &response.url)
        {
            return Some(refresh);
        }
    }

    let lower = html.to_ascii_lowercase();
    if !lower.contains("http-equiv") || !lower.contains("refresh") {
        return None;
    }
    let dom = crate::dom::Dom::parse_document(html);
    let base = base_with_doc_base(html, &response.url);
    for node in dom.descendants(crate::dom::DOCUMENT) {
        if dom.tag_name(node) != Some("meta")
            || !dom
                .attr(node, "http-equiv")
                .is_some_and(|value| value.trim().eq_ignore_ascii_case("refresh"))
        {
            continue;
        }
        let Some(content) = dom.attr(node, "content").filter(|value| !value.is_empty()) else {
            continue;
        };
        if let Some((delay, target)) = parse_declarative_refresh_input(content)
            && let Some(refresh) = resolve(delay, target, &base)
        {
            return Some(refresh);
        }
    }
    None
}

/// The base URL for resolving a document's `src`/`href`: the document URL,
/// overridden by the first `<base href>` if present (mirrors `baseHref()` in
/// the JS pipeline). Takes the raw HTML so it works before the real arena
/// exists (the prefetch phase needs it too).
fn base_with_doc_base(html: &str, doc_url: &Url) -> Url {
    let dom = crate::dom::Dom::parse_document(html);
    for id in dom.flat_descendants(crate::dom::DOCUMENT) {
        if dom.tag_name(id) == Some("base")
            && let Some(href) = dom.attr(id, "href")
            && let Ok(u) = doc_url.join(href.trim())
        {
            return u;
        }
    }
    doc_url.clone()
}

/// Resolve a frame's `src` to the URL it would navigate to, applying the same
/// gating in the prefetch and install passes: network URLs cannot pivot into a
/// private address space, while local `data:` resources are handled by the
/// scheme-fetch path below. The circular-navigation guard follows HTML (a
/// frame may not load a URL already held by an inclusive ancestor navigable).
/// `None` means "don't load".
fn resolve_frame_src(src: &str, base: &Url, page_url: &Url, ancestors: &[String]) -> Option<Url> {
    let url = base.join(src.trim()).ok()?;
    if !matches!(url.scheme(), "http" | "https" | "file" | "data") {
        return None;
    }
    if url.scheme() != "data" && !subresource_allowed(page_url, &url) {
        return None;
    }
    if ancestors.iter().any(|a| a == strip_fragment(url.as_str())) {
        return None;
    }
    Some(url)
}

/// Fetch a local `data:` frame document using Fetch's data-URL processor. The
/// no-JavaScript presentation path has no in-realm `fetch`, so it performs the
/// same byte decode directly before the normal HTML media-type gate.
fn data_frame_document(url: &Url) -> Option<String> {
    let media = url
        .as_str()
        .strip_prefix("data:")
        .and_then(|rest| rest.split_once(',').map(|(metadata, _)| metadata))
        .map(|metadata| {
            let metadata = metadata.trim();
            let metadata = metadata
                .strip_suffix(";base64")
                .or_else(|| metadata.strip_suffix(";BASE64"))
                .unwrap_or(metadata);
            if metadata.is_empty() || metadata.starts_with(';') {
                "text/plain"
            } else {
                metadata.split(';').next().unwrap_or("text/plain")
            }
        })
        .unwrap_or("text/plain")
        .to_ascii_lowercase();
    if media != "text/html" && media != "application/xhtml+xml" {
        return None;
    }
    let bytes = crate::img::decode_data_url(url.as_str())?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// Scan one document's markup for its frames' `src` URLs to fetch and inline
/// `srcdoc` contents to recurse into. Synchronous (a throwaway parse, dropped
/// before the caller's next `.await`), the same parse-don't-pattern-match
/// approach as `external_stylesheets`. Returns `(src URLs, srcdoc bodies)`.
fn scan_frame_sources(
    html: &str,
    base: &Url,
    page_url: &Url,
    ancestors: &[String],
) -> (Vec<Url>, Vec<String>) {
    let dom = crate::dom::Dom::parse_document(html);
    let mut srcs = Vec::new();
    let mut srcdocs = Vec::new();
    for id in dom.flat_descendants(crate::dom::DOCUMENT) {
        match dom.tag_name(id) {
            Some("iframe") | Some("frame") => {}
            _ => continue,
        }
        // srcdoc wins over src (HTML "process the iframe attributes").
        if let Some(srcdoc) = dom.attr(id, "srcdoc") {
            srcdocs.push(srcdoc.to_string());
            continue;
        }
        if let Some(src) = dom.attr(id, "src").map(str::trim).filter(|s| !s.is_empty())
            && let Some(url) = resolve_frame_src(src, base, page_url, ancestors)
        {
            srcs.push(url);
        }
    }
    (srcs, srcdocs)
}

/// Fetch every frame document a script-less page (and its nested frames) needs,
/// into a `url → content` map, breadth-first so each level's `src` fetches
/// overlap. `srcdoc` frames hold no URL but their markup is still scanned for
/// nested `src` frames. Only successful HTML responses are kept; local
/// `data:` documents use their scheme fetch algorithm without touching the
/// network.
async fn prefetch_frame_documents(
    html: &str,
    base: &Url,
    page_url: &Url,
) -> std::collections::HashMap<String, String> {
    use std::collections::VecDeque;
    let mut map: HashMap<String, String> = HashMap::new();
    // (document markup, its base, fragment-stripped ancestor URLs)
    let mut queue: VecDeque<(String, Url, Vec<String>)> = VecDeque::new();
    queue.push_back((
        html.to_string(),
        base.clone(),
        vec![strip_fragment(page_url.as_str()).to_string()],
    ));

    while let Some((markup, base, ancestors)) = queue.pop_front() {
        let (srcs, srcdocs) = scan_frame_sources(&markup, &base, page_url, &ancestors);

        // Fetch this level's `src` documents concurrently.
        let fetched: Vec<Option<(Url, String)>> =
            futures::stream::iter(srcs.into_iter().map(|url| async move {
                if url.scheme() == "data" {
                    return data_frame_document(&url).map(|body| (url, body));
                }
                let resp = fetch(&Request::get(url.clone())).await.ok()?;
                let media = resp
                    .content_type
                    .split(';')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_ascii_lowercase();
                let is_html = media == "text/html" || media == "application/xhtml+xml";
                (resp.status >= 200 && resp.status < 300 && is_html)
                    .then(|| (url, decode_body(&resp.content_type, &resp.body)))
            }))
            .buffered(PREFETCH_CONCURRENCY)
            .collect()
            .await;

        for (url, body) in fetched.into_iter().flatten() {
            let mut child_ancestors = ancestors.clone();
            child_ancestors.push(strip_fragment(url.as_str()).to_string());
            // Nested frames in the fetched content resolve against ITS url.
            queue.push_back((body.clone(), url.clone(), child_ancestors));
            map.insert(url.to_string(), body);
        }
        // srcdoc bodies hold no URL of their own; recurse to load THEIR frames
        // (base/origin inherit the parent document, per about:srcdoc).
        for srcdoc in srcdocs {
            queue.push_back((srcdoc, base.clone(), ancestors.clone()));
        }
    }
    map
}

/// Install the prefetched frame documents into the real arena, reusing the
/// same `Dom::install_frame_document` the JS pipeline drives. Walks the live
/// frames breadth-first; a `src` frame takes its content from `fetched`, a
/// `srcdoc` frame from its attribute (base = the parent document). Synchronous
/// — `Dom` never crosses an `.await`. Bounded identically to the prefetch.
fn install_page_frames(
    dom: &mut crate::dom::Dom,
    page_url: &Url,
    fetched: &HashMap<String, String>,
) {
    use crate::dom::{DOCUMENT, NodeId};
    use std::collections::VecDeque;

    let base = {
        let mut b = page_url.clone();
        for id in dom.descendants(DOCUMENT) {
            if dom.tag_name(id) == Some("base")
                && let Some(href) = dom.attr(id, "href")
                && let Ok(u) = page_url.join(href.trim())
            {
                b = u;
                break;
            }
        }
        b
    };
    // (subtree root, base for that subtree, fragment-stripped ancestor URLs)
    let mut queue: VecDeque<(NodeId, Url, Vec<String>)> = VecDeque::new();
    queue.push_back((
        DOCUMENT,
        base,
        vec![strip_fragment(page_url.as_str()).to_string()],
    ));

    while let Some((root, base, ancestors)) = queue.pop_front() {
        // Collect each frame's content first (immutable borrow), then install
        // (mutable borrow) — can't hold the descendants borrow across install.
        let mut plans: Vec<(NodeId, String, Url, Option<Url>)> = Vec::new();
        for id in dom.descendants(root) {
            match dom.tag_name(id) {
                Some("iframe") | Some("frame") => {}
                _ => continue,
            }
            if let Some(srcdoc) = dom.attr(id, "srcdoc") {
                plans.push((id, srcdoc.to_string(), base.clone(), None));
                continue;
            }
            if let Some(src) = dom.attr(id, "src").map(str::trim).filter(|s| !s.is_empty())
                && let Some(url) = resolve_frame_src(src, &base, page_url, &ancestors)
                && let Some(content) = fetched.get(url.as_str())
            {
                plans.push((id, content.clone(), url.clone(), Some(url)));
            }
        }

        for (frame, content, frame_base, frame_url) in plans {
            if let Some(body) = dom.install_frame_document(frame, &content, frame_base.as_str()) {
                let mut child_ancestors = ancestors.clone();
                if let Some(u) = &frame_url {
                    child_ancestors.push(strip_fragment(u.as_str()).to_string());
                }
                queue.push_back((body, frame_base, child_ancestors));
            }
        }
    }
}

/// Known ad / tracker network domains we neither fetch nor run. A terminal
/// browser can't render ads and shouldn't pretend they loaded: running an ad
/// SDK wastes the wire, leaks privacy, and triggers behaviours a single-view
/// client can't satisfy — erome's age gate only takes its broken pop-under
/// branch (content to a 2nd tab, ad to the main frame) when its ad SDK defines
/// `window.NativeAd`; blocked, it takes the clean no-ad path and the page
/// loads. This is the recognised ad-blocker category — host-based and GENERAL
/// (every site benefits), NOT per-site special-casing. Matched by exact host
/// or subdomain (`cdn.tsyndicate.com` matches `tsyndicate.com`).
const AD_TRACKER_HOSTS: &[&str] = &[
    "tsyndicate.com",
    "magsrv.com",
    "pemsrv.com",
    "exoclick.com",
    "exosrv.com",
    "doubleclick.net",
    "googlesyndication.com",
    "googletagservices.com",
    "googletagmanager.com",
    "google-analytics.com",
    "adservice.google.com",
    "amazon-adsystem.com",
    "adnxs.com",
    "criteo.com",
    "taboola.com",
    "outbrain.com",
    "scorecardresearch.com",
    "quantserve.com",
    "moatads.com",
    "popads.net",
    "popcash.net",
    "propellerads.com",
    "juicyads.com",
    "trafficjunky.net",
    "adsterra.com",
];

/// Whether `host` is, or is a subdomain of, a known ad/tracker network.
pub(crate) fn is_ad_or_tracker_host(host: &str) -> bool {
    let h = host.trim_end_matches('.').to_ascii_lowercase();
    AD_TRACKER_HOSTS
        .iter()
        .any(|&d| h == d || h.ends_with(&format!(".{d}")))
}

/// A public page must not pivot us into fetching subresources (scripts,
/// page-initiated fetch/XHR) from private address space; same-host is
/// always fine (localhost dev included). Known ad/tracker networks are
/// blocked outright (see `AD_TRACKER_HOSTS`).
pub(crate) fn subresource_allowed(page: &Url, script: &Url) -> bool {
    // A shared absent host is not an origin or filesystem permission. In
    // particular, data:/about: documents must never inherit file access.
    if script.scheme() == "file" {
        return crate::file::allowed_from(page, script);
    }
    if let Some(url::Host::Domain(d)) = script.host()
        && is_ad_or_tracker_host(d)
    {
        return false;
    }
    if page.host() == script.host() {
        return true;
    }
    match script.host() {
        Some(url::Host::Domain(d)) => !d.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => {
            !(ip.is_loopback() || ip.is_private() || ip.is_link_local() || ip.is_unspecified())
        }
        Some(url::Host::Ipv6(ip)) => !(ip.is_loopback() || ip.is_unspecified()),
        None => false,
    }
}

/// Two URLs share an origin (scheme + host + port, default ports folded).
fn same_origin(a: &Url, b: &Url) -> bool {
    a.scheme() == b.scheme()
        && a.host_str()
            .zip(b.host_str())
            .is_some_and(|(left, right)| left.eq_ignore_ascii_case(right))
        && a.port_or_known_default() == b.port_or_known_default()
}

pub(crate) fn same_origin_for_host(a: &Url, b: &Url) -> bool {
    same_origin(a, b)
}

/// A URL is potentially trustworthy when it uses authenticated transport or
/// is a local development endpoint. This is the gate used by Fetch Metadata's
/// integration algorithm; ordinary `http:` origins must not receive the
/// `Sec-Fetch-*` family merely because a page supplied a navigation context.
fn potentially_trustworthy(url: &Url) -> bool {
    match url.scheme() {
        "https" | "wss" => true,
        "http" => match url.host() {
            Some(Host::Domain(host)) => {
                let host = host.strip_suffix('.').unwrap_or(host);
                host.eq_ignore_ascii_case("localhost") || host.ends_with(".localhost")
            }
            Some(Host::Ipv4(ip)) => ip.is_loopback(),
            Some(Host::Ipv6(ip)) => ip.is_loopback(),
            None => false,
        },
        _ => false,
    }
}

/// Fetch Metadata sends a `Sec-Fetch-Site` value on every redirect hop. A
/// direct user navigation remains `none`; otherwise a redirect can never make
/// the chain more trusted. Same-origin and schemeful same-site are equivalence
/// relations, so retaining the least-trusted relation over successive hops is
/// equivalent to checking the request origin against the whole URL list.
fn update_navigation_metadata_for_redirect(request: &mut Request, target: &Url) {
    let relation = fetch_site(&request.url, target);
    let Some(metadata) = request.fetch_metadata.as_mut() else {
        return;
    };
    if metadata.site == FetchSite::SameOrigin
        || (metadata.site == FetchSite::SameSite && relation == FetchSite::CrossSite)
    {
        metadata.site = relation;
    }
}

/// HTML #obtain-a-site / #same-site and URL #host-registrable-domain. URL has
/// already applied IDNA/canonicalization. Preserve the trailing dot: the URL
/// standard's registrable-domain algorithm does not equate example.com and
/// example.com. Use the byte helper, not psl::domain_str (which trims it).
fn fetch_site(source: &Url, target: &Url) -> FetchSite {
    if same_origin(source, target) {
        return FetchSite::SameOrigin;
    }
    let (url::Origin::Tuple(a_scheme, a_host, _), url::Origin::Tuple(b_scheme, b_host, _)) =
        (source.origin(), target.origin())
    else {
        return FetchSite::CrossSite;
    };
    if a_scheme != b_scheme {
        return FetchSite::CrossSite;
    }
    let equal_sites = match (&a_host, &b_host) {
        (Host::Domain(a), Host::Domain(b)) => {
            let a = psl::domain(a.as_bytes()).map_or(a.as_bytes(), |d| d.as_bytes());
            let b = psl::domain(b.as_bytes()).map_or(b.as_bytes(), |d| d.as_bytes());
            a == b
        }
        _ => a_host == b_host,
    };
    if equal_sites {
        FetchSite::SameSite
    } else {
        FetchSite::CrossSite
    }
}

/// Fetch Metadata #fetch-integration: caller-owned request context, never
/// inferred from Referer (which may be suppressed) or author Sec-* headers.
pub(crate) fn set_fetch_metadata(
    request: &mut Request,
    source: &Url,
    destination: &'static str,
    mode: &'static str,
) {
    request.timing_client = Some(source.clone());
    request.fetch_metadata = Some(FetchMetadata {
        destination,
        mode,
        site: fetch_site(source, &request.url),
        user_activation: false,
    });
}

/// Mark a request as a top-level, user-activated document navigation. Fetch
/// Metadata is deliberately represented outside `Request::headers`, because
/// `Sec-Fetch-*` are forbidden request-header names for page JavaScript.
///
/// The initial address-bar navigation has no referrer (`none`). A navigation
/// from the current document uses its actual schemeful site. Callers must use
/// this only for browser/user navigation; script and iframe fetches carry their
/// own source context through `set_fetch_metadata`.
pub fn set_navigation_metadata(req: &mut Request, referrer: Option<&Url>) {
    req.timing_client = referrer.cloned();
    let site = match referrer {
        None => FetchSite::None,
        Some(source) => fetch_site(source, &req.url),
    };
    req.fetch_metadata = Some(FetchMetadata {
        destination: "document",
        mode: "navigate",
        site,
        user_activation: true,
    });
}

/// The `Referer` value to send when the document at `page` requests
/// `target`, under the browser default referrer policy
/// (`strict-origin-when-cross-origin`):
///   - `page` isn't http(s): no referrer.
///   - https → http (a downgrade): no referrer (never leak a secure URL).
///   - same origin: the full page URL, minus fragment and credentials.
///   - cross origin: the page's origin only (`scheme://host[:port]/`).
///
/// This is host-agnostic browser behaviour. Hotlink-protected image/media
/// CDNs (gelbooru and many boorus, plenty of CDNs) answer a refererless
/// request with a 302/403 to a placeholder instead of the file; sending
/// what a browser sends is what makes their subresources load.
pub fn referrer_for(page: &Url, target: &Url) -> Option<String> {
    if !matches!(page.scheme(), "http" | "https") {
        return None;
    }
    if page.scheme() == "https" && target.scheme() == "http" {
        return None;
    }
    if same_origin(page, target) {
        let mut r = page.clone();
        r.set_fragment(None);
        let _ = r.set_username("");
        let _ = r.set_password(None);
        Some(r.to_string())
    } else {
        Some(format!("{}/", page.origin().ascii_serialization()))
    }
}

/// Add a `Referer` header to `req` (for `page`) unless the page-supplied
/// headers already carry one. Used by every document-initiated request —
/// subresource loads and the page's own `fetch()`/XHR — so they look like
/// a browser's. No-op when policy says to send nothing.
pub fn set_referrer(req: &mut Request, page: &Url) {
    if req
        .headers
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("referer"))
    {
        return;
    }
    if let Some(referer) = referrer_for(page, &req.url) {
        req.headers.push((String::from("Referer"), referer));
    }
}

/// Set Fetch's image-destination `Accept` default on an image subresource.
/// Preserve an existing value so a future caller that intentionally supplied
/// an image-specific preference still controls content negotiation.
pub fn set_image_accept(req: &mut Request) {
    if req
        .headers
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("accept"))
    {
        return;
    }
    req.headers
        .push((String::from("Accept"), String::from(IMAGE_ACCEPT)));
}

/// Render a response body into a document. `images` maps already-decoded
/// image URLs to intrinsic pixels; pass an empty map on the first parse
/// (the app re-lays-out once its decode pipeline fills it).
pub fn parse(
    url: &Url,
    content_type: &str,
    body: &[u8],
    width: usize,
    viewport_h: usize,
    images: &crate::layout2::ImageSizes,
) -> Doc {
    parse_terminal(url, content_type, body, width, viewport_h, (8, 16), images)
}

/// Terminal parse entry with explicit font-cell metrics. This is the normal
/// frontend boundary; [`parse`] is the deterministic 8×16-cell convenience
/// used by protocol-independent tests and tools.
pub fn parse_terminal(
    url: &Url,
    content_type: &str,
    body: &[u8],
    width: usize,
    viewport_h: usize,
    cell_px: (u16, u16),
    images: &crate::layout2::ImageSizes,
) -> Doc {
    // `parse` is the pre-decode entry (`images` empty, per the doc above), so
    // there is nothing to alpha-composite yet — pass the shared empty alpha map.
    parse_seeded(
        url,
        content_type,
        body,
        width,
        viewport_h,
        cell_px,
        None,
        images,
        no_alpha(),
    )
}

/// Like `parse`, seeding form field values from a previous parse of the
/// same page (resize re-wraps and edits must not lose what was typed).
/// `viewport_h` is the terminal inner height in cells — the basis for `vh`/
/// `vmin`/`vmax` and definite region heights (0 ⇒ unknown, `vh` unresolved).
/// A shared empty `has_alpha` map for callers with no decoded-image alpha info
/// (the first pre-decode parse, tests, harnesses). An empty map disables the P8
/// overlap compositor — the always-correct default (images render separately).
pub fn no_alpha() -> &'static std::collections::HashMap<String, bool> {
    static EMPTY: std::sync::OnceLock<std::collections::HashMap<String, bool>> =
        std::sync::OnceLock::new();
    EMPTY.get_or_init(std::collections::HashMap::new)
}

#[allow(clippy::too_many_arguments)]
pub fn parse_seeded(
    url: &Url,
    content_type: &str,
    body: &[u8],
    width: usize,
    viewport_h: usize,
    cell_px: (u16, u16),
    seed: Option<&[Form]>,
    images: &crate::layout2::ImageSizes,
    // `alpha` = URL→`has_alpha` from the app's decoded cache, threaded to
    // layout2's overlap compositor (P8). Pass `no_alpha()` when unknown.
    alpha: &std::collections::HashMap<String, bool>,
) -> Doc {
    let width = width.max(10);
    let media = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let mut forms = Vec::new();
    let mut rows = Vec::new();
    let mut carousels = Vec::new();
    let mut fixed = Vec::new();
    let mut regions = Vec::new();
    let mut scroll_clips = Vec::new();
    let mut boundaries = Vec::new();
    let mut image_urls = Vec::new();
    let mut hover_ids = std::collections::HashMap::new();
    let mut anchor_rows = std::collections::HashMap::new();
    let mut composites = std::collections::HashMap::new();
    let lines = if media == "text/html" || media == "application/xhtml+xml" {
        let html = decode_body(content_type, body);
        // The HTTP renderer: our own arena DOM laid out into rows of
        // positioned items (multi-link rows, real CSS, live form
        // controls). Forms are extracted from the SAME arena so the
        // control map's node ids line up with the layout pass. HTML no
        // longer uses the line model — `rows` is the whole story.
        // Gated phase timing (TRUST_DIAG_FRAME): the live-reparse peg is here,
        // so split the cost into DOM parse vs cascade+layout to aim the fix.
        let diag = std::env::var_os("TRUST_DIAG_FRAME").is_some();
        let t0 = std::time::Instant::now();
        let mut dom = crate::dom::Dom::parse_document(&html);
        let terminal_viewport =
            crate::layout2::TerminalViewport::from_font_pixels(width, viewport_h, cell_px);
        let css_viewport = terminal_viewport.css_viewport();
        dom.set_doc_url(Some(url.clone()));
        dom.set_viewport_px(css_viewport.width, css_viewport.height);
        dom.set_device_pixel_ratio(1.0);
        // Turn renderable inline <svg> into <img data:…> so vectors render
        // (silhouette-tinted) through the image pipeline instead of as text.
        dom.rewrite_inline_svgs(Some(url));
        let t_dom = t0.elapsed();
        let t1 = std::time::Instant::now();
        let (found, controls) = extract_forms_arena(&dom, url, seed);
        forms = found;
        image_urls = collect_image_urls(&dom, url, css_viewport, 1.0).all;
        let t_forms = t1.elapsed();
        let t2 = std::time::Instant::now();
        let (
            laid,
            found_carousels,
            found_regions,
            found_clips,
            found_boundaries,
            found_fixed,
            found_anchors,
        ) = {
            // The fragment-tree engine (layout2 architecture). It emits the
            // pinned fixed layer (P4), vertical scroll regions + their
            // scroll_clips (P5b), carousels (P5c), and incremental-layout
            // boundaries (P7 — block-filling IFC boxes, so a live mutation
            // confined to one patches its subtree instead of a full relayout).
            let out = crate::layout2::lay_out_document(
                &dom,
                url,
                terminal_viewport,
                &forms,
                &controls,
                images,
                alpha,
            );
            composites = out.composites;
            (
                out.rows,
                out.carousels,
                out.regions,
                out.scroll_clips,
                out.boundaries,
                out.fixed,
                out.anchor_rows,
            )
        };
        if diag {
            let c = crate::dom::take_casc_diag();
            eprintln!(
                "DIAGPARSE html={}KB nodes={} dom={}ms forms={}ms layout={}ms total={}ms | cascade={}ms css_parse={}ms rules={}",
                html.len() / 1024,
                dom.node_count(),
                t_dom.as_millis(),
                t_forms.as_millis(),
                t2.elapsed().as_millis(),
                t0.elapsed().as_millis(),
                c.cascaded_us / 1000,
                c.style_index_us / 1000,
                c.rules,
            );
        }
        rows = laid;
        carousels = found_carousels;
        fixed = found_fixed;
        regions = found_regions;
        scroll_clips = found_clips;
        boundaries = found_boundaries;
        hover_ids = collect_hover_ids(&dom);
        anchor_rows = found_anchors;
        Vec::new()
    } else if crate::download::mime_is_renderable(&media, false) {
        crate::doc::wrap_plain(&decode_body(content_type, body), width)
    } else {
        vec![DocLine {
            kind: Kind::Error,
            text: format!("unsupported media type: {content_type}"),
            link: None,
        }]
    };
    Doc {
        url: Link::Http(url.clone()),
        lines,
        finger: None,
        gopher: None,
        whois: None,
        rdap: None,
        dict: None,
        raw: body.to_vec(),
        wrapped_to: width,
        cp437: false,
        meta: Some(content_type.to_string()),
        forms,
        rows,
        eager_image_urls: image_urls.clone(),
        image_urls,
        deferred_images: Vec::new(),
        blobs: None,
        carousels,
        fixed,
        regions,
        scroll_clips,
        boundaries,
        hover_ids,
        anchor_rows,
        composites,
    }
}

/// Resolve every layout-DOM element under an actor marker to its NEAREST
/// marker's actor node id, while the parsed DOM is still alive (it doesn't
/// survive into `Doc`, so the hover hit-test can't walk ancestors later).
/// Markers: `data-trust-hover` (a hover-listener host) AND `x-trust-js`
/// anchors (clickables) — folding both into one top-down walk makes
/// "deepest marker wins" structural, which is the correct hover target
/// either way around: a Steam row anchor INSIDE a delegating container
/// resolves to the row (so `e.target.closest('.row')` works), and a
/// hover-only div INSIDE a clickable card resolves to the div (bubbling
/// still reaches the card). Non-live pages have no markers → empty map.
fn collect_hover_ids(
    dom: &crate::dom::Dom,
) -> std::collections::HashMap<crate::dom::NodeId, usize> {
    let mut map = std::collections::HashMap::new();
    let mut stack: Vec<(crate::dom::NodeId, Option<usize>)> = vec![(crate::dom::DOCUMENT, None)];
    while let Some((id, mut cur)) = stack.pop() {
        let marker = dom
            .attr(id, "data-trust-hover")
            .and_then(|v| v.parse().ok())
            .or_else(|| {
                dom.attr(id, "href")
                    .filter(|_| dom.tag_name(id) == Some("a"))
                    .and_then(|h| h.strip_prefix("x-trust-js:"))
                    .and_then(|rest| rest.split_once(':'))
                    .and_then(|(n, _)| n.parse().ok())
            });
        if let Some(actor) = marker {
            cur = Some(actor);
        }
        if let Some(actor) = cur
            && dom.tag_name(id).is_some()
        {
            map.insert(id, actor);
        }
        for c in dom.children(id) {
            stack.push((c, cur));
        }
    }
    map
}

/// The result of laying out one incremental-layout region patch
/// (incremental-layout contract): the boundary's freshly laid buffer + the
/// metadata the app needs to swap it into the live `Region`.
pub struct RegionPatch {
    /// The region's new scrollable content buffer (replaces `Region.buffer`).
    pub rows: Vec<crate::layout2::Row>,
    /// Carousels found inside the new buffer (buffer-relative).
    pub carousels: Vec<crate::layout2::Carousel>,
    /// Absolute http(s) image URLs in the new buffer, to feed the decode pipe.
    pub image_urls: Vec<String>,
    /// The page's CSS-pixel `scrollTop` signal, quantized to terminal rows at
    /// this adapter boundary, if it pinned the scroll this update (a chat
    /// re-pinning to bottom); else the app keeps the reader's offset. Mirrors
    /// `flow_region`'s `data-trust-scroll-top` read.
    pub scroll_top: Option<usize>,
    /// Clip boxes `(live_node, rows, cells)` of every definite-height scroll box
    /// nested in the fragment — the app merges them into `Doc.scroll_clips` so a
    /// nested scroller's `clientHeight` stays honest across region relayouts.
    pub scroll_clips: Vec<(usize, u16, u16)>,
}

/// Parse and lay out a single relayout-boundary fragment (a scroll region) for
/// an incremental patch — WITHOUT re-parsing or re-laying the whole document.
/// `fragment_html` is `Dom::serialize_patch`'s output (the boundary inside a
/// context wrapper carrying its inherited style). Returns `None` when the
/// boundary can't be found in the fragment (treat as a resync). Mirrors the
/// HTML arm of `parse_seeded`, scoped to the one subtree.
pub fn lay_region_patch(
    url: &Url,
    fragment_html: &[u8],
    content_width: usize,
    viewport: (usize, usize),
    cell_px: (u16, u16),
    images: &crate::layout2::ImageSizes,
    boundary_node: usize,
) -> Option<RegionPatch> {
    let diag = std::env::var_os("TRUST_DIAG_PATCH").is_some();
    let t0 = std::time::Instant::now();
    let html = decode_body("text/html; charset=utf-8", fragment_html);
    let mut dom = crate::dom::Dom::parse_document(&html);
    let terminal_viewport =
        crate::layout2::TerminalViewport::from_font_pixels(viewport.0, viewport.1, cell_px);
    let css_viewport = terminal_viewport.css_viewport();
    dom.set_doc_url(Some(url.clone()));
    dom.set_viewport_px(css_viewport.width, css_viewport.height);
    dom.set_device_pixel_ratio(1.0);
    dom.rewrite_inline_svgs(Some(url));
    let key = boundary_node.to_string();
    let boundary = dom
        .descendants(crate::dom::DOCUMENT)
        .into_iter()
        .find(|&id| dom.attr(id, "data-trust-node") == Some(key.as_str()))?;
    let (_forms, controls) = extract_forms_arena(&dom, url, None);
    let image_urls = collect_image_urls(&dom, url, css_viewport, 1.0).all;
    let scroll_top = dom
        .attr(boundary, "data-trust-scroll-top")
        .and_then(|s| s.parse::<f32>().ok())
        .filter(|v| v.is_finite() && *v >= 0.0)
        .map(|px| (px / f32::from(cell_px.1.max(1))).round().max(0.0) as usize);
    let t_parse = t0.elapsed();
    let t1 = std::time::Instant::now();
    // Re-lay the region's content into a fresh buffer, through the SAME engine
    // that laid the page out — so a patched region is consistent with the full
    // render (layout2 architecture P7). The row cache is not used here (v1,
    // correctness over the reuse memo).
    let (rows, carousels, scroll_clips) = crate::layout2::lay_region_fragment(
        &dom,
        url,
        content_width,
        terminal_viewport,
        &controls,
        images,
        boundary,
    );
    if diag {
        let n_nodes = dom.descendants(crate::dom::DOCUMENT).count();
        eprintln!(
            "DIAGPATCH region-split parse={}us layout={}us nodes={} rows={}",
            t_parse.as_micros(),
            t1.elapsed().as_micros(),
            n_nodes,
            rows.len(),
        );
    }
    Some(RegionPatch {
        rows,
        carousels,
        image_urls,
        scroll_top,
        scroll_clips,
    })
}

/// The result of laying one INLINE incremental-layout boundary patch
/// (incremental-layout contract §14): the boundary's freshly laid rows (fragment-
/// relative cols) + the metadata the app needs to splice them into `Doc.rows`.
/// (Distinct from `js::SubtreePatch`, the actor→app protocol message — this is
/// the laid-out geometry the app derives from it.)
pub struct SubtreeLaid {
    /// The boundary's new content rows (cols from 0 — the app shifts by the
    /// cached `origin_col`).
    pub rows: Vec<crate::layout2::Row>,
    /// New content height (rows). `delta = height − old row span` drives the
    /// Tier-2 shift of following content.
    pub height: usize,
    /// Painted content extent (cells). A sanity bound (≤ `content_width`).
    pub width: u16,
    /// Absolute http(s) image URLs the patch introduced, to feed the decode pipe.
    pub image_urls: Vec<String>,
    /// Non-empty iff the box grew a scroll region / carousel since capture — the
    /// app then resyncs (the box is no longer a pure-`Doc.rows` inline boundary).
    pub has_subframes: bool,
}

/// Parse and lay out a single INLINE relayout-boundary fragment for the general
/// incremental splice — WITHOUT re-parsing or re-laying the whole document.
/// `fragment_html` is `Dom::serialize_patch`'s output (the boundary inside a
/// context wrapper carrying its inherited style). `content_width` is the cached
/// outer band the box fills. Returns `None` when the boundary can't be found
/// (treat as a resync). Sibling of `lay_region_patch`, scoped to one inline box.
#[allow(clippy::too_many_arguments)]
pub fn lay_subtree_patch(
    url: &Url,
    fragment_html: &[u8],
    content_width: usize,
    viewport: (usize, usize),
    cell_px: (u16, u16),
    images: &crate::layout2::ImageSizes,
    boundary_node: usize,
    sub_box: bool,
    quantization_phase: (f32, f32),
) -> Option<SubtreeLaid> {
    let html = decode_body("text/html; charset=utf-8", fragment_html);
    let mut dom = crate::dom::Dom::parse_document(&html);
    let terminal_viewport =
        crate::layout2::TerminalViewport::from_font_pixels(viewport.0, viewport.1, cell_px);
    let css_viewport = terminal_viewport.css_viewport();
    dom.set_doc_url(Some(url.clone()));
    dom.set_viewport_px(css_viewport.width, css_viewport.height);
    dom.set_device_pixel_ratio(1.0);
    dom.rewrite_inline_svgs(Some(url));
    let key = boundary_node.to_string();
    let boundary = dom
        .descendants(crate::dom::DOCUMENT)
        .into_iter()
        .find(|&id| dom.attr(id, "data-trust-node") == Some(key.as_str()))?;
    let (_forms, controls) = extract_forms_arena(&dom, url, None);
    let image_urls = collect_image_urls(&dom, url, css_viewport, 1.0).all;
    // The subtree lays through the SAME engine that rendered the page, so a
    // patched boundary is byte-consistent with a full relayout
    // (layout2 architecture P7).
    let frag = crate::layout2::lay_subtree_fragment(
        &dom,
        url,
        content_width,
        terminal_viewport,
        &controls,
        images,
        boundary,
        sub_box,
        quantization_phase,
    );
    Some(SubtreeLaid {
        height: frag.height,
        width: frag.width,
        rows: frag.rows,
        has_subframes: !frag.regions.is_empty()
            || !frag.carousels.is_empty()
            || !frag.scroll_clips.is_empty(),
        image_urls,
    })
}

/// The absolute image URLs referenced by replaced elements and CSS image
/// properties, de-duplicated in document order (the decode pipeline fetches
/// each once).
pub(crate) struct CollectedImages {
    pub(crate) all: Vec<String>,
    pub(crate) eager: Vec<String>,
    pub(crate) lazy_handles: std::collections::HashSet<crate::render::ImageHandle>,
    pub(crate) lazy_nodes: Vec<(crate::dom::NodeId, String)>,
}

pub(crate) fn collect_image_urls(
    dom: &crate::dom::Dom,
    base: &Url,
    viewport: crate::layout2::Viewport,
    device_pixel_ratio: f32,
) -> CollectedImages {
    collect_image_urls_for_boxes(dom, base, viewport, device_pixel_ratio, None)
}

fn collect_image_urls_for_boxes(
    dom: &crate::dom::Dom,
    base: &Url,
    viewport: crate::layout2::Viewport,
    device_pixel_ratio: f32,
    boxes: Option<&std::collections::HashMap<crate::dom::NodeId, crate::layout2::PxRect>>,
) -> CollectedImages {
    let mut urls = Vec::new();
    let mut eager = Vec::new();
    let mut lazy_handles = std::collections::HashSet::new();
    let mut lazy_nodes = Vec::new();
    for id in dom.flat_descendants(crate::dom::DOCUMENT) {
        // `<img src>` and `<video poster>` (the poster renders as the video's
        // clickable thumbnail) both feed the decode pipeline.
        let selected = (dom.tag_name(id) == Some("img"))
            .then(|| crate::responsive_image::select(dom, id, base, viewport, device_pixel_ratio))
            .flatten()
            .filter(crate::responsive_image::loadable_source);
        let svg = (dom.tag_name(id) == Some("svg"))
            .then(|| dom.svg_image_data(id, Some(base)))
            .flatten()
            .map(|(source, _)| source);
        let poster = (dom.tag_name(id) == Some("video"))
            .then(|| dom.attr(id, "poster"))
            .flatten()
            .map(str::trim)
            .filter(|source| !source.is_empty());
        let Some(src) = selected
            .as_ref()
            .map(|selected| selected.source.as_str())
            .or(svg.as_deref())
            .or(poster)
        else {
            continue;
        };
        // A `data:` image (inline SVG rewritten to one, or a page's own data
        // image) carries its bytes inline — decoded locally, never fetched.
        // A `blob:` image resolves from the page's blob byte mirror
        // (`Doc.blobs`) the same local way (Steam's client-generated QR).
        let u = if src.starts_with("data:") || src.starts_with("blob:") {
            src.to_string()
        } else if let Link::Http(u) = resolve(base, src) {
            u.to_string()
        } else {
            continue;
        };
        let is_lazy = selected.is_some()
            && dom
                .attr(id, "loading")
                .is_some_and(|value| value.eq_ignore_ascii_case("lazy"));
        let handle = crate::render::ImageHandle::for_source(&u);
        if !urls.contains(&u) {
            urls.push(u.clone());
        }
        if is_lazy && !eager.contains(&u) {
            lazy_handles.insert(handle);
            lazy_nodes.push((id, u));
        } else {
            lazy_handles.remove(&handle);
            if !eager.contains(&u) {
                eager.push(u);
            }
        }
    }
    // CSS image-valued properties participate in the same fetch/discovery
    // pipeline as replaced elements. In particular, the initial value of
    // `background-repeat` is `repeat`, so a background image must be available
    // before the first paint rather than waiting for a later display-list
    // rebuild (CSS Backgrounds 3 §§2.1, 2.3).
    for id in dom.flat_descendants(crate::dom::DOCUMENT) {
        // CSS Backgrounds 3 §2.3 permits deferring invisible images; CSS
        // Display 3 §2.5 excludes display:none subtrees from the box tree.
        // The desktop already has measured boxes: don't cascade thousands
        // of unrendered descendants just to discover unused CSS resources.
        // Visible descendants resolve inherited cursor/list images through
        // display:contents ancestors normally. Offscreen boxes still count.
        // HTML image selection above remains independent of CSS rendering.
        if dom.tag_name(id).is_none() || boxes.is_some_and(|boxes| !boxes.contains_key(&id)) {
            continue;
        }
        for property in ["background-image", "list-style-image", "cursor"] {
            let Some(value) = dom.computed_value_resolved(id, property) else {
                continue;
            };
            for source in css_image_sources(&value) {
                let Some(url) = resolve_css_image_source(base, &source) else {
                    continue;
                };
                if !urls.contains(&url) {
                    urls.push(url.clone());
                }
                if !eager.contains(&url) {
                    eager.push(url);
                }
            }
        }
    }
    // A `<video>` whose source is MSE/blob (no `src`/`<source>`/`poster` — every
    // modern streaming player) renders as a "play in mpv" representation; give
    // it a preview frame from the page's standard Open Graph image so the
    // representation has a thumbnail. The PAGE-LEVEL fallback needs the same
    // frame for the opposite reason: NO `<video>`/`<audio>` exists at all but
    // the page declares itself a video page via `og:video` (exactly
    // `flow_page_level_media_fallback`'s gate) — and since a drawn preview IS
    // the mpv link, the frame must reach the decode pipe or the affordance
    // stays a bare text line forever.
    let mut has_video = false;
    let mut has_media = false;
    for id in dom.flat_descendants(crate::dom::DOCUMENT) {
        match dom.tag_name(id) {
            Some("video") => {
                has_video = true;
                has_media = true;
            }
            Some("audio") => has_media = true,
            _ => {}
        }
    }
    // Mirrors the layout gates exactly: a present `<video>` only borrows the
    // page preview when the page IS a video page. Every `<video>` has an mpv
    // activation surface, but a generic page's `og:image` still describes the
    // page rather than that particular element (a homepage logo must not be
    // borrowed as a phantom poster). The page-level fallback needs the same
    // declaration when no media element mounts at all.
    let page_is_video = crate::layout2::page_declares_video(dom);
    if page_is_video
        && (has_video || !has_media)
        && let Some(preview) = crate::layout2::page_preview_image(dom, base)
        && !urls.contains(&preview)
    {
        lazy_handles.remove(&crate::render::ImageHandle::for_source(&preview));
        eager.push(preview.clone());
        urls.push(preview);
    }
    CollectedImages {
        all: urls,
        eager,
        lazy_handles,
        lazy_nodes,
    }
}

fn css_image_sources(value: &str) -> Vec<String> {
    let lower = value.to_ascii_lowercase();
    let mut sources = Vec::new();
    let mut cursor = 0usize;
    while let Some(relative) = lower[cursor..].find("url(") {
        let open = cursor + relative + 4;
        let Some(close) = value[open..].find(')') else {
            break;
        };
        let source = value[open..open + close]
            .trim()
            .trim_matches(['\'', '"'])
            .to_string();
        if !source.is_empty() {
            sources.push(source);
        }
        cursor = open + close + 1;
    }
    sources
}

fn resolve_css_image_source(base: &Url, source: &str) -> Option<String> {
    if source.starts_with("data:") || source.starts_with("blob:") {
        return Some(source.to_string());
    }
    match resolve(base, source) {
        Link::Http(url) => Some(url.to_string()),
        _ => None,
    }
}

/// Resolve an href against the page, mapping schemes to our link types.
pub(crate) fn resolve(base: &Url, target: &str) -> Link {
    if target
        .split_once(':')
        .is_some_and(|(scheme, _)| crate::gopher::is_scheme(scheme))
    {
        return crate::gopher::GopherUrl::parse(target)
            .map(Link::Gopher)
            .unwrap_or_else(|| Link::External(target.into()));
    }
    // Living-page click markers: `x-trust-js:<node>:<original-href>`.
    if let Some(rest) = target.strip_prefix("x-trust-js:")
        && let Some((node, href)) = rest.split_once(':')
        && let Ok(node) = node.parse::<usize>()
    {
        return Link::JsClick {
            node,
            href: href.to_string(),
        };
    }
    match base.join(target) {
        Ok(joined) => match joined.scheme() {
            "http" | "https" | "file" => Link::Http(joined),
            "gemini" => crate::gemini::GeminiUrl::parse(joined.as_str())
                .map(Link::Gemini)
                .unwrap_or_else(|| Link::External(joined.to_string())),
            "gopher" | "gophers" => crate::gopher::GopherUrl::parse(joined.as_str())
                .map(Link::Gopher)
                .unwrap_or_else(|| Link::External(joined.to_string())),
            "dict" => crate::dict::Target::parse(joined.as_str())
                .map(Link::Dict)
                .unwrap_or_else(|_| Link::External(joined.to_string())),
            "finger" | "whois" => crate::oneshot::OneShotUrl::parse(joined.as_str())
                .map(Link::OneShot)
                .unwrap_or_else(|| Link::External(joined.to_string())),
            "telnet" | "telnets" => joined.host_str().map_or_else(
                || Link::External(joined.to_string()),
                |host| Link::Telnet {
                    host: host.to_string(),
                    port: joined.port().unwrap_or(if joined.scheme() == "telnets" {
                        992
                    } else {
                        23
                    }),
                    tls: joined.scheme() == "telnets",
                },
            ),
            _ => Link::External(joined.to_string()),
        },
        Err(_) => Link::External(target.to_string()),
    }
}

/// Extract the page's forms from our own arena DOM (the layout path's
/// source of truth), returning the forms plus a map from each rendering
/// control's `NodeId` to its `(form, field)` indices so the layout can
/// make those items selectable `Link::Form`s.
pub fn extract_forms_arena(
    dom: &crate::dom::Dom,
    base: &Url,
    seed: Option<&[Form]>,
) -> (Vec<Form>, std::collections::HashMap<usize, (usize, usize)>) {
    let mut forms = Vec::new();
    let mut map = std::collections::HashMap::new();
    // Controls outside any `<form>` share an implicit form owner (HTML's
    // null-form-owner concept): they're still interactive — typed into,
    // toggled — they just don't submit anywhere. React/SPA inputs are very
    // often formless (search-as-you-type, filters, settings), so without
    // this they'd render as inert stubs the user can't edit. Created lazily
    // (only if such a control exists), appended like any other form.
    let mut implicit = None;
    walk_forms_arena(
        dom,
        crate::dom::DOCUMENT,
        None,
        base,
        &mut forms,
        &mut map,
        &mut implicit,
    );

    // Seed values typed into a previous parse of this page (resize/edit
    // re-parses must not lose what was entered), shape permitting.
    if let Some(seed) = seed {
        for (new, old) in forms.iter_mut().zip(seed) {
            if new.fields.len() == old.fields.len()
                && new
                    .fields
                    .iter()
                    .zip(&old.fields)
                    .all(|(a, b)| a.name == b.name)
            {
                for (a, b) in new.fields.iter_mut().zip(&old.fields) {
                    a.value = b.value.clone();
                    if let Some(number) = &mut a.number {
                        number.editing = b.number.as_ref().and_then(|n| n.editing.clone());
                    }
                    a.checked = b.checked;
                }
            }
        }
    }
    (forms, map)
}

fn walk_forms_arena(
    dom: &crate::dom::Dom,
    id: usize,
    current: Option<usize>,
    base: &Url,
    forms: &mut Vec<Form>,
    map: &mut std::collections::HashMap<usize, (usize, usize)>,
    implicit: &mut Option<usize>,
) {
    for child in dom.flat_children(id) {
        match dom.tag_name(child) {
            Some("form") => {
                let method = match dom.attr(child, "method") {
                    Some(m) if m.eq_ignore_ascii_case("post") => FormMethod::Post,
                    _ => FormMethod::Get,
                };
                let action = base
                    .join(dom.attr(child, "action").unwrap_or(""))
                    .unwrap_or_else(|_| base.clone());
                forms.push(Form {
                    method,
                    action,
                    fields: Vec::new(),
                    live_node: live_node(dom, child),
                });
                let form = forms.len() - 1;
                walk_forms_arena(dom, child, Some(form), base, forms, map, implicit);
                // WHATWG HTML §4.10.22.2 defines implicit submission as an
                // Enter-key algorithm.  It does not create a submit control in
                // the DOM or rendering tree.  Keeping only authored controls
                // here prevents a button-less search form from acquiring a
                // visible `[ Submit ]` flex item.
            }
            Some(tag @ ("input" | "button" | "select" | "textarea")) => {
                let Some(field) = field_from_arena(dom, child, tag) else {
                    continue;
                };
                // WHATWG HTML §4.10.6 says that a <button> is labeled by
                // its CONTENTS, and Rendering §15.5.3 makes those child boxes
                // the anonymous button content box. A button with no form
                // owner therefore must not be turned into our synthetic form
                // atom: doing so discards an icon-only button's <img>/<svg>
                // child and paints "[ Button ]" instead. Living pages route
                // such controls through their x-trust-js click-marker wrapper;
                // leaving the element unmapped lets normal button layout paint
                // its authored children inside that clickable wrapper.
                if current.is_none() && tag == "button" {
                    continue;
                }
                // A formless submit control (a bare <button>/<input type=submit>
                // with no form owner) has nothing to submit — its onClick is the
                // whole interaction, so leave it to the JsClick stub path rather
                // than claiming it as a form field here.
                if current.is_none() && field.kind == FieldKind::Submit {
                    continue;
                }
                // Bind to the enclosing <form>, or to the lazily-created
                // implicit form for an editable control with no form owner: a
                // text field stays editable, a checkbox toggleable — the live
                // page sees the input/change/click events. It gets no synthetic
                // submit (nowhere to submit to).
                let form = match current {
                    Some(f) => f,
                    None => *implicit.get_or_insert_with(|| {
                        forms.push(Form {
                            method: FormMethod::Get,
                            action: base.clone(),
                            fields: Vec::new(),
                            live_node: None,
                        });
                        forms.len() - 1
                    }),
                };
                let renders = field.kind != FieldKind::Hidden;
                forms[form].fields.push(field);
                if renders {
                    map.insert(child, (form, forms[form].fields.len() - 1));
                }
            }
            _ if dom.is_contenteditable_host(child) => {
                // A `contenteditable` host (a rich-text editor root — ProseMirror/
                // TipTap, Quill, a comment box) edits like a textarea but isn't a
                // form control. Surface it as a synthetic, un-submitted Textarea
                // field so it rides the existing editable machinery (selection, the
                // edit prompt, the live `SetValue` path). Bound to the enclosing
                // form, or the implicit form when formless; `name` stays empty so it
                // never contributes to a submit. Its own markup is the editor's — we
                // don't recurse into it (the host is one widget).
                let form = match current {
                    Some(f) => f,
                    None => *implicit.get_or_insert_with(|| {
                        forms.push(Form {
                            method: FormMethod::Get,
                            action: base.clone(),
                            fields: Vec::new(),
                            live_node: None,
                        });
                        forms.len() - 1
                    }),
                };
                // Whitespace-only content is an EMPTY editor (a plain editable's
                // stray newline, ProseMirror's `<p><br></p>`) — treat it as empty
                // so the placeholder shows instead of a blank `[]`; real content
                // (including its own leading/trailing spaces) is kept verbatim.
                let raw = dom.text_content(child);
                let value = if raw.trim().is_empty() {
                    String::new()
                } else {
                    raw
                };
                forms[form].fields.push(Field {
                    name: String::new(),
                    default_value: value.clone(),
                    value,
                    checked: false,
                    default_checked: false,
                    label: contenteditable_placeholder(dom, child),
                    kind: FieldKind::Textarea,
                    number: None,
                    live_node: live_node(dom, child),
                });
                map.insert(child, (form, forms[form].fields.len() - 1));
            }
            _ => walk_forms_arena(dom, child, current, base, forms, map, implicit),
        }
    }
}

/// The placeholder hint for a `contenteditable` host: its own hint attribute,
/// else a descendant's (rich editors put the placeholder on an inner block —
/// ProseMirror writes `data-placeholder` on the first paragraph). Empty when
/// none is declared, which renders as an empty `[]` box like any blank field.
fn contenteditable_placeholder(dom: &crate::dom::Dom, id: usize) -> String {
    for attr in [
        "aria-label",
        "aria-placeholder",
        "placeholder",
        "data-placeholder",
        "title",
    ] {
        if let Some(v) = dom.attr(id, attr)
            && !v.trim().is_empty()
        {
            return v.trim().to_string();
        }
    }
    for d in dom.descendants(id) {
        for attr in ["data-placeholder", "aria-placeholder", "placeholder"] {
            if let Some(v) = dom.attr(d, attr)
                && !v.trim().is_empty()
            {
                return v.trim().to_string();
            }
        }
    }
    String::new()
}

/// Build a `Field` from an arena control element (mirrors `field_from`
/// but over our own DOM), or `None` for controls we drop.
fn live_node(dom: &crate::dom::Dom, id: usize) -> Option<usize> {
    if dom.render_live() {
        Some(id)
    } else {
        dom.attr(id, "data-trust-node")?.parse().ok()
    }
}

fn field_from_arena(dom: &crate::dom::Dom, id: usize, tag: &str) -> Option<Field> {
    let name = dom.attr(id, "name").unwrap_or("").to_string();
    let value = if tag == "input" {
        dom.input_value(id)
    } else {
        dom.attr(id, "value").unwrap_or("").to_string()
    };
    let checked = dom.attr(id, "checked").is_some();
    let mut label = String::new();
    let kind = match tag {
        "input" => {
            let ty = dom.attr(id, "type").unwrap_or("").to_ascii_lowercase();
            match ty.as_str() {
                "hidden" => FieldKind::Hidden,
                "number" => FieldKind::Number,
                "password" => FieldKind::Password,
                "checkbox" => FieldKind::Checkbox,
                "radio" => FieldKind::Radio,
                "submit" | "image" => {
                    label = if value.is_empty() {
                        String::from("Submit")
                    } else {
                        value.clone()
                    };
                    FieldKind::Submit
                }
                "button" => {
                    label = if value.is_empty() {
                        String::from("Button")
                    } else {
                        value.clone()
                    };
                    FieldKind::Button
                }
                "reset" => {
                    label = if value.is_empty() {
                        String::from("Reset")
                    } else {
                        value.clone()
                    };
                    FieldKind::Reset
                }
                "file" => return None,
                _ => {
                    label = dom.attr(id, "placeholder").unwrap_or("").to_string();
                    FieldKind::Text
                }
            }
        }
        "button" => {
            let ty = dom.attr(id, "type").unwrap_or("").to_ascii_lowercase();
            let text = dom.text_content(id).trim().to_string();
            label = if !text.is_empty() {
                text
            } else if !value.is_empty() {
                value.clone()
            } else {
                match ty.as_str() {
                    "reset" => String::from("Reset"),
                    "button" => String::from("Button"),
                    _ => String::from("Submit"),
                }
            };
            match ty.as_str() {
                "reset" => FieldKind::Reset,
                "button" => FieldKind::Button,
                "" | "submit" => FieldKind::Submit,
                _ => return None,
            }
        }
        "textarea" => {
            let value = dom.text_content(id);
            return Some(Field {
                name,
                default_value: value.clone(),
                value,
                checked: false,
                default_checked: false,
                label,
                kind: FieldKind::Textarea,
                number: None,
                live_node: live_node(dom, id),
            });
        }
        "select" => {
            let mut options: Vec<(String, String)> = Vec::new();
            let mut selected = None;
            for option in dom.children(id) {
                if dom.tag_name(option) != Some("option") {
                    continue;
                }
                let text = dom.text_content(option).trim().to_string();
                let value = dom
                    .attr(option, "value")
                    .map(str::to_owned)
                    .unwrap_or_else(|| text.clone());
                if dom.attr(option, "selected").is_some() {
                    selected = Some(options.len());
                }
                options.push((text, value));
            }
            if options.is_empty() {
                return None;
            }
            let value = options[selected.unwrap_or(0)].1.clone();
            return Some(Field {
                name,
                default_value: value.clone(),
                value,
                checked: false,
                default_checked: false,
                label,
                kind: FieldKind::Select(options),
                number: None,
                live_node: live_node(dom, id),
            });
        }
        _ => return None,
    };
    let number = matches!(&kind, FieldKind::Number).then(|| {
        let step_raw = dom.attr(id, "step").unwrap_or("");
        let step_any = step_raw.eq_ignore_ascii_case("any");
        crate::doc::NumberConstraints {
            min: dom.attr(id, "min").map(str::to_string),
            max: dom.attr(id, "max").map(str::to_string),
            step: dom.attr(id, "step").map(str::to_string),
            step_any,
            value_base: dom.attr(id, "value").map(str::to_string),
            mutable: dom.input_mutable(id),
            spin_buttons: dom.input_spin_buttons(id),
            editing: dom.input_editing_value(id).map(str::to_string),
        }
    });
    Some(Field {
        name,
        default_value: if tag == "input" {
            dom.attr(id, "value").unwrap_or("").to_string()
        } else {
            value.clone()
        },
        value,
        checked,
        default_checked: checked,
        label,
        kind,
        number,
        live_node: live_node(dom, id),
    })
}

#[cfg(test)]
#[path = "fetch_metadata_tests.rs"]
mod fetch_metadata_tests;

#[cfg(test)]
#[path = "resource_timing_tests.rs"]
mod resource_timing_tests;

#[cfg(test)]
mod tests {
    use super::*;

    async fn idle_connection_pair() -> (BufReader<Conn>, tokio::net::TcpStream) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (client, server) = tokio::join!(
            tokio::net::TcpStream::connect(listener.local_addr().unwrap()),
            listener.accept(),
        );
        (
            BufReader::new(Conn::Plain(client.unwrap())),
            server.unwrap().0,
        )
    }

    async fn assert_idle_peer_closed(peer: &mut tokio::net::TcpStream) {
        use tokio::io::AsyncReadExt;
        let mut byte = [0];
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), peer.read(&mut byte))
                .await
                .expect("idle connection closed promptly")
                .unwrap(),
            0,
        );
    }

    #[tokio::test]
    async fn idle_connection_pool_prunes_unvisited_origins_at_the_expiry_boundary() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let mut pool = ConnectionPool::default();
        let now = Instant::now();
        let expired = ("http".into(), "old.example".into(), 80);
        let recent = ("http".into(), "recent.example".into(), 80);
        let (old_io, mut old_peer) = idle_connection_pair().await;
        let (recent_io, mut recent_peer) = idle_connection_pair().await;
        pool.put(expired.clone(), old_io, now - POOL_IDLE_TTL);
        pool.put(recent.clone(), recent_io, now - Duration::from_secs(1));
        assert_eq!(pool.idle.len(), 2);
        // This is the same sweep used by navigation, including navigation to a
        // non-HTTP document. No request to old.example is needed to expire it.
        pool.prune(now);
        assert!(!pool.idle.contains_key(&expired));
        assert_idle_peer_closed(&mut old_peer).await;
        let mut reused = pool.get(&recent, now).unwrap();
        assert!(
            pool.idle.is_empty(),
            "taking the last socket removes its key"
        );
        recent_peer.write_all(b"usable").await.unwrap();
        let mut bytes = [0; 6];
        reused.read_exact(&mut bytes).await.unwrap();
        assert_eq!(&bytes, b"usable");
    }

    #[tokio::test]
    async fn idle_connection_pool_bounds_all_origins_and_prefers_recent_sockets() {
        let mut pool = ConnectionPool::default();
        let now = Instant::now();
        let mut peers = Vec::new();
        for index in 0..=POOL_MAX_IDLE {
            let (io, peer) = idle_connection_pair().await;
            pool.put(
                ("http".into(), format!("host-{index}.example"), 80),
                io,
                now + Duration::from_millis(index as u64),
            );
            peers.push(peer);
        }
        assert_eq!(pool.idle.len(), POOL_MAX_IDLE);
        assert!(
            !pool
                .idle
                .contains_key(&("http".into(), "host-0.example".into(), 80))
        );
        assert_idle_peer_closed(&mut peers[0]).await;

        let key = ("https".into(), "one.example".into(), 443);
        let mut same_origin_peers = Vec::new();
        for index in 0..=POOL_MAX_IDLE_PER_KEY {
            let (io, peer) = idle_connection_pair().await;
            pool.put(
                key.clone(),
                io,
                now + Duration::from_secs(1) + Duration::from_millis(index as u64),
            );
            same_origin_peers.push(peer);
        }
        assert_eq!(pool.idle[&key].len(), POOL_MAX_IDLE_PER_KEY);
        assert_eq!(
            pool.idle.values().map(Vec::len).sum::<usize>(),
            POOL_MAX_IDLE
        );
        assert_idle_peer_closed(&mut same_origin_peers[0]).await;
        let newest = pool.get(&key, now + Duration::from_secs(2)).unwrap();
        drop(newest);
        assert_idle_peer_closed(same_origin_peers.last_mut().unwrap()).await;
    }

    #[tokio::test]
    async fn idle_connection_pool_access_sweeps_other_origins() {
        let mut pool = ConnectionPool::default();
        let now = Instant::now();
        let old_key = ("http".into(), "old.example".into(), 80);
        let missing_key = ("http".into(), "new.example".into(), 80);
        let (io, mut peer) = idle_connection_pair().await;
        pool.put(old_key.clone(), io, now - POOL_IDLE_TTL);
        assert!(pool.get(&missing_key, now).is_none());
        assert!(pool.idle.is_empty());
        assert_idle_peer_closed(&mut peer).await;
        let (io, mut peer) = idle_connection_pair().await;
        pool.put(old_key, io, now - POOL_IDLE_TTL);
        let (io, _peer) = idle_connection_pair().await;
        pool.put(missing_key, io, now);
        assert_eq!(pool.idle.len(), 1);
        assert_idle_peer_closed(&mut peer).await;
    }

    #[tokio::test]
    async fn idle_connection_pool_is_swept_by_navigation_without_an_http_fetch() {
        let (io, mut peer) = idle_connection_pair().await;
        let key = (
            "http".into(),
            "navigation-pool-test.invalid".into(),
            peer.local_addr().unwrap().port(),
        );
        POOL.lock()
            .unwrap()
            .put(key.clone(), io, Instant::now() - POOL_IDLE_TTL);
        let mut browser = crate::core::BrowserController::new(
            tokio::runtime::Handle::current(),
            || {},
            crate::core::CssSize::new(800.0, 600.0),
        );
        browser.open_internal_gemtext("about:help", b"# Help\n".to_vec());
        assert!(!POOL.lock().unwrap().idle.contains_key(&key));
        assert_idle_peer_closed(&mut peer).await;
    }

    #[tokio::test]
    async fn idle_connection_pool_eviction_sends_tls_close_notify_outside_runtime_context() {
        use std::sync::Arc;
        use tokio_rustls::{TlsAcceptor, TlsConnector, rustls};

        let signed = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let mut roots = rustls::RootCertStore::empty();
        roots.add(signed.cert.der().clone()).unwrap();
        for version in [&rustls::version::TLS12, &rustls::version::TLS13] {
            let key =
                rustls::pki_types::PrivateKeyDer::try_from(signed.signing_key.serialize_der())
                    .unwrap();
            let server_config = rustls::ServerConfig::builder_with_protocol_versions(&[version])
                .with_no_client_auth()
                .with_single_cert(vec![signed.cert.der().clone()], key)
                .unwrap();
            let client_config = rustls::ClientConfig::builder_with_protocol_versions(&[version])
                .with_root_certificates(roots.clone())
                .with_no_client_auth();
            let acceptor = TlsAcceptor::from(Arc::new(server_config));
            let connector = TlsConnector::from(Arc::new(client_config));
            let (client, server) = idle_connection_pair().await;
            let Conn::Plain(client) = client.into_inner() else {
                unreachable!()
            };
            let (client, server) = tokio::time::timeout(Duration::from_secs(5), async {
                tokio::join!(
                    connector.connect(
                        rustls::pki_types::ServerName::try_from("localhost").unwrap(),
                        client
                    ),
                    acceptor.accept(server),
                )
            })
            .await
            .unwrap();
            let mut server = server.unwrap();
            let mut pool = ConnectionPool::default();
            let now = Instant::now();
            pool.put(
                ("https".into(), "localhost".into(), 443),
                BufReader::new(Conn::Tls(Box::new(client.unwrap()))),
                now - POOL_IDLE_TTL,
            );
            // The winit event thread has no entered Tokio context. Pool entries
            // carry the runtime that owns their transport for a pending flush.
            std::thread::spawn(move || pool.prune(now)).join().unwrap();
            let mut byte = [0];
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(2), server.read(&mut byte))
                    .await
                    .unwrap()
                    .expect("TLS closure must include close_notify"),
                0,
            );
        }
    }

    #[test]
    fn cached_module_speculation_is_once_per_response_entry() {
        let cache = PageCache::default();
        let url = Url::parse("https://example.test/module.js").unwrap();
        assert!(!cache.claim_module_import_scan(&url));
        assert!(cache.peek(&url).is_none(), "a claim cannot create a fetch");
        cache.seed(
            url.to_string(),
            200,
            "text/javascript".into(),
            b"export const x=1".to_vec(),
        );
        let winners = std::sync::atomic::AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    if cache.claim_module_import_scan(&url) {
                        winners.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                });
            }
        });
        assert_eq!(winners.load(std::sync::atomic::Ordering::Relaxed), 1);
        for _ in 0..3 {
            assert!(!cache.claim_module_import_scan(&url));
            let response = PageCache::block_on_fetch(None, cache.peek(&url).unwrap()).unwrap();
            assert_eq!(
                response.body, b"export const x=1",
                "required consumers retain the source"
            );
        }
        cache.seed(
            url.to_string(),
            200,
            "text/javascript".into(),
            b"export const x=2".to_vec(),
        );
        assert!(
            cache.claim_module_import_scan(&url),
            "a new response needs discovery"
        );
        cache.cancel();
        assert!(!cache.claim_module_import_scan(&url));
    }

    #[test]
    fn file_links_resolve_like_other_url_references() {
        let base = Url::parse("file:///tmp/site/index.html").unwrap();
        assert!(matches!(
            resolve(&base, "../assets/icon.png"),
            Link::Http(url) if url.as_str() == "file:///tmp/assets/icon.png"
        ));
        assert!(matches!(
            resolve(&base, "file:///tmp/other.html#section"),
            Link::Http(url) if url.scheme() == "file" && url.fragment() == Some("section")
        ));
    }

    #[tokio::test]
    async fn file_stylesheet_base_imports_and_images_keep_the_real_document_client() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("assets")).unwrap();
        std::fs::write(
            dir.path().join("assets/main.css"),
            "@import 'more.css'; .main { color: red }",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("assets/more.css"),
            ".imported { background-image: url('icon.png') }",
        )
        .unwrap();
        let page = Url::from_file_path(dir.path().join("index.html")).unwrap();
        let sheets = fetch_page_sheets(
            r#"<base href="assets/"><link rel="stylesheet" href="main.css">"#,
            &page,
        )
        .await;
        assert_eq!(sheets.len(), 1);
        assert!(sheets[0].1.contains(".imported"));
        assert!(sheets[0].1.contains(".main"));
        assert!(
            sheets[0]
                .1
                .contains(page.join("assets/icon.png").unwrap().as_str())
        );

        let remote = Url::parse("https://example.test/").unwrap();
        let spoofed_base = format!(
            r#"<base href="{}"><link rel="stylesheet" href="main.css">"#,
            page.join("assets/").unwrap()
        );
        assert!(fetch_page_sheets(&spoofed_base, &remote).await.is_empty());
        assert!(resolve_frame_src(page.as_str(), &page, &remote, &[]).is_none());
        assert!(
            fetch_graphical_image(&remote, page.as_str(), None)
                .await
                .is_err()
        );
    }

    fn refresh_response(url: &str, html: &str) -> Response {
        Response {
            url: Url::parse(url).unwrap(),
            status: 200,
            content_type: String::from("text/html"),
            headers: Vec::new(),
            body: html.as_bytes().to_vec(),
            rendered: None,
            js: None,
            blobs: None,
            live: None,
            declarative_refresh: None,
            challenge: None,
            from_post: false,
            timing: None,
        }
    }

    fn policy_response(url: &str, status: u16, headers: &[(&str, &str)]) -> Response {
        Response {
            url: Url::parse(url).unwrap(),
            status,
            content_type: String::from("text/plain"),
            headers: headers
                .iter()
                .map(|(name, value)| (String::from(*name), String::from(*value)))
                .collect(),
            body: b"ok".to_vec(),
            rendered: None,
            js: None,
            blobs: None,
            live: None,
            declarative_refresh: None,
            challenge: None,
            from_post: false,
            timing: None,
        }
    }

    fn scripted_request(url: &str, mode: RequestMode, credentials: CredentialsMode) -> Request {
        let url = Url::parse(url).unwrap();
        Request {
            method: String::from("GET"),
            url: url.clone(),
            body: None,
            headers: Vec::new(),
            fetch_metadata: None,
            timing_client: None,
            fetch_policy: Some(FetchPolicy {
                origin: Url::parse("https://app.example").unwrap(),
                mode,
                credentials,
            }),
        }
    }

    #[test]
    fn fetch_metadata_element_cors_keeps_internal_nosniff_headers() {
        let page = Url::parse("https://app.example/").unwrap();
        let url = Url::parse("https://cdn.example/a.js").unwrap();
        let request = Request::subresource(
            url.clone(),
            &page,
            "script",
            Some(CredentialsMode::SameOrigin),
        );
        let response = enforce_fetch_policy(
            &request,
            policy_response(
                url.as_str(),
                200,
                &[
                    ("access-control-allow-origin", "*"),
                    ("x-content-type-options", "nosniff"),
                ],
            ),
        )
        .unwrap();
        assert!(!classic_script_response_allowed(
            response.status,
            &response.content_type,
            &response.headers
        ));
        assert!(
            response
                .headers
                .iter()
                .any(|(name, _)| name == "x-content-type-options")
        );
    }

    #[test]
    fn script_fetch_enforces_cors_and_opaque_no_cors_responses() {
        let request = scripted_request(
            "https://api.example/data",
            RequestMode::Cors,
            CredentialsMode::SameOrigin,
        );
        assert!(
            enforce_fetch_policy(
                &request,
                policy_response("https://api.example/data", 200, &[])
            )
            .is_err()
        );
        let allowed = enforce_fetch_policy(
            &request,
            policy_response(
                "https://api.example/data",
                200,
                &[
                    ("access-control-allow-origin", "https://app.example"),
                    ("x-secret", "hidden"),
                ],
            ),
        )
        .unwrap();
        assert_eq!(allowed.status, 200);
        assert_eq!(allowed.body, b"ok");
        assert!(
            !allowed.headers.iter().any(|(name, _)| name == "x-secret"),
            "non-exposed cross-origin response headers stay hidden"
        );

        let credentialed = scripted_request(
            "https://api.example/data",
            RequestMode::Cors,
            CredentialsMode::Include,
        );
        assert!(
            enforce_fetch_policy(
                &credentialed,
                policy_response(
                    "https://api.example/data",
                    200,
                    &[
                        ("access-control-allow-origin", "*"),
                        ("access-control-allow-credentials", "true"),
                    ],
                ),
            )
            .is_err()
        );

        let opaque_request = scripted_request(
            "https://api.example/data",
            RequestMode::NoCors,
            CredentialsMode::Omit,
        );
        let opaque = enforce_fetch_policy(
            &opaque_request,
            policy_response("https://api.example/data", 200, &[]),
        )
        .unwrap();
        assert_eq!(opaque.status, 0);
        assert!(opaque.headers.is_empty());
        assert!(opaque.body.is_empty());
    }

    #[test]
    fn cors_preflight_detects_unsafe_requests_and_validates_permissions() {
        let mut request = scripted_request(
            "https://api.example/update",
            RequestMode::Cors,
            CredentialsMode::Include,
        );
        request.method = String::from("PATCH");
        request
            .headers
            .push((String::from("authorization"), String::from("Bearer x")));
        assert!(cors_preflight_required(&request));
        let names = cors_non_safelisted_header_names(&request);
        assert_eq!(names, vec![String::from("authorization")]);
        let response = policy_response(
            "https://api.example/update",
            204,
            &[
                ("access-control-allow-origin", "https://app.example"),
                ("access-control-allow-credentials", "true"),
                ("access-control-allow-methods", "PATCH"),
                ("access-control-allow-headers", "authorization"),
            ],
        );
        validate_cors_preflight(&response, &request, &names).unwrap();
        assert!(
            validate_cors_preflight(
                &policy_response(
                    "https://api.example/update",
                    204,
                    &[("access-control-allow-origin", "https://app.example")],
                ),
                &request,
                &names,
            )
            .is_err()
        );

        // A safelisted method can still be preflighted for an unsafe header;
        // Fetch does not require AC-Allow-Methods for that case. An explicit
        // Content-Type on a body-less request is likewise author-controlled
        // and must participate in the CORS header-name check.
        let mut get = scripted_request(
            "https://api.example/query",
            RequestMode::Cors,
            CredentialsMode::SameOrigin,
        );
        get.headers.push((
            String::from("content-type"),
            String::from("application/json"),
        ));
        assert!(cors_preflight_required(&get));
        let names = cors_non_safelisted_header_names(&get);
        assert_eq!(names, vec![String::from("content-type")]);
        validate_cors_preflight(
            &policy_response(
                "https://api.example/query",
                204,
                &[
                    ("access-control-allow-origin", "https://app.example"),
                    ("access-control-allow-headers", "content-type"),
                ],
            ),
            &get,
            &names,
        )
        .unwrap();
    }

    #[test]
    fn cors_preflight_excludes_browser_referrer_but_keeps_author_headers() {
        // Fetch §§4.4, 4.6, 4.8: Referer is appended to the HTTP request
        // after the author-header preflight decision, not advertised in ACRH.
        let mut request = scripted_request(
            "https://cdn.example/icon.svg",
            RequestMode::Cors,
            CredentialsMode::SameOrigin,
        );
        set_referrer(
            &mut request,
            &Url::parse("https://app.example/page").unwrap(),
        );
        assert!(!cors_preflight_required(&request));
        assert!(cors_non_safelisted_header_names(&request).is_empty());
        request.headers[0].0 = String::from("rEfErEr");
        assert!(!cors_preflight_required(&request));
        request
            .headers
            .push((String::from("X-Custom"), String::from("yes")));
        assert!(cors_preflight_required(&request));
        assert_eq!(cors_non_safelisted_header_names(&request), ["x-custom"]);
    }

    #[tokio::test]
    async fn script_fetch_with_browser_referrer_gets_svg_without_preflight() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        // Two origins, real HTTP, no permissive preflight handler. Exercise
        // the same browser-owned referrer attachment as the page actor; a
        // header-less Request fixture alone misses this integration defect.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            for _ in 0..3 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut buffer = [0; 2048];
                while !request.windows(4).any(|part| part == b"\r\n\r\n") {
                    let read = socket.read(&mut buffer).await.unwrap();
                    assert_ne!(read, 0);
                    request.extend_from_slice(&buffer[..read]);
                    assert!(request.len() < 16_384);
                }
                let head = String::from_utf8(request).unwrap().to_ascii_lowercase();
                let allowed = !head.starts_with("get /denied.svg ");
                let cors = if allowed {
                    "Access-Control-Allow-Origin: *\r\n"
                } else {
                    ""
                };
                let body = "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 24 24\"><path d=\"M2 2h20v20H2z\"/></svg>";
                socket.write_all(format!(
                    "HTTP/1.1 200 OK\r\n{cors}Content-Type: image/svg+xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()
                ).as_bytes()).await.unwrap();
                requests.push(head);
            }
            requests
        });
        let page = Url::parse("http://127.0.0.1:1/page").unwrap();
        for (path, mode) in [
            ("icon.svg", RequestMode::Cors),
            ("opaque.svg", RequestMode::NoCors),
            ("denied.svg", RequestMode::Cors),
        ] {
            let mut request = scripted_request(
                &format!("http://{address}/{path}"),
                mode,
                CredentialsMode::SameOrigin,
            );
            request.fetch_policy.as_mut().unwrap().origin = page.clone();
            set_referrer(&mut request, &page);
            let result = tokio::time::timeout(Duration::from_secs(5), fetch_script(&request))
                .await
                .unwrap();
            if path == "denied.svg" {
                assert_eq!(result.unwrap_err(), "CORS check failed");
            } else {
                let response = result.expect("browser referrer must not require CORS permission");
                if mode == RequestMode::NoCors {
                    assert_eq!(response.status, 0);
                    assert!(response.body.is_empty());
                } else {
                    assert_eq!(response.status, 200);
                    assert!(response.body.starts_with(b"<svg"));
                    crate::img::decode_graphical(&response.body).expect("fetched SVG rasterizes");
                }
            }
        }
        let requests = tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap();
        for head in requests {
            assert!(head.starts_with("get /"), "unexpected preflight: {head}");
            assert!(head.contains("referer: http://127.0.0.1:1/\r\n"), "{head}");
            assert!(!head.contains("access-control-request-headers:"), "{head}");
        }
    }

    #[test]
    fn supported_image_navigation_uses_the_graphical_image_document_path() {
        let response = Response {
            url: Url::parse("https://example.test/photo.png").unwrap(),
            status: 200,
            content_type: String::from("image/png"),
            headers: Vec::new(),
            body: b"\x89PNG\r\n\x1a\n".to_vec(),
            rendered: None,
            js: None,
            blobs: None,
            live: None,
            declarative_refresh: None,
            challenge: None,
            from_post: false,
            timing: None,
        };
        let response = image_navigation_response(response, "image/png");
        assert_eq!(response.content_type, "text/html; charset=utf-8");
        assert!(
            String::from_utf8(response.body)
                .unwrap()
                .contains("data:image/png;base64,iVBORw0KGgo=")
        );
    }

    #[tokio::test]
    async fn graphical_image_fetch_resolves_blob_from_the_page_mirror() {
        let page = Url::parse("https://example.test/login").unwrap();
        let blob_url = "blob:https://example.test/qr-code";
        let blobs = crate::js::BlobMap::default();
        blobs.lock().unwrap().insert(
            blob_url.to_owned(),
            (vec![0, 1, 2, 255], "image/png".to_owned()),
        );

        assert_eq!(
            fetch_graphical_image(&page, &format!("{blob_url}#fragment"), Some(&blobs))
                .await
                .unwrap(),
            vec![0, 1, 2, 255]
        );
        assert!(fetch_graphical_image(&page, blob_url, None).await.is_err());
    }

    #[test]
    fn declarative_refresh_parser_follows_the_html_algorithm() {
        assert_eq!(
            parse_declarative_refresh_input(" 12.75 ; URL = '/next?q=1' trailing"),
            Some((Duration::from_secs(12), Some(String::from("/next?q=1"))))
        );
        assert_eq!(
            parse_declarative_refresh_input(".5,relative.html"),
            Some((Duration::ZERO, Some(String::from("relative.html"))))
        );
        assert_eq!(
            parse_declarative_refresh_input("3"),
            Some((Duration::from_secs(3), None))
        );
        assert!(parse_declarative_refresh_input("soon; url=/no").is_none());
        assert!(parse_declarative_refresh_input("1x; url=/no").is_none());
    }

    #[test]
    fn callies_zero_second_meta_refresh_becomes_replacement_navigation() {
        let response = refresh_response(
            "https://callieswebgarden.neocities.org/",
            r#"<head><base href="/garden/"><meta http-equiv="refresh"
                 content="0; url=http://callie.garden"></head><body></body>"#,
        );
        assert_eq!(
            detect_declarative_refresh(
                &response,
                &String::from_utf8_lossy(response.body.as_slice())
            ),
            Some(DeclarativeRefresh {
                delay: Duration::ZERO,
                url: Url::parse("http://callie.garden/").unwrap(),
            })
        );
    }

    #[test]
    fn first_successful_refresh_wins_and_relative_meta_uses_document_base() {
        let response = refresh_response(
            "https://example.test/root/index.html",
            r#"<head><base href="/section/">
                <meta http-equiv=refresh content="invalid">
                <meta http-equiv=REFRESH content="5; URL=next.html">
                <meta http-equiv=refresh content="0; URL=/too-late">
               </head>"#,
        );
        assert_eq!(
            detect_declarative_refresh(
                &response,
                &String::from_utf8_lossy(response.body.as_slice())
            ),
            Some(DeclarativeRefresh {
                delay: Duration::from_secs(5),
                url: Url::parse("https://example.test/section/next.html").unwrap(),
            })
        );
    }

    #[test]
    fn refresh_header_precedes_meta_and_javascript_targets_are_rejected() {
        let mut response = refresh_response(
            "https://example.test/index.html",
            r#"<meta http-equiv=refresh content="1; URL=/meta">"#,
        );
        response
            .headers
            .push((String::from("refresh"), String::from("7; /header")));
        assert_eq!(
            detect_declarative_refresh(
                &response,
                &String::from_utf8_lossy(response.body.as_slice())
            ),
            Some(DeclarativeRefresh {
                delay: Duration::from_secs(7),
                url: Url::parse("https://example.test/header").unwrap(),
            })
        );

        response.headers[0].1 = String::from("0; URL=javascript:alert(1)");
        assert_eq!(
            detect_declarative_refresh(
                &response,
                &String::from_utf8_lossy(response.body.as_slice())
            ),
            Some(DeclarativeRefresh {
                delay: Duration::from_secs(1),
                url: Url::parse("https://example.test/meta").unwrap(),
            }),
            "an invalid header does not set will-declaratively-refresh"
        );
    }

    #[test]
    fn css_imports_are_found_in_order_and_urls_use_sheet_base() {
        let imports = stylesheet_imports(
            "/* lead */ @import url('../fonts/fonts.css');\
             @import \"theme.css\" screen and (min-width: 30em);p{color:red}",
        );
        assert_eq!(imports.len(), 2);
        assert_eq!(imports[0].url, "../fonts/fonts.css");
        assert_eq!(imports[1].url, "theme.css");
        assert_eq!(imports[1].condition, "screen and (min-width: 30em)");

        let base = Url::parse("https://example.test/css/components/main.css").unwrap();
        assert_eq!(
            absolutize_css_urls(
                ".hero{background:url(../../img/hero.png)}\
                 @font-face{src:url('../fonts/site.woff2') format('woff2')}",
                &base,
            ),
            ".hero{background:url(https://example.test/img/hero.png)}\
             @font-face{src:url('https://example.test/css/fonts/site.woff2') format('woff2')}"
        );
    }

    #[test]
    fn stylesheet_url_resolution_obeys_css_token_boundaries() {
        let base = Url::parse("https://cdn.example.test/css/app.css").unwrap();
        let data = "data:image/svg+xml,%3Cg clip-path='url(%23a)'%3E";
        let css = format!(
            ".icon{{background-image:url(\"{data}\")}}\
             /* url(ignored.png) */\
             .label::before{{content:\"url(also-ignored.png)\"}}\
             .asset{{background:url(../img/panel.png)}}\
             .h-5{{height:1.25rem}}.w-5{{width:1.25rem}}"
        );
        let resolved = absolutize_css_urls(&css, &base);
        assert!(resolved.contains(&format!("url(\"{data}\")")));
        assert!(resolved.contains("/* url(ignored.png) */"));
        assert!(resolved.contains("content:\"url(also-ignored.png)\""));
        assert!(resolved.contains("url(https://cdn.example.test/img/panel.png)"));

        // A nested `url(#id)` inside the quoted SVG data must not terminate or
        // corrupt the outer token and swallow all following style rules.
        let mut dom = crate::dom::Dom::parse_document(
            "<head><link rel=stylesheet href=app.css></head>\
             <body><svg id=icon class='icon h-5 w-5' viewBox='0 0 40 40'></svg></body>",
        );
        dom.attach_external_sheets(&[("app.css".into(), resolved)]);
        let icon = dom.get_by_id("icon").unwrap();
        assert_eq!(
            dom.computed_style(icon, "width").as_deref(),
            Some("1.25rem")
        );
        assert_eq!(
            dom.computed_style(icon, "height").as_deref(),
            Some("1.25rem")
        );
    }

    #[test]
    fn font_face_parser_uses_css_family_descriptor_and_ordered_url_sources() {
        let faces = stylesheet_font_faces(
            "@font-face{font-family:'Site Icons';\
             src:local('Site Icons'),url(\"https://cdn.test/icons.woff2\") format('woff2')}\
             @font-face{src:url(no-family.woff2)}",
        );
        assert_eq!(
            faces,
            vec![CssFontFace {
                family: "Site Icons".into(),
                sources: vec!["https://cdn.test/icons.woff2".into()],
            }]
        );

        let faces = stylesheet_font_faces(
            "@font-face{font-family:Site;src:url(site.eot?#iefix) format('embedded-opentype'),\
             url(site.woff2) format('woff2'),url(site.ttf) format('truetype')}",
        );
        assert_eq!(
            faces[0].sources,
            vec!["site.eot?#iefix", "site.woff2", "site.ttf"]
        );
    }

    #[test]
    fn document_font_faces_include_inline_sheets_and_use_each_css_base() {
        let page = Url::parse("https://example.test/path/page.html").unwrap();
        let html = "<base href='/assets/'><style>\
                    @font-face{font-family:Inline;src:url(fonts/inline.ttf)}\
                    </style>";
        // External sheets have already passed through
        // `expand_stylesheet_imports`, which absolutizes their URL tokens
        // against the stylesheet URL rather than the document base.
        let sheets = vec![(
            "../css/site.css".into(),
            "@font-face{font-family:External;\
             src:url(https://cdn.example.test/fonts/external.woff2)}"
                .into(),
        )];
        let faces = document_font_faces(html, &sheets, &page);
        assert_eq!(faces.len(), 2);
        assert_eq!(faces[0].0, "Inline");
        assert_eq!(
            faces[0].1[0].as_str(),
            "https://example.test/assets/fonts/inline.ttf"
        );
        assert_eq!(faces[1].0, "External");
        assert_eq!(
            faces[1].1[0].as_str(),
            "https://cdn.example.test/fonts/external.woff2"
        );
    }

    #[test]
    fn discovers_css_background_image_urls_for_eager_fetch() {
        let base = Url::parse("https://example.test/path/").unwrap();
        let document = parse(
            &base,
            "text/html",
            br#"<html style="background-image:url('/images/strawbackground.webp')"><body><ul style="list-style-image:url('/images/HeartDot.png')"><li>x</li></ul></body></html>"#,
            80,
            24,
            &Default::default(),
        );
        assert!(
            document
                .image_urls
                .contains(&"https://example.test/images/strawbackground.webp".to_string())
        );
        assert!(
            document
                .image_urls
                .contains(&"https://example.test/images/HeartDot.png".to_string())
        );
    }

    #[test]
    fn desktop_css_images_follow_boxes_and_reappear_after_display_changes() {
        let base = Url::parse("https://images.example/").unwrap();
        let mut dom = crate::dom::Dom::parse_document(
            r#"<style>
              #hidden { display:none; background-image:url(hidden.png) }
              #hidden span { background-image:url(child.png) }
              #contents { display:contents; cursor:url(cursor.png),pointer; list-style-image:url(marker.png) }
              #offscreen { position:absolute; top:2000px; background-image:url(offscreen.png) }
            </style><div id=hidden><span>hidden</span><img src=html.png></div>
            <ul id=contents><li>visible item</li></ul><div id=offscreen>offscreen</div>"#,
        );
        let render = |dom: &crate::dom::Dom| {
            render_arena(
                dom,
                &base,
                crate::layout2::Viewport::new(800.0, 600.0),
                1.0,
                None,
                &Default::default(),
            )
        };
        let first = render(&dom);
        for file in ["html.png", "cursor.png", "marker.png", "offscreen.png"] {
            assert!(
                first
                    .image_urls
                    .contains(&base.join(file).unwrap().to_string()),
                "{file}"
            );
        }
        for file in ["hidden.png", "child.png"] {
            assert!(
                !first
                    .image_urls
                    .contains(&base.join(file).unwrap().to_string()),
                "{file}"
            );
        }
        dom.set_attr(dom.get_by_id("hidden").unwrap(), "style", "display:block");
        let shown = render(&dom);
        for file in ["hidden.png", "child.png"] {
            assert!(
                shown
                    .image_urls
                    .contains(&base.join(file).unwrap().to_string()),
                "{file}"
            );
        }
    }

    #[test]
    fn graphical_image_discovery_is_document_ordered_resolved_and_deduplicated() {
        let base = Url::parse("https://www.example.test/gallery/index.html").unwrap();
        let dom = crate::dom::Dom::parse_document(
            r#"<img src="//cdn.example.test/header.png">
                <img src="thumbs/one.jpg">
                <img src="//cdn.example.test/header.png">
                <video poster="/poster.webp"></video>"#,
        );

        assert_eq!(
            collect_image_urls(
                &dom,
                &base,
                crate::layout2::Viewport::new(800.0, 600.0),
                1.0,
            )
            .all,
            vec![
                String::from("https://cdn.example.test/header.png"),
                String::from("https://www.example.test/gallery/thumbs/one.jpg"),
                String::from("https://www.example.test/poster.webp"),
            ]
        );
    }

    #[test]
    fn canonical_render_discovers_shadow_controls_images_and_flat_ancestry() {
        // CSS Shadow 1 §4.1 makes the flattened tree the input to box
        // construction. The typed presentation metadata must use the same
        // tree or web-component search fields and resources disappear even
        // though layout paints their surrounding component.
        let base = Url::parse("https://www.example.test/app/").unwrap();
        let mut dom =
            crate::dom::Dom::parse_document("<body><search-box id=host></search-box></body>");
        let host = dom.get_by_id("host").unwrap();
        let shadow = dom.attach_shadow(host);
        let panel = dom.create_element("div");
        dom.set_attr(
            panel,
            "style",
            "--panel-art:url('panel.png');background-image:var(--panel-art)",
        );
        dom.append(shadow, panel);
        let input = dom.create_element("input");
        dom.set_attr(input, "name", "q");
        dom.set_attr(input, "placeholder", "Search");
        dom.append(panel, input);
        let image = dom.create_element("img");
        dom.set_attr(image, "src", "glass.png");
        dom.set_attr(image, "width", "16");
        dom.set_attr(image, "height", "16");
        dom.append(panel, image);

        let rendered = render_arena(
            &dom,
            &base,
            crate::layout2::Viewport::new(800.0, 600.0),
            1.0,
            None,
            &Default::default(),
        );
        assert!(
            rendered.controls.contains_key(&input),
            "shadow input is editable"
        );
        assert!(
            rendered
                .image_urls
                .contains(&"https://www.example.test/app/glass.png".into())
        );
        assert!(
            rendered
                .image_urls
                .contains(&"https://www.example.test/app/panel.png".into())
        );
        assert_eq!(rendered.parents.get(&input), Some(&panel));
        assert_eq!(rendered.parents.get(&panel), Some(&host));
        assert!(rendered.semantics.nodes.iter().any(|node| {
            node.dom_node == Some(input) && node.role == crate::accessibility::Role::TextInput
        }));
    }

    #[test]
    fn graphical_discovery_selects_responsive_candidate_and_defers_lazy_image() {
        let base = Url::parse("https://www.example.test/gallery/").unwrap();
        let dom = crate::dom::Dom::parse_document(
            r#"<img loading="lazy" src="giant-3840.webp"
                 srcset="small-640.webp 640w, medium-960.webp 960w, giant-3840.webp 3840w"
                 sizes="100vw" width="800" height="450">
               <video poster="poster.webp"></video>"#,
        );
        let images = collect_image_urls(
            &dom,
            &base,
            crate::layout2::Viewport::new(800.0, 600.0),
            1.0,
        );
        assert_eq!(
            images.all,
            vec![
                String::from("https://www.example.test/gallery/medium-960.webp"),
                String::from("https://www.example.test/gallery/poster.webp"),
            ]
        );
        assert_eq!(
            images.eager,
            vec![String::from("https://www.example.test/gallery/poster.webp")]
        );
        assert!(
            images
                .lazy_handles
                .contains(&crate::render::ImageHandle::for_source(
                    "https://www.example.test/gallery/medium-960.webp"
                ))
        );

        let rendered = render_arena(
            &dom,
            &base,
            crate::layout2::Viewport::new(800.0, 600.0),
            1.0,
            None,
            &Default::default(),
        );
        let doc = adapt_rendered_terminal(
            &base,
            "text/html",
            Vec::new(),
            rendered,
            crate::layout2::TerminalViewport::from_font_pixels(100, 38, (8, 16)),
            no_alpha(),
        );
        assert_eq!(
            doc.eager_image_urls,
            vec![String::from("https://www.example.test/gallery/poster.webp")],
            "the terminal commit must not widen the eager set back to all images"
        );
        assert!(doc.deferred_images.iter().any(|image| {
            image.source == "https://www.example.test/gallery/medium-960.webp"
                && image.rect.width > 0.0
                && image.rect.height > 0.0
        }));
    }

    #[test]
    fn graphical_discovery_marks_lazy_images_in_fixed_subtrees_viewport_relative() {
        let base = Url::parse("https://www.example.test/").unwrap();
        let dom = crate::dom::Dom::parse_document(
            r#"<div style="position:fixed;left:0;bottom:0">
                 <img loading="lazy" src="status.webp" width="80" height="20">
               </div>"#,
        );
        let rendered = render_arena(
            &dom,
            &base,
            crate::layout2::Viewport::new(800.0, 600.0),
            1.0,
            None,
            &Default::default(),
        );

        assert!(rendered.deferred_images.iter().any(|image| {
            image.source == "https://www.example.test/status.webp" && image.fixed
        }));
    }

    #[test]
    fn canonical_pixel_layout_drives_graphical_and_terminal_adapters() {
        // CSS Display 3 box-tree generation happens once against the resident
        // DOM. Desktop reads `layout.paint`; terminal quantizes the retained
        // fragments, and both keep the canonical actor node identity without a
        // data-trust-* serialization/reparse bridge.
        let base = Url::parse("https://example.test/page").unwrap();
        let mut dom = crate::dom::Dom::parse_document(
            r#"<body><a href="/next">next</a><input name="q" value="typed"></body>"#,
        );
        let anchor = dom
            .descendants(crate::dom::DOCUMENT)
            .find(|&node| dom.tag_name(node) == Some("a"))
            .unwrap();
        dom.set_doc_url(Some(base.clone()));
        dom.set_viewport_px(640.0, 384.0);
        dom.set_render_clickables([anchor].into_iter().collect(), true);

        let rendered = render_arena(
            &dom,
            &base,
            crate::layout2::Viewport::new(640.0, 384.0),
            1.0,
            None,
            &Default::default(),
        );
        assert!(rendered.direct_actor_nodes);
        assert!(rendered.layout.boxes.contains_key(&anchor));
        assert!(rendered.layout.paint.primitives.iter().any(|primitive| {
            matches!(
                primitive,
                crate::render::DisplayCommand::HitRegion(hit)
                    if hit.node == anchor && hit.actor == Some(anchor)
            )
        }));

        let doc = adapt_rendered_terminal(
            &base,
            "text/html",
            Vec::new(),
            rendered,
            crate::layout2::TerminalViewport::new(80, 24, 8.0, 16.0),
            &Default::default(),
        );
        assert_eq!(doc.hover_ids.get(&anchor), Some(&anchor));
        assert!(doc.rows.iter().flat_map(|row| &row.items).any(|item| {
            matches!(item.link, Some(Link::JsClick { node, .. }) if node == anchor)
        }));
    }

    #[test]
    fn clickable_accessible_names_do_not_paint_in_either_frontend() {
        // AccName #comp_label supplies accessibility metadata. CSS Content 3
        // #content-property / #strings and CSS Pseudo 4 #generated-content
        // still allow authors to explicitly paint an attribute via attr().
        let base = Url::parse("https://example.test/").unwrap();
        let mut dom = crate::dom::Dom::parse_document(
            r#"<style>
                body { margin:0; font:16px/16px sans-serif }
                .dot { display:inline-block; vertical-align:top; width:16px;
                       height:16px; margin-right:16px; border-radius:50%; background:gray }
                #generated::before { content:attr(aria-label) }
                #pseudo { background:none }
                #pseudo::before { content:""; display:block; width:16px;
                                  height:16px; border-radius:50%; background:gray }
            </style><body>
                <div><div id=dot class=dot role=button tabindex=0
                          aria-label="Show slide 1 of 8"></div><span id=pseudo class=dot
                          role=button tabindex=0 aria-label="Show slide 2 of 8"></span><span
                          id=titled class=dot role=button tabindex=0 title="Show slide 3 of 8"></span></div>
                <div id=generated role=button aria-label="Generated label"></div>
                <div id=authored role=button aria-label="Accessible action">Visible action</div>
            </body>"#,
        );
        let names = ["dot", "pseudo", "titled", "generated", "authored"];
        let clickables = names.map(|name| dom.get_by_id(name).unwrap());
        dom.set_doc_url(Some(base.clone()));
        dom.set_viewport_px(640.0, 384.0);
        dom.set_render_clickables(clickables.into_iter().collect(), true);
        let html = dom.serialize_live(crate::dom::DOCUMENT, &clickables.into_iter().collect());
        let mut snapshot = crate::dom::Dom::parse_document(&html);
        snapshot.set_doc_url(Some(base.clone()));
        snapshot.set_viewport_px(640.0, 384.0);

        for arena in [&dom, &snapshot] {
            let rendered = render_arena(
                arena,
                &base,
                crate::layout2::Viewport::new(640.0, 384.0),
                1.0,
                None,
                &Default::default(),
            );
            let glyphs = rendered
                .layout
                .paint
                .primitives
                .iter()
                .filter_map(|command| match command {
                    crate::render::DisplayCommand::GlyphRun { shaped, .. } => {
                        Some(shaped.text.as_str())
                    }
                    _ => None,
                })
                .collect::<String>();
            assert!(!glyphs.contains("Show"), "{glyphs:?}");
            assert!(!glyphs.contains("Accessible action"), "{glyphs:?}");
            assert_eq!(glyphs.matches("Generated label").count(), 1, "{glyphs:?}");
            assert!(glyphs.contains("Visible action"), "{glyphs:?}");
            for (index, name) in names[..3].iter().enumerate() {
                let node = arena.get_by_id(name).unwrap();
                let bounds = rendered.layout.boxes[&node];
                assert_eq!((bounds.width, bounds.height), (16.0, 16.0));
                let semantic = rendered
                    .semantics
                    .nodes
                    .iter()
                    .find(|semantic| semantic.dom_node == Some(node))
                    .unwrap();
                assert_eq!(semantic.name, format!("Show slide {} of 8", index + 1));
                assert_eq!(semantic.role, crate::accessibility::Role::Button);
                assert!(
                    semantic
                        .actions
                        .contains(&crate::accessibility::Action::Activate)
                );
                assert!(
                    rendered.layout.paint.primitives.iter().any(|command| {
                        matches!(command, crate::render::DisplayCommand::HitRegion(hit)
                        if hit.node == node && hit.actor == Some(clickables[index]))
                    }),
                    "missing activation target for {name}"
                );
            }
            let terminal = adapt_rendered_terminal(
                &base,
                "text/html",
                Vec::new(),
                rendered,
                crate::layout2::TerminalViewport::new(80, 24, 8.0, 16.0),
                &Default::default(),
            );
            let text = terminal
                .rows
                .iter()
                .flat_map(|row| &row.items)
                .map(|item| item.text.as_str())
                .collect::<String>();
            assert!(!text.contains("Show"), "{text:?}");
            assert!(!text.contains("Accessible action"), "{text:?}");
            assert_eq!(text.matches("Generated label").count(), 1, "{text:?}");
            assert!(text.contains("Visible action"), "{text:?}");
            for node in &clickables[..3] {
                assert!(terminal.rows.iter().flat_map(|row| &row.items).any(|item| {
                    matches!(item.link, Some(Link::JsClick { node: target, .. }) if target == *node)
                }), "missing terminal activation target for {node}, direct={}",
                    arena.render_live());
            }
        }
    }

    #[test]
    fn direct_live_layout_retains_generated_search_and_svg_icon_handles() {
        let base = Url::parse("https://example.test/").unwrap();
        let mut dom = crate::dom::Dom::parse_document(
            r##"<body><button id=search aria-label=Search></button>
                 <button id=menu aria-label=Options><svg class="svg-fa"><use href="#fa-ellipsis"></use></svg></button>
                 <span id=label aria-label="Open filters"></span></body>"##,
        );
        let search = dom.get_by_id("search").unwrap();
        let menu = dom.get_by_id("menu").unwrap();
        let label = dom.get_by_id("label").unwrap();
        dom.set_render_clickables([search, menu, label].into_iter().collect(), true);

        let rendered = render_arena(
            &dom,
            &base,
            crate::layout2::Viewport::new(640.0, 480.0),
            1.0,
            None,
            &Default::default(),
        );
        let glyphs = rendered
            .layout
            .paint
            .primitives
            .iter()
            .filter_map(|command| match command {
                crate::render::DisplayCommand::GlyphRun { shaped, .. } => {
                    Some(shaped.text.as_str())
                }
                _ => None,
            })
            .collect::<String>();
        assert!(
            glyphs.contains('⌕'),
            "search placeholder paints: {glyphs:?}"
        );
        assert!(
            glyphs.contains('⋯'),
            "SVG menu placeholder paints: {glyphs:?}"
        );
        assert!(
            !glyphs.contains("Open filters"),
            "an accessible name alone is not visual content: {glyphs:?}"
        );
    }

    #[test]
    fn live_form_buttons_keep_authored_content_in_both_frontends() {
        // HTML Rendering §15.5.3: a button's child boxes are its anonymous
        // button content. A live form also commonly contains a contenteditable
        // editor plus several icon buttons; treating those buttons as static
        // form atoms throws their content away and paints one synthetic
        // "Button" label for each of them.
        let base = Url::parse("https://example.test/chat").unwrap();
        let mut dom = crate::dom::Dom::parse_document(
            r#"<body><form style="border:1px solid #888">
                 <div id="editor" contenteditable="true" aria-placeholder="Message"></div>
                 <button id="attach" type="button"><span>Attach</span></button>
                 <button id="send" type="submit"><span>Send</span></button>
               </form></body>"#,
        );
        let attach = dom.get_by_id("attach").unwrap();
        let send = dom.get_by_id("send").unwrap();
        dom.set_render_clickables([attach, send].into_iter().collect(), true);

        let rendered = render_arena(
            &dom,
            &base,
            crate::layout2::Viewport::new(640.0, 480.0),
            1.0,
            None,
            &Default::default(),
        );
        assert!(rendered.controls.contains_key(&attach));
        assert!(rendered.controls.contains_key(&send));
        let glyphs = rendered
            .layout
            .paint
            .primitives
            .iter()
            .filter_map(|command| match command {
                crate::render::DisplayCommand::GlyphRun { shaped, .. } => {
                    Some(shaped.text.as_str())
                }
                _ => None,
            })
            .collect::<String>();
        assert!(
            glyphs.contains("Attach"),
            "authored button content: {glyphs:?}"
        );
        assert!(
            glyphs.contains("Send"),
            "authored button content: {glyphs:?}"
        );
        assert!(
            !glyphs.contains("Button"),
            "synthetic button leaked: {glyphs:?}"
        );

        let terminal = adapt_rendered_terminal(
            &base,
            "text/html",
            Vec::new(),
            rendered,
            crate::layout2::TerminalViewport::new(80, 24, 8.0, 16.0),
            &Default::default(),
        );
        let terminal_text = terminal
            .rows
            .iter()
            .flat_map(|row| row.items.iter())
            .map(|item| item.text.as_str())
            .collect::<String>();
        assert!(terminal_text.contains("Attach"));
        assert!(terminal_text.contains("Send"));
        assert!(!terminal_text.contains("Button"));
    }

    #[test]
    fn graphical_live_chat_controls_have_visible_native_surfaces_and_icon_pixels() {
        // HTML Rendering §15.5 allows native control appearance. The
        // graphical path must provide that appearance independently of the
        // terminal-only bracket affordance. Contenteditable is an ordinary
        // CSS box, not a native textarea widget (CSS UI 4 appearance:auto).
        let base = Url::parse("https://chatgpt.com/").unwrap();
        let mut dom = crate::dom::Dom::parse_document(
            r##"<html><body style="margin:0;background:#000;color:#fff"><form>
                <div id="editor" contenteditable="true" aria-placeholder="Message"></div>
                <button id="send" type="submit" aria-label="Send">
                    <svg viewBox="0 0 24 24"><path fill="currentColor" d="M3 20 21 12 3 4v6l12 2-12 2v6Z"/></svg>
                </button>
            </form></body></html>"##,
        );
        let send = dom.get_by_id("send").unwrap();
        dom.set_render_clickables([send].into_iter().collect(), true);
        let rendered = render_arena(
            &dom,
            &base,
            crate::layout2::Viewport::new(640.0, 480.0),
            1.0,
            None,
            &Default::default(),
        );
        let surfaces = rendered
            .layout
            .paint
            .primitives
            .iter()
            .filter(|command| {
                matches!(
                    command,
                    crate::render::DisplayCommand::Fill {
                        brush: crate::render::PaintBrush::Solid(crate::render::PaintColor::Rgba(
                            31, 34, 38, 255
                        )),
                        ..
                    }
                )
            })
            .count();
        assert_eq!(surfaces, 1, "only the button has a native surface");
        let source = rendered
            .image_urls
            .iter()
            .find(|source| source.starts_with("data:image/svg+xml"))
            .expect("Send SVG enters desktop image discovery");
        let bytes = crate::img::decode_data_url(source).unwrap();
        let markup = String::from_utf8(bytes.clone()).unwrap();
        assert!(
            markup.contains("#fff"),
            "currentColor was not resolved: {markup}"
        );
        let (image, _) = crate::img::decode(&bytes).expect("Send SVG decodes for desktop");
        assert!(
            image.to_rgba8().pixels().any(|pixel| {
                pixel[0] > 220 && pixel[1] > 220 && pixel[2] > 220 && pixel[3] > 0
            })
        );
    }

    #[test]
    fn direct_shadow_svg_emits_an_image_command_before_decode() {
        // SVG 2 §5.1 makes an outermost inline SVG a replaced element in an
        // HTML/CSS layout. A declared box must retain its paint resource while
        // intrinsic image data is pending; URL discovery alone is not paint.
        let base = Url::parse("https://example.test/").unwrap();
        let mut dom = crate::dom::Dom::parse_document("<body><x-logo id=h></x-logo></body>");
        let host = dom.get_by_id("h").unwrap();
        let shadow = dom.attach_shadow(host);
        let svg = dom.create_element("svg");
        dom.set_attr(svg, "width", "24");
        dom.set_attr(svg, "height", "24");
        dom.set_attr(svg, "viewBox", "0 0 24 24");
        let path = dom.create_element("path");
        dom.set_attr(path, "d", "M1 1h22v22H1z");
        dom.append(svg, path);
        dom.append(shadow, svg);

        let rendered = render_arena(
            &dom,
            &base,
            crate::layout2::Viewport::new(640.0, 480.0),
            1.0,
            None,
            &Default::default(),
        );
        let source = rendered
            .image_urls
            .iter()
            .find(|source| source.starts_with("data:image/svg+xml"))
            .expect("shadow SVG is a discovered resource");
        assert!(
            rendered
                .layout
                .paint
                .image_requests
                .iter()
                .any(|request| request.source == *source)
        );
        assert!(rendered.layout.paint.primitives.iter().any(|command| {
            matches!(command, crate::render::DisplayCommand::Image { node, .. } if *node == svg)
        }));
    }

    #[test]
    fn clickable_svg_survives_terminal_decode_transition() {
        // SVG 2 §5.1 gives the outermost inline SVG a replaced-element box.
        // Fetching and decoding that resource is asynchronous, so the
        // terminal adapter must retain the same activation target before and
        // after intrinsic dimensions become available.
        let base = Url::parse("https://example.test/").unwrap();
        let mut dom = crate::dom::Dom::parse_document(
            r##"<body><button id="close" aria-label="Close" style="width:34px;height:34px">
                <svg id="icon" aria-label="Close" width="18" height="18" viewBox="0 0 18 18">
                    <path fill="#fff" d="M2 3.4 3.4 2 9 7.6 14.6 2 16 3.4 10.4 9 16 14.6 14.6 16 9 10.4 3.4 16 2 14.6 7.6 9Z"/>
                </svg>
            </button></body>"##,
        );
        let close = dom.get_by_id("close").unwrap();
        let icon = dom.get_by_id("icon").unwrap();
        dom.set_render_clickables([close].into_iter().collect(), true);
        let viewport = crate::layout2::Viewport::new(640.0, 480.0);
        let terminal = crate::layout2::TerminalViewport::new(80, 24, 8.0, 16.0);

        let pending = render_arena(&dom, &base, viewport, 1.0, None, &Default::default());
        let source = pending
            .image_urls
            .iter()
            .find(|source| source.starts_with("data:image/svg+xml"))
            .cloned()
            .expect("close SVG is discovered before decode");
        let pending = adapt_rendered_terminal(
            &base,
            "text/html",
            Vec::new(),
            pending,
            terminal,
            &Default::default(),
        );
        let pending_item = pending
            .rows
            .iter()
            .flat_map(|row| row.items.iter())
            .find(|item| item.node == icon)
            .expect("pending close SVG retains a terminal item");
        assert_eq!(pending_item.kind, crate::layout2::ItemKind::Image);
        assert_eq!(
            pending_item.link,
            Some(crate::doc::Link::JsClick {
                node: close,
                href: String::new(),
            })
        );

        let bytes = crate::img::decode_data_url(&source).expect("valid close SVG data URL");
        let info = crate::img::info(&bytes).expect("close SVG decodes");
        let mut images = crate::layout2::ImageSizes::new();
        images.insert(source.clone(), (info.width, info.height));
        let decoded = render_arena(&dom, &base, viewport, 1.0, None, &images);
        let decoded = adapt_rendered_terminal(
            &base,
            "text/html",
            Vec::new(),
            decoded,
            terminal,
            &Default::default(),
        );
        let decoded_item = decoded
            .rows
            .iter()
            .flat_map(|row| row.items.iter())
            .find(|item| item.node == icon)
            .expect("decoded close SVG retains a terminal item");
        assert_eq!(decoded_item.image.as_deref(), Some(source.as_str()));
        assert_eq!(decoded_item.link, pending_item.link);
    }

    #[test]
    fn canonical_direct_render_preserves_legacy_presentation_semantics() {
        // Migration oracle: the removed serializer used to materialize shadow
        // content, resolved CSS variables, controls, backgrounds, and empty
        // clickable handles into a second DOM. The canonical path must retain
        // those observable products without retaining that second authority.
        let base = Url::parse("https://example.test/app/").unwrap();
        let mut dom = crate::dom::Dom::parse_document(
            r#"<html><body><x-search id="host"></x-search></body></html>"#,
        );
        dom.set_doc_url(Some(base.clone()));
        dom.set_viewport_px(640.0, 480.0);
        let host = dom.get_by_id("host").unwrap();
        let shadow = dom.attach_shadow(host);
        let style = dom.create_element("style");
        dom.append_text(
            style,
            ":host{--surface:#243447;--art:url('surface.png');display:block;background-color:var(--surface);background-image:var(--art)}",
        );
        dom.append(shadow, style);
        let input = dom.create_element("input");
        dom.set_attr(input, "name", "q");
        dom.set_attr(input, "placeholder", "Search archive");
        dom.append(shadow, input);
        let button = dom.create_element("button");
        dom.set_attr(button, "aria-label", "Search");
        dom.append(shadow, button);
        let clickables = std::collections::HashSet::from([button]);
        dom.set_render_clickables(clickables.clone(), true);

        let direct = render_arena(
            &dom,
            &base,
            crate::layout2::Viewport::new(640.0, 480.0),
            1.0,
            None,
            &Default::default(),
        );
        let snapshot = dom.serialize_live(crate::dom::DOCUMENT, &clickables);
        let mut legacy = crate::dom::Dom::parse_document(&snapshot);
        legacy.set_doc_url(Some(base.clone()));
        legacy.set_viewport_px(640.0, 480.0);
        let serialized = render_arena(
            &legacy,
            &base,
            crate::layout2::Viewport::new(640.0, 480.0),
            1.0,
            None,
            &Default::default(),
        );

        let paint_text = |rendered: &RenderedPage| {
            rendered
                .layout
                .paint
                .primitives
                .iter()
                .filter_map(|command| match command {
                    crate::render::DisplayCommand::GlyphRun { shaped, .. } => {
                        Some(shaped.text.as_str())
                    }
                    _ => None,
                })
                .collect::<String>()
        };
        assert_eq!(direct.forms.len(), serialized.forms.len());
        assert_eq!(direct.forms[0].fields[0].name, "q");
        assert_eq!(direct.image_urls, serialized.image_urls);
        assert!(paint_text(&direct).contains('⌕'));
        assert!(paint_text(&serialized).contains('⌕'));
        let surface = crate::render::PaintColor::Rgba(36, 52, 71, 255);
        for rendered in [&direct, &serialized] {
            assert!(
                rendered
                    .layout
                    .paint
                    .primitives
                    .iter()
                    .any(|command| matches!(
                        command,
                        crate::render::DisplayCommand::Fill {
                            brush: crate::render::PaintBrush::Solid(color),
                            ..
                        } if *color == surface
                    ))
            );
        }
    }

    #[test]
    fn script_and_style_response_metadata_checks_follow_fetch_and_html() {
        let nosniff = vec![(
            "X-Content-Type-Options".to_string(),
            " nosniff, ignored".to_string(),
        )];
        assert!(classic_script_response_allowed(200, "text/plain", &[]));
        assert!(!classic_script_response_allowed(
            200,
            "text/plain",
            &nosniff
        ));
        assert!(classic_script_response_allowed(
            200,
            "Application/JavaScript; charset=utf-8",
            &nosniff
        ));
        assert!(classic_script_response_allowed(
            200,
            "text/javascript1.5",
            &nosniff
        ));
        assert!(!classic_script_response_allowed(
            404,
            "text/javascript",
            &[]
        ));

        // Module scripts never get classic script's historical MIME leniency.
        assert!(module_script_response_allowed(200, "text/javascript"));
        assert!(!module_script_response_allowed(200, "text/plain"));
        assert!(!module_script_response_allowed(404, "text/javascript"));

        assert!(stylesheet_response_allowed(200, "text/plain", &[]));
        assert!(!stylesheet_response_allowed(200, "text/plain", &nosniff));
        assert!(stylesheet_response_allowed(
            200,
            "text/css; charset=utf-8",
            &nosniff
        ));
    }

    /// Find the first laid-out item whose text contains `needle`.
    fn item<'a>(doc: &'a Doc, needle: &str) -> &'a crate::layout2::Item {
        doc.rows
            .iter()
            .flat_map(|r| &r.items)
            .find(|it| it.text.contains(needle))
            .unwrap_or_else(|| panic!("no item containing {needle:?}"))
    }

    /// Whether any laid-out item's text contains `needle`.
    fn has_item(doc: &Doc, needle: &str) -> bool {
        doc.rows
            .iter()
            .flat_map(|r| &r.items)
            .any(|it| it.text.contains(needle))
    }

    #[test]
    fn anchor_rows_maps_id_and_name_to_first_row() {
        let url = parse_url("http://example.com/").unwrap();
        let html = b"<body><h1 id=top>Top</h1>\
            <p>a</p><p>b</p><p>c</p>\
            <section id=mid><p>middle content</p></section>\
            <a name=named></a><p>after named</p>\
            <h2 id=bottom>Bottom</h2><p>tail</p></body>";
        let doc = parse(
            &url,
            "text/html",
            html,
            80,
            24,
            &crate::layout2::ImageSizes::new(),
        );
        let a = &doc.anchor_rows;
        eprintln!("anchor_rows = {a:?}, rows = {}", doc.rows.len());
        // Every named anchor resolved to a row, in ascending document order.
        let top = a.get("top").copied().expect("id=top");
        let mid = a.get("mid").copied().expect("id=mid");
        let named = a.get("named").copied().expect("a name=named");
        let bottom = a.get("bottom").copied().expect("id=bottom");
        assert!(top <= mid, "{top} <= {mid}");
        assert!(mid <= named, "{mid} <= {named}");
        assert!(named <= bottom, "{named} <= {bottom}");
        assert!(bottom < doc.rows.len(), "bottom row in range");
    }

    #[test]
    fn detects_bot_mitigation_challenges_from_headers() {
        let h = |k: &str, v: &str| {
            let mut m = Headers::new();
            m.insert(k.to_string(), v.to_string());
            m
        };
        // AWS WAF (IMDb, Amazon storefronts, …): the challenge action.
        assert_eq!(
            detect_challenge(&h("x-amzn-waf-action", "challenge")).as_deref(),
            Some("AWS WAF (challenge)")
        );
        assert_eq!(
            detect_challenge(&h("x-amzn-waf-action", "captcha")).as_deref(),
            Some("AWS WAF (captcha)")
        );
        // `allow` is the pass-through value — the real page, not a wall.
        assert_eq!(detect_challenge(&h("x-amzn-waf-action", "allow")), None);
        // Cloudflare managed challenge.
        assert_eq!(
            detect_challenge(&h("cf-mitigated", "challenge")).as_deref(),
            Some("Cloudflare (challenge)")
        );
        // An ordinary response is not a wall.
        assert_eq!(detect_challenge(&Headers::new()), None);
    }

    #[tokio::test]
    async fn execute_js_transforms_the_page_and_fetches_scripts() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let mut req = Vec::new();
                let mut buf = [0u8; 2048];
                while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                    match sock.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => req.extend_from_slice(&buf[..n]),
                    }
                }
                let text = String::from_utf8_lossy(&req).into_owned();
                let reply: Vec<u8> = if text.starts_with("GET /page ") {
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nConnection: close\r\n\r\n\
                      <body><div id=out></div><noscript>js is off</noscript>\
                      <script src=\"/app.js\"></script>\
                      <script>document.getElementById('out').appendChild(\
                      document.createTextNode(' + inline'));</script></body>"
                        .to_vec()
                } else if text.starts_with("GET /app.js ") {
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/javascript\r\nConnection: close\r\n\r\n\
                      document.getElementById('out').textContent = 'external ran';"
                        .to_vec()
                } else {
                    b"HTTP/1.1 404 Nope\r\nContent-Length: 0\r\n\r\n".to_vec()
                };
                let _ = sock.write_all(&reply).await;
            }
        });

        let url = parse_url(&format!("http://127.0.0.1:{port}/page")).unwrap();
        let response = fetch(&Request::get(url)).await.unwrap();
        let response = execute_js(response, (80, 24), (8, 16), Default::default()).await;
        let body = String::from_utf8_lossy(&response.body).into_owned();
        assert!(body.contains("external ran + inline"), "{body}");
        assert!(!body.contains("js is off"), "{body}");
        assert!(!body.contains("<script"), "{body}");
        assert_eq!(response.content_type, "text/html; charset=utf-8");
        let outcome = response.js.expect("outcome recorded");
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        server.abort();
    }

    #[tokio::test]
    async fn script_created_external_svg_use_is_loaded_before_canonical_layout() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let mut req = Vec::new();
                    let mut buf = [0u8; 2048];
                    while !req.windows(4).any(|window| window == b"\r\n\r\n") {
                        match sock.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => req.extend_from_slice(&buf[..n]),
                        }
                    }
                    let request = String::from_utf8_lossy(&req);
                    let reply: &[u8] = if request.starts_with("GET /page ") {
                        b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nConnection: close\r\n\r\n\
                          <head><base href='/assets/'></head><body><script>\
                          document.body.innerHTML='<svg width=24 height=24><use href=\"icons.svg#mark\"></use></svg>';\
                          </script></body>"
                    } else if request.starts_with("GET /assets/icons.svg ") {
                        b"HTTP/1.1 200 OK\r\nContent-Type: image/svg+xml\r\nConnection: close\r\n\r\n\
                          <svg xmlns='http://www.w3.org/2000/svg'><symbol id='mark' viewBox='0 0 24 24'>\
                          <path d='M1 1h22v22H1z'/></symbol></svg>"
                    } else {
                        b"HTTP/1.1 404 Nope\r\nContent-Length: 0\r\n\r\n"
                    };
                    let _ = sock.write_all(reply).await;
                });
            }
        });

        let url = parse_url(&format!("http://127.0.0.1:{port}/page")).unwrap();
        let response = fetch(&Request::get(url)).await.unwrap();
        let response = execute_js(response, (80, 24), (8, 16), Default::default()).await;
        let rendered = response
            .rendered
            .expect("actor produced a typed pixel layout");
        assert!(
            rendered
                .image_urls
                .iter()
                .any(|source| source.starts_with("data:image/svg+xml")),
            "the actor resolved the script-created external symbol into its canonical image list"
        );
        server.abort();
    }

    #[tokio::test]
    async fn execute_js_enforces_status_nosniff_and_module_mime_metadata() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let mut req = Vec::new();
                let mut buf = [0u8; 2048];
                while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                    match sock.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => req.extend_from_slice(&buf[..n]),
                    }
                }
                let text = String::from_utf8_lossy(&req).into_owned();
                let reply = if text.starts_with("GET /page ") {
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nConnection: close\r\n\r\n\
                     <body><div id=out>start</div>\
                     <script src=/nosniff.js></script>\
                     <script src=/legacy.js></script>\
                     <script src=/gone.js></script>\
                     <script type=module src=/wrong-module.js></script>\
                     <script>document.getElementById('out').textContent += '-inline';</script></body>"
                        .to_string()
                } else if text.starts_with("GET /nosniff.js ") {
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n\
                     document.getElementById('out').textContent='BAD-NOSNIFF';"
                        .to_string()
                } else if text.starts_with("GET /legacy.js ") {
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n\
                     document.getElementById('out').textContent += '-legacy';"
                        .to_string()
                } else if text.starts_with("GET /gone.js ") {
                    "HTTP/1.1 404 Not Found\r\nContent-Type: text/javascript\r\nConnection: close\r\n\r\n\
                     document.getElementById('out').textContent='BAD-STATUS';"
                        .to_string()
                } else if text.starts_with("GET /wrong-module.js ") {
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n\
                     document.getElementById('out').textContent='BAD-MODULE-MIME';"
                        .to_string()
                } else {
                    "HTTP/1.1 404 Nope\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        .to_string()
                };
                let _ = sock.write_all(reply.as_bytes()).await;
            }
        });

        let url = parse_url(&format!("http://127.0.0.1:{port}/page")).unwrap();
        let response = fetch(&Request::get(url)).await.unwrap();
        let response = execute_js(response, (80, 24), (8, 16), Default::default()).await;
        let body = String::from_utf8_lossy(&response.body).into_owned();
        assert!(body.contains("start-legacy-inline"), "{body}");
        assert!(
            !body.contains("BAD-"),
            "forbidden response body ran: {body}"
        );
        let outcome = response.js.as_ref().expect("js ran");
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert_eq!(outcome.modules_skipped, 1);
        server.abort();
    }

    // A relative subresource resolves against the document BASE URL (the first
    // `<base href>`, HTML §4.2.3), NOT the document URL. peer.tube serves
    // `<base href="/client/en-US/">` with a relative `<script src=app.js>`; the
    // real asset lives under the base path, while the document URL's path is an
    // Angular SPA catch-all that answers HTML for any unknown route. Resolving
    // against the document URL fetches HTML and the script dies with
    // "unexpected token '<'"; resolving against the base href runs it.
    #[tokio::test]
    async fn execute_js_resolves_subresources_against_base_href() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let mut req = Vec::new();
                let mut buf = [0u8; 2048];
                while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                    match sock.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => req.extend_from_slice(&buf[..n]),
                    }
                }
                let text = String::from_utf8_lossy(&req).into_owned();
                let reply: Vec<u8> = if text.starts_with("GET /assets/app.js ") {
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/javascript\r\nConnection: close\r\n\r\n\
                      document.getElementById('out').textContent = 'external ran';"
                        .to_vec()
                } else {
                    // The SPA catch-all: ANY other path (including the wrong
                    // /client/app.js a document-URL resolution would request)
                    // answers the index HTML, exactly like peer.tube.
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nConnection: close\r\n\r\n\
                      <html><head><base href=\"/assets/\"></head>\
                      <body><div id=out></div><script src=\"app.js\"></script></body></html>"
                        .to_vec()
                };
                let _ = sock.write_all(&reply).await;
            }
        });

        let url = parse_url(&format!("http://127.0.0.1:{port}/client/page")).unwrap();
        let response = fetch(&Request::get(url)).await.unwrap();
        let response = execute_js(response, (80, 24), (8, 16), Default::default()).await;
        let body = String::from_utf8_lossy(&response.body);
        let outcome = response.js.expect("outcome recorded");
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert!(body.contains("external ran"), "{body}");
        server.abort();
    }

    // A same-origin <iframe src> is fetched and its nested document flows into
    // the page inline (HTML "process the iframe attributes" → "navigate an
    // iframe"). No chrome (no border/scrollbar, no surviving <iframe>); the
    // frame's relative links resolve against ITS url, not the parent's.
    #[tokio::test]
    async fn execute_js_renders_a_same_origin_iframe_src_inline() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let mut req = Vec::new();
                let mut buf = [0u8; 2048];
                while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                    match sock.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => req.extend_from_slice(&buf[..n]),
                    }
                }
                let text = String::from_utf8_lossy(&req).into_owned();
                let reply: Vec<u8> = if text.starts_with("GET /page ") {
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nConnection: close\r\n\r\n\
                      <body><h1>PARENT PAGE</h1><iframe src=\"/inner\"></iframe>\
                      <script>void 0;</script></body>"
                        .to_vec()
                } else if text.starts_with("GET /inner ") {
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nConnection: close\r\n\r\n\
                      <!DOCTYPE html><html><head><title>FRAME TITLE</title></head>\
                      <body><p>INNER FRAME BODY</p><a href=\"deep.html\">go</a></body></html>"
                        .to_vec()
                } else {
                    b"HTTP/1.1 404 Nope\r\nContent-Length: 0\r\n\r\n".to_vec()
                };
                let _ = sock.write_all(&reply).await;
            }
        });

        let url = parse_url(&format!("http://127.0.0.1:{port}/page")).unwrap();
        let response = fetch(&Request::get(url)).await.unwrap();
        let mut response = execute_js(response, (80, 24), (8, 16), Default::default()).await;
        let mut body = String::from_utf8_lossy(&response.body).into_owned();
        // Cross-document navigation runs in parallel and completes as a later task. The parsed
        // parent already contains the iframe's replaced viewport before that navigation realizes,
        // so wait for the CHILD BODY projection rather than merely its outer frame marker.
        if !body.contains("INNER FRAME BODY") {
            let live = response
                .live
                .as_mut()
                .expect("initial frame navigation keeps the page live");
            let deadline = std::time::Instant::now() + Duration::from_secs(15);
            while !body.contains("INNER FRAME BODY") && std::time::Instant::now() < deadline {
                let left = deadline.saturating_duration_since(std::time::Instant::now());
                match tokio::time::timeout(left, live.events.recv()).await {
                    Ok(Some(crate::js::PageEvt::Updated { html, outcome })) => {
                        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
                        body = html;
                    }
                    Ok(Some(_)) => {}
                    Ok(None) | Err(_) => break,
                }
            }
        }
        assert!(
            body.contains("data-trust-frame"),
            "frame wrapper missing: {body}"
        );
        assert!(
            body.contains("INNER FRAME BODY"),
            "frame body missing: {body}"
        );
        assert!(!body.contains("<iframe"), "iframe element survived: {body}");
        assert!(
            body.contains(&format!("http://127.0.0.1:{port}/deep.html")),
            "relative link not resolved against the frame url: {body}"
        );
        assert!(
            !body.contains("FRAME TITLE"),
            "frame head leaked into flow: {body}"
        );
        let outcome = response.js.expect("outcome recorded");
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        server.abort();
    }

    #[tokio::test]
    async fn execute_js_loads_data_and_blob_html_iframe_sources() {
        // HTML's iframe navigation accepts all Fetch URL schemes.  data: and
        // blob: documents are local scheme fetches, not HTTP requests, and
        // must replace the child navigable's initial about:blank document.
        let response = Response {
            url: parse_url("https://example.test/schemes").unwrap(),
            status: 200,
            content_type: String::from("text/html"),
            headers: Vec::new(),
            body: br#"<body><script>
                const dataFrame = document.createElement('iframe');
                dataFrame.src = 'data:text/html;base64,PGJvZHk+PHAgaWQ9ImRhdGEtY2hpbGQiPkRBVEEgRlJBTUUgQk9EWTwvcD48L2JvZHk+';
                document.body.appendChild(dataFrame);
                const blobFrame = document.createElement('iframe');
                const documentBlob = new Blob(['<body><p id="blob-child">BLOB FRAME BODY</p></body>'], {type:'text/html'});
                blobFrame.src = URL.createObjectURL(documentBlob);
                document.body.appendChild(blobFrame);
            </script></body>"#.to_vec(),
            rendered: None,
            js: None,
            blobs: None,
            live: None,
            declarative_refresh: None,
            challenge: None,
            from_post: false,
            timing: None,
        };
        let mut response = execute_js(response, (80, 24), (8, 16), Default::default()).await;
        let mut body = String::from_utf8_lossy(&response.body).into_owned();
        if !body.contains("DATA FRAME BODY") || !body.contains("BLOB FRAME BODY") {
            let live = response
                .live
                .as_mut()
                .expect("local-scheme iframe navigation keeps the page live");
            let deadline = std::time::Instant::now() + Duration::from_secs(15);
            while (!body.contains("DATA FRAME BODY") || !body.contains("BLOB FRAME BODY"))
                && std::time::Instant::now() < deadline
            {
                let left = deadline.saturating_duration_since(std::time::Instant::now());
                match tokio::time::timeout(left, live.events.recv()).await {
                    Ok(Some(crate::js::PageEvt::Updated { html, outcome })) => {
                        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
                        body = html;
                    }
                    Ok(Some(_)) => {}
                    Ok(None) | Err(_) => break,
                }
            }
        }
        assert!(
            body.contains("DATA FRAME BODY"),
            "data iframe missing: {body}"
        );
        assert!(
            body.contains("BLOB FRAME BODY"),
            "blob iframe missing: {body}"
        );
        assert!(!body.contains("<iframe"), "iframe element survived: {body}");
        assert!(
            response
                .js
                .as_ref()
                .is_none_or(|outcome| outcome.errors.is_empty())
        );
    }

    #[tokio::test]
    async fn execute_js_does_not_resurrect_a_detached_iframe_navigation() {
        // HTML destroys a child navigable when its iframe is removed.  The
        // queued attribute-processing task must therefore abort rather than
        // execute a detached srcdoc script against the parent Document.
        let response = Response {
            url: parse_url("https://example.test/detached-frame").unwrap(),
            status: 200,
            content_type: String::from("text/html"),
            headers: Vec::new(),
            body: br#"<body><div id=out>OK</div><script>
                const frame = document.createElement('iframe');
                frame.srcdoc = '<script>parent.document.getElementById("out").textContent="BAD"<\/script>';
                document.body.appendChild(frame);
                frame.remove();
                document.body.dataset.done = 'yes';
            </script></body>"#.to_vec(),
            rendered: None,
            js: None,
            blobs: None,
            live: None,
            declarative_refresh: None,
            challenge: None,
            from_post: false,
            timing: None,
        };
        let response = execute_js(response, (80, 24), (8, 16), Default::default()).await;
        let body = String::from_utf8_lossy(&response.body);
        assert!(
            body.contains(">OK</div>"),
            "detached frame changed parent: {body}"
        );
        assert!(!body.contains("BAD"), "detached frame script ran: {body}");
        assert!(
            body.contains("data-done=\"yes\""),
            "script did not run: {body}"
        );
        assert!(
            response
                .js
                .as_ref()
                .is_none_or(|outcome| outcome.errors.is_empty())
        );
    }

    // A nested browsing context owns its own stylesheet set (HTML iframe
    // navigable + CSS Cascade tree scope).  Its link must still be obtained
    // and applied to the projected frame body; otherwise controls such as
    // reCAPTCHA's checkbox retain only their inline defaults and paint with
    // no visible border or hit-sized box.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn execute_js_applies_a_stylesheet_loaded_inside_an_iframe() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let mut req = Vec::new();
                    let mut buf = [0u8; 2048];
                    while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                        match sock.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => req.extend_from_slice(&buf[..n]),
                        }
                    }
                    let text = String::from_utf8_lossy(&req);
                    let reply: &[u8] = if text.starts_with("GET /page ") {
                        b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nConnection: close\r\n\r\n\
                          <body><div id=host></div><script>\
                          document.body.addEventListener('click',function(){});\
                          document.getElementById('host').innerHTML=\
                          '<iframe src=\"/frame\"></iframe>';</script></body>"
                    } else if text.starts_with("GET /frame ") {
                        b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nConnection: close\r\n\r\n\
                          <html><head><link rel=\"stylesheet\" href=\"/frame.css\"></head>\
                          <body><div class=box>FRAME CONTROL</div></body></html>"
                    } else if text.starts_with("GET /frame.css ") {
                        b"HTTP/1.1 200 OK\r\nContent-Type: text/css\r\nConnection: close\r\n\r\n\
                          .box{display:block;width:160px;height:24px;border:3px solid red}"
                    } else {
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    };
                    let _ = sock.write_all(reply).await;
                });
            }
        });

        let url = parse_url(&format!("http://127.0.0.1:{port}/page")).unwrap();
        let response = fetch(&Request::get(url)).await.unwrap();
        let mut response = execute_js(response, (80, 24), (8, 16), Default::default()).await;
        let mut latest = String::from_utf8_lossy(&response.body).into_owned();
        assert!(
            response.live.is_some(),
            "iframe stylesheet regression needs the live JS path"
        );
        if let Some(mut live) = response.live.take() {
            let deadline = std::time::Instant::now() + Duration::from_secs(15);
            while !latest.contains("width:160px") && std::time::Instant::now() < deadline {
                let left = deadline.saturating_duration_since(std::time::Instant::now());
                match tokio::time::timeout(left, live.events.recv()).await {
                    Ok(Some(crate::js::PageEvt::Updated { html, outcome })) => {
                        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
                        latest = html;
                    }
                    Ok(Some(_)) => {}
                    Ok(None) | Err(_) => break,
                }
            }
        }
        assert!(
            latest.contains("width:160px") && latest.contains("border-top-width:3px"),
            "stylesheet inside iframe was not applied: {latest}"
        );
        server.abort();
    }

    // A SCRIPT-LESS page (the `css_only` path) loads its frames too: `srcdoc`
    // inline, a `src` document fetched + flowed inline (relative link resolved
    // against the frame url), and a frame NESTED inside the fetched document is
    // followed one level deeper. The whole point is parity with the JS pipeline
    // without spinning up the engine.
    #[tokio::test]
    async fn css_only_loads_iframe_src_srcdoc_and_nested_frames() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let mut req = Vec::new();
                let mut buf = [0u8; 2048];
                while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                    match sock.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => req.extend_from_slice(&buf[..n]),
                    }
                }
                let text = String::from_utf8_lossy(&req).into_owned();
                let reply: &[u8] = if text.starts_with("GET /page ") {
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nConnection: close\r\n\r\n\
                      <body><h1>PARENT PAGE</h1>\
                      <iframe srcdoc=\"<p>SRCDOC BODY</p>\"></iframe>\
                      <iframe src=\"/inner\"></iframe>\
                      <iframe src=\"data:text/html;base64,PGJvZHk+PHA+REFUQSBTVEFUSUMgRlJBTUUgQk9EWTwvcD48L2JvZHk+\"></iframe></body>"
                } else if text.starts_with("GET /inner ") {
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nConnection: close\r\n\r\n\
                      <body><p>INNER FRAME BODY</p><a href=\"deep.html\">go</a>\
                      <iframe src=\"/nested\"></iframe></body>"
                } else if text.starts_with("GET /nested ") {
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nConnection: close\r\n\r\n\
                      <body><p>NESTED FRAME BODY</p></body>"
                } else {
                    b"HTTP/1.1 404 Nope\r\nContent-Length: 0\r\n\r\n"
                };
                let _ = sock.write_all(reply).await;
            }
        });

        let url = parse_url(&format!("http://127.0.0.1:{port}/page")).unwrap();
        let response = fetch(&Request::get(url)).await.unwrap();
        // No <script> in the page → the css_only branch.
        let response = execute_js(response, (80, 24), (8, 16), Default::default()).await;
        let body = String::from_utf8_lossy(&response.body);
        assert!(body.contains("SRCDOC BODY"), "srcdoc frame missing: {body}");
        assert!(
            body.contains("INNER FRAME BODY"),
            "src frame missing: {body}"
        );
        assert!(
            body.contains("NESTED FRAME BODY"),
            "nested frame missing: {body}"
        );
        assert!(
            body.contains("DATA STATIC FRAME BODY"),
            "data frame missing: {body}"
        );
        assert!(!body.contains("<iframe"), "iframe element survived: {body}");
        assert!(
            body.contains(&format!("http://127.0.0.1:{port}/deep.html")),
            "relative link not resolved against the frame url: {body}"
        );
        server.abort();
    }

    #[tokio::test]
    async fn css_only_follows_finite_srcdoc_nesting_without_depth_cutoff() {
        // A finite document tree is valid regardless of nesting depth. The
        // queue is iterative, so following this eleven-level tree must not
        // depend on a hidden frame-depth budget.
        let mut nested = String::from("<p>DEEP FRAME BODY</p>");
        for _ in 0..11 {
            let encoded = crate::img::base64_encode(nested.as_bytes());
            nested = format!("<iframe src=\"data:text/html;base64,{encoded}\"></iframe>");
        }
        let response = Response {
            url: parse_url("https://example.test/deep-frames").unwrap(),
            status: 200,
            content_type: String::from("text/html"),
            headers: Vec::new(),
            body: format!("<body>{nested}</body>").into_bytes(),
            rendered: None,
            js: None,
            blobs: None,
            live: None,
            declarative_refresh: None,
            challenge: None,
            from_post: false,
            timing: None,
        };
        let response = execute_js(response, (80, 24), (8, 16), Default::default()).await;
        let body = String::from_utf8_lossy(&response.body);
        assert!(
            body.contains("DEEP FRAME BODY"),
            "deep frame was truncated: {body}"
        );
        assert!(!body.contains("<iframe"), "iframe element survived: {body}");
    }

    #[tokio::test]
    async fn scriptless_css_hover_keeps_a_live_page_for_pointer_restyle() {
        // A stylesheet-only document still has observable user-action state.
        // It must not take the inert css_only shortcut, otherwise `a:hover`
        // can never receive the pointer transition.
        let url = parse_url("https://example.test/").unwrap();
        let response = Response {
            url,
            status: 200,
            content_type: String::from("text/html"),
            headers: Vec::new(),
            body: br#"<html><head><style>
                a { color: #ff6666 }
                a:hover { color: #ff8888 }
            </style></head><body><a href="/post">post</a></body></html>"#
                .to_vec(),
            rendered: None,
            js: None,
            blobs: None,
            live: None,
            declarative_refresh: None,
            challenge: None,
            from_post: false,
            timing: None,
        };
        let mut response = execute_js(response, (80, 24), (8, 16), Default::default()).await;
        let initial = String::from_utf8_lossy(&response.body).into_owned();
        assert!(
            response.live.is_some(),
            "CSS hover page became static: {initial}"
        );
        assert!(
            initial.contains("color:#ff6666"),
            "initial link color: {initial}"
        );
        let mut live = response.live.take().unwrap();
        let marker = initial
            .split("data-trust-hover=\"")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .unwrap()
            .parse::<usize>()
            .unwrap();
        assert!(live.handle.send_hover(Some(marker), 4.0, 8.0));
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut hovered = None;
        while tokio::time::Instant::now() < deadline {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(left, live.events.recv()).await {
                Ok(Some(crate::js::PageEvt::Patched { patches, .. })) => {
                    if patches
                        .iter()
                        .any(|patch| patch.html.contains("color:#ff8888"))
                    {
                        hovered = Some(true);
                        break;
                    }
                }
                Ok(Some(crate::js::PageEvt::Updated { html, .. })) => {
                    if html.contains("color:#ff8888") {
                        hovered = Some(true);
                        break;
                    }
                }
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => break,
            }
        }
        assert_eq!(
            hovered,
            Some(true),
            "hover color never reached the live page"
        );
    }

    // Parallel parse (Step 5a): TWO external classic scripts trip the parse
    // pool (raw-parsed off the page thread), and they interact across the
    // boundary — `b.js` reads a global set by `a.js` and the inline script runs
    // too. The whole point is that this is byte-identical to sequential
    // execution: scripts must still compile + run in document order, so the
    // cross-script global resolves and every mutation lands.
    #[tokio::test]
    async fn execute_js_runs_parallel_parsed_scripts_in_order() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let mut req = Vec::new();
                let mut buf = [0u8; 2048];
                while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                    match sock.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => req.extend_from_slice(&buf[..n]),
                    }
                }
                let text = String::from_utf8_lossy(&req).into_owned();
                let reply: Vec<u8> = if text.starts_with("GET /page ") {
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nConnection: close\r\n\r\n\
                      <body><div id=out></div>\
                      <script src=\"/a.js\"></script>\
                      <script src=\"/b.js\"></script>\
                      <script>document.getElementById('out')\
                      .setAttribute('data-inline', 'I');</script></body>"
                        .to_vec()
                } else if text.starts_with("GET /a.js ") {
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/javascript\r\nConnection: close\r\n\r\n\
                      window.SHARED = 40;\
                      document.getElementById('out').setAttribute('data-a', 'A');"
                        .to_vec()
                } else if text.starts_with("GET /b.js ") {
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/javascript\r\nConnection: close\r\n\r\n\
                      document.getElementById('out').textContent = 'sum=' + (window.SHARED + 2);"
                        .to_vec()
                } else {
                    b"HTTP/1.1 404 Nope\r\nContent-Length: 0\r\n\r\n".to_vec()
                };
                let _ = sock.write_all(&reply).await;
            }
        });

        let url = parse_url(&format!("http://127.0.0.1:{port}/page")).unwrap();
        let response = fetch(&Request::get(url)).await.unwrap();
        let response = execute_js(response, (80, 24), (8, 16), Default::default()).await;
        let body = String::from_utf8_lossy(&response.body);
        // b.js read a.js's global → it ran AFTER a.js (document order preserved).
        assert!(
            body.contains("sum=42"),
            "cross-script global / order: {body}"
        );
        // a.js's and the inline script's mutations both landed.
        assert!(body.contains("data-a=\"A\""), "a.js mutation: {body}");
        assert!(
            body.contains("data-inline=\"I\""),
            "inline mutation: {body}"
        );
        let outcome = response.js.expect("outcome recorded");
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        server.abort();
    }

    // CDN compile cache (Phase 2): the SAME page (an external IIFE "library"
    // plus an inline script) is loaded TWICE in one process. The first load
    // compiles `/lib.js` and caches its detached image; the second is a cache
    // HIT — the library is rehydrated, not re-parsed/compiled. Both renders must
    // be byte-identical and error-free: the whole point is that reuse is
    // observably transparent. (Proof the hit path is wired and faithful; the
    // backend-specific tests prove the cache mechanics directly.)
    #[tokio::test]
    async fn execute_js_reuses_a_cached_cdn_library_across_two_loads() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let mut req = Vec::new();
                let mut buf = [0u8; 2048];
                while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                    match sock.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => req.extend_from_slice(&buf[..n]),
                    }
                }
                let text = String::from_utf8_lossy(&req).into_owned();
                let reply: Vec<u8> = if text.starts_with("GET /page ") {
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nConnection: close\r\n\r\n\
                      <body><div id=out></div>\
                      <script src=\"/lib.js\"></script>\
                      <script>document.getElementById('out').textContent = \
                      window.__cdnLib('cache');</script></body>"
                        .to_vec()
                } else if text.starts_with("GET /lib.js ") {
                    // A realm-portable IIFE: it installs a global by name only.
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/javascript\r\nConnection: close\r\n\r\n\
                      (function(g){ g.__cdnLib = function(s){ return 'lib:' + s; }; })(window);"
                        .to_vec()
                } else {
                    b"HTTP/1.1 404 Nope\r\nContent-Length: 0\r\n\r\n".to_vec()
                };
                let _ = sock.write_all(&reply).await;
            }
        });

        let load = || async {
            let url = parse_url(&format!("http://127.0.0.1:{port}/page")).unwrap();
            let response = fetch(&Request::get(url)).await.unwrap();
            let response = execute_js(response, (80, 24), (8, 16), Default::default()).await;
            let body = String::from_utf8_lossy(&response.body).into_owned();
            let outcome = response.js.expect("outcome recorded");
            assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
            assert!(body.contains("lib:cache"), "library output missing: {body}");
            body
        };

        // First load compiles + caches `/lib.js`; second is a rehydrate hit.
        let first = load().await;
        let second = load().await;
        assert_eq!(
            first, second,
            "a rehydrated CDN library must render identically to its cold compile"
        );
        server.abort();
    }

    // A socket.io-style app opens its WebSocket DURING page load (from a
    // top-level script), not from a later click. The `PageWs` host must
    // therefore be registered BEFORE scripts run — it used to be wired up only
    // after `load_page` returned, so the first `new WebSocket(...)` hit a
    // missing host, `__ws_open` returned -1, and the socket never opened on that
    // attempt (the page then had no transport for its streamed reply until a
    // framework's reconnect timer fired at rest and retried — an avoidable
    // delay). Here a load-time script opens a socket; the open + first frame
    // must reach the page.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_websocket_opened_during_page_load_connects_and_delivers() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                // Read request headers (through the blank line).
                let mut req = Vec::new();
                let mut buf = [0u8; 2048];
                while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                    match sock.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => req.extend_from_slice(&buf[..n]),
                    }
                }
                let text = String::from_utf8_lossy(&req).into_owned();
                if text.contains("Upgrade: websocket") {
                    // RFC 6455 §4.2.2: the server proves it received the client
                    // handshake by deriving Sec-WebSocket-Accept from the nonce.
                    let key = text
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("Sec-WebSocket-Key")
                                .then(|| value.trim())
                        })
                        .expect("client sends Sec-WebSocket-Key");
                    let accept = crate::ws::websocket_accept(key);
                    // Complete the handshake, push one unmasked text frame "hi",
                    // and hold the socket open.
                    let mut reply = format!(
                        "HTTP/1.1 101 Switching Protocols\r\n\
                             Upgrade: websocket\r\nConnection: Upgrade\r\n\
                             Sec-WebSocket-Accept: {accept}\r\n\r\n"
                    )
                    .into_bytes();
                    reply.extend_from_slice(&[0x81, 0x02, b'h', b'i']);
                    let _ = sock.write_all(&reply).await;
                    // Keep the connection alive so the client doesn't see a drop.
                    let mut sink = [0u8; 256];
                    let _ = sock.read(&mut sink).await;
                } else {
                    // Serve the page: a load-time script opens a WebSocket and a
                    // button keeps the page resident so its events dispatch.
                    let body = format!(
                        "<body><div id=s>pending</div><button>x</button><script>\
                         var ws = new WebSocket('ws://127.0.0.1:{port}/ws');\
                         ws.onopen = function(){{ document.getElementById('s').textContent = 'open'; }};\
                         ws.onmessage = function(e){{ document.getElementById('s').textContent = 'msg:' + e.data; }};\
                         </script></body>"
                    );
                    let reply = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nConnection: close\r\n\r\n{body}"
                    );
                    let _ = sock.write_all(reply.as_bytes()).await;
                }
            }
        });

        let url = parse_url(&format!("http://127.0.0.1:{port}/page")).unwrap();
        let response = fetch(&Request::get(url)).await.unwrap();
        let mut response = execute_js(response, (80, 24), (8, 16), Default::default()).await;
        let live = response
            .live
            .as_mut()
            .expect("page stays live (has a button)");
        // Drain events: the socket opened during load, so its open + the "hi"
        // frame dispatch and mutate the DOM shortly after first paint.
        let mut saw = String::new();
        for _ in 0..8 {
            match tokio::time::timeout(Duration::from_secs(5), live.events.recv()).await {
                Ok(Some(crate::js::PageEvt::Updated { html, .. })) => {
                    saw = html;
                    if saw.contains("msg:hi") {
                        break;
                    }
                }
                Ok(Some(_)) => continue,
                _ => break,
            }
        }
        assert!(
            saw.contains("msg:hi"),
            "the load-time WebSocket must connect and deliver its frame: {saw:?}"
        );
        drop(response.live.take());
        server.abort();
    }

    // A bare hostname opened without a scheme tries https first and falls
    // back to plain http when the TLS connection fails. The server peeks the
    // first byte: 0x16 = a TLS ClientHello (the https attempt) — drop it so
    // the handshake fails fast; 'G' = the http GET retry — serve it.
    #[tokio::test]
    async fn fetch_web_default_falls_back_to_http() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let mut first = [0u8; 1];
                match sock.read(&mut first).await {
                    Ok(1) if first[0] == b'G' => {
                        let mut buf = [0u8; 1024];
                        let _ = sock.read(&mut buf).await; // drain the headers
                        let _ = sock
                            .write_all(
                                b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nConnection: close\r\n\r\n\
                                  <body>plain http served</body>",
                            )
                            .await;
                    }
                    // A TLS ClientHello (or empty read): drop the socket so the
                    // https handshake fails and the caller retries over http.
                    _ => {}
                }
            }
        });

        let https = parse_url(&format!("https://127.0.0.1:{port}/")).unwrap();
        let response = fetch_web_default(&https)
            .await
            .expect("the http fallback served the page");
        assert_eq!(response.url.scheme(), "http", "fell back to http");
        assert!(
            String::from_utf8_lossy(&response.body).contains("plain http served"),
            "got: {}",
            String::from_utf8_lossy(&response.body)
        );
        server.abort();
    }

    // Headline regression for parallel fetch. A page fires N fetches via
    // Promise.all against a server that delays every response. A serial
    // engine (block-at-call-time) costs ~N*delay; a concurrent engine
    // costs ~1*delay. The threshold sits between, so only the parallel
    // engine passes. The eprintln captures the before-number when run on
    // a serial build.
    #[tokio::test]
    async fn page_fetches_run_concurrently() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        const N: usize = 12;
        const DELAY_MS: u64 = 120;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        // Overlap is proven by what the SERVER sees, not by wall-clock: the
        // delayed handler counts how many requests are in flight at once and
        // records the peak. A serial engine never exceeds 1; a concurrent one
        // reaches ~N. This is immune to CPU load (the fixed JS parse/compile
        // overhead used to swamp a wall-clock ratio under a busy test suite
        // and fail spuriously). `elapsed` is kept only as a diagnostic.
        let inflight = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let (srv_inflight, srv_peak) = (inflight.clone(), peak.clone());

        let server = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                // One task per connection: the server must not serialize
                // the responses itself, or it would mask client-side
                // concurrency and the test would prove nothing.
                let (inflight, peak) = (srv_inflight.clone(), srv_peak.clone());
                tokio::spawn(async move {
                    let mut req = Vec::new();
                    let mut buf = [0u8; 2048];
                    while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                        match sock.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => req.extend_from_slice(&buf[..n]),
                        }
                    }
                    let text = String::from_utf8_lossy(&req).into_owned();
                    let reply: Vec<u8> = if text.starts_with("GET /page ") {
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nConnection: close\r\n\r\n\
                             <body><div id=out></div><script>\
                             var urls=[];for(var i=0;i<{N};i++)urls.push('/slow/'+i);\
                             Promise.all(urls.map(function(u){{return fetch(u).then(function(r){{return r.text();}});}}))\
                             .then(function(rs){{document.getElementById('out').textContent='got '+rs.length;}});\
                             </script></body>"
                        )
                        .into_bytes()
                    } else if text.starts_with("GET /slow/") {
                        let cur = inflight.fetch_add(1, Relaxed) + 1;
                        peak.fetch_max(cur, Relaxed);
                        tokio::time::sleep(std::time::Duration::from_millis(DELAY_MS)).await;
                        inflight.fetch_sub(1, Relaxed);
                        b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\nok"
                            .to_vec()
                    } else {
                        b"HTTP/1.1 404 Nope\r\nContent-Length: 0\r\n\r\n".to_vec()
                    };
                    let _ = sock.write_all(&reply).await;
                });
            }
        });

        let url = parse_url(&format!("http://127.0.0.1:{port}/page")).unwrap();
        let response = fetch(&Request::get(url)).await.unwrap();
        let started = std::time::Instant::now();
        let mut response = execute_js(response, (80, 24), (8, 16), Default::default()).await;
        let elapsed = started.elapsed();
        let peak = peak.load(Relaxed);
        eprintln!(
            "page_fetches_run_concurrently: {N} fetches @ {DELAY_MS}ms, peak in-flight {peak}, took {elapsed:?}"
        );
        let mut body = String::from_utf8_lossy(&response.body).into_owned();
        if !body.contains(&format!("got {N}")) {
            let live = response
                .live
                .as_mut()
                .expect("pending fetch group keeps the page live");
            loop {
                match tokio::time::timeout(Duration::from_secs(5), live.events.recv()).await {
                    Ok(Some(crate::js::PageEvt::Updated { html, .. })) => {
                        body = html;
                        if body.contains(&format!("got {N}")) {
                            break;
                        }
                    }
                    other => panic!("expected Promise.all render, got {other:?}"),
                }
            }
        }
        assert!(
            body.contains(&format!("got {N}")),
            "all fetches resolved: {body}"
        );
        assert!(
            peak >= N / 2,
            "fetches did not overlap: peak in-flight {peak} of {N} (serial engine never exceeds 1)"
        );
        server.abort();
    }

    // Response headers reach page JS, and binary bodies stay byte-exact,
    // through BOTH response builders — the one-shot/awaited path
    // (`response_to_array`) and the resident-actor networking-task path
    // (`fetch_result_value`) — plus the XHR
    // `responseType='arraybuffer'` reader. Steam's QR login needs all of it:
    // its WebAPI transport reads the EResult from the `x-eresult` response
    // header and protobuf-decodes the body from `arrayBuffer()`. The
    // background path used to drop the byte-exact element ("Failed to read
    // varint" → no auth session) and only content-type was ever visible.
    // Set-Cookie must stay hidden (a forbidden response-header name — it
    // would leak HttpOnly cookies past the jar). Server on 127.0.0.3: the
    // cookie jar is process-global, so the Set-Cookie this test emits must
    // not share a host with other cookie tests.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn response_headers_and_binary_bodies_reach_page_js() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        const SCRIPT: &str = r##"
            function readAll(tag, cb) {
              fetch('/api').then(function (r) {
                var h = 'h:' + r.headers.get('x-eresult') + ' sc:' + String(r.headers.get('set-cookie'));
                return r.arrayBuffer().then(function (ab) {
                  var x = new XMLHttpRequest();
                  x.open('GET', '/api');
                  x.responseType = 'arraybuffer';
                  x.onload = function () {
                    cb(tag + ' ' + h + ' f:' + Array.from(new Uint8Array(ab)).join(',') +
                       ' xh:' + x.getResponseHeader('x-eresult') +
                       ' x:' + Array.from(new Uint8Array(x.response)).join(','));
                  };
                  x.send();
                });
              });
            }
            readAll('ld', function (s) { document.getElementById('out').textContent = s; });
            document.getElementById('go').addEventListener('click', function () {
              readAll('bg', function (s) { document.getElementById('out').textContent = s; });
            });
        "##;
        let listener = tokio::net::TcpListener::bind("127.0.0.3:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let mut req = Vec::new();
                    let mut buf = [0u8; 2048];
                    while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                        match sock.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => req.extend_from_slice(&buf[..n]),
                        }
                    }
                    let text = String::from_utf8_lossy(&req).into_owned();
                    let reply: Vec<u8> = if text.starts_with("GET /page ") {
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nConnection: close\r\n\r\n\
                             <body><div id=out></div><button id=go>go</button><script>{SCRIPT}</script></body>"
                        )
                        .into_bytes()
                    } else if text.starts_with("GET /api ") {
                        // A protobuf-ish binary body: 0x96/0xFF are not valid
                        // UTF-8 here, so only the byte-exact path preserves
                        // them. X-EResult is the out-of-band API result;
                        // Set-Cookie must NOT surface to JS.
                        let mut r =
                            b"HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\n\
                                      X-EResult: 1\r\nSet-Cookie: secret=1; HttpOnly\r\n\
                                      Content-Length: 4\r\nConnection: close\r\n\r\n"
                                .to_vec();
                        r.extend_from_slice(&[0x08, 0x96, 0x01, 0xFF]);
                        r
                    } else {
                        b"HTTP/1.1 404 Nope\r\nContent-Length: 0\r\n\r\n".to_vec()
                    };
                    let _ = sock.write_all(&reply).await;
                });
            }
        });

        let url = parse_url(&format!("http://127.0.0.3:{port}/page")).unwrap();
        let response = fetch(&Request::get(url)).await.unwrap();
        let mut response = execute_js(response, (80, 24), (8, 16), Default::default()).await;
        let body = String::from_utf8_lossy(&response.body).into_owned();
        let mut live = response
            .live
            .take()
            .expect("a live page (it has a listener)");
        // The XHR's deferred `__finish` is a setTimeout(0), so the actor fires
        // it as a later event-loop task. Drain until the initial request result
        // text shows up.
        let mut latest = body.clone();
        async fn wait_for(live: &mut LivePage, latest: &mut String, needle: &str) {
            let deadline = std::time::Instant::now() + Duration::from_secs(20);
            while !latest.contains(needle) && std::time::Instant::now() < deadline {
                let left = deadline.saturating_duration_since(std::time::Instant::now());
                match tokio::time::timeout(left, live.events.recv()).await {
                    Ok(Some(crate::js::PageEvt::Updated { html, outcome })) => {
                        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
                        *latest = html;
                    }
                    Ok(Some(_)) => {}
                    Ok(None) | Err(_) => break,
                }
            }
        }
        wait_for(&mut live, &mut latest, "ld h:").await;
        assert!(
            latest.contains("ld h:1 sc:null f:8,150,1,255 xh:1 x:8,150,1,255"),
            "load-time fetch/XHR results: {latest}"
        );
        // The live phase: a click dispatch runs the same reads through the
        // BACKGROUND fetch path (the one that dropped the bytes).
        let button = body
            .split("x-trust-js:")
            .nth(1)
            .and_then(|r| r.split(':').next())
            .expect("a clickable marker for the button")
            .parse::<usize>()
            .unwrap();
        live.handle
            .cmds
            .send(crate::js::PageCmd::Click(button))
            .await
            .unwrap();
        wait_for(&mut live, &mut latest, "bg h:").await;
        assert!(
            latest.contains("bg h:1 sc:null f:8,150,1,255 xh:1 x:8,150,1,255"),
            "background fetch/XHR results: {latest}"
        );
        drop(live);
        server.abort();
    }

    // A `<link rel=stylesheet>` INJECTED by page JS (webpack's mini-css chunk
    // loader: rel/href set as properties, appended to <head>) is fetched, fires
    // `load`, and its CSS body JOINS THE LIVE CASCADE (HTML §4.2.4: obtaining
    // the resource adds the sheet to the document's style sheets). A code-split
    // app ships its layout in these chunks — Steam's login QR frame width and
    // checkbox cap live there; dropping the body left late-loaded routes
    // unstyled (giant check-mark SVG, intrinsic-tiny QR).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn injected_stylesheet_joins_the_live_cascade() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        const SCRIPT: &str = r##"
            var l = document.createElement('link');
            l.rel = 'stylesheet';
            l.href = '/chunk.css';
            l.onload = function () {
                document.getElementById('out').textContent = 'css loaded';
            };
            document.head.appendChild(l);
        "##;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let mut req = Vec::new();
                    let mut buf = [0u8; 2048];
                    while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                        match sock.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => req.extend_from_slice(&buf[..n]),
                        }
                    }
                    let text = String::from_utf8_lossy(&req).into_owned();
                    let reply: Vec<u8> = if text.starts_with("GET /page ") {
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nConnection: close\r\n\r\n\
                             <html><head></head><body><div id=out></div>\
                             <div class=wide>styled</div><script>{SCRIPT}</script></body></html>"
                        )
                        .into_bytes()
                    } else if text.starts_with("GET /chunk.css ") {
                        b"HTTP/1.1 200 OK\r\nContent-Type: text/css\r\nConnection: close\r\n\r\n\
                          .wide{width:160px;text-transform:uppercase}"
                            .to_vec()
                    } else {
                        b"HTTP/1.1 404 Nope\r\nContent-Length: 0\r\n\r\n".to_vec()
                    };
                    let _ = sock.write_all(&reply).await;
                });
            }
        });

        let url = parse_url(&format!("http://127.0.0.1:{port}/page")).unwrap();
        let response = fetch(&Request::get(url)).await.unwrap();
        let mut response = execute_js(response, (80, 24), (8, 16), Default::default()).await;
        let mut latest = String::from_utf8_lossy(&response.body).into_owned();
        // The fetch lands via an async job; drain live renders until the chunk
        // CSS is visible in the serialized (baked) output.
        if let Some(mut live) = response.live.take() {
            let deadline = std::time::Instant::now() + Duration::from_secs(15);
            while !(latest.contains("css loaded") && latest.contains("width:160px"))
                && std::time::Instant::now() < deadline
            {
                let left = deadline.saturating_duration_since(std::time::Instant::now());
                match tokio::time::timeout(left, live.events.recv()).await {
                    Ok(Some(crate::js::PageEvt::Updated { html, outcome })) => {
                        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
                        latest = html;
                    }
                    Ok(Some(_)) => {}
                    Ok(None) | Err(_) => break,
                }
            }
        }
        assert!(latest.contains("css loaded"), "link onload fired: {latest}");
        assert!(
            latest.contains("width:160px"),
            "chunk CSS baked into the cascade output: {latest}"
        );
        assert!(
            latest.contains("text-transform:uppercase"),
            "all chunk declarations joined: {latest}"
        );
        server.abort();
    }

    // `WebAssembly.instantiateStreaming(fetch(url))` end to end: the wasm is
    // served as `application/wasm` and its bytes must survive the fetch EXACTLY
    // (`i32.const 200` encodes a 0xC8 LEB byte ≥ 0x80 — a UTF-8-lossy body would
    // corrupt it into a CompileError). Proves the byte-exact `arrayBuffer()` path
    // + the streaming MIME check.
    #[tokio::test]
    async fn wasm_instantiate_streaming_fetches_and_runs() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let wasm =
            wat::parse_str(r#"(module (func (export "f") (result i32) i32.const 200))"#).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let wasm = wasm.clone();
                tokio::spawn(async move {
                    let mut req = Vec::new();
                    let mut buf = [0u8; 2048];
                    while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                        match sock.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => req.extend_from_slice(&buf[..n]),
                        }
                    }
                    let text = String::from_utf8_lossy(&req).into_owned();
                    if text.starts_with("GET /page ") {
                        let html = b"<body><pre id=o>pending</pre><script>\
                            WebAssembly.instantiateStreaming(fetch('/mod.wasm')).then(function(res){\
                                document.getElementById('o').textContent='r='+res.instance.exports.f();\
                            });\
                            </script></body>";
                        let mut reply = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            html.len()
                        )
                        .into_bytes();
                        reply.extend_from_slice(html);
                        let _ = sock.write_all(&reply).await;
                    } else if text.starts_with("GET /mod.wasm ") {
                        let mut reply = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/wasm\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            wasm.len()
                        )
                        .into_bytes();
                        reply.extend_from_slice(&wasm);
                        let _ = sock.write_all(&reply).await;
                    } else {
                        let _ = sock
                            .write_all(b"HTTP/1.1 404 Nope\r\nContent-Length: 0\r\n\r\n")
                            .await;
                    }
                });
            }
        });
        let url = parse_url(&format!("http://127.0.0.1:{port}/page")).unwrap();
        let response = fetch(&Request::get(url)).await.unwrap();
        let mut response = execute_js(response, (80, 24), (8, 16), Default::default()).await;
        let mut body = String::from_utf8_lossy(&response.body).into_owned();
        if !body.contains("r=200") {
            let live = response
                .live
                .as_mut()
                .expect("streaming fetch keeps the page live");
            loop {
                match tokio::time::timeout(Duration::from_secs(5), live.events.recv()).await {
                    Ok(Some(crate::js::PageEvt::Updated { html, .. })) => {
                        body = html;
                        if body.contains("r=200") {
                            break;
                        }
                    }
                    other => panic!("expected instantiateStreaming render, got {other:?}"),
                }
            }
        }
        assert!(body.contains("r=200"), "instantiateStreaming ran: {body}");
        server.abort();
    }

    // A <script src> the page INJECTS at runtime (the SDK-loader idiom — how
    // reCAPTCHA/analytics/embeds load) is fetched and executed, and its `load`
    // event fires for code that waits on `script.onload`. Without this an
    // injected dependency silently never loads (pixiv login's reCAPTCHA hung
    // the submit polling for a `grecaptcha` that never arrived).
    #[tokio::test]
    async fn an_injected_external_script_is_fetched_executed_and_fires_load() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let mut req = Vec::new();
                    let mut buf = [0u8; 2048];
                    while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                        match sock.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => req.extend_from_slice(&buf[..n]),
                        }
                    }
                    let text = String::from_utf8_lossy(&req).into_owned();
                    let reply: Vec<u8> = if text.starts_with("GET /page ") {
                        b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nConnection: close\r\n\r\n\
                          <body><pre id=out>before</pre><script>\
                          var s=document.createElement('script');\
                          s.src='/sdk.js';\
                          s.onload=function(){var o=document.getElementById('out');o.textContent=o.textContent+' loaded';};\
                          document.body.appendChild(s);\
                          </script></body>"
                            .to_vec()
                    } else if text.starts_with("GET /sdk.js ") {
                        b"HTTP/1.1 200 OK\r\nContent-Type: text/javascript\r\nConnection: close\r\n\r\n\
                          document.getElementById('out').textContent='sdk-ran';"
                            .to_vec()
                    } else {
                        b"HTTP/1.1 404 Nope\r\nContent-Length: 0\r\n\r\n".to_vec()
                    };
                    let _ = sock.write_all(&reply).await;
                });
            }
        });
        let url = parse_url(&format!("http://127.0.0.1:{port}/page")).unwrap();
        let response = fetch(&Request::get(url)).await.unwrap();
        let mut response = execute_js(response, (80, 24), (8, 16), Default::default()).await;
        let mut body = String::from_utf8_lossy(&response.body).into_owned();
        if !body.contains("sdk-ran loaded")
            && let Some(live) = response.live.as_mut()
        {
            loop {
                match tokio::time::timeout(Duration::from_secs(5), live.events.recv()).await {
                    Ok(Some(crate::js::PageEvt::Updated { html, .. }))
                    | Ok(Some(crate::js::PageEvt::Static { html, .. })) => {
                        body = html;
                        if body.contains("sdk-ran loaded") {
                            break;
                        }
                    }
                    other => panic!("expected injected script render, got {other:?}"),
                }
            }
        }
        assert!(body.contains("sdk-ran"), "injected script ran: {body}");
        assert!(body.contains("sdk-ran loaded"), "load event fired: {body}");
        assert!(
            response.js.map(|j| j.errors.is_empty()).unwrap_or(true),
            "no JS errors"
        );
        server.abort();
    }

    // A dynamically inserted classic script is its own HTML networking task:
    // it fetches in parallel and executes when ready, rather than borrowing
    // an earlier event-loop task. This mirrors HTML §4.12.1.1's force-async
    // path and protects real SPA bundles whose CDN response takes time.
    #[tokio::test]
    async fn a_slow_dom_ready_injected_script_runs_as_its_own_networking_task() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let mut req = Vec::new();
                    let mut buf = [0u8; 2048];
                    while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                        match sock.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => req.extend_from_slice(&buf[..n]),
                        }
                    }
                    let text = String::from_utf8_lossy(&req).into_owned();
                    if text.starts_with("GET /sdk.js ") {
                        // Deliberately take longer than the historical
                        // one-second dispatch bound.
                        tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
                    }
                    let reply: &[u8] = if text.starts_with("GET /page ") {
                        b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nConnection: close\r\n\r\n\
                          <body><pre id=out>before</pre><button>keep live</button><script>\
                          document.addEventListener('DOMContentLoaded', function(){\
                            var s=document.createElement('script'); s.async=true; s.src='/sdk.js';\
                            s.onload=function(){document.getElementById('out').textContent='loaded';};\
                            document.body.appendChild(s);\
                          });</script></body>"
                    } else if text.starts_with("GET /sdk.js ") {
                        b"HTTP/1.1 200 OK\r\nContent-Type: text/javascript\r\nConnection: close\r\n\r\n\
                          document.getElementById('out').textContent='sdk-ran';"
                    } else {
                        b"HTTP/1.1 404 Nope\r\nContent-Length: 0\r\n\r\n"
                    };
                    let _ = sock.write_all(reply).await;
                });
            }
        });

        let url = parse_url(&format!("http://127.0.0.1:{port}/page")).unwrap();
        let response = fetch(&Request::get(url)).await.unwrap();
        let mut response = execute_js(response, (80, 24), (8, 16), Default::default()).await;
        let shell = String::from_utf8_lossy(&response.body);
        assert!(
            shell.contains(">before<"),
            "the async task must not delay shell paint: {shell}"
        );
        let mut live = response.live.take().expect("button keeps the page live");
        match tokio::time::timeout(std::time::Duration::from_secs(5), live.events.recv()).await {
            Ok(Some(crate::js::PageEvt::Updated { html, outcome })) => {
                assert!(
                    html.contains(">loaded<"),
                    "slow injected script ran: {html}"
                );
                assert!(
                    outcome.errors.is_empty(),
                    "no JS errors: {:?}",
                    outcome.errors
                );
            }
            other => panic!("expected the delayed script's Updated event, got {other:?}"),
        }
        server.abort();
    }

    #[tokio::test]
    async fn an_injected_external_module_is_fetched_evaluated_and_fires_load() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let mut req = Vec::new();
                    let mut buf = [0u8; 2048];
                    while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                        match sock.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => req.extend_from_slice(&buf[..n]),
                        }
                    }
                    let text = String::from_utf8_lossy(&req);
                    let reply: Vec<u8> = if text.starts_with("GET /page ") {
                        b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nConnection: close\r\n\r\n\
                          <body><pre id=out>before</pre><script>\
                          var s=document.createElement('script');\
                          s.type='module'; s.src='/gallery.js';\
                          s.onload=function(){document.body.setAttribute('data-module-load','yes');};\
                          s.onerror=function(){document.body.setAttribute('data-module-load','no');};\
                          document.body.appendChild(s);\
                          </script></body>"
                            .to_vec()
                    } else if text.starts_with("GET /gallery.js ") {
                        b"HTTP/1.1 200 OK\r\nContent-Type: text/javascript\r\nConnection: close\r\n\r\n\
                          document.getElementById('out').textContent='module-ran'; export {};"
                            .to_vec()
                    } else {
                        b"HTTP/1.1 404 Nope\r\nContent-Length: 0\r\n\r\n".to_vec()
                    };
                    let _ = sock.write_all(&reply).await;
                });
            }
        });
        let url = parse_url(&format!("http://127.0.0.1:{port}/page")).unwrap();
        let response = fetch(&Request::get(url)).await.unwrap();
        let mut response = execute_js(response, (80, 24), (8, 16), Default::default()).await;
        let mut body = String::from_utf8_lossy(&response.body).into_owned();
        if !body.contains("module-ran")
            && let Some(live) = response.live.as_mut()
        {
            loop {
                match tokio::time::timeout(Duration::from_secs(5), live.events.recv()).await {
                    Ok(Some(crate::js::PageEvt::Updated { html, .. }))
                    | Ok(Some(crate::js::PageEvt::Static { html, .. })) => {
                        body = html;
                        if body.contains("module-ran") {
                            break;
                        }
                    }
                    other => panic!("expected injected module render, got {other:?}"),
                }
            }
        }
        assert!(body.contains("module-ran"), "external module ran: {body}");
        assert!(
            body.contains(r#"data-module-load="yes""#),
            "module load fired: {body}"
        );
        server.abort();
    }

    // A dynamically injected `<script src>` whose fetch returns a NON-OK status
    // (a 404'd webpack chunk, served — as CDNs do — with an HTML error page)
    // must fire `error`, NOT execute its body. Running the 404 HTML as JS was a
    // spurious `SyntaxError: unexpected token '<'` (crunchyroll's missing
    // "Remote Plugin" chunk). The loader's own onerror is the faithful signal.
    #[tokio::test]
    async fn an_injected_script_that_404s_fires_error_and_does_not_execute() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let mut req = Vec::new();
                    let mut buf = [0u8; 2048];
                    while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                        match sock.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => req.extend_from_slice(&buf[..n]),
                        }
                    }
                    let text = String::from_utf8_lossy(&req).into_owned();
                    let reply: Vec<u8> = if text.starts_with("GET /page ") {
                        b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nConnection: close\r\n\r\n\
                          <body><pre id=out>before</pre><script>\
                          var s=document.createElement('script');\
                          s.src='/chunk.js';\
                          s.onload=function(){var o=document.getElementById('out');o.textContent='LOADED';};\
                          s.onerror=function(){var o=document.getElementById('out');o.textContent='ERRORED';};\
                          document.body.appendChild(s);\
                          </script></body>"
                            .to_vec()
                    } else if text.starts_with("GET /chunk.js ") {
                        // A 404 served as an HTML error page (the real CDN shape).
                        b"HTTP/1.1 404 Not Found\r\nContent-Type: text/html\r\nConnection: close\r\n\r\n\
                          <!doctype html><html><body>Not found</body></html>"
                            .to_vec()
                    } else {
                        b"HTTP/1.1 404 Nope\r\nContent-Length: 0\r\n\r\n".to_vec()
                    };
                    let _ = sock.write_all(&reply).await;
                });
            }
        });
        let url = parse_url(&format!("http://127.0.0.1:{port}/page")).unwrap();
        let response = fetch(&Request::get(url)).await.unwrap();
        let mut response = execute_js(response, (80, 24), (8, 16), Default::default()).await;
        let mut body = String::from_utf8_lossy(&response.body).into_owned();
        if !body.contains("ERRORED")
            && let Some(live) = response.live.as_mut()
        {
            loop {
                match tokio::time::timeout(Duration::from_secs(5), live.events.recv()).await {
                    Ok(Some(crate::js::PageEvt::Updated { html, .. }))
                    | Ok(Some(crate::js::PageEvt::Static { html, .. })) => {
                        body = html;
                        if body.contains("ERRORED") {
                            break;
                        }
                    }
                    other => panic!("expected injected script error render, got {other:?}"),
                }
            }
        }
        assert!(body.contains("ERRORED"), "error event fired: {body}");
        assert!(
            !body.contains("LOADED"),
            "load must not fire on 404: {body}"
        );
        // The 404 HTML body was NOT run as JS, so no SyntaxError lands.
        let errors = response.js.map(|j| j.errors).unwrap_or_default();
        assert!(
            !errors.iter().any(|e| e.contains("SyntaxError")),
            "no SyntaxError from running the 404 page as JS: {errors:?}"
        );
        server.abort();
    }

    // A classic `<script src>` that the parallel prefetch did NOT grab (it sat
    // beyond MAX_PAGE_SCRIPTS, or its prefetch failed) is fetched ON DEMAND when
    // the execution loop reaches it — the prefetch cap is a parallelism lid, not
    // a correctness cliff. A code-split SPA whose chunk count exceeds the lid
    // (crunchyroll ships ~24) still boots. Driven at the `transform` seam with an
    // empty `externals` so the script is reached cold and pulled through the cache.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_unprefetched_classic_script_is_fetched_on_demand() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let mut req = Vec::new();
                    let mut buf = [0u8; 2048];
                    while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                        match sock.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => req.extend_from_slice(&buf[..n]),
                        }
                    }
                    let reply = b"HTTP/1.1 200 OK\r\nContent-Type: text/javascript\r\nConnection: close\r\n\r\n\
                          document.getElementById('o').textContent = 'late-ran';"
                        .to_vec();
                    let _ = sock.write_all(&reply).await;
                });
            }
        });
        let page_url = format!("http://127.0.0.1:{port}/page");
        let html =
            String::from("<body><pre id=o>before</pre><script src=\"/late.js\"></script></body>");
        let mut env = crate::js::PageEnv::bare(&page_url);
        // Deliberately leave `externals` empty: /late.js was never prefetched.
        env.net = Some(tokio::runtime::Handle::current());
        // `transform` blocks on the fetch; keep it off the runtime workers.
        let (out, outcome) = tokio::task::spawn_blocking(move || crate::js::transform(&html, &env))
            .await
            .unwrap();
        assert!(out.contains("late-ran"), "on-demand script ran: {out}");
        assert!(outcome.errors.is_empty(), "no errors: {:?}", outcome.errors);
        server.abort();
    }

    /// The speculative-import-prefetch win: an entry module that STATICALLY
    /// imports many chunks pulls them concurrently (the scanner fires them
    /// ahead of a serial module loader) instead of one-RTT-at-a-time. Mirrors
    /// `page_fetches_run_concurrently` for the module graph.
    #[tokio::test]
    async fn static_module_graph_prefetches_concurrently() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        const N: usize = 8;
        const DELAY_MS: u64 = 120;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        // Prove overlap by the server's peak in-flight count, not wall-clock
        // (the JS overhead swamps a timing ratio under a busy suite — see
        // `page_fetches_run_concurrently`). Serial never exceeds 1.
        let inflight = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let (srv_inflight, srv_peak) = (inflight.clone(), peak.clone());
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                // One task per connection so the server never serializes —
                // otherwise it would mask client concurrency.
                let (inflight, peak) = (srv_inflight.clone(), srv_peak.clone());
                tokio::spawn(async move {
                    let mut req = Vec::new();
                    let mut buf = [0u8; 2048];
                    while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                        match sock.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => req.extend_from_slice(&buf[..n]),
                        }
                    }
                    let text = String::from_utf8_lossy(&req).into_owned();
                    let js = "Content-Type: text/javascript";
                    let reply: Vec<u8> = if text.starts_with("GET /page ") {
                        // The entry (a classic <script src>) statically
                        // imports m0..m{N-1}; their top-level code runs
                        // before the entry body, which reports how many ran.
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nConnection: close\r\n\r\n\
                         <body><div id=out></div>\
                         <script type=module src='/entry.js'></script></body>"
                            .as_bytes()
                            .to_vec()
                    } else if text.starts_with("GET /entry.js ") {
                        let mut imports = String::new();
                        for i in 0..N {
                            imports.push_str(&format!("import '/m{i}.js';\n"));
                        }
                        format!(
                            "HTTP/1.1 200 OK\r\n{js}\r\nConnection: close\r\n\r\n\
                             {imports}\
                             document.getElementById('out').textContent='got '+(globalThis.__c||0);"
                        )
                        .into_bytes()
                    } else if text.starts_with("GET /m") {
                        let cur = inflight.fetch_add(1, Relaxed) + 1;
                        peak.fetch_max(cur, Relaxed);
                        tokio::time::sleep(std::time::Duration::from_millis(DELAY_MS)).await;
                        inflight.fetch_sub(1, Relaxed);
                        format!(
                            "HTTP/1.1 200 OK\r\n{js}\r\nConnection: close\r\n\r\n\
                             globalThis.__c=(globalThis.__c||0)+1;"
                        )
                        .into_bytes()
                    } else {
                        b"HTTP/1.1 404 Nope\r\nContent-Length: 0\r\n\r\n".to_vec()
                    };
                    let _ = sock.write_all(&reply).await;
                });
            }
        });

        let url = parse_url(&format!("http://127.0.0.1:{port}/page")).unwrap();
        let response = fetch(&Request::get(url)).await.unwrap();
        let started = std::time::Instant::now();
        let mut response = execute_js(response, (80, 24), (8, 16), Default::default()).await;
        let elapsed = started.elapsed();
        let peak = peak.load(Relaxed);
        let mut body = String::from_utf8_lossy(&response.body).into_owned();
        // A rendering opportunity may expose the interactive shell while the entry module is
        // suspended at top-level await. The module script still delays `load`; observe the actor's
        // later rendering update rather than requiring all frontends to suppress that valid paint.
        if !body.contains(&format!("got {N}")) {
            let live = response
                .live
                .as_mut()
                .expect("pending module evaluation keeps the page actor live");
            body = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    match live.events.recv().await {
                        Some(crate::js::PageEvt::Updated { html, .. })
                            if html.contains(&format!("got {N}")) =>
                        {
                            break html;
                        }
                        Some(_) => {}
                        None => panic!("page actor closed before module evaluation completed"),
                    }
                }
            })
            .await
            .expect("module evaluation render timed out");
        }
        eprintln!(
            "static_module_graph_prefetches_concurrently: {N}@{DELAY_MS}ms, peak in-flight {peak}, took {elapsed:?}"
        );
        assert!(
            body.contains(&format!("got {N}")),
            "all modules ran: {body}"
        );
        assert!(
            peak >= N / 2,
            "module fetches did not overlap: peak in-flight {peak} of {N} (serial engine never exceeds 1)"
        );
        server.abort();
    }

    /// The loader-concurrency win (Lever B): sibling DYNAMIC `import()`s
    /// overlap on the network. This isolates the `load_imported_module`
    /// `.await` change from speculation — the import scanner deliberately
    /// skips dynamic `import()` (a router fans out to every route), so
    /// these bodies are NOT prefetched. With the old blocking loader each
    /// `import()` parked the page thread on its own RTT and they ran
    /// strictly serial (the archive.org boot staircase); awaiting lets the
    /// concurrently-enqueued graph-load jobs fetch at once. Mirrors the
    /// real boot pattern (`Promise.all([import(a), import(b), …])`).
    #[tokio::test]
    async fn dynamic_sibling_imports_load_concurrently() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        const N: usize = 8;
        const DELAY_MS: u64 = 120;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        // Prove the loader overlapped the imports by the server's peak
        // in-flight count, not wall-clock — the old timing ratio failed
        // spuriously under a busy suite because the fixed JS parse/compile
        // overhead (which dwarfs DELAY_MS) balloons under CPU contention.
        // The blocking loader these served strictly serial (peak 1); the
        // awaiting loader fetches them at once (peak ~N).
        let inflight = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let (srv_inflight, srv_peak) = (inflight.clone(), peak.clone());
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                // One task per connection so the server never serializes —
                // otherwise it would mask the client concurrency we test.
                let (inflight, peak) = (srv_inflight.clone(), srv_peak.clone());
                tokio::spawn(async move {
                    let mut req = Vec::new();
                    let mut buf = [0u8; 2048];
                    while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                        match sock.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => req.extend_from_slice(&buf[..n]),
                        }
                    }
                    let text = String::from_utf8_lossy(&req).into_owned();
                    let js = "Content-Type: text/javascript";
                    let reply: Vec<u8> = if text.starts_with("GET /page ") {
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nConnection: close\r\n\r\n\
                         <body><div id=out></div>\
                         <script type=module src='/entry.js'></script></body>"
                            .as_bytes()
                            .to_vec()
                    } else if text.starts_with("GET /entry.js ") {
                        // Dynamic, NOT static, imports — the scanner won't
                        // prefetch these, so the overlap is the loader's.
                        let mut specs = String::new();
                        for i in 0..N {
                            if i > 0 {
                                specs.push(',');
                            }
                            specs.push_str(&format!("import('/m{i}.js')"));
                        }
                        format!(
                            "HTTP/1.1 200 OK\r\n{js}\r\nConnection: close\r\n\r\n\
                             await Promise.all([{specs}]);\
                             document.getElementById('out').textContent='got '+(globalThis.__c||0);"
                        )
                        .into_bytes()
                    } else if text.starts_with("GET /m") {
                        let cur = inflight.fetch_add(1, Relaxed) + 1;
                        peak.fetch_max(cur, Relaxed);
                        tokio::time::sleep(std::time::Duration::from_millis(DELAY_MS)).await;
                        inflight.fetch_sub(1, Relaxed);
                        format!(
                            "HTTP/1.1 200 OK\r\n{js}\r\nConnection: close\r\n\r\n\
                             globalThis.__c=(globalThis.__c||0)+1;"
                        )
                        .into_bytes()
                    } else {
                        b"HTTP/1.1 404 Nope\r\nContent-Length: 0\r\n\r\n".to_vec()
                    };
                    let _ = sock.write_all(&reply).await;
                });
            }
        });

        let url = parse_url(&format!("http://127.0.0.1:{port}/page")).unwrap();
        let response = fetch(&Request::get(url)).await.unwrap();
        let started = std::time::Instant::now();
        let mut response = execute_js(response, (80, 24), (8, 16), Default::default()).await;
        let elapsed = started.elapsed();
        let peak = peak.load(Relaxed);
        let mut body = String::from_utf8_lossy(&response.body).into_owned();
        // A rendering opportunity may expose the interactive shell while the entry module is
        // suspended at top-level await. The module script still delays `load`; observe the actor's
        // later rendering update rather than requiring all frontends to suppress that valid paint.
        if !body.contains(&format!("got {N}")) {
            let live = response
                .live
                .as_mut()
                .expect("pending module evaluation keeps the page actor live");
            body = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    match live.events.recv().await {
                        Some(crate::js::PageEvt::Updated { html, .. })
                            if html.contains(&format!("got {N}")) =>
                        {
                            break html;
                        }
                        Some(_) => {}
                        None => panic!("page actor closed before module evaluation completed"),
                    }
                }
            })
            .await
            .expect("module evaluation render timed out");
        }
        eprintln!(
            "dynamic_sibling_imports_load_concurrently: {N}@{DELAY_MS}ms, peak in-flight {peak}, took {elapsed:?}"
        );
        assert!(
            body.contains(&format!("got {N}")),
            "all modules ran: {body}"
        );
        assert!(
            peak >= N / 2,
            "dynamic sibling imports did not overlap: peak in-flight {peak} of {N} (serial engine never exceeds 1)"
        );
        server.abort();
    }

    #[tokio::test]
    async fn page_js_gets_fetch_xhr_and_session_storage() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let mut req = Vec::new();
                let mut buf = [0u8; 2048];
                while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                    match sock.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => req.extend_from_slice(&buf[..n]),
                    }
                }
                // Drain the POST body before replying: closing with
                // unread bytes in the buffer RSTs the connection, which
                // can destroy the client's unread reply (flaky XHR).
                let header_end = req
                    .windows(4)
                    .position(|w| w == b"\r\n\r\n")
                    .map_or(req.len(), |p| p + 4);
                let content_length = String::from_utf8_lossy(&req[..header_end])
                    .lines()
                    .find_map(|l| {
                        let (k, v) = l.split_once(':')?;
                        k.eq_ignore_ascii_case("content-length")
                            .then(|| v.trim().parse::<usize>().ok())?
                    })
                    .unwrap_or(0);
                while req.len() < header_end + content_length {
                    match sock.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => req.extend_from_slice(&buf[..n]),
                    }
                }
                let text = String::from_utf8_lossy(&req).into_owned();
                let reply: Vec<u8> = if text.starts_with("GET /page1 ") {
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nConnection: close\r\n\r\n\
                      <body><div id=out></div><div id=guard></div><script>\
                      localStorage.setItem('seen', 'page1');\
                      fetch('/api.json').then(function (r) { return r.json(); })\
                        .then(function (d) { document.getElementById('out').textContent = 'fetched ' + d.name; });\
                      fetch('http://10.255.255.1/x')\
                        .catch(function () { document.getElementById('guard').textContent = 'blocked'; });\
                      </script></body>"
                        .to_vec()
                } else if text.starts_with("GET /api.json ") {
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n\
                      {\"name\":\"trust\"}"
                        .to_vec()
                } else if text.starts_with("GET /page2 ") {
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nConnection: close\r\n\r\n\
                      <body><div id=out></div><script>\
                      var x = new XMLHttpRequest();\
                      x.open('POST', '/echo');\
                      x.onload = function () {\
                        document.getElementById('out').textContent =\
                          x.responseText + ' / ' + localStorage.getItem('seen');\
                      };\
                      x.send('ping');\
                      </script></body>"
                        .to_vec()
                } else if text.starts_with("POST /echo ") {
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\npong"
                        .to_vec()
                } else {
                    b"HTTP/1.1 404 Nope\r\nContent-Length: 0\r\n\r\n".to_vec()
                };
                let _ = sock.write_all(&reply).await;
            }
        });

        let storage: crate::js::WebStorage = Default::default();

        // Page 1: fetch + JSON render; the private-space probe rejects.
        let url = parse_url(&format!("http://127.0.0.1:{port}/page1")).unwrap();
        let response = fetch(&Request::get(url)).await.unwrap();
        let mut response = execute_js(response, (80, 24), (8, 16), storage.clone()).await;
        let mut body = String::from_utf8_lossy(&response.body).into_owned();
        if !body.contains("fetched trust") {
            let live = response
                .live
                .as_mut()
                .expect("initial fetch keeps page live");
            loop {
                match tokio::time::timeout(Duration::from_secs(5), live.events.recv()).await {
                    Ok(Some(crate::js::PageEvt::Updated { html, .. })) => {
                        body = html;
                        if body.contains("fetched trust") {
                            break;
                        }
                    }
                    other => panic!("expected fetch render, got {other:?}"),
                }
            }
        }
        assert!(body.contains("fetched trust"), "{body}");
        assert!(body.contains(">blocked<"), "{body}");
        let outcome = response.js.expect("outcome");
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert_eq!(outcome.fetches, 1); // the blocked probe never counted

        // Page 2: async XHR POST + storage written by page 1.
        let url = parse_url(&format!("http://127.0.0.1:{port}/page2")).unwrap();
        let response = fetch(&Request::get(url)).await.unwrap();
        let mut response = execute_js(response, (80, 24), (8, 16), storage.clone()).await;
        let mut body = String::from_utf8_lossy(&response.body).into_owned();
        if !body.contains("pong / page1") {
            let live = response.live.as_mut().expect("initial XHR keeps page live");
            loop {
                match tokio::time::timeout(Duration::from_secs(5), live.events.recv()).await {
                    Ok(Some(crate::js::PageEvt::Updated { html, .. })) => {
                        body = html;
                        if body.contains("pong / page1") {
                            break;
                        }
                    }
                    other => panic!("expected XHR render, got {other:?}"),
                }
            }
        }
        assert!(body.contains("pong / page1"), "{body}");
        server.abort();
    }

    #[tokio::test]
    async fn execute_js_hands_back_a_live_clickable_page() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let mut buf = [0u8; 2048];
            let _ = sock.read(&mut buf).await;
            let _ = sock
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nConnection: close\r\n\r\n\
                      <body><strong id=count>0</strong>\
                      <button onclick=\"var c = document.getElementById('count');\
                      c.textContent = String(Number(c.textContent) + 1);\">go</button>\
                      <script>void 0;</script></body>",
                )
                .await;
        });

        let url = parse_url(&format!("http://127.0.0.1:{port}/counter")).unwrap();
        let response = fetch(&Request::get(url)).await.unwrap();
        let mut response = execute_js(response, (80, 24), (8, 16), Default::default()).await;
        let mut live = response.live.take().expect("clickable page stays live");
        let body = String::from_utf8_lossy(&response.body).into_owned();
        let node: usize = body
            .split("x-trust-js:")
            .nth(1)
            .and_then(|r| r.split(':').next())
            .expect("click marker in body")
            .parse()
            .unwrap();

        live.handle
            .cmds
            .send(crate::js::PageCmd::Click(node))
            .await
            .unwrap();
        match live.events.recv().await {
            Some(crate::js::PageEvt::Updated { html, .. }) => {
                assert!(html.contains(">1</strong>"), "{html}");
            }
            other => panic!("expected Updated, got {other:?}"),
        }
        server.abort();
    }

    /// Lever A (settle-when-interactive): a live page paints its SHELL the
    /// moment it's interactive — BEFORE `settle_page` drains the data
    /// fetches a DOMContentLoaded handler kicks off — then emits a filled
    /// render once they land. This is what drops archive.org's first paint
    /// from ~9s (waiting for its serial collections pagination) to ~5s.
    #[tokio::test]
    async fn execute_js_paints_shell_before_background_fetch_fills_it() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let mut req = Vec::new();
                    let mut buf = [0u8; 2048];
                    while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                        match sock.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => req.extend_from_slice(&buf[..n]),
                        }
                    }
                    let text = String::from_utf8_lossy(&req).into_owned();
                    let reply: &[u8] = if text.starts_with("GET /page ") {
                        b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nConnection: close\r\n\r\n\
                          <body><button onclick=\"void 0\">menu</button>\
                          <div id=tiles>SHELL</div>\
                          <script>document.addEventListener('DOMContentLoaded',function(){\
                          fetch('/data').then(function(r){return r.text();}).then(function(t){\
                          document.getElementById('tiles').textContent=t;});});</script></body>"
                    } else if text.starts_with("GET /data ") {
                        // Delay so the shell is forced out before this lands.
                        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                        b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\nBACKGROUND-TILE"
                    } else {
                        b"HTTP/1.1 404 Nope\r\nContent-Length: 0\r\n\r\n"
                    };
                    let _ = sock.write_all(reply).await;
                });
            }
        });

        let url = parse_url(&format!("http://127.0.0.1:{port}/page")).unwrap();
        let response = fetch(&Request::get(url)).await.unwrap();
        let mut response = execute_js(response, (80, 24), (8, 16), Default::default()).await;
        let mut live = response.live.take().expect("clickable page stays live");
        let shell = String::from_utf8_lossy(&response.body).into_owned();
        assert!(shell.contains("SHELL"), "shell painted: {shell}");
        assert!(
            !shell.contains("BACKGROUND-TILE"),
            "first paint precedes the background fetch resolving: {shell}"
        );

        // The background fetch resolves, mutates the DOM, and a filled
        // render follows on the live channel.
        match tokio::time::timeout(std::time::Duration::from_secs(5), live.events.recv()).await {
            Ok(Some(crate::js::PageEvt::Updated { html, .. })) => {
                assert!(
                    html.contains("BACKGROUND-TILE"),
                    "filled render carries the fetched content: {html}"
                );
            }
            other => panic!("expected a filled Updated, got {other:?}"),
        }
        server.abort();
    }

    /// Phase 2 (background fetch): a `fetch()` fired from a CLICK runs OFF the
    /// dispatch — the dispatch returns IMMEDIATELY with the loading state and the
    /// actor stays responsive, then the result lands as a SEPARATE render when
    /// the wire completes. Before this the dispatch BLOCKED on the fetch,
    /// freezing the live engine. We prove the no-block contract by delaying
    /// `/data` so a "loading" render is forced out
    /// strictly before the "DATA-OK" render.
    #[tokio::test]
    async fn a_click_fetch_runs_in_the_background_not_blocking_the_dispatch() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let mut req = Vec::new();
                    let mut buf = [0u8; 2048];
                    while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                        match sock.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => req.extend_from_slice(&buf[..n]),
                        }
                    }
                    let text = String::from_utf8_lossy(&req).into_owned();
                    let reply: &[u8] = if text.starts_with("GET /page ") {
                        b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nConnection: close\r\n\r\n\
                          <body><div id=r>none</div>\
                          <button id=go>go</button>\
                          <script>document.getElementById('go').addEventListener('click',function(){\
                          document.getElementById('r').textContent='loading';\
                          fetch('/data').then(function(x){return x.text();}).then(function(t){\
                          document.getElementById('r').textContent=t;});});</script></body>"
                    } else if text.starts_with("GET /data ") {
                        // Delay so the loading-state render precedes this landing.
                        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                        b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\nDATA-OK"
                    } else {
                        b"HTTP/1.1 404 Nope\r\nContent-Length: 0\r\n\r\n"
                    };
                    let _ = sock.write_all(reply).await;
                });
            }
        });

        let url = parse_url(&format!("http://127.0.0.1:{port}/page")).unwrap();
        let response = fetch(&Request::get(url)).await.unwrap();
        let mut response = execute_js(response, (80, 24), (8, 16), Default::default()).await;
        let mut live = response.live.take().expect("clickable page stays live");
        let body = String::from_utf8_lossy(&response.body).into_owned();
        let node: usize = body
            .split("x-trust-js:")
            .nth(1)
            .and_then(|r| r.split(':').next())
            .expect("click marker in body")
            .parse()
            .unwrap();

        live.handle
            .cmds
            .send(crate::js::PageCmd::Click(node))
            .await
            .unwrap();

        // Drain renders: a "loading" one (the dispatch, fetch still in flight)
        // must arrive STRICTLY BEFORE the "DATA-OK" one (the background result).
        let mut saw_loading_before_data = false;
        let mut filled = false;
        for _ in 0..10 {
            match tokio::time::timeout(std::time::Duration::from_secs(5), live.events.recv()).await
            {
                Ok(Some(crate::js::PageEvt::Updated { html, .. })) => {
                    if html.contains("DATA-OK") {
                        filled = true;
                        break;
                    }
                    if html.contains("loading") {
                        saw_loading_before_data = true;
                    }
                }
                Ok(Some(_)) => continue, // Settled etc.
                other => panic!("expected an Updated, got {other:?}"),
            }
        }
        assert!(
            saw_loading_before_data,
            "the click rendered a loading state before the fetch resolved — the dispatch did not block on the wire"
        );
        assert!(
            filled,
            "the background fetch's result arrived as a later, separate render"
        );
        server.abort();
    }

    /// Regression: a load-time JS error must be counted ONCE, not once per
    /// render. The actor paints a shell (errors → `response.js`) then a
    /// filled settle render. `Updated` is a DELTA, so the settle emit must
    /// NOT re-carry the load error — else the app's `page_js_errors +=`
    /// double-counts it and a single error shows as `· JS:2!`.
    #[tokio::test]
    async fn a_load_error_is_reported_once_across_shell_and_settle() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let mut req = Vec::new();
                    let mut buf = [0u8; 2048];
                    while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                        match sock.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => req.extend_from_slice(&buf[..n]),
                        }
                    }
                    let text = String::from_utf8_lossy(&req).into_owned();
                    let reply: &[u8] = if text.starts_with("GET /page ") {
                        // A script throws at load (the one error), there's a
                        // clickable (so a shell paints), and a background fetch
                        // mutates the DOM during settle (so a second render
                        // follows on the live channel).
                        b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nConnection: close\r\n\r\n\
                          <body><button onclick=\"void 0\">menu</button>\
                          <div id=tiles>SHELL</div>\
                          <script>null.boom;</script>\
                          <script>document.addEventListener('DOMContentLoaded',function(){\
                          fetch('/data').then(function(r){return r.text();}).then(function(t){\
                          document.getElementById('tiles').textContent=t;});});</script></body>"
                    } else if text.starts_with("GET /data ") {
                        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                        b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\nBACKGROUND-TILE"
                    } else {
                        b"HTTP/1.1 404 Nope\r\nContent-Length: 0\r\n\r\n"
                    };
                    let _ = sock.write_all(reply).await;
                });
            }
        });

        let url = parse_url(&format!("http://127.0.0.1:{port}/page")).unwrap();
        let response = fetch(&Request::get(url)).await.unwrap();
        let mut response = execute_js(response, (80, 24), (8, 16), Default::default()).await;
        let mut live = response.live.take().expect("clickable page stays live");
        // The shell (response.js) reports the single load error.
        assert_eq!(
            response.js.as_ref().map_or(0, |o| o.errors.len()),
            1,
            "the load error is reported once via the shell: {:?}",
            response.js
        );

        // The filled settle render mutates the DOM but must NOT re-report the
        // load error — its outcome is a delta with zero new errors.
        match tokio::time::timeout(std::time::Duration::from_secs(5), live.events.recv()).await {
            Ok(Some(crate::js::PageEvt::Updated { html, outcome })) => {
                assert!(
                    html.contains("BACKGROUND-TILE"),
                    "filled render carries the fetched content: {html}"
                );
                assert_eq!(
                    outcome.errors.len(),
                    0,
                    "settle delta must not re-report the load error: {:?}",
                    outcome.errors
                );
            }
            other => panic!("expected a filled Updated, got {other:?}"),
        }
        server.abort();
    }

    /// Regression for the Lever-A first-paint stamp: a live page's `load`
    /// event must STILL fire after the shell paints. `outcome.elapsed` is
    /// the cumulative-COMPUTE accumulator `run_script`'s budget gate reads
    /// (`>= COMPUTE_BUDGET` 2s ⇒ skip). The actor used to overwrite it with
    /// WALL-clock at first paint; on any page that took >2s of wall to
    /// reach interactive (archive.org: serial module graph) the `load`
    /// event was then skipped — the page settled but its load handlers
    /// never ran ("load: skipped, page JS budget exhausted"; the page
    /// half-loaded). Here the entry module burns ~2.1s of WALL on a slow
    /// fetch (module top-level work is NOT charged to `outcome.elapsed`),
    /// so without the fix the shell-paint stamp pushes `elapsed` past the
    /// 2s gate and `load` is skipped; with it `load` runs and fills `#out`.
    #[tokio::test]
    async fn load_event_fires_even_when_wall_time_at_first_paint_exceeds_compute_budget() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let mut req = Vec::new();
                    let mut buf = [0u8; 2048];
                    while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                        match sock.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => req.extend_from_slice(&buf[..n]),
                        }
                    }
                    let text = String::from_utf8_lossy(&req).into_owned();
                    let reply: Vec<u8> = if text.starts_with("GET /page ") {
                        b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nConnection: close\r\n\r\n\
                          <body><button onclick=\"void 0\">menu</button>\
                          <div id=out>SHELL</div>\
                          <script type=module src='/entry.js'></script></body>"
                            .to_vec()
                    } else if text.starts_with("GET /entry.js ") {
                        // Register a load handler, then burn WALL (not
                        // compute) on a slow fetch so first paint lands past
                        // the 2s COMPUTE_BUDGET. Top-level module work isn't
                        // charged to outcome.elapsed, so the only thing that
                        // can push the gate over is the (buggy) wall stamp.
                        b"HTTP/1.1 200 OK\r\nContent-Type: text/javascript\r\nConnection: close\r\n\r\n\
                          window.addEventListener('load', function(){\
                          document.getElementById('out').textContent='LOADED';});\
                          await fetch('/slow');"
                            .to_vec()
                    } else if text.starts_with("GET /slow ") {
                        tokio::time::sleep(std::time::Duration::from_millis(2100)).await;
                        b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\nok"
                            .to_vec()
                    } else {
                        b"HTTP/1.1 404 Nope\r\nContent-Length: 0\r\n\r\n".to_vec()
                    };
                    let _ = sock.write_all(&reply).await;
                });
            }
        });

        let url = parse_url(&format!("http://127.0.0.1:{port}/page")).unwrap();
        let response = fetch(&Request::get(url)).await.unwrap();
        let mut response = execute_js(response, (80, 24), (8, 16), Default::default()).await;
        let mut live = response.live.take().expect("clickable page stays live");

        // Drain to the settled render: the shell paints first ("SHELL"),
        // then the `load` handler's mutation arrives ("LOADED").
        let mut last = String::from_utf8_lossy(&response.body).into_owned();
        let mut last_errors = response.js.map(|o| o.errors).unwrap_or_default();
        while let Ok(Some(evt)) =
            tokio::time::timeout(Duration::from_secs(10), live.events.recv()).await
        {
            if let crate::js::PageEvt::Updated { html, outcome } = evt {
                last = html;
                last_errors = outcome.errors;
            }
        }
        assert!(
            last.contains("LOADED"),
            "load handler ran and filled #out: {last}"
        );
        assert!(
            !last_errors.iter().any(|e| e.contains("load: skipped")),
            "load event was not budget-skipped: {last_errors:?}"
        );
        server.abort();
    }

    /// Her expandtest.html, byte-for-byte through the full live path.
    #[tokio::test]
    async fn execute_js_live_path_handles_the_expander_page() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        const PAGE: &str = "<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n\
            <meta charset=\"UTF-8\">\n<title>Pure JS Toggle</title>\n</head>\n<body>\n\
            <!-- Main toggle link -->\n\
            <a href=\"#\" id=\"toggleLink\">Click here to show/hide links</a>\n\
            <div id=\"hiddenLinks\">\n\
            <a href=\"#\">Additional Link 1</a>\n\
            <a href=\"#\">Additional Link 2</a>\n\
            </div>\n<script>\n\
            document.addEventListener('DOMContentLoaded', () => {\n\
              const toggleLink = document.getElementById('toggleLink');\n\
              const hiddenLinks = document.getElementById('hiddenLinks');\n\
              hiddenLinks.style.display = 'none';\n\
              toggleLink.addEventListener('click', (event) => {\n\
                event.preventDefault();\n\
                hiddenLinks.style.display = hiddenLinks.style.display === 'none' ? 'block' : 'none';\n\
              });\n\
            });\n</script>\n</body>\n</html>";

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let mut buf = [0u8; 2048];
            let _ = sock.read(&mut buf).await;
            let reply = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nConnection: close\r\n\r\n{PAGE}"
            );
            let _ = sock.write_all(reply.as_bytes()).await;
        });

        let url = parse_url(&format!("http://127.0.0.1:{port}/expand")).unwrap();
        let response = fetch(&Request::get(url)).await.unwrap();
        let mut response = execute_js(response, (80, 24), (8, 16), Default::default()).await;
        let body = String::from_utf8_lossy(&response.body).into_owned();
        let outcome = response.js.as_ref().expect("JS ran");
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        // Initial hide applied at first paint.
        assert!(!body.contains("Additional Link 1"), "{body}");
        let mut live = response.live.take().expect("toggle keeps the page alive");
        let toggle: usize = body
            .split("x-trust-js:")
            .nth(1)
            .and_then(|r| r.split(':').next())
            .expect("toggle marker")
            .parse()
            .unwrap();
        live.handle
            .cmds
            .send(crate::js::PageCmd::Click(toggle))
            .await
            .unwrap();
        match live.events.recv().await {
            Some(crate::js::PageEvt::Updated { html, .. }) => {
                assert!(html.contains("Additional Link 1"), "{html}");
            }
            other => panic!("expected Updated, got {other:?}"),
        }
        server.abort();
    }

    /// Diagnostic: fetch a REAL url, find a clickable by its link text,
    /// click it through the live actor, and dump the classes of two probe
    /// elements before/after. `TRUST_CLICK_DIAG=<url> TRUST_CLICK_TEXT=<linktext>
    /// TRUST_CLICK_PROBE=<substr> cargo test click_diag -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "manual diagnostic, needs TRUST_CLICK_DIAG=<url>"]
    async fn click_diag() {
        let Ok(target) = std::env::var("TRUST_CLICK_DIAG") else {
            eprintln!("set TRUST_CLICK_DIAG");
            return;
        };
        let link_text = std::env::var("TRUST_CLICK_TEXT").unwrap_or_default();
        let url = parse_url(&target).expect("absolute http(s) url");
        // Same auth seeding as net_diag: TRUST_DIAG_COOKIE seeds the jar before
        // the cold fetch so a cookie-gated SPA serves its authenticated render.
        if let Ok(cookies) = std::env::var("TRUST_DIAG_COOKIE") {
            for c in cookies.split(';') {
                let c = c.trim();
                if !c.is_empty() {
                    set_cookie_from_js(&url, c);
                }
            }
        }
        let mut response = fetch(&Request::get(url)).await.unwrap();
        // TRUST_DIAG_INJECT=<file>: splice a probe <script> at the top of <head>.
        if let Ok(inj) = std::env::var("TRUST_DIAG_INJECT") {
            let js = std::fs::read_to_string(&inj).unwrap();
            let mut b = String::from_utf8_lossy(&response.body).to_string();
            let at = b
                .find("<head>")
                .map(|i| i + "<head>".len())
                .or_else(|| b.find("<head "))
                .unwrap_or(0);
            b.insert_str(at, &format!("<script>{js}</script>"));
            response.body = b.into_bytes();
        }
        let vp: (u16, u16) = std::env::var("TRUST_DIAG_VP")
            .ok()
            .and_then(|s| {
                s.split_once('x')
                    .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
            })
            .unwrap_or((120, 40));
        let mut response = execute_js(response, vp, (8, 16), Default::default()).await;
        let mut body = String::from_utf8_lossy(&response.body).to_string();
        // Probe substring whose presence we report before/after the click
        // (e.g. `id="disclaimer"` — is the overlay still in the DOM?).
        let probe =
            std::env::var("TRUST_CLICK_PROBE").unwrap_or_else(|_| "id=\"disclaimer\"".into());
        eprintln!(
            "js errors at load = {:?}",
            response.js.as_ref().map(|j| &j.errors)
        );
        // TRUST_DIAG_SETTLE: drain post-shell Updated events so an SPA that mounts
        // its clickable (the suggestion buttons, the editor) AFTER first paint is
        // present before we look for the click target.
        if std::env::var_os("TRUST_DIAG_SETTLE").is_some()
            && let Some(live) = response.live.as_mut()
        {
            let __drain: usize = std::env::var("TRUST_DIAG_DRAIN")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(6);
            let __to: u64 = std::env::var("TRUST_DIAG_DRAIN_TO")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(20);
            for _ in 0..__drain {
                match tokio::time::timeout(Duration::from_secs(__to), live.events.recv()).await {
                    Ok(Some(crate::js::PageEvt::Updated { html, outcome })) => {
                        eprintln!(
                            "SETTLE EVT errors={:?} console={:?}",
                            outcome.errors, outcome.console
                        );
                        body = html;
                        // The diagnostic's target exists now; do not spend a
                        // full idle timeout draining unrelated background
                        // events before dispatching the requested click.
                        if !link_text.is_empty() && body.contains(&link_text) {
                            break;
                        }
                    }
                    Ok(Some(other)) => eprintln!("SETTLE EVT {other:?}"),
                    _ => {
                        eprintln!("SETTLE <no more within 20s>");
                        break;
                    }
                }
            }
        }
        eprintln!("BEFORE probe {probe:?} present = {}", body.contains(&probe));
        // The newest post-action snapshot, dumped to TRUST_NET_DIAG_OUT for
        // inspection (e.g. a JS-built lightbox/modal the click opens).
        let mut last_html: Option<String> = None;
        // Optional SetValue before any click (TRUST_SET_FIND=<substr in the target
        // tag, e.g. id="chat-input"> + TRUST_SET_VALUE=<text>): exercise the
        // editable path — a contenteditable host or a textarea — then a follow-on
        // click (type a chat message, then press Send via TRUST_CLICK_FIND2).
        if let (Some(find), Ok(val)) = (
            std::env::var("TRUST_SET_FIND")
                .ok()
                .filter(|s| !s.is_empty()),
            std::env::var("TRUST_SET_VALUE"),
        ) && let Some(node) = body.find(&find).and_then(|at| {
            let tstart = body[..at].rfind('<')?;
            let tag = &body[tstart..body[tstart..].find('>').map(|i| tstart + i)?];
            let tn = tag.find("data-trust-node=\"")? + 17;
            tag[tn..].split('"').next()?.parse::<usize>().ok()
        }) {
            eprintln!("--- SetValue node {node} = {val:?} (found via {find:?}) ---");
            let live = response.live.as_mut().expect("page is live");
            live.handle
                .cmds
                .send(crate::js::PageCmd::SetValue {
                    node,
                    value: val,
                    checked: None,
                })
                .await
                .unwrap();
            for _ in 0..4 {
                match tokio::time::timeout(Duration::from_secs(8), live.events.recv()).await {
                    Ok(Some(crate::js::PageEvt::Updated { html, outcome })) => {
                        eprintln!(
                            "SET EVT errors={:?} console={:?}",
                            outcome.errors, outcome.console
                        );
                        body = html.clone();
                        last_html = Some(html);
                    }
                    Ok(Some(other)) => eprintln!("SET EVT {other:?}"),
                    _ => {
                        eprintln!("SET <no more within 8s>");
                        break;
                    }
                }
            }
        }
        // Marker on the clickable wrapping link_text (skipped when empty).
        if !link_text.is_empty() {
            let at = body.find(&link_text).expect("link text in body");
            let marker = body[..at]
                .rfind("x-trust-js:")
                .map(|i| {
                    body[i + "x-trust-js:".len()..]
                        .split(':')
                        .next()
                        .unwrap()
                        .parse::<usize>()
                        .unwrap()
                })
                .expect("marker");
            eprintln!("clicking node {marker} (\"{link_text}\")");
            let live = response.live.as_mut().expect("page is live");
            live.handle.try_send_navigation_click(marker).unwrap();
            let click_wait = std::env::var("TRUST_CLICK_WAIT")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(8);
            let deadline = tokio::time::Instant::now() + Duration::from_secs(click_wait);
            let mut probe_dismissed = probe.is_empty();
            // Drain until the requested page predicate changes, not for an
            // arbitrary event count. A framework can emit several scheduler or
            // animation updates before a network mutation completes; dropping
            // the actor after four such updates made a successful interaction
            // look stuck. Keep both an overall deadline and a generous event
            // cap so a noisy page cannot turn this diagnostic into an idle spin.
            for _ in 0..128 {
                let Some(remaining) = deadline.checked_duration_since(tokio::time::Instant::now())
                else {
                    eprintln!("EVT <click deadline reached after {click_wait}s>");
                    break;
                };
                let ev = match tokio::time::timeout(remaining, live.events.recv()).await {
                    Ok(ev) => ev,
                    Err(_) => {
                        eprintln!("EVT <no more within {click_wait}s>");
                        break;
                    }
                };
                match ev {
                    Some(crate::js::PageEvt::Updated { html, outcome }) => {
                        eprintln!("EVT Updated ({}B): errors={:?}", html.len(), outcome.errors);
                        eprintln!("   console={:?}", outcome.console);
                        eprintln!(
                            "   probe {probe:?} present AFTER = {}",
                            html.contains(&probe)
                        );
                        probe_dismissed = !probe.is_empty() && !html.contains(&probe);
                        last_html = Some(html);
                        if probe_dismissed {
                            break;
                        }
                    }
                    Some(crate::js::PageEvt::Static { html, outcome }) => {
                        eprintln!("EVT Static: errors={:?}", outcome.errors);
                        eprintln!("   console={:?}", outcome.console);
                        eprintln!(
                            "   probe {probe:?} present AFTER = {}",
                            html.contains(&probe)
                        );
                        last_html = Some(html);
                        break;
                    }
                    Some(crate::js::PageEvt::Navigate(u)) => eprintln!("EVT Navigate -> {u}"),
                    Some(crate::js::PageEvt::SubmitDefault) => eprintln!("EVT SubmitDefault"),
                    Some(other) => {
                        eprintln!("EVT {other:?}");
                        break;
                    }
                    None => {
                        eprintln!("EVT <channel closed>");
                        break;
                    }
                }
            }
            if !probe.is_empty() {
                assert!(probe_dismissed, "probe remained after click: {probe:?}");
            }
            if let Ok(retain) = std::env::var("TRUST_CLICK_RETAIN")
                && !retain.is_empty()
            {
                let html = last_html.as_deref().expect("post-click resident HTML");
                assert!(
                    html.contains(&retain),
                    "post-click resident page lost retained marker {retain:?} ({}B)",
                    html.len()
                );
            }
        }
        // Optional SECOND click (TRUST_CLICK_FIND2=<substr in the target tag, e.g.
        // `id="send-message-button"`>): resolve the node from THIS run's post-click
        // HTML (ids drift across runs) and click it — e.g. fill the editor via a
        // suggestion click, then click the send button to test submission.
        if let Some(find2) = std::env::var("TRUST_CLICK_FIND2")
            .ok()
            .filter(|s| !s.is_empty())
            && let Some(html) = last_html.as_ref()
            && let Some(node2) = html.find(&find2).and_then(|at| {
                let tstart = html[..at].rfind('<')?;
                let tag = &html[tstart..html[tstart..].find('>').map(|i| tstart + i)?];
                let tn = tag.find("data-trust-node=\"")? + 17;
                tag[tn..].split('"').next()?.parse::<usize>().ok()
            })
        {
            eprintln!("--- second click: node {node2} (found via {find2:?}) ---");
            let live = response.live.as_mut().expect("page is live");
            live.handle
                .cmds
                .send(crate::js::PageCmd::Click(node2))
                .await
                .unwrap();
            // Generous: a click that fires an LLM completion streams its reply
            // back over the WebSocket as many `Updated` events. Keep watching
            // through `Settled` (the submit's own settle) until the stream goes
            // quiet for `secs`, so we observe progressive token rendering — not
            // just the first ack. (Diagnostic only.)
            let secs = std::env::var("TRUST_CLICK_WAIT")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(8);
            let mut updates = 0usize;
            for _ in 0..2000 {
                match tokio::time::timeout(Duration::from_secs(secs), live.events.recv()).await {
                    Ok(Some(crate::js::PageEvt::Updated { html, outcome })) => {
                        updates += 1;
                        if !outcome.errors.is_empty() {
                            eprintln!("EVT2 Updated #{updates}: errors={:?}", outcome.errors);
                        }
                        let console = if outcome.console.is_empty() {
                            String::new()
                        } else {
                            format!(" console={:?}", outcome.console)
                        };
                        eprintln!("EVT2 Updated #{updates} ({}B){console}", html.len());
                        // TRUST_EVT2_DUMP=<dir>: write each streamed snapshot as
                        // <dir>/evt2-<n>.html so the progressive render can be
                        // inspected frame by frame.
                        if let Ok(dir) = std::env::var("TRUST_EVT2_DUMP") {
                            let _ = std::fs::write(format!("{dir}/evt2-{updates}.html"), &html);
                        }
                        last_html = Some(html);
                    }
                    Ok(Some(crate::js::PageEvt::Navigate(u))) => eprintln!("EVT2 Navigate -> {u}"),
                    Ok(Some(crate::js::PageEvt::SubmitDefault)) => eprintln!("EVT2 SubmitDefault"),
                    Ok(Some(crate::js::PageEvt::Settled)) => {} // keep watching the stream
                    Ok(Some(other)) => {
                        eprintln!("EVT2 {other:?}");
                        break;
                    }
                    _ => {
                        eprintln!("EVT2 <stream quiet for {secs}s after {updates} updates>");
                        break;
                    }
                }
            }
        }
        // Re-fetch the page (same process-global COOKIE_JAR) to see whether the
        // click's server POST marked our session — i.e. is the overlay STILL
        // served on a fresh navigation? (erome's gate persists across pages
        // unless the disclaimer POST's session sticks.)
        let again = fetch(&Request::get(parse_url(&target).unwrap()))
            .await
            .unwrap();
        let again_body = String::from_utf8_lossy(&again.body);
        eprintln!(
            "RE-FETCH probe {probe:?} present = {} ({}B)",
            again_body.contains(&probe),
            again.body.len()
        );
        eprintln!(
            "RE-FETCH sent cookies = {:?}",
            cookies_for_request(&parse_url(&target).unwrap())
        );
        if let Some(html) = last_html
            && let Ok(out) = std::env::var("TRUST_NET_DIAG_OUT")
        {
            std::fs::write(&out, &html).unwrap();
            eprintln!("post-click body ({}B) -> {out}", html.len());
        }
        drop(response.live.take());
    }

    /// Diagnostic: load a REAL login-style page live, fill the email +
    /// password fields, submit, and report what the page does — whether the
    /// submit button gets enabled, whether a `submit`/navigation results, and
    /// any errors. `TRUST_NET_DIAG=<url> cargo test --release form_fill_submit_diag -- --ignored --nocapture`
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "manual diagnostic, needs TRUST_NET_DIAG=<url>"]
    async fn form_fill_submit_diag() {
        let Ok(target) = std::env::var("TRUST_NET_DIAG") else {
            eprintln!("set TRUST_NET_DIAG to a login URL");
            return;
        };
        let url = parse_url(&target).unwrap();
        let response = fetch(&Request::get(url)).await.unwrap();
        let mut response = execute_js(response, (120, 40), (8, 16), Default::default()).await;
        let live = response.live.as_mut().expect("page is live");
        // Drain the settle so the React form is mounted.
        let mut html = String::from_utf8_lossy(&response.body).to_string();
        async fn drain(live: &mut LivePage, html: &mut String) {
            while let Ok(Some(ev)) =
                tokio::time::timeout(Duration::from_secs(15), live.events.recv()).await
            {
                match ev {
                    crate::js::PageEvt::Updated { html: h, outcome } => {
                        eprintln!(
                            "  Updated errors={:?} console={:?}",
                            outcome.errors, outcome.console
                        );
                        *html = h;
                    }
                    crate::js::PageEvt::Navigate(u) => {
                        eprintln!("  >>> Navigate -> {u}");
                        return;
                    }
                    crate::js::PageEvt::SubmitDefault => {
                        eprintln!("  >>> SubmitDefault (TRust would run the GET/POST)");
                        return;
                    }
                    other => eprintln!("  {other:?}"),
                }
            }
        }
        drain(live, &mut html).await;
        // data-trust-node id of the first tag matching `pred`.
        let node = |html: &str, pred: &dyn Fn(&str) -> bool| -> Option<usize> {
            for m in html.match_indices("data-trust-node=\"") {
                let tag_start = html[..m.0].rfind('<')?;
                // `data-trust-node` is appended last, so the tag's other attrs
                // (type/name/...) all precede `m.0` — an ASCII boundary.
                let tag = &html[tag_start..m.0];
                if pred(tag) {
                    let v = &html[m.0 + 17..];
                    return v[..v.find('"')?].parse().ok();
                }
            }
            None
        };
        let email = node(&html, &|t| {
            t.starts_with("<input") && t.contains("type=\"text\"")
        });
        let password = node(&html, &|t| {
            t.starts_with("<input") && t.contains("type=\"password\"")
        });
        let submit_text =
            std::env::var("TRUST_FORM_SUBMIT_TEXT").unwrap_or_else(|_| "ログイン".into());
        // The submit button: the `<button>` whose exact text is the login
        // label (`>ログイン</button>` — not the substring inside
        // "…でログイン"), which sits in the email/password form after the
        // password field. Extract owned values so no borrow of `html` lingers.
        let needle = format!(">{submit_text}</button>");
        let button: Option<(usize, bool)> = html.match_indices(&needle).find_map(|(at, _)| {
            let tstart = html[..at].rfind("<button")?;
            let btag = &html[tstart..=at];
            let tnode = btag.find("data-trust-node=\"")? + 17;
            let n = btag[tnode..].split('"').next()?.parse::<usize>().ok()?;
            Some((n, btag.contains("disabled")))
        });
        eprintln!(
            "email node={email:?} password node={password:?} submit={:?}",
            button.map(|b| b.0)
        );
        if let Some((_, disabled)) = button {
            eprintln!("BEFORE fill, submit button disabled = {disabled}");
        }
        if let Some(n) = email {
            live.handle
                .cmds
                .send(crate::js::PageCmd::SetValue {
                    node: n,
                    value: "tester@example.com".into(),
                    checked: None,
                })
                .await
                .unwrap();
            drain(live, &mut html).await;
        }
        if let Some(n) = password {
            live.handle
                .cmds
                .send(crate::js::PageCmd::SetValue {
                    node: n,
                    value: "hunter2password".into(),
                    checked: None,
                })
                .await
                .unwrap();
            drain(live, &mut html).await;
        }
        // Re-find the button after fills (node ids stable; re-read disabled).
        let after = html.match_indices(&needle).find_map(|(at, _)| {
            let tstart = html[..at].rfind("<button")?;
            Some(&html[tstart..at])
        });
        eprintln!(
            "AFTER fill, submit button tag has disabled = {:?}",
            after.map(|t| t.contains("disabled"))
        );
        // The form node = the <form> enclosing the password field.
        let form = html
            .find("type=\"password\"")
            .and_then(|pi| html[..pi].rfind("<form"))
            .and_then(|fs| {
                let tag = &html[fs..html[fs..].find('>').map(|i| fs + i)?];
                let tn = tag.find("data-trust-node=\"")? + 17;
                tag[tn..].split('"').next()?.parse::<usize>().ok()
            });
        // CLICK the button exactly as the app does when the user presses it
        // (a live submit button is a JsClick, NOT a Submit field) — so we
        // exercise the real click→form-submission path, not a synthetic Submit.
        eprintln!(
            "clicking login button: node={:?} (form={form:?})",
            button.map(|b| b.0)
        );
        let _ = form;
        if let Some((btn, _)) = button {
            live.handle
                .cmds
                .send(crate::js::PageCmd::Click(btn))
                .await
                .unwrap();
            drain(live, &mut html).await;
        }
        if let Ok(out) = std::env::var("TRUST_NET_DIAG_OUT") {
            std::fs::write(&out, html.as_bytes()).unwrap();
            eprintln!("final html -> {out}");
        }
        drop(response.live.take());
    }

    /// Diagnostic: fetch a REAL url through the full JS pipeline and
    /// Full JS-error survey at a chosen viewport (`TRUST_DIAG_VP=WxH`,
    /// default 200x50), draining the whole live settle so it sees EVERY
    /// unique error + stack the way the running app accumulates them — not
    /// just the load-time set `net_diag` shows. This is how the post-fix
    /// archive error count was tracked down.
    /// `TRUST_NET_DIAG=<url> cargo test --release diag_all_errors -- --ignored --nocapture`
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "manual diagnostic, needs TRUST_NET_DIAG=<url>"]
    async fn diag_all_errors() {
        let Ok(target) = std::env::var("TRUST_NET_DIAG") else {
            return;
        };
        let vp: (u16, u16) = std::env::var("TRUST_DIAG_VP")
            .ok()
            .and_then(|s| {
                s.split_once('x')
                    .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
            })
            .unwrap_or((200, 50));
        let url = parse_url(&target).unwrap();
        let resp = fetch(&Request::get(url)).await.unwrap();
        let mut resp = execute_js(resp, vp, (8, 16), Default::default()).await;
        let mut errs: Vec<String> = resp
            .js
            .as_ref()
            .map(|o| o.errors.clone())
            .unwrap_or_default();
        let mut cons: Vec<String> = resp
            .js
            .as_ref()
            .map(|o| o.console.clone())
            .unwrap_or_default();
        eprintln!("--- load errors: {} ---", errs.len());
        let mut last_html: Option<String> = None;
        if let Some(mut live) = resp.live.take() {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(70);
            loop {
                let left = deadline.saturating_duration_since(std::time::Instant::now());
                if left.is_zero() {
                    break;
                }
                match tokio::time::timeout(left, live.events.recv()).await {
                    Ok(Some(crate::js::PageEvt::Updated { html, outcome })) => {
                        last_html = Some(html);
                        for e in outcome.errors {
                            if !errs.contains(&e) {
                                errs.push(e);
                            }
                        }
                        for c in outcome.console {
                            if !cons.contains(&c) {
                                cons.push(c);
                            }
                        }
                    }
                    Ok(Some(crate::js::PageEvt::Trouble(es))) => {
                        for e in es {
                            if !errs.contains(&e) {
                                errs.push(e);
                            }
                        }
                    }
                    Ok(Some(crate::js::PageEvt::Static { html, outcome })) => {
                        last_html = Some(html);
                        for e in outcome.errors {
                            if !errs.contains(&e) {
                                errs.push(e);
                            }
                        }
                        break;
                    }
                    Ok(Some(_)) => {}
                    Ok(None) | Err(_) => break,
                }
            }
        }
        if let Some(blobs) = &resp.blobs {
            for (u, (b, t)) in blobs.lock().unwrap().iter() {
                eprintln!(
                    "BLOB {} type={:?} len={} head={:?}",
                    u,
                    t,
                    b.len(),
                    String::from_utf8_lossy(&b[..b.len().min(60)])
                );
            }
        }
        eprintln!("=== {} UNIQUE ERRORS ===", errs.len());
        for (i, e) in errs.iter().enumerate() {
            eprintln!("\n[{i}] {e}");
        }
        eprintln!("=== {} CONSOLE LINES ===", cons.len());
        for (i, c) in cons.iter().enumerate() {
            eprintln!("\n(c{i}) {c}");
        }
        // The post-SETTLE body (net_diag dumps only the first-paint shell; a
        // live SPA fills its content during settle). Dump it for inspection.
        if let Some(html) = last_html
            && let Ok(out) = std::env::var("TRUST_NET_DIAG_OUT")
        {
            std::fs::write(&out, &html).unwrap();
            eprintln!("post-settle body ({}B) -> {out}", html.len());
        }
    }

    /// Live browser-workload acceptance gate. Unlike `diag_all_errors`, this diagnostic fails on
    /// engine-fatal errors or when a site's meaningful DOM milestone is not reached. It is kept
    /// ignored because it intentionally depends on the current public site and network:
    ///
    /// `TRUST_BROWSER_GATE=https://www.youtube.com/ TRUST_BROWSER_GATE_CLICK='Reject the use' TRUST_BROWSER_GATE_EXPECT_CONTROL_GONE='Reject the use' cargo test browser_workload_gate -- --ignored --nocapture`
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "live network acceptance gate, needs TRUST_BROWSER_GATE=<url>"]
    async fn browser_workload_gate() {
        let target = std::env::var("TRUST_BROWSER_GATE")
            .expect("set TRUST_BROWSER_GATE to an absolute http(s) URL");
        let settle_seconds = std::env::var("TRUST_BROWSER_GATE_SECONDS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(45);
        let expected_html = std::env::var("TRUST_BROWSER_GATE_EXPECT_HTML_CONTAINS").ok();
        // A named-control gate may need to click before its final milestone
        // exists (Speedometer starts on `data-visible-section="home"` and
        // only creates `summary` after the click). Keep the old behavior when
        // this optional precondition is omitted, while allowing a precise
        // initial-vs-final pair for interactive workloads.
        let initial_expected_html =
            std::env::var("TRUST_BROWSER_GATE_EXPECT_INITIAL_HTML_CONTAINS")
                .ok()
                .or_else(|| expected_html.clone());
        let requested_click = std::env::var("TRUST_BROWSER_GATE_CLICK").ok();
        let unexpected_html = std::env::var("TRUST_BROWSER_GATE_EXPECT_HTML_NOT_CONTAINS").ok();
        let expect_no_errors = std::env::var_os("TRUST_BROWSER_GATE_EXPECT_NO_ERRORS").is_some();
        let viewport: (u16, u16) = std::env::var("TRUST_DIAG_VP")
            .ok()
            .and_then(|value| {
                value
                    .split_once('x')
                    .and_then(|(width, height)| Some((width.parse().ok()?, height.parse().ok()?)))
            })
            .unwrap_or((200, 50));
        let url = parse_url(&target).expect("browser workload URL is absolute");
        let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
        let response = fetch(&Request::get(url))
            .await
            .expect("workload fetch succeeds");
        let mut response = execute_js(response, viewport, (8, 16), Default::default()).await;
        let mut html = String::from_utf8_lossy(&response.body).into_owned();
        let mut rendered = response.rendered.take().map(|rendered| *rendered);
        let mut errors = response
            .js
            .as_ref()
            .map(|outcome| outcome.errors.clone())
            .unwrap_or_default();
        let mut updates = 0usize;
        let mut had_live_actor = false;
        let mut scheduler_responded = true;
        let started = std::time::Instant::now();
        if let Some(mut live) = response.live.take() {
            had_live_actor = true;
            scheduler_responded = false;
            // A browser command must remain dispatchable even while the page has
            // recursively queued microtasks, timers, resource completions, or
            // animation frames. An invalid element scroll is an intentional
            // no-op which the resident actor acknowledges with `Settled`; unlike
            // lifecycle/platform tasks, it cannot be confused with unsolicited
            // page work. This is the real command path used by the frontend and
            // catches the starvation that previously hid YouTube's search UI.
            live.handle
                .cmds
                .send(crate::js::PageCmd::SetScroll {
                    node: usize::MAX,
                    top: 0.0,
                    left: 0.0,
                })
                .await
                .expect("live browser actor accepts responsiveness probe");
            let scheduler_deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                let remaining =
                    scheduler_deadline.saturating_duration_since(std::time::Instant::now());
                assert!(
                    !remaining.is_zero(),
                    "{host} resident page actor did not dispatch a browser command within 10s; errors={errors:#?}"
                );
                match tokio::time::timeout(remaining, live.events.recv()).await {
                    Ok(Some(crate::js::PageEvt::Settled)) => {
                        scheduler_responded = true;
                        break;
                    }
                    Ok(Some(crate::js::PageEvt::Updated {
                        html: updated,
                        mut outcome,
                    })) => {
                        updates += 1;
                        html = updated;
                        if let Some(next) = outcome.rendered.take() {
                            rendered = Some(*next);
                        }
                        errors.extend(outcome.errors);
                    }
                    Ok(Some(crate::js::PageEvt::Static {
                        html: updated,
                        mut outcome,
                    })) => {
                        html = updated;
                        if let Some(next) = outcome.rendered.take() {
                            rendered = Some(*next);
                        }
                        errors.extend(outcome.errors);
                        break;
                    }
                    Ok(Some(crate::js::PageEvt::Trouble(mut trouble))) => {
                        errors.append(&mut trouble)
                    }
                    Ok(Some(_)) => {}
                    Ok(None) => break,
                    Err(_) => continue,
                }
            }
            assert!(
                scheduler_responded,
                "{host} resident page actor ended before acknowledging the browser command probe; errors={errors:#?}"
            );
            // Exercise the same first displayed-page correction as `App::sync_page_viewport`.
            // The fetch starts against the provisional content area, while committing the page
            // changes that area by at least the status row. Real applications (YouTube among
            // them) use the resulting CSSOM View `resize` task to select their responsive UI.
            live.handle
                .cmds
                .send(crate::js::PageCmd::Viewport(crate::layout2::Viewport::new(
                    f32::from(viewport.0) * 8.0,
                    f32::from(viewport.1.saturating_sub(1)) * 16.0,
                )))
                .await
                .expect("live browser actor accepts initial viewport correction");
            let deadline = started + std::time::Duration::from_secs(settle_seconds);
            loop {
                if initial_expected_html
                    .as_deref()
                    .is_some_and(|expected| html.contains(expected))
                    || (expect_no_errors && !errors.is_empty())
                {
                    break;
                }
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                if remaining.is_zero() {
                    break;
                }
                match tokio::time::timeout(remaining, live.events.recv()).await {
                    Ok(Some(crate::js::PageEvt::Updated {
                        html: updated,
                        mut outcome,
                    })) => {
                        updates += 1;
                        html = updated;
                        if let Some(next) = outcome.rendered.take() {
                            rendered = Some(*next);
                        }
                        errors.extend(outcome.errors);
                    }
                    Ok(Some(crate::js::PageEvt::Static {
                        html: updated,
                        mut outcome,
                    })) => {
                        html = updated;
                        if let Some(next) = outcome.rendered.take() {
                            rendered = Some(*next);
                        }
                        errors.extend(outcome.errors);
                        break;
                    }
                    Ok(Some(crate::js::PageEvt::Trouble(mut trouble))) => {
                        errors.append(&mut trouble)
                    }
                    Ok(Some(_)) => {}
                    Ok(None) | Err(_) => break,
                }
            }

            // Drive a named control through the same canonical actor node used by the
            // frontends. Public-site experiments make raw arena ids unstable, while the
            // renderer-independent semantic tree retains the HTML-AAM accessible name and
            // activation target. This turns the workload gate into an interaction check (for
            // example YouTube's consent buttons or Speedometer's Start button) without a
            // site-specific selector or coordinate guess.
            if let Some(requested_name) = requested_click.as_deref() {
                let requested_name = requested_name.to_ascii_lowercase();
                let target = rendered
                    .as_ref()
                    .and_then(|page| {
                        page.semantics.nodes.iter().find(|node| {
                            node.dom_node.is_some()
                                && node.actions.contains(&crate::accessibility::Action::Activate)
                                && node.name.to_ascii_lowercase().contains(&requested_name)
                        })
                    })
                    .unwrap_or_else(|| {
                        panic!(
                            "{host} did not expose an activatable control whose accessible name contains {requested_name:?}"
                        )
                    });
                let target_node = target
                    .dom_node
                    .expect("matched semantic node has a DOM node");
                let target_name = target.name.clone();
                eprintln!(
                    "BROWSER_GATE activating {target_name:?} at canonical node {target_node}"
                );
                live.handle
                    .cmds
                    .send(crate::js::PageCmd::Click(target_node))
                    .await
                    .expect("live browser actor accepts named activation");

                let click_seconds = std::env::var("TRUST_BROWSER_GATE_CLICK_SECONDS")
                    .ok()
                    .and_then(|value| value.parse::<u64>().ok())
                    .unwrap_or(20);
                let click_deadline =
                    std::time::Instant::now() + std::time::Duration::from_secs(click_seconds);
                loop {
                    if expected_html
                        .as_deref()
                        .is_some_and(|expected| html.contains(expected))
                        || (expect_no_errors && !errors.is_empty())
                    {
                        break;
                    }
                    let remaining =
                        click_deadline.saturating_duration_since(std::time::Instant::now());
                    if remaining.is_zero() {
                        break;
                    }
                    match tokio::time::timeout(remaining, live.events.recv()).await {
                        Ok(Some(crate::js::PageEvt::Updated {
                            html: updated,
                            mut outcome,
                        })) => {
                            updates += 1;
                            html = updated;
                            if let Some(next) = outcome.rendered.take() {
                                rendered = Some(*next);
                            }
                            errors.extend(outcome.errors);
                        }
                        Ok(Some(crate::js::PageEvt::Static {
                            html: updated,
                            mut outcome,
                        })) => {
                            html = updated;
                            if let Some(next) = outcome.rendered.take() {
                                rendered = Some(*next);
                            }
                            errors.extend(outcome.errors);
                            break;
                        }
                        Ok(Some(crate::js::PageEvt::Trouble(mut trouble))) => {
                            errors.append(&mut trouble)
                        }
                        Ok(Some(_)) => {}
                        Ok(None) | Err(_) => break,
                    }
                }
            }
        }

        if host.ends_with("youtube.com")
            || host.ends_with("twitch.tv")
            || host.ends_with("steampowered.com")
        {
            assert!(
                had_live_actor,
                "{host} did not retain a resident page actor"
            );
            assert!(
                scheduler_responded,
                "{host} resident page actor ended before acknowledging a browser command"
            );
        }
        if requested_click.is_some() {
            assert!(
                had_live_actor,
                "{host} named-control gate requested a click but the page was classified static"
            );
        }

        if let Ok(expected_gone) = std::env::var("TRUST_BROWSER_GATE_EXPECT_CONTROL_GONE") {
            let expected_gone = expected_gone.to_ascii_lowercase();
            let lingering = rendered.as_ref().and_then(|page| {
                page.semantics.nodes.iter().find(|node| {
                    node.actions
                        .contains(&crate::accessibility::Action::Activate)
                        && node.name.to_ascii_lowercase().contains(&expected_gone)
                })
            });
            assert!(
                lingering.is_none(),
                "{host} still exposes an activatable control matching {expected_gone:?} after the requested click: {lingering:#?}; errors={errors:#?}"
            );
        }

        let fatal_errors = errors
            .iter()
            .filter(|error| {
                error.contains("Maximum call stack size exceeded")
                    || error.contains("stack overflow")
                    || error.contains("fatal runtime error")
            })
            .collect::<Vec<_>>();
        assert!(
            fatal_errors.is_empty(),
            "browser workload hit fatal engine errors: {fatal_errors:#?}"
        );
        if !errors.is_empty() {
            eprintln!("BROWSER_GATE errors ({}):", errors.len());
            for (index, error) in errors.iter().enumerate() {
                eprintln!("  [{index}] {error}");
            }
        }

        let dom = crate::dom::Dom::parse_document(&html);
        let node_count = dom.node_count();
        if let Ok(path) = std::env::var("TRUST_BROWSER_GATE_OUT") {
            std::fs::write(&path, html.as_bytes()).expect("write browser gate snapshot");
            eprintln!("BROWSER_GATE snapshot={}B -> {path}", html.len());
        }
        if let Some(expected_html) = expected_html {
            assert!(
                html.contains(&expected_html),
                "{host} did not reach the required HTML milestone {expected_html:?} after {settle_seconds}s; errors={errors:#?}"
            );
        }
        if host.ends_with("browserbench.org") {
            // Speedometer's result page marks a completed, statistically
            // usable run with `class="valid"` and writes the numeric score to
            // #result-number. Reaching the summary section alone is not enough:
            // a failed/aborted run can display that section too.
            assert!(
                html.contains("class=\"valid\""),
                "Speedometer did not report a valid summary; errors={errors:#?}"
            );
            let score = html
                .split("id=\"result-number\"")
                .nth(1)
                .and_then(|rest| rest.split('>').nth(1))
                .and_then(|rest| rest.split('<').next())
                .and_then(|value| value.trim().parse::<f64>().ok());
            assert!(
                score.is_some_and(|value| value.is_finite() && value > 0.0),
                "Speedometer summary did not contain a positive numeric score; errors={errors:#?}"
            );
        }
        if let Some(unexpected_html) = unexpected_html {
            assert!(
                !html.contains(&unexpected_html),
                "{host} retained forbidden HTML milestone {unexpected_html:?} after {settle_seconds}s; errors={errors:#?}"
            );
        }
        if expect_no_errors {
            assert!(
                errors.is_empty(),
                "{host} reported script errors during a no-error workload gate: {errors:#?}"
            );
        }
        let minimum_nodes = if host.ends_with("youtube.com") {
            // Presentation serialization varies with experiments, consent, and
            // history settings. A signed-out page with history disabled has no
            // recommendation cards and is currently about 570 nodes. This is
            // only an empty-shell guard; the search control below is the
            // functional acceptance condition.
            400
        } else if host.ends_with("twitch.tv") {
            800
        } else if host.ends_with("steampowered.com") {
            // Steam's useful presentation is currently about 2,450 nodes; the
            // old 3,000-node assertion rejected a complete storefront. Semantic
            // landmarks below are the acceptance condition, not raw bulk.
            2_000
        } else {
            std::env::var("TRUST_BROWSER_GATE_MIN_NODES")
                .ok()
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap_or(1)
        };
        assert!(
            node_count >= minimum_nodes,
            "{host} stopped at {node_count} DOM nodes; expected at least {minimum_nodes}; errors={errors:#?}"
        );

        if host.ends_with("youtube.com") {
            let has_search_input = dom.descendants(crate::dom::DOCUMENT).any(|node| {
                dom.tag_name(node) == Some("input")
                    && (dom.attr(node, "id") == Some("search")
                        || dom.attr(node, "name") == Some("search_query")
                        || dom
                            .attr(node, "aria-label")
                            .is_some_and(|label| label.eq_ignore_ascii_case("search")))
            });
            assert!(
                has_search_input,
                "YouTube did not create its searchable input after {settle_seconds}s; nodes={node_count}; errors={errors:#?}"
            );
        } else if host.ends_with("twitch.tv") {
            let has_search_input = dom.descendants(crate::dom::DOCUMENT).any(|node| {
                dom.tag_name(node) == Some("input")
                    && (dom.attr(node, "data-a-target") == Some("tw-input")
                        || dom.attr(node, "aria-label") == Some("Search Input"))
            });
            let has_front_page_carousel = dom
                .descendants(crate::dom::DOCUMENT)
                .any(|node| dom.attr(node, "data-a-target") == Some("front-page-carousel"));
            let live_channel_cards = dom
                .descendants(crate::dom::DOCUMENT)
                .filter(|node| dom.attr(*node, "data-a-target") == Some("side-nav-live-status"))
                .count();
            let front_page_channel_cards = dom
                .descendants(crate::dom::DOCUMENT)
                .filter(|node| dom.attr(*node, "data-a-target") == Some("preview-card-title-link"))
                .count();
            assert!(
                has_search_input
                    && has_front_page_carousel
                    && (front_page_channel_cards >= 3 || live_channel_cards >= 3),
                "Twitch homepage milestones missing after {settle_seconds}s: search={has_search_input} carousel={has_front_page_carousel} front_page_channel_cards={front_page_channel_cards} live_side_nav_cards={live_channel_cards}; nodes={node_count}; errors={errors:#?}"
            );
        } else if host.ends_with("steampowered.com") {
            let has_search_input = dom.descendants(crate::dom::DOCUMENT).any(|node| {
                dom.tag_name(node) == Some("input")
                    && dom.attr(node, "name") == Some("term")
                    && dom
                        .attr(node, "placeholder")
                        .is_some_and(|value| value.to_ascii_lowercase().contains("search"))
            });
            let has_featured_carousel = dom
                .descendants(crate::dom::DOCUMENT)
                .any(|node| dom.attr(node, "id") == Some("home_maincap_v7"));
            let has_special_offers = dom
                .descendants(crate::dom::DOCUMENT)
                .any(|node| dom.attr(node, "id") == Some("module_special_offers"));
            let tab_titles = dom
                .descendants(crate::dom::DOCUMENT)
                .filter(|node| {
                    dom.attr(*node, "class").is_some_and(|classes| {
                        classes
                            .split_ascii_whitespace()
                            .any(|class| class == "tab_item_title")
                    })
                })
                .count();
            assert!(
                has_search_input && has_featured_carousel && has_special_offers && tab_titles >= 5,
                "Steam storefront milestones missing after {settle_seconds}s: search={has_search_input} featured={has_featured_carousel} offers={has_special_offers} catalog_titles={tab_titles}; nodes={node_count}; errors={errors:#?}"
            );
        }

        if let Some(rendered) = rendered {
            eprintln!(
                "BROWSER_GATE resources={} eager={} deferred={}",
                rendered.image_urls.len(),
                rendered.eager_image_urls.len(),
                rendered.deferred_images.len()
            );
        }

        eprintln!(
            "BROWSER_GATE host={host} elapsed={:.3}s nodes={node_count} updates={updates} errors={}",
            started.elapsed().as_secs_f64(),
            errors.len()
        );
    }

    /// Fetch a page, run JS through settle, decode the REAL images, lay it out,
    /// and dump every image's rendered box (col,row,WxH) + the element's CSS.
    /// This is the ground-truth for image-sizing bugs (too-big/too-small).
    /// `TRUST_NET_DIAG=<url> [TRUST_DIAG_VP=WxH] cargo test --release img_box_diag -- --ignored --nocapture`
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "manual diagnostic, needs TRUST_NET_DIAG=<url>"]
    async fn img_box_diag() {
        let Ok(target) = std::env::var("TRUST_NET_DIAG") else {
            return;
        };
        let vp: (u16, u16) = std::env::var("TRUST_DIAG_VP")
            .ok()
            .and_then(|s| {
                s.split_once('x')
                    .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
            })
            .unwrap_or((200, 50));
        let url = parse_url(&target).unwrap();
        let resp = fetch(&Request::get(url.clone())).await.unwrap();
        let mut resp = execute_js(resp, vp, (8, 16), Default::default()).await;
        // Drain settle so the SPA swaps its data-image-url placeholders.
        let mut last_html: Option<String> = None;
        if let Some(mut live) = resp.live.take() {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(40);
            loop {
                let left = deadline.saturating_duration_since(std::time::Instant::now());
                if left.is_zero() {
                    break;
                }
                match tokio::time::timeout(left, live.events.recv()).await {
                    Ok(Some(crate::js::PageEvt::Updated { html, .. }))
                    | Ok(Some(crate::js::PageEvt::Static { html, .. })) => last_html = Some(html),
                    Ok(Some(_)) => {}
                    Ok(None) | Err(_) => break,
                }
            }
        }
        let html = last_html.unwrap_or_else(|| String::from_utf8_lossy(&resp.body).to_string());
        let dom = crate::dom::Dom::parse_document(&html);
        // Decode every real <img src>.
        let mut images = crate::layout2::ImageSizes::new();
        let mut srcs: Vec<(crate::dom::NodeId, url::Url)> = Vec::new();
        for id in dom.descendants(crate::dom::DOCUMENT) {
            if dom.tag_name(id) == Some("img")
                && let Some(src) = dom.attr(id, "src")
                && let Link::Http(u) = resolve(&url, src)
            {
                srcs.push((id, u));
            }
        }
        for (_, u) in &srcs {
            if images.contains_key(u.as_str()) {
                continue;
            }
            if let Ok(r) = fetch(&Request::get(u.clone())).await
                && let Ok((im, _)) = crate::img::decode(&r.body)
            {
                images.insert(u.to_string(), (im.width(), im.height()));
            }
        }
        eprintln!("decoded {} images", images.len());
        let rows = crate::layout2::lay_out_document(
            &dom,
            &url,
            crate::layout2::TerminalViewport::new(vp.0 as usize, vp.1 as usize, 8.0, 16.0),
            &[],
            &crate::layout2::ControlMap::new(),
            &images,
            no_alpha(),
        )
        .rows;
        for (y, row) in rows.iter().enumerate() {
            let visual: std::collections::HashMap<usize, u16> =
                crate::layout2::visual_columns(row, &[], y)
                    .into_iter()
                    .map(|(item, col, _, _)| (item, col))
                    .collect();
            for (ii, it) in row.items.iter().enumerate() {
                if let Some(src) = &it.image {
                    let tail: String = src
                        .rsplit('/')
                        .next()
                        .unwrap_or(src)
                        .chars()
                        .take(30)
                        .collect();
                    let cw = dom.computed_style(it.node, "width").unwrap_or_default();
                    let ch = dom.computed_style(it.node, "height").unwrap_or_default();
                    let cls = dom
                        .attr(it.node, "class")
                        .unwrap_or("")
                        .chars()
                        .take(28)
                        .collect::<String>();
                    eprintln!(
                        "row{y:>3} col{:>3}→{:>3} {:>3}x{:<3} css(w={cw},h={ch}) [{cls}] {tail}",
                        it.col,
                        visual.get(&ii).copied().unwrap_or(it.col),
                        it.width,
                        it.height,
                    );
                }
            }
        }
    }

    /// report. `TRUST_NET_DIAG=https://… cargo test net_diag -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "manual diagnostic, needs TRUST_NET_DIAG=<url>"]
    async fn net_diag() {
        let Ok(target) = std::env::var("TRUST_NET_DIAG") else {
            eprintln!("set TRUST_NET_DIAG to a URL");
            return;
        };
        let url = parse_url(&target).expect("absolute http(s) url");
        let vp: (u16, u16) = std::env::var("TRUST_DIAG_VP")
            .ok()
            .and_then(|s| {
                s.split_once('x')
                    .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
            })
            .unwrap_or((80, 24));
        // TRUST_DIAG_COOKIE="name=value[; name2=value2]": seed the process
        // cookie jar BEFORE the cold fetch, so a cookie-gated SPA serves its
        // authenticated render instead of the login shell (the live login
        // handshake seeds the jar via the signin POST; a one-shot diag can't,
        // so it otherwise only ever sees the logged-out page).
        if let Ok(cookies) = std::env::var("TRUST_DIAG_COOKIE") {
            for c in cookies.split(';') {
                let c = c.trim();
                if !c.is_empty() {
                    set_cookie_from_js(&url, c);
                }
            }
        }
        // The diagnostic exercises a top-level address-bar navigation, not a
        // page `fetch()`. Preserve Fetch Metadata so servers that vary their
        // SSR response by navigation context (HTML Fetch Metadata §2) see the
        // same request shape as the real frontend.
        let mut navigation = Request::get(url.clone());
        set_navigation_metadata(&mut navigation, None);
        let mut response = fetch(&navigation).await.unwrap();
        // TRUST_DIAG_INJECT=<file>: splice a probe <script> at the very top of
        // <head> so it runs before the page's own scripts (mirror-harness style).
        if let Ok(inj) = std::env::var("TRUST_DIAG_INJECT") {
            let js = std::fs::read_to_string(&inj).unwrap();
            let mut body = String::from_utf8_lossy(&response.body).to_string();
            let at = body
                .find("<head>")
                .map(|i| i + "<head>".len())
                .or_else(|| body.find("<head "))
                .unwrap_or(0);
            body.insert_str(at, &format!("<script>{js}</script>"));
            response.body = body.into_bytes();
        }
        eprintln!(
            "fetched: status={} content_type={:?} body={}B vp={vp:?}",
            response.status,
            response.content_type,
            response.body.len()
        );
        let mut response = execute_js(response, vp, (8, 16), Default::default()).await;
        eprintln!("js outcome: {:?}", response.js);
        eprintln!("live: {}", response.live.is_some());
        eprintln!(
            "body after: {}",
            String::from_utf8_lossy(&response.body[..response.body.len().min(1200)])
        );
        // TRUST_DIAG_SETTLE: for a LIVE page, `execute_js` returns the
        // interactive SHELL (before `settle_page` fires `load` and drains
        // background timers), so the default dump misses anything an SPA mounts
        // after first paint — and the console/errors of that work. Drain the
        // actor's post-shell `Updated` events to capture the SETTLED render +
        // accumulated console instead. (A blank-shell SPA whose mount throws
        // only AFTER the shell — e.g. pixiv's React login — is invisible
        // without this.)
        if std::env::var_os("TRUST_DIAG_SETTLE").is_some()
            && let Some(live) = response.live.as_mut()
        {
            let __drain: usize = std::env::var("TRUST_DIAG_DRAIN")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(6);
            let __to: u64 = std::env::var("TRUST_DIAG_DRAIN_TO")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(20);
            for _ in 0..__drain {
                match tokio::time::timeout(Duration::from_secs(__to), live.events.recv()).await {
                    Ok(Some(crate::js::PageEvt::Updated { html, outcome })) => {
                        eprintln!(
                            "SETTLED EVT errors={:?} console={:?}",
                            outcome.errors, outcome.console
                        );
                        response.body = html.into_bytes();
                    }
                    Ok(Some(other)) => {
                        eprintln!("SETTLED EVT {other:?}");
                    }
                    _ => {
                        eprintln!("SETTLED <no more within 20s>");
                        break;
                    }
                }
            }
        }
        // TRUST_DIAG_CLICK=<nodeid>: dispatch a click to that live node and drain
        // events, to see whether it navigates / re-renders / errors.
        if let Ok(nstr) = std::env::var("TRUST_DIAG_CLICK")
            && let Some(live) = response.live.as_mut()
        {
            let node: usize = nstr.parse().unwrap();
            eprintln!("DIAG_CLICK -> node {node}");
            let _ = live.handle.cmds.send(crate::js::PageCmd::Click(node)).await;
            for _ in 0..10 {
                match tokio::time::timeout(Duration::from_secs(20), live.events.recv()).await {
                    Ok(Some(crate::js::PageEvt::Navigate(u))) => {
                        eprintln!("CLICK EVT Navigate({u})");
                    }
                    Ok(Some(crate::js::PageEvt::Updated { html, outcome })) => {
                        eprintln!(
                            "CLICK EVT Updated errors={:?} console={:?}",
                            outcome.errors, outcome.console
                        );
                        response.body = html.into_bytes();
                    }
                    Ok(Some(other)) => eprintln!("CLICK EVT {other:?}"),
                    _ => {
                        eprintln!("CLICK <no more within 20s>");
                        break;
                    }
                }
            }
        }
        // TRUST_DIAG_SCROLL=N: drive infinite scroll — send N PageCmd::Scroll
        // toward the document bottom (huge y, setScroll clamps to the bottom; as
        // content loads the bottom moves so successive steps make progress),
        // draining + capturing the render between. Pair with TRUST_NET_TRACE=1 to
        // see whether the page fetches more on scroll.
        if let Ok(nstr) = std::env::var("TRUST_DIAG_SCROLL")
            && let Some(live) = response.live.as_mut()
        {
            let n: usize = nstr.parse().unwrap_or(5);
            let step: f64 = std::env::var("TRUST_DIAG_SCROLL_STEP")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(100_000.0);
            for i in 1..=n {
                let y = (i as f64) * step;
                eprintln!("DIAG_SCROLL step {i}/{n} -> y={y}");
                let _ = live
                    .handle
                    .cmds
                    .send(crate::js::PageCmd::Scroll { x: 0.0, y })
                    .await;
                for _ in 0..8 {
                    match tokio::time::timeout(Duration::from_secs(4), live.events.recv()).await {
                        Ok(Some(crate::js::PageEvt::Updated { html, outcome })) => {
                            eprintln!("SCROLL EVT errors={:?}", outcome.errors);
                            response.body = html.into_bytes();
                        }
                        Ok(Some(other)) => eprintln!("SCROLL EVT {other:?}"),
                        _ => break,
                    }
                }
            }
        }
        if let Ok(out) = std::env::var("TRUST_NET_DIAG_OUT") {
            std::fs::write(&out, &response.body).unwrap();
            eprintln!("full post-JS body ({}B) -> {out}", response.body.len());
        }
        drop(response.live.take());
    }

    #[test]
    fn a_subtree_patch_keeps_the_documents_rem_basis() {
        // The size-fighting bug: a patch fragment re-parses STANDALONE, so its
        // synthesized root reset the `rem` basis to 16px — rem lengths inside
        // an incremental patch resolved 1.6× larger than the full parse on a
        // 62.5%-root page (archive.org's `minmax(16rem,1fr)` tile grid flipped
        // 3↔5 columns between live updates and the first full resync).
        // `serialize_patch` now carries the real root on an `<html>` shell and
        // the boundary's inherited font-size resolved to px.
        let mut dom = crate::dom::Dom::parse_document(
            r#"<html style="font-size:62.5%"><body><div id="b"><div style="width:30rem"><img src="/x.jpg" style="width:100%"></div></div></body></html>"#,
        );
        let b = dom.get_by_id("b").unwrap();
        dom.set_attr(b, "data-trust-node", "42");
        let frag = dom.serialize_patch(b, &Default::default());
        assert!(
            frag.starts_with("<html style=\"font-size:10px;\">"),
            "the shell carries the document's rem basis: {frag}"
        );
        let url = parse_url("https://example.com/").unwrap();
        let mut images = crate::layout2::ImageSizes::new();
        images.insert("https://example.com/x.jpg".to_owned(), (10, 5));
        let laid = lay_subtree_patch(
            &url,
            frag.as_bytes(),
            200,
            (200, 64),
            (8, 16),
            &images,
            42,
            false,
            (0.0, 0.0),
        )
        .unwrap();
        let img_w = laid
            .rows
            .iter()
            .flat_map(|r| &r.items)
            .find(|i| i.image.is_some())
            .map(|i| i.width)
            .expect("the image lays");
        assert_eq!(
            img_w, 38,
            "30rem at the 10px root = 300px = 38 cells, not 60"
        );
    }

    /// Lay out a (post-JS) HTML FILE and dump the rows + carousels, to see
    /// exactly what reaches the screen. `TRUST_LAYOUT_FILE=<html> [TRUST_DIAG_VP=WxH]
    /// [TRUST_LAYOUT_GREP=substr] cargo test layout_dump -- --ignored --nocapture`
    /// Add `TRUST_FRAG_DIAG=1` to also dump the resolved fragment tree
    /// (tag/x/y/w/h/clip per box) — for laid-right-but-paints-wrong bugs.
    #[tokio::test]
    #[ignore = "manual diagnostic, needs TRUST_LAYOUT_FILE=<html>"]
    async fn layout_dump() {
        let Ok(path) = std::env::var("TRUST_LAYOUT_FILE") else {
            eprintln!("set TRUST_LAYOUT_FILE to a post-JS html file");
            return;
        };
        let html = std::fs::read(&path).unwrap();
        let (w, vh): (usize, usize) = std::env::var("TRUST_DIAG_VP")
            .ok()
            .and_then(|s| {
                s.split_once('x')
                    .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().unwrap_or(0))))
            })
            .unwrap_or((80, 0));
        let grep = std::env::var("TRUST_LAYOUT_GREP").ok();
        let url = parse_url(
            &std::env::var("TRUST_DIAG_URL")
                .unwrap_or_else(|_| "https://store.steampowered.com/".into()),
        )
        .unwrap();
        // TRUST_LAYOUT_SPRITE="path": prime the sprite sheet from a local file
        // (keyed at EVERY external `<use>` sheet the doc references, resolved
        // against `url`) so an offline replay resolves external-sprite icons.
        if let Ok(p) = std::env::var("TRUST_LAYOUT_SPRITE")
            && let Ok(text) = std::fs::read_to_string(&p)
        {
            for sheet in crate::js::sprite_use_sheets(&String::from_utf8_lossy(&html)) {
                if let Link::Http(abs) = resolve(&url, &sheet) {
                    crate::dom::prime_sprite_sheet(abs.as_str(), &text);
                }
            }
        }
        // TRUST_LAYOUT_IMG_CELL=WxH: seed EVERY <img>'s src with a decoded cell
        // box, so a layout that only shows the real box once decoded (an
        // `object-fit`/`width:100%` product tile) can be reproduced offline.
        let mut images = crate::layout2::ImageSizes::new();
        if let Ok(spec) = std::env::var("TRUST_LAYOUT_IMG_CELL")
            && let Some((cw, ch)) = spec
                .split_once('x')
                .and_then(|(a, b)| Some((a.parse::<u16>().ok()?, b.parse::<u16>().ok()?)))
        {
            let mut probe = crate::dom::Dom::parse_document(&decode_body("text/html", &html));
            probe.rewrite_inline_svgs(Some(&url));
            for id in 0..probe.node_count() {
                if probe.tag_name(id) == Some("img")
                    && let Some(src) = probe.attr(id, "src")
                {
                    let key = if src.trim().starts_with("data:") {
                        src.trim().to_string()
                    } else if let Link::Http(u) = resolve(&url, src.trim()) {
                        u.to_string()
                    } else {
                        continue;
                    };
                    // This diagnostic environment variable remains expressed
                    // in terminal cells; convert once at its terminal boundary.
                    images.insert(key, (u32::from(cw) * 8, u32::from(ch) * 16));
                }
            }
        }
        // TRUST_LAYOUT_IMG_ALPHA=1 marks every seeded <img> transparent, so the
        // offline replay can reproduce a P8 overlap composite.
        let alpha: std::collections::HashMap<String, bool> =
            if std::env::var_os("TRUST_LAYOUT_IMG_ALPHA").is_some() {
                images.keys().map(|k| (k.clone(), true)).collect()
            } else {
                std::collections::HashMap::new()
            };
        let doc = parse_seeded(
            &url,
            "text/html",
            &html,
            w,
            vh,
            (8, 16),
            None,
            &images,
            &alpha,
        );
        let last_nonempty = doc
            .rows
            .iter()
            .rposition(|r| !r.items.is_empty())
            .map(|i| i + 1)
            .unwrap_or(0);
        eprintln!(
            "=== DOC: {} rows total, last non-empty row {} ({} blank trailing rows) ===",
            doc.rows.len(),
            last_nonempty,
            doc.rows.len().saturating_sub(last_nonempty),
        );
        // TRUST_LAYOUT_MEASURE=<id-substr>: print the measure_boxes (getBounding
        // ClientRect) geometry for elements whose id or class contains the substr.
        if let Ok(sub) = std::env::var("TRUST_LAYOUT_MEASURE") {
            let mdom = crate::dom::Dom::parse_document(&decode_body("text/html", &html));
            let (forms2, controls2) = extract_forms_arena(&mdom, &url, None);
            let boxes = crate::layout2::measure_boxes_terminal(
                &mdom,
                &url,
                (w, vh),
                &forms2,
                &controls2,
                (8, 16),
                &images,
            )
            .0;
            for id in 0..mdom.node_count() {
                let matches = mdom.attr(id, "id").is_some_and(|v| v.contains(&sub))
                    || mdom.attr(id, "class").is_some_and(|v| v.contains(&sub));
                if matches && let Some(r) = boxes.get(&id) {
                    eprintln!(
                        "  MEASURE n{id} <{}> .{} → left={:.0} top={:.0} w={:.0} h={:.0}",
                        mdom.tag_name(id).unwrap_or("?"),
                        mdom.attr(id, "class")
                            .unwrap_or("")
                            .split_whitespace()
                            .next()
                            .unwrap_or(""),
                        r.left,
                        r.top,
                        r.width,
                        r.height,
                    );
                }
            }
        }
        // Legend: reproduce the layout DOM (same NodeIds) so we can map an
        // item's `node` back to its element (tag/id/class + box/flex props).
        let mut legend_dom = crate::dom::Dom::parse_document(&decode_body("text/html", &html));
        legend_dom.rewrite_inline_svgs(Some(&url));
        let mut seen_nodes: std::collections::BTreeSet<usize> = Default::default();
        for (ri, row) in doc.rows.iter().enumerate() {
            let mut s = String::new();
            let visual: std::collections::HashMap<usize, u16> =
                crate::layout2::visual_columns(row, &doc.carousels, ri)
                    .into_iter()
                    .map(|(item, col, _, _)| (item, col))
                    .collect();
            for (ii, it) in row.items.iter().enumerate() {
                let t = it.text.replace('\n', "\\n");
                let tag = match &it.kind {
                    crate::layout2::ItemKind::Image => "IMG",
                    crate::layout2::ItemKind::Border => "BRD",
                    _ => "txt",
                };
                seen_nodes.insert(it.node);
                s.push_str(&format!(
                    "[c{}→{} w{} h{} {tag} n{} {:?}{}{}] ",
                    it.col,
                    visual.get(&ii).copied().unwrap_or(it.col),
                    it.width,
                    it.height,
                    it.node,
                    t,
                    if it.link.is_some() { "*" } else { "" },
                    if it.invisible { " INVIS" } else { "" }
                ));
            }
            if s.is_empty() {
                continue;
            }
            if let Some(g) = &grep
                && !s.to_lowercase().contains(&g.to_lowercase())
            {
                continue;
            }
            println!("r{ri:>3}: {s}");
        }
        println!("--- {} carousels ---", doc.carousels.len());
        for c in &doc.carousels {
            println!(
                "  rows {}..{} band {}..{} width {} stops {:?}",
                c.start, c.end, c.left, c.right, c.width, c.stops
            );
        }
        println!("--- {} regions ---", doc.regions.len());
        for (i, rg) in doc.regions.iter().enumerate() {
            println!(
                "  #{i} node n{} live={:?} start_row {} left {} {}x{} buffer {} rows",
                rg.node,
                rg.live_node,
                rg.start_row,
                rg.left,
                rg.width,
                rg.height,
                rg.buffer.len()
            );
            if let Some(g) = &grep {
                for (ri, row) in rg.buffer.iter().enumerate() {
                    let s: String = row
                        .items
                        .iter()
                        .map(|it| {
                            format!("[c{} w{} n{} {:?}] ", it.col, it.width, it.node, it.text)
                        })
                        .collect();
                    if s.to_lowercase().contains(&g.to_lowercase()) {
                        println!("    buf r{ri}: {s}");
                    }
                }
            }
        }
        // TRUST_LAYOUT_NODES=n1,n2,…: also print the legend for ancestors of
        // these nodes (parent chain), to see the flex containers above an item.
        let extra: std::collections::BTreeSet<usize> = std::env::var("TRUST_LAYOUT_NODES")
            .ok()
            .map(|s| {
                s.split(',')
                    .filter_map(|t| t.trim().trim_start_matches('n').parse().ok())
                    .collect()
            })
            .unwrap_or_default();
        for &nid in &extra {
            let mut cur = nid;
            loop {
                seen_nodes.insert(cur);
                match legend_dom.node(cur).parent {
                    Some(p) if p != cur => cur = p,
                    _ => break,
                }
            }
        }
        // The synthetic node id (usize::MAX) marks generated items (pseudo
        // content, tooltips) with no backing arena node — skip it.
        seen_nodes.retain(|&n| n < legend_dom.node_count());
        println!("--- legend ({} nodes) ---", seen_nodes.len());
        for &nid in &seen_nodes {
            let tag = legend_dom.tag_name(nid).unwrap_or("·").to_string();
            let id = legend_dom
                .attr(nid, "id")
                .map(|s| format!("#{s}"))
                .unwrap_or_default();
            let cls = legend_dom
                .attr(nid, "class")
                .map(|s| {
                    let c: String = s.split_whitespace().take(3).collect::<Vec<_>>().join(".");
                    if c.is_empty() {
                        String::new()
                    } else {
                        format!(".{c}")
                    }
                })
                .unwrap_or_default();
            let dtn = legend_dom
                .attr(nid, "data-trust-node")
                .map(|s| format!(" dtn={s}"))
                .unwrap_or_default();
            let g = |p: &str| legend_dom.computed_style(nid, p).unwrap_or_default();
            let disp = legend_dom.computed_display(nid).unwrap_or_default();
            println!(
                "  n{nid}: <{tag}>{id}{cls}{dtn} disp={disp} flex=({}/{}/{}) w={} minw={} maxw={} pos={} ws={} overflow={}/{}",
                g("flex-grow"),
                g("flex-shrink"),
                g("flex-basis"),
                g("width"),
                g("min-width"),
                g("max-width"),
                g("position"),
                g("white-space"),
                g("overflow"),
                g("overflow-x"),
            );
        }
    }

    /// Run a web-platform-tests page and report its testharness results.
    /// WPT's `testharness.js` only renders a visible table under a real
    /// runner; standalone it stays silent. So — exactly like a real WPT
    /// runner — we hook `add_completion_callback` (injected as a trailing
    /// script) and serialize each subtest's status+name+message into a
    /// `<pre id=wptresult>`, then read it back after settle. This is the
    /// gap-finder: every FAIL names a platform primitive we're missing or
    /// get wrong. `TRUST_NET_DIAG=<wpt url> [TRUST_DIAG_VP=WxH]
    /// cargo test --release wpt_diag -- --ignored --nocapture`
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "manual diagnostic, needs TRUST_NET_DIAG=<wpt url>"]
    async fn wpt_diag() {
        let Ok(target) = std::env::var("TRUST_NET_DIAG") else {
            eprintln!("set TRUST_NET_DIAG to a wpt.live test URL");
            return;
        };
        let vp: (u16, u16) = std::env::var("TRUST_DIAG_VP")
            .ok()
            .and_then(|s| {
                s.split_once('x')
                    .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
            })
            .unwrap_or((200, 50));
        let url = parse_url(&target).expect("absolute http(s) url");
        let mut resp = fetch(&Request::get(url)).await.unwrap();
        // Append the results hook AFTER the page's scripts (so testharness.js
        // has defined add_completion_callback and the page's tests are
        // registered). html5ever reparents a trailing script into <body>.
        let inject = r#"<script>(function(){
          function names(s){return ['PASS','FAIL','TIMEOUT','NOTRUN','OPTIONAL'][s]||('S'+s);}
          function put(id,txt){var p=document.createElement('pre');p.id=id;p.textContent=txt;(document.body||document.documentElement).appendChild(p);}
          function dump(tests,status){
            var s='HARNESS '+names(status.status)+(status.message?' '+status.message:'')+'\n';
            for(var i=0;i<tests.length;i++){s+=names(tests[i].status)+' | '+tests[i].name+(tests[i].message?' | '+tests[i].message:'')+'\n';}
            put('wptresult',s);
          }
          if(typeof add_completion_callback==='function'){add_completion_callback(dump);}
          else{put('wptresult','NO-HARNESS (testharness.js did not load/run)');}
        })();</script>"#;
        let mut body = String::from_utf8_lossy(&resp.body).to_string();
        // Insert after the page's own scripts so testharness.js is loaded and
        // the page's tests are registered. Prefer just before </head>.
        let at = ["</head>", "</body>", "</html>"]
            .iter()
            .find_map(|m| body.find(m))
            .unwrap_or(body.len());
        body.insert_str(at, inject);
        resp.body = body.into_bytes();

        let mut resp = execute_js(resp, vp, (8, 16), Default::default()).await;
        eprintln!("js outcome: {:?}", resp.js);
        let mut html = String::from_utf8_lossy(&resp.body).to_string();
        // Completion runs during settle, so a static page already carries the
        // result in its body — only drain a live page if it isn't there yet.
        if !html.contains("wptresult")
            && let Some(mut live) = resp.live.take()
        {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(70);
            loop {
                let left = deadline.saturating_duration_since(std::time::Instant::now());
                if left.is_zero() {
                    break;
                }
                match tokio::time::timeout(left, live.events.recv()).await {
                    Ok(Some(crate::js::PageEvt::Updated { html: h, .. }))
                    | Ok(Some(crate::js::PageEvt::Static { html: h, .. })) => {
                        html = h;
                        if html.contains("wptresult") {
                            break;
                        }
                    }
                    Ok(Some(_)) => {}
                    Ok(None) | Err(_) => break,
                }
            }
        }
        // Extract the injected <pre id=wptresult> content.
        let results = html
            .split_once("id=\"wptresult\">")
            .and_then(|(_, rest)| rest.split_once("</pre>"))
            .map(|(r, _)| html_unescape(r))
            .unwrap_or_else(|| "(no wptresult — harness never completed)".to_string());
        let fails = results.lines().filter(|l| l.starts_with("FAIL")).count();
        let passes = results.lines().filter(|l| l.starts_with("PASS")).count();
        eprintln!("=== WPT {target}\n=== {passes} PASS / {fails} FAIL ===\n{results}");
        if let Ok(out) = std::env::var("TRUST_NET_DIAG_OUT") {
            std::fs::write(&out, &html).unwrap();
        }
    }

    /// Minimal HTML entity decode for reading serialized text back out.
    fn html_unescape(s: &str) -> String {
        s.replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&#39;", "'")
            .replace("&amp;", "&")
    }

    /// A real module graph: page → entry module → static import →
    /// dynamic import, all fetched through our stack.
    #[tokio::test]
    async fn module_graphs_load_over_the_network() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let mut req = Vec::new();
                let mut buf = [0u8; 2048];
                while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                    match sock.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => req.extend_from_slice(&buf[..n]),
                    }
                }
                let text = String::from_utf8_lossy(&req).into_owned();
                let (ctype, body): (&str, &str) = if text.starts_with("GET /page ") {
                    (
                        "text/html",
                        "<body><div id=t></div>\
                         <script type=module src=\"/js/main.js\"></script></body>",
                    )
                } else if text.starts_with("GET /js/main.js ") {
                    (
                        "text/javascript",
                        "import { greet } from './lib.js';\n\
                         document.getElementById('t').textContent = greet('TRust');\n\
                         const dyn_ = await import('./extra.js');\n\
                         document.getElementById('t').textContent += dyn_.suffix;",
                    )
                } else if text.starts_with("GET /js/lib.js ") {
                    (
                        "text/javascript",
                        "export function greet(name) { return 'modules drive ' + name; }",
                    )
                } else if text.starts_with("GET /js/extra.js ") {
                    ("text/javascript", "export const suffix = ' — dynamically';")
                } else {
                    ("text/plain", "nope")
                };
                let reply = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nConnection: close\r\n\r\n{body}"
                );
                let _ = sock.write_all(reply.as_bytes()).await;
            }
        });

        let url = parse_url(&format!("http://127.0.0.1:{port}/page")).unwrap();
        let response = fetch(&Request::get(url)).await.unwrap();
        let mut response = execute_js(response, (80, 24), (8, 16), Default::default()).await;
        let mut body = String::from_utf8_lossy(&response.body).into_owned();
        if !body.contains("modules drive TRust — dynamically") {
            let live = response
                .live
                .as_mut()
                .expect("top-level await keeps the module page live");
            body = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    match live.events.recv().await {
                        Some(crate::js::PageEvt::Updated { html, .. })
                            if html.contains("modules drive TRust — dynamically") =>
                        {
                            break html;
                        }
                        Some(_) => {}
                        None => panic!("module page actor closed before evaluation completed"),
                    }
                }
            })
            .await
            .expect("module graph render timed out");
        }
        let outcome = response.js.as_ref().expect("js ran");
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert_eq!(outcome.modules_skipped, 0);
        assert!(body.contains("modules drive TRust — dynamically"), "{body}");
        server.abort();
    }

    // A DIAMOND module graph (two siblings import one shared module)
    // served by a CONCURRENT, delaying server, so the two loads of the
    // shared module genuinely overlap. Before the loader serialized its
    // fetch+parse, this raced: duplicate module records could corrupt the
    // graph into a stack overflow (archive.org exposed this reliably). The
    // shared module must load and EVALUATE exactly once.
    #[tokio::test]
    async fn concurrent_diamond_module_graph_loads_shared_once() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                // One task per connection so the sibling/shared module
                // fetches actually overlap (a serial server would hide
                // the race this test exists to catch).
                tokio::spawn(async move {
                    let mut req = Vec::new();
                    let mut buf = [0u8; 2048];
                    while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                        match sock.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => req.extend_from_slice(&buf[..n]),
                        }
                    }
                    let text = String::from_utf8_lossy(&req).into_owned();
                    let (ctype, body): (&str, &str) = if text.starts_with("GET /page ") {
                        (
                            "text/html",
                            "<body><div id=t></div>\
                             <script type=module src=\"/m/entry.js\"></script></body>",
                        )
                    } else if text.starts_with("GET /m/entry.js ") {
                        (
                            "text/javascript",
                            "import './a.js';\nimport './b.js';\n\
                             document.getElementById('t').textContent = 'shared=' + window.__shared;",
                        )
                    } else if text.starts_with("GET /m/a.js ") || text.starts_with("GET /m/b.js ") {
                        ("text/javascript", "import './shared.js';")
                    } else if text.starts_with("GET /m/shared.js ") {
                        (
                            "text/javascript",
                            "window.__shared = (window.__shared || 0) + 1;",
                        )
                    } else {
                        ("text/plain", "nope")
                    };
                    // Delay so the two shared.js loads are in flight at once.
                    tokio::time::sleep(std::time::Duration::from_millis(60)).await;
                    let reply = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nConnection: close\r\n\r\n{body}"
                    );
                    let _ = sock.write_all(reply.as_bytes()).await;
                });
            }
        });

        let url = parse_url(&format!("http://127.0.0.1:{port}/page")).unwrap();
        let response = fetch(&Request::get(url)).await.unwrap();
        // Reaching past this line at all is half the test: the old race
        // could stack-overflow and abort the whole test binary.
        let response = execute_js(response, (80, 24), (8, 16), Default::default()).await;
        let body = String::from_utf8_lossy(&response.body);
        let outcome = response.js.as_ref().expect("js ran");
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert_eq!(outcome.modules_skipped, 0);
        assert!(
            body.contains("shared=1"),
            "shared module ran exactly once: {body}"
        );
        server.abort();
    }

    /// `<link rel=stylesheet>` fetched over the wire feeds the
    /// display/visibility cascade end to end.
    #[tokio::test]
    async fn link_stylesheets_feed_the_cascade() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let mut req = Vec::new();
                let mut buf = [0u8; 2048];
                while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                    match sock.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => req.extend_from_slice(&buf[..n]),
                    }
                }
                let text = String::from_utf8_lossy(&req).into_owned();
                let (ctype, body): (&str, &str) = if text.starts_with("GET /page ") {
                    (
                        "text/html",
                        "<head><link rel=\"stylesheet\" href=\"/s.css\"></head>\
                         <body><p class=sec>css secret</p><p>css public</p>\
                         <script>void 0;</script></body>",
                    )
                } else if text.starts_with("GET /s.css ") {
                    ("text/css", ".sec { display: none }")
                } else {
                    ("text/plain", "nope")
                };
                let reply = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nConnection: close\r\n\r\n{body}"
                );
                let _ = sock.write_all(reply.as_bytes()).await;
            }
        });

        let url = parse_url(&format!("http://127.0.0.1:{port}/page")).unwrap();
        let response = fetch(&Request::get(url)).await.unwrap();
        let response = execute_js(response, (80, 24), (8, 16), Default::default()).await;
        let body = String::from_utf8_lossy(&response.body);
        let outcome = response.js.as_ref().expect("js ran");
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert!(!body.contains("css secret"), "{body}");
        assert!(body.contains("css public"), "{body}");
        server.abort();
    }

    /// REAL Lit 3 (lit-core.min.js from target/canary) driving the full
    /// pipeline: reactive properties, tagged
    /// templates, shadow rendering, @click bindings.
    /// `cargo test --release lit_canary -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "manual acceptance: needs target/canary/lit-core.min.js"]
    async fn lit_canary() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let Ok(lit) = std::fs::read("target/canary/lit-core.min.js") else {
            eprintln!(
                "no target/canary/lit-core.min.js — curl it from cdn.jsdelivr.net/gh/lit/dist@3/core/lit-core.min.js"
            );
            return;
        };

        const APP: &str = r#"
import { LitElement, html } from './lit-core.min.js';
class LitCounter extends LitElement {
    static properties = { count: { type: Number } };
    constructor() { super(); this.count = 0; }
    render() {
        return html`<p>Lit says: ${this.count} clicks</p>
            <button @click=${() => { this.count = this.count + 1; }}>more</button>`;
    }
}
customElements.define('lit-counter', LitCounter);
"#;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let lit_body = lit.clone();
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let mut req = Vec::new();
                let mut buf = [0u8; 2048];
                while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                    match sock.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => req.extend_from_slice(&buf[..n]),
                    }
                }
                let text = String::from_utf8_lossy(&req).into_owned();
                let (ctype, body): (&str, Vec<u8>) = if text.starts_with("GET /page ") {
                    (
                        "text/html",
                        b"<body><lit-counter></lit-counter>\
                          <script type=module src=\"/app.js\"></script></body>"
                            .to_vec(),
                    )
                } else if text.starts_with("GET /app.js ") {
                    ("text/javascript", APP.as_bytes().to_vec())
                } else if text.starts_with("GET /lit-core.min.js ") {
                    ("text/javascript", lit_body.clone())
                } else {
                    ("text/plain", b"nope".to_vec())
                };
                let mut reply = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nConnection: close\r\n\r\n"
                )
                .into_bytes();
                reply.extend_from_slice(&body);
                let _ = sock.write_all(&reply).await;
            }
        });

        let url = parse_url(&format!("http://127.0.0.1:{port}/page")).unwrap();
        let response = fetch(&Request::get(url)).await.unwrap();
        let mut response = execute_js(response, (80, 24), (8, 16), Default::default()).await;
        let body = String::from_utf8_lossy(&response.body).into_owned();
        let outcome = response.js.as_ref().expect("js ran");
        eprintln!(
            "lit page outcome: errors={:?} fetches={}",
            outcome.errors, outcome.fetches
        );
        eprintln!("rendered: {body}");
        assert!(
            body.contains("Lit says:") && body.contains("0 clicks"),
            "{body}"
        );

        let mut live = response.live.take().expect("lit page stays alive");
        let button: usize = body
            .split("x-trust-js:")
            .nth(1)
            .and_then(|r| r.split(':').next())
            .expect("button marker")
            .parse()
            .unwrap();
        live.handle
            .cmds
            .send(crate::js::PageCmd::Click(button))
            .await
            .unwrap();
        match tokio::time::timeout(Duration::from_secs(10), live.events.recv()).await {
            Ok(Some(crate::js::PageEvt::Updated { html, outcome })) => {
                eprintln!("after click: errors={:?}", outcome.errors);
                assert!(html.contains("1 clicks"), "{html}");
            }
            Ok(other) => panic!("expected Updated, got {other:?}"),
            Err(_) => panic!("click produced no event within 10s"),
        }
        server.abort();
    }

    #[test]
    fn script_sources_cannot_pivot_into_private_space() {
        let page = Url::parse("https://example.com/a").unwrap();
        let ok = |s: &str| subresource_allowed(&page, &Url::parse(s).unwrap());
        assert!(ok("https://cdn.example.net/lib.js"));
        assert!(ok("https://example.com/own.js"));
        assert!(!ok("http://localhost/x.js"));
        assert!(!ok("http://127.0.0.1/x.js"));
        assert!(!ok("http://192.168.1.1/x.js"));
        assert!(!ok("http://10.0.0.7/x.js"));
        assert!(!ok("http://[::1]/x.js"));
        // ...but a page already on localhost may use its own host.
        let local_page = Url::parse("http://localhost:8000/").unwrap();
        assert!(subresource_allowed(
            &local_page,
            &Url::parse("http://localhost:8000/app.js").unwrap()
        ));
    }

    #[tokio::test]
    async fn a_referer_unblocks_a_hotlink_protected_image() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        // A booru-style image CDN: a refererless GET is bounced with a 302 to
        // a placeholder; a GET carrying a Referer gets the bytes. This is the
        // gelbooru thumbnail behaviour, hermetic. `set_referrer` (what the
        // image-load path now applies) must make the second case happen.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            for _ in 0..2 {
                let (mut sock, _) = listener.accept().await.unwrap();
                let mut req = Vec::new();
                let mut buf = [0u8; 2048];
                while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                    match sock.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => req.extend_from_slice(&buf[..n]),
                    }
                }
                let head = String::from_utf8_lossy(&req);
                let has_referer = head.lines().any(|l| {
                    l.to_ascii_lowercase().starts_with("referer:") && l.contains("127.0.0.1")
                });
                let resp: &[u8] = if has_referer {
                    b"HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: 3\r\nConnection: close\r\n\r\nPNG"
                } else {
                    b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                };
                let _ = sock.write_all(resp).await;
            }
        });
        let page = parse_url(&format!("http://127.0.0.1:{port}/index.php")).unwrap();
        let img = parse_url(&format!("http://127.0.0.1:{port}/thumb.png")).unwrap();

        // Without a Referer: the CDN bounces us (non-200 → no image).
        let bare = fetch(&Request::get(img.clone())).await.unwrap();
        assert_eq!(bare.status, 403, "refererless request is bounced");

        // With the browser-default Referer: we get the image bytes.
        let mut req = Request::get(img);
        set_referrer(&mut req, &page);
        let ok = fetch(&req).await.unwrap();
        assert_eq!(ok.status, 200, "a Referer unblocks the image");
        assert_eq!(ok.body, b"PNG");
        server.abort();
    }

    #[tokio::test]
    async fn image_subresource_uses_image_destination_accept() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let captured = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let cap2 = captured.clone();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut req = Vec::new();
            let mut buf = [0u8; 2048];
            while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                match sock.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => req.extend_from_slice(&buf[..n]),
                }
            }
            *cap2.lock().unwrap() = String::from_utf8_lossy(&req).into_owned();
            let _ = sock
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: 3\r\nConnection: close\r\n\r\nPNG",
                )
                .await;
        });

        let url = parse_url(&format!("http://127.0.0.1:{port}/thumb.png")).unwrap();
        let mut request = Request::get(url);
        set_image_accept(&mut request);
        let response = fetch(&request).await.unwrap();
        assert_eq!(response.content_type, "image/png");
        assert_eq!(response.body, b"PNG");
        server.abort();

        let head = captured.lock().unwrap().clone();
        assert!(
            head.contains(&format!("Accept: {IMAGE_ACCEPT}")),
            "image destination Accept reaches the wire: {head}"
        );
        assert_eq!(head.matches("Accept:").count(), 1);

        // An explicitly selected image representation remains authoritative.
        let mut explicit = Request::get(parse_url("https://example.test/x.webp").unwrap());
        explicit
            .headers
            .push((String::from("accept"), String::from("image/avif")));
        set_image_accept(&mut explicit);
        assert_eq!(
            explicit
                .headers
                .iter()
                .filter(|(name, _)| name.eq_ignore_ascii_case("accept"))
                .map(|(_, value)| value.as_str())
                .collect::<Vec<_>>(),
            vec!["image/avif"]
        );
    }

    #[tokio::test]
    async fn fetch_metadata_preserves_redirect_taint_and_final_referrer() {
        use std::sync::Arc;
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let a = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let b = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a_url = format!("http://127.0.0.1:{}", a.local_addr().unwrap().port());
        let b_url = format!("http://127.0.0.1:{}", b.local_addr().unwrap().port());
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        async fn serve(
            listener: tokio::net::TcpListener,
            replies: Vec<String>,
            requests: Arc<std::sync::Mutex<Vec<String>>>,
        ) {
            for reply in replies {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let mut buffer = [0; 1024];
                while !bytes.windows(4).any(|w| w == b"\r\n\r\n") {
                    let n = stream.read(&mut buffer).await.unwrap();
                    assert_ne!(n, 0);
                    bytes.extend_from_slice(&buffer[..n]);
                }
                requests
                    .lock()
                    .unwrap()
                    .push(String::from_utf8(bytes).unwrap());
                stream.write_all(reply.as_bytes()).await.unwrap();
            }
        }
        let a_job = tokio::spawn(serve(
            a,
            vec![
                format!(
                    "HTTP/1.1 302 Found\r\nLocation: {b_url}/hop\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                ),
                "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok".into(),
            ],
            requests.clone(),
        ));
        let b_job = tokio::spawn(serve(
            b,
            vec![format!(
                "HTTP/1.1 302 Found\r\nLocation: {a_url}/final\r\nReferrer-Policy: origin, no-referrer\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            )],
            requests.clone(),
        ));
        let mut request = Request::get(Url::parse(&format!("{a_url}/start")).unwrap());
        set_referrer(
            &mut request,
            &Url::parse(&format!("{a_url}/document?q=1#secret")).unwrap(),
        );
        let details = fetch_with_metadata(&request, Default::default())
            .await
            .unwrap();
        assert_eq!(details.response.body, b"ok");
        assert_eq!(
            details.url_list.iter().map(Url::as_str).collect::<Vec<_>>(),
            vec![
                format!("{a_url}/start"),
                format!("{b_url}/hop"),
                format!("{a_url}/final")
            ]
        );
        assert_eq!(details.referrer, "");
        a_job.await.unwrap();
        b_job.await.unwrap();
        let requests = requests.lock().unwrap();
        assert!(requests[0].contains(&format!("Referer: {a_url}/document?q=1\r\n")));
        assert!(requests[1].contains(&format!("Referer: {a_url}/\r\n")));
        assert!(!requests[2].to_ascii_lowercase().contains("referer:"));
    }

    #[tokio::test]
    async fn a_referer_is_re_evaluated_across_a_cross_origin_redirect() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        // Referrer policy is per hop. A same-origin request carries the FULL
        // page URL; if it redirects to a DIFFERENT origin, the Referer must be
        // reduced to the origin (no path leak), exactly as a browser does.
        async fn one_shot<F>(reply: F) -> (u16, std::sync::Arc<std::sync::Mutex<String>>)
        where
            F: FnOnce(u16) -> Vec<u8> + Send + 'static,
        {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let cap = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
            let cap2 = cap.clone();
            tokio::spawn(async move {
                let (mut sock, _) = listener.accept().await.unwrap();
                let mut req = Vec::new();
                let mut buf = [0u8; 2048];
                while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                    match sock.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => req.extend_from_slice(&buf[..n]),
                    }
                }
                *cap2.lock().unwrap() = String::from_utf8_lossy(&req).into_owned();
                let _ = sock.write_all(&reply(port)).await;
            });
            (port, cap)
        }

        // B: the redirect target on a different origin — captures the Referer.
        let (b_port, b_cap) = one_shot(|_| {
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok".to_vec()
        })
        .await;
        // A: same origin as the page; 302s to B.
        let (a_port, _a_cap) = one_shot(move |_| {
            format!(
                "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{b_port}/landing\r\n\
                 Content-Length: 0\r\nConnection: close\r\n\r\n"
            )
            .into_bytes()
        })
        .await;

        let page = parse_url(&format!("http://127.0.0.1:{a_port}/some/page?q=1")).unwrap();
        let start = parse_url(&format!("http://127.0.0.1:{a_port}/start")).unwrap();
        let mut req = Request::get(start);
        set_referrer(&mut req, &page); // same-origin → full page URL
        assert!(
            req.headers
                .iter()
                .any(|(k, v)| k == "Referer" && v.contains("/some/page"))
        );

        let resp = fetch(&req).await.unwrap();
        assert_eq!(resp.body, b"ok", "followed the redirect to B");
        let at_b = b_cap.lock().unwrap().clone();
        let referer_line = at_b
            .lines()
            .find(|l| l.to_ascii_lowercase().starts_with("referer:"))
            .unwrap_or("");
        assert!(
            referer_line.contains(&format!("http://127.0.0.1:{a_port}/"))
                && !referer_line.contains("/some/page"),
            "cross-origin redirect reduced the Referer to the origin: {referer_line:?}"
        );
    }

    #[test]
    fn referrer_follows_strict_origin_when_cross_origin() {
        let page = Url::parse("https://gelbooru.com/index.php?page=post&s=list#frag").unwrap();
        // Cross-origin (the image CDN): origin only, with a trailing slash —
        // exactly what unblocks a hotlink-protected booru thumbnail.
        let cdn = Url::parse("https://img4.gelbooru.com/thumbnails/x.jpg").unwrap();
        assert_eq!(
            referrer_for(&page, &cdn).as_deref(),
            Some("https://gelbooru.com/")
        );
        // Same-origin: full URL, fragment stripped, query kept.
        let same = Url::parse("https://gelbooru.com/css/style.css").unwrap();
        assert_eq!(
            referrer_for(&page, &same).as_deref(),
            Some("https://gelbooru.com/index.php?page=post&s=list")
        );
        // Credentials are never leaked in a same-origin referrer.
        let creds = Url::parse("https://user:pw@site.test/a").unwrap();
        let creds_sub = Url::parse("https://site.test/b.js").unwrap();
        assert_eq!(
            referrer_for(&creds, &creds_sub).as_deref(),
            Some("https://site.test/a")
        );
        // HTTPS → HTTP downgrade: send nothing.
        let insecure = Url::parse("http://img.site.test/x.jpg").unwrap();
        assert_eq!(referrer_for(&page, &insecure), None);
        // A non-http(s) page (data:, file:) has no referrer to give.
        let data = Url::parse("data:text/html,<p>").unwrap();
        assert_eq!(referrer_for(&data, &cdn), None);

        // set_referrer wires it onto a request, and never clobbers a
        // page-supplied one.
        let mut req = Request::get(cdn.clone());
        set_referrer(&mut req, &page);
        assert_eq!(
            req.headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("referer"))
                .map(|(_, v)| v.as_str()),
            Some("https://gelbooru.com/")
        );
        let mut req2 = Request::get(cdn);
        req2.headers
            .push(("Referer".into(), "https://override.test/".into()));
        set_referrer(&mut req2, &page);
        assert_eq!(
            req2.headers
                .iter()
                .filter(|(k, _)| k.eq_ignore_ascii_case("referer"))
                .count(),
            1,
            "page-supplied Referer is kept, not duplicated"
        );
    }

    #[test]
    fn ad_and_tracker_networks_are_blocked() {
        // Exact host and subdomains of a known ad/tracker network are blocked;
        // unrelated hosts (even a lookalike that merely contains the name) are
        // not. A terminal browser can't render ads — running their SDKs only
        // breaks pages (erome's pop-under gate). General, host-based, no
        // per-site sniffing.
        assert!(is_ad_or_tracker_host("tsyndicate.com"));
        assert!(is_ad_or_tracker_host("cdn.tsyndicate.com"));
        assert!(is_ad_or_tracker_host("a.magsrv.com"));
        assert!(is_ad_or_tracker_host("www.googletagmanager.com"));
        assert!(!is_ad_or_tracker_host("example.com"));
        assert!(!is_ad_or_tracker_host("nottsyndicate.com"));
        assert!(!is_ad_or_tracker_host("tsyndicate.com.evil.example"));
        let page = Url::parse("https://www.erome.com/").unwrap();
        assert!(!subresource_allowed(
            &page,
            &Url::parse("https://cdn.tsyndicate.com/sdk/v1/n.js").unwrap()
        ));
        assert!(subresource_allowed(
            &page,
            &Url::parse("https://www.erome.com/js/main.js").unwrap()
        ));
    }

    #[tokio::test]
    async fn speaks_http_with_redirects_chunking_and_post() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let mut req = Vec::new();
                let mut buf = [0u8; 2048];
                // Read until end of headers (plus any body by length).
                while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                    match sock.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => req.extend_from_slice(&buf[..n]),
                    }
                }
                let header_end = req
                    .windows(4)
                    .position(|w| w == b"\r\n\r\n")
                    .map_or(req.len(), |p| p + 4);
                let content_length = String::from_utf8_lossy(&req[..header_end])
                    .lines()
                    .find_map(|l| {
                        let (k, v) = l.split_once(':')?;
                        k.eq_ignore_ascii_case("content-length")
                            .then(|| v.trim().parse::<usize>().ok())?
                    })
                    .unwrap_or(0);
                while req.len() < header_end + content_length {
                    match sock.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => req.extend_from_slice(&buf[..n]),
                    }
                }
                let text = String::from_utf8_lossy(&req).into_owned();
                let reply: Vec<u8> = if text.starts_with("GET /old ") {
                    b"HTTP/1.1 302 Found\r\nLocation: /new\r\n\r\n".to_vec()
                } else if text.starts_with("GET /new ") {
                    assert!(text.contains("User-Agent: TRust/0.1"));
                    assert!(text.contains("Accept-Language: en-US,en;q=0.9\r\n"));
                    assert!(text.contains("Connection: keep-alive"));
                    // Chunked HTML with a link.
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\
                      Transfer-Encoding: chunked\r\n\r\n\
                      1c\r\n<h1>Arrived</h1><p><a href=\"\r\n\
                      15\r\n/next\">onward</a></p>\r\n\
                      0\r\n\r\n"
                        .to_vec()
                } else if text.starts_with("POST /prg ") {
                    // Post/Redirect/Get: 303 rewrites the method to GET.
                    b"HTTP/1.1 303 See Other\r\nLocation: /new\r\n\r\n".to_vec()
                } else if text.starts_with("POST /submit ") {
                    assert!(text.contains("Content-Type: application/x-www-form-urlencoded"));
                    assert!(text.ends_with("k=v&x=y"), "body arrived: {text:?}");
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\r\nposted ok".to_vec()
                } else if text.starts_with("POST /empty ") || text.starts_with("PUT /empty ") {
                    assert!(
                        text.lines()
                            .any(|line| line.eq_ignore_ascii_case("Content-Length: 0")),
                        "null-body POST/PUT needs explicit zero framing: {text:?}"
                    );
                    b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok".to_vec()
                } else {
                    b"HTTP/1.1 404 Nope\r\nContent-Length: 0\r\n\r\n".to_vec()
                };
                let _ = sock.write_all(&reply).await;
            }
        });

        // GET with a redirect into chunked HTML.
        let url = Url::parse(&format!("http://127.0.0.1:{port}/old")).unwrap();
        let response = fetch(&Request::get(url)).await.unwrap();
        assert_eq!(response.status, 200);
        assert!(response.url.path().ends_with("/new"), "followed redirect");
        let doc = parse(
            &response.url,
            &response.content_type,
            &response.body,
            60,
            0,
            &Default::default(),
        );
        assert_eq!(
            item(&doc, "Arrived").kind,
            crate::layout2::ItemKind::Heading(1)
        );
        let link = doc
            .rows
            .iter()
            .flat_map(|r| &r.items)
            .find_map(|it| it.link.as_ref())
            .expect("a link");
        assert_eq!(
            *link,
            Link::Http(Url::parse(&format!("http://127.0.0.1:{port}/next")).unwrap())
        );

        // POST carries content-type and body.
        let url = Url::parse(&format!("http://127.0.0.1:{port}/submit")).unwrap();
        let request = Request {
            method: String::from("POST"),
            url,
            body: Some((
                String::from("application/x-www-form-urlencoded"),
                b"k=v&x=y".to_vec(),
            )),
            headers: Vec::new(),
            fetch_metadata: None,
            timing_client: None,
            fetch_policy: None,
        };
        let response = fetch(&request).await.unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"posted ok");
        assert!(
            response.from_post,
            "a direct 2xx off the POST is a POST result"
        );

        // Fetch's HTTP-network algorithm and RFC 9110 §8.6 require explicit
        // zero framing for a null-body POST/PUT. YouTube's JSON API rejects an
        // unframed empty POST with 411 before its application sees it.
        for method in ["POST", "PUT"] {
            let url = Url::parse(&format!("http://127.0.0.1:{port}/empty")).unwrap();
            let request = Request {
                method: method.into(),
                url,
                body: None,
                headers: Vec::new(),
                fetch_metadata: None,
                timing_client: None,
                fetch_policy: None,
            };
            let response = fetch(&request).await.unwrap();
            assert_eq!(response.status, 200, "{method} null body was accepted");
            assert_eq!(response.body, b"ok");
        }

        // Post/Redirect/Get: the 303 hop lands on a GET, so the final page
        // is refetchable — NOT marked as a POST result.
        let url = Url::parse(&format!("http://127.0.0.1:{port}/prg")).unwrap();
        let request = Request {
            method: String::from("POST"),
            url,
            body: Some((
                String::from("application/x-www-form-urlencoded"),
                b"k=v".to_vec(),
            )),
            headers: Vec::new(),
            fetch_metadata: None,
            timing_client: None,
            fetch_policy: None,
        };
        let response = fetch(&request).await.unwrap();
        assert_eq!(response.status, 200);
        assert!(response.url.path().ends_with("/new"), "followed the 303");
        assert!(!response.from_post, "a PRG flow ends on a GET");

        server.abort();
    }

    #[tokio::test]
    async fn cross_origin_redirect_strips_authorization_and_preserves_put() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let b_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let b_port = b_listener.local_addr().unwrap().port();
        let captured = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let captured_b = captured.clone();
        let b_task = tokio::spawn(async move {
            let (mut sock, _) = b_listener.accept().await.unwrap();
            let mut req = Vec::new();
            let mut buf = [0u8; 2048];
            while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = sock.read(&mut buf).await.unwrap();
                assert_ne!(n, 0, "redirect target closed before request");
                req.extend_from_slice(&buf[..n]);
            }
            *captured_b.lock().unwrap() = String::from_utf8_lossy(&req).into_owned();
            sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .await
                .unwrap();
        });

        let a_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a_port = a_listener.local_addr().unwrap().port();
        let a_task = tokio::spawn(async move {
            let (mut sock, _) = a_listener.accept().await.unwrap();
            let mut req = Vec::new();
            let mut buf = [0u8; 2048];
            while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = sock.read(&mut buf).await.unwrap();
                assert_ne!(n, 0, "redirect source closed before request");
                req.extend_from_slice(&buf[..n]);
            }
            sock.write_all(
                format!(
                    "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{b_port}/land\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        });

        let request = Request {
            method: String::from("PUT"),
            url: Url::parse(&format!("http://127.0.0.1:{a_port}/start")).unwrap(),
            body: Some((String::from("text/plain"), b"payload".to_vec())),
            headers: vec![(
                String::from("Authorization"),
                String::from("Bearer should-not-cross-origin"),
            )],
            fetch_metadata: None,
            timing_client: None,
            fetch_policy: None,
        };
        let response = fetch(&request).await.unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"ok");
        a_task.await.unwrap();
        b_task.await.unwrap();

        let target_request = captured.lock().unwrap().clone();
        assert!(
            target_request.starts_with("PUT /land "),
            "302 preserves PUT: {target_request}"
        );
        assert!(
            !target_request.contains("Authorization:"),
            "Authorization is removed on a cross-origin redirect: {target_request}"
        );
    }

    #[test]
    fn cookie_jar_honors_domain_path_and_request_visibility() {
        let _guard = COOKIE_TEST_LOCK.lock().unwrap();
        set_cookies_enabled(true);
        // Unique domain so the process-global jar doesn't collide with
        // other tests (same caveat as the TOFU pins).
        let resp = parse_url("https://shop.ckjar-test.example/p").unwrap();
        store_cookie(
            &resp,
            "sid=abc; Domain=ckjar-test.example; Path=/; Secure",
            false,
        );
        store_cookie(&resp, "secret=xyz; HttpOnly", false); // sent, hidden from JS
        store_cookie(&resp, "pref=dark", false); // readable and sent

        let page = parse_url("https://shop.ckjar-test.example/foo").unwrap();
        let c = cookies_for_js(&page);
        assert!(c.contains("sid=abc"), "secure cookie over https: {c}");
        assert!(c.contains("pref=dark"), "host cookie: {c}");
        assert!(!c.contains("secret"), "HttpOnly hidden from JS: {c}");
        let req = cookies_for_request(&page);
        assert!(req.contains("sid=abc"), "sent to exact host: {req}");
        assert!(req.contains("secret=xyz"), "HttpOnly still sent: {req}");

        // An explicit Domain scope reaches matching sibling/parent hosts, while
        // the host-only cookies remain isolated to the response host.
        let sib = parse_url("https://other.ckjar-test.example/").unwrap();
        assert!(
            cookies_for_js(&sib).contains("sid=abc"),
            "sibling sees Domain cookie"
        );
        assert!(
            cookies_for_request(&sib).contains("sid=abc"),
            "sibling sends Domain cookie"
        );
        assert!(
            !cookies_for_request(&sib).contains("pref=dark"),
            "host-only cookie leaked"
        );
        assert!(
            !cookies_for_request(&sib).contains("secret=xyz"),
            "HttpOnly host-only cookie leaked"
        );
        let parent = parse_url("https://ckjar-test.example/").unwrap();
        assert!(
            cookies_for_request(&parent).contains("sid=abc"),
            "parent sends Domain cookie"
        );

        // A Domain attribute outside the response host is rejected, and path
        // matching does not confuse `/foo` with `/foobar`.
        store_cookie(&resp, "bad=1; Domain=attacker.example; Path=/", false);
        assert!(
            !cookies_for_request(&page).contains("bad=1"),
            "invalid Domain accepted"
        );
        store_cookie(&resp, "scoped=1; Path=/foo", false);
        let child = parse_url("https://shop.ckjar-test.example/foo/bar").unwrap();
        assert!(
            cookies_for_request(&child).contains("scoped=1"),
            "path child matched"
        );
        let sibling_path = parse_url("https://shop.ckjar-test.example/foobar").unwrap();
        assert!(
            !cookies_for_request(&sibling_path).contains("scoped=1"),
            "path prefix overmatched"
        );

        // Secure cookies don't surface or send over http.
        let http = parse_url("http://shop.ckjar-test.example/").unwrap();
        assert!(
            !cookies_for_js(&http).contains("sid=abc"),
            "secure hidden on http"
        );
        assert!(
            !cookies_for_request(&http).contains("sid=abc"),
            "secure not sent on http"
        );

        // Max-Age=0 deletes. A script cannot create an HttpOnly cookie or
        // replace/delete an existing protected cookie.
        store_cookie(&resp, "pref=; Max-Age=0", false);
        assert!(
            !cookies_for_js(&page).contains("pref"),
            "deleted by Max-Age=0"
        );
        set_cookie_from_js(&page, "fromjs=1; HttpOnly");
        assert!(
            !cookies_for_js(&page).contains("fromjs"),
            "JS cannot set HttpOnly"
        );
        set_cookie_from_js(&page, "secret=changed; Path=/");
        assert!(
            cookies_for_request(&page).contains("secret=xyz"),
            "JS cannot replace an HttpOnly cookie"
        );
        set_cookie_from_js(&page, "secret=; Max-Age=0; Path=/");
        assert!(
            cookies_for_request(&page).contains("secret=xyz"),
            "JS cannot delete an HttpOnly cookie"
        );
        assert!(
            cookies_for_request(&page).contains("secret=xyz"),
            "HttpOnly cookie remains sent to exact host"
        );
    }

    #[test]
    fn cookie_dates_follow_rfc6265_token_order_and_calendar_rules() {
        // RFC 6265 §5.1.1 and erratum 4148: optional suffixes, unordered
        // date components, two-digit years, and a real UTC Gregorian date.
        for value in [
            "Thu, 01 Jan 1970 00:00:00 GMT",
            "Thursday, 01-Jan-70 00:00:00 GMT",
            "Thu Jan 1 00:00:00 1970",
            "00:00:00 ignored JANuary 01st 1970year",
            "[1970]/(Jan)/{01} 0:0:0suffix",
        ] {
            assert_eq!(parse_cookie_date(value), Some(0), "{value}");
        }
        assert_eq!(parse_cookie_date("31 Dec 1969 23:59:59"), Some(-1000));
        assert_eq!(
            parse_cookie_date("01 Jan 69 00:00:00"),
            parse_cookie_date("01 Jan 2069 00:00:00")
        );
        assert_eq!(parse_cookie_date("01 Jan 70 00:00:00"), Some(0));
        assert!(parse_cookie_date("29 Feb 2000 00:00:00").is_some());
        assert!(parse_cookie_date("01 Jan 1601 00:00:00").is_some());
        for value in [
            "01 Jan 1600 00:00:00",
            "29 Feb 1900 00:00:00",
            "29 Feb 2025 00:00:00",
            "31 Apr 2026 00:00:00",
            "00 Jan 2026 00:00:00",
            "32 Jan 2026 00:00:00",
            "01 Jan 2026 24:00:00",
            "01 Jan 2026 00:60:00",
            "01 Jan 2026 00:00:60",
            "01 Jan 2026 000:00:00",
            "001 Jan 2026 00:00:00",
            "01 Jan 10000 00:00:00",
            "01 Jan 2026",
            "01 Nope 2026 00:00:00",
            "not a date",
        ] {
            assert_eq!(parse_cookie_date(value), None, "{value}");
        }
    }

    #[test]
    fn cookie_expiry_removes_empty_host_cookie_before_domain_token_reload() {
        let _guard = COOKIE_TEST_LOCK.lock().unwrap();
        set_cookies_enabled(true);
        // Keep the fake clock and its jar isolated from concurrent page tests.
        let mut jar = Vec::new();
        let page = parse_url("https://www.cookie-expiry.example.test/").unwrap();
        let now = parse_cookie_date("14 Sep 2026 12:00:00 GMT").unwrap();
        // A verification script deletes the old host cookie before writing
        // the replacement domain cookie. The deleted cookie must never be
        // inserted as an empty value ahead of the replacement in the header.
        store_cookie_at(&mut jar, &page, "verification=old; Path=/", true, now);
        store_cookie_at(
            &mut jar,
            &page,
            "verification=; Path=/; Expires=Thu, 01 Jan 1970 00:00:00 GMT",
            true,
            now,
        );
        store_cookie_at(
            &mut jar,
            &page,
            "verification=fresh; Domain=cookie-expiry.example.test; Path=/; Secure",
            true,
            now,
        );
        assert_eq!(
            cookies_for_js_at(&mut jar, &page, now),
            "verification=fresh"
        );
        assert_eq!(
            cookies_for_request_at(&mut jar, &page, now),
            "verification=fresh"
        );
    }

    #[test]
    fn cookie_expiry_precedence_lifetimes_and_httponly_protection() {
        let _guard = COOKIE_TEST_LOCK.lock().unwrap();
        set_cookies_enabled(true);
        // Keep the fake clock and its jar isolated from concurrent page tests.
        let mut jar = Vec::new();
        let page = parse_url("https://cookie-lifetimes.example.test/").unwrap();
        let now = parse_cookie_date("14 Sep 2026 12:00:00 GMT").unwrap() + 123;
        for line in [
            "valid=a; Max-Age=2; Expires=Thu, 01 Jan 1970 00:00:00 GMT; Max-Age=invalid",
            "removed=a; Expires=Thu, 01 Jan 2099 00:00:00 GMT; Max-Age=0",
            "expired=a; Expires=Thu, 01 Jan 1970 00:00:00 GMT; Expires=invalid",
            "session=a; Expires=invalid; Max-Age=+2",
            "last=a; Max-Age=0; Max-Age=2",
            "http=a; Max-Age=1; HttpOnly",
            "absolute=a; Expires=Mon, 14 Sep 2026 12:00:02 GMT",
        ] {
            store_cookie_at(&mut jar, &page, line, false, now);
        }
        assert_eq!(
            cookies_for_js_at(&mut jar, &page, now),
            "valid=a; session=a; last=a; absolute=a"
        );
        assert!(cookies_for_request_at(&mut jar, &page, now).contains("http=a"));
        store_cookie_at(
            &mut jar,
            &page,
            "http=; Expires=Thu, 01 Jan 1970 00:00:00 GMT",
            true,
            now,
        );
        assert!(
            cookies_for_request_at(&mut jar, &page, now).contains("http=a"),
            "JS cannot delete live HttpOnly cookies"
        );
        assert!(!cookies_for_request_at(&mut jar, &page, now + 1000).contains("http="));
        assert_eq!(
            cookies_for_js_at(&mut jar, &page, now + 1999),
            "valid=a; session=a; last=a"
        );
        assert_eq!(
            cookies_for_request_at(&mut jar, &page, now + 2000),
            "session=a"
        );
        // Once expired, the HttpOnly cookie no longer prevents a JS write.
        store_cookie_at(&mut jar, &page, "http=new", true, now + 2000);
        assert!(cookies_for_js_at(&mut jar, &page, now + 2000).contains("http=new"));
        for value in ["-", "+1", "1x", "1.0", ""] {
            assert_eq!(cookie_max_age(value, now), None);
        }
        assert_eq!(
            cookie_max_age("-999999999999999999999", now),
            Some(i64::MIN)
        );
        assert_eq!(
            cookie_max_age("999999999999999999999", now),
            Some(now + COOKIE_MAX_AGE_SECS * 1000)
        );
        store_cookie_at(
            &mut jar,
            &page,
            "capped=yes; Expires=31 Dec 9999 23:59:59 GMT",
            false,
            now,
        );
        assert!(
            !cookies_for_request_at(&mut jar, &page, now + COOKIE_MAX_AGE_SECS * 1000)
                .contains("capped=")
        );
    }

    #[test]
    fn cookie_trace_metadata_never_confuses_names_with_values() {
        assert_eq!(cookie_trace_counts(["", "  "].into_iter()), (0, 0));
        assert_eq!(
            cookie_trace_counts(
                [
                    "cf_clearance=opaque-test-only; Secure; HttpOnly; Path=/",
                    "preference=cf_clearance=not-a-cookie-name",
                    "CF_CLEARANCE=case-sensitive-name",
                ]
                .into_iter()
            ),
            (3, 1)
        );
        assert_eq!(
            cookie_trace_counts("a=1; cf_clearance=opaque-test-only; b=2".split(';')),
            (3, 1)
        );
    }

    #[test]
    fn cookie_integrity_requires_exact_bytes_and_matching_scope() {
        let page = parse_url("https://cookie-integrity.example.test/path/page").unwrap();
        let receipt = Cookie {
            name: "cf_clearance".into(),
            value: "original-test.%2F+/:=end".into(),
            domain: "cookie-integrity.example.test".into(),
            host_only: true,
            path: "/path".into(),
            secure: true,
            http_only: true,
            expires_at: None,
        };
        let line = "other=ignored; cf_clearance=original-test.%2F+/:=end";
        let receipts = [receipt];
        assert_eq!(cookie_wire_integrity(&page, line, &receipts), (1, 1, 0));
        assert_eq!(cookie_wire_integrity(&page, line, &[]), (1, 0, 0));
        assert_eq!(
            cookie_wire_integrity(&page, "cf_clearance=original-test.%2F+/:=en", &receipts),
            (1, 0, 0)
        );
        assert_eq!(
            cookie_wire_integrity(&page, "cf_clearance=original-test./+/:=end", &receipts),
            (1, 0, 0)
        );
        assert_eq!(
            cookie_wire_integrity(&page, "cf_clearance=", &receipts),
            (1, 0, 1)
        );
        for url in [
            "https://other.example.test/path/page",
            "https://cookie-integrity.example.test/path-other",
            "http://cookie-integrity.example.test/path/page",
        ] {
            assert_eq!(
                cookie_wire_integrity(&parse_url(url).unwrap(), line, &receipts),
                (1, 0, 0)
            );
        }
    }

    #[test]
    fn google_consent_domain_cookie_survives_sibling_redirect() {
        let _guard = COOKIE_TEST_LOCK.lock().unwrap();
        set_cookies_enabled(true);
        let save = parse_url("https://consent.google.ca/save").unwrap();
        let continue_url = parse_url("https://translate.google.ca/?ucbcb=1").unwrap();
        store_cookie(
            &save,
            "SOCS=accepted; Domain=.google.ca; Path=/; Secure",
            false,
        );
        assert!(
            cookies_for_request(&continue_url).contains("SOCS=accepted"),
            "Google's Domain=.google.ca consent state must reach the sibling continuation host"
        );
    }

    /// A page reads back the `Set-Cookie` from its own response via
    /// `document.cookie`; matching cookies are also sent on later requests.
    // Holds COOKIE_TEST_LOCK (a std guard) across awaits to serialize the
    // process-global jar/enabled flag. Safe: each #[tokio::test] is its own
    // current-thread runtime on its own thread, so a contending lock() blocks
    // a separate thread and never the holder.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn document_cookie_reflects_captured_set_cookie() {
        let _guard = COOKIE_TEST_LOCK.lock().unwrap();
        set_cookies_enabled(true);
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = [0u8; 1024];
                let _ = sock.read(&mut buf).await;
                let body = "<body><div id=t></div><script>\
                    document.getElementById('t').textContent='C['+document.cookie+']';\
                    </script></body>";
                let reply = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\
                     Set-Cookie: tjar=ckval123; Path=/\r\n\
                     Set-Cookie: tj_secret=hidden; HttpOnly\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(reply.as_bytes()).await;
            }
        });
        let url = parse_url(&format!("http://127.0.0.1:{port}/")).unwrap();
        let response = fetch(&Request::get(url)).await.unwrap();
        let response = execute_js(response, (80, 24), (8, 16), Default::default()).await;
        let out = String::from_utf8_lossy(&response.body);
        assert!(out.contains("tjar=ckval123"), "cookie visible to JS: {out}");
        assert!(
            !out.contains("tj_secret"),
            "HttpOnly cookie hidden from JS: {out}"
        );
        server.abort();
    }

    #[tokio::test]
    async fn navigation_timing_response_bytes_and_interim_boundaries() {
        use std::io::Write as _;
        let payload = b"original timing test payload ".repeat(128);
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&payload).unwrap();
        let compressed = encoder.finish().unwrap();
        let encoded_size = compressed.len();
        let (mut writer, reader) = tokio::io::duplex(8192);
        let server = tokio::spawn(async move {
            writer
                .write_all(b"HTTP/1.1 103 Early Hints\r\n\r\n")
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(10)).await;
            writer.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Encoding: gzip\r\nContent-Length: {encoded_size}\r\n\r\n").as_bytes()).await.unwrap();
            tokio::time::sleep(Duration::from_millis(10)).await;
            writer.write_all(&compressed).await.unwrap();
        });
        let mut timing = FetchTiming::new();
        let (status, _, body, _, _) =
            read_response_with_timing(&mut BufReader::new(reader), false, false, Some(&mut timing))
                .await
                .unwrap();
        server.await.unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, payload);
        assert!(timing.first_interim_response_start >= timing.start_time);
        assert!(timing.final_response_start > timing.first_interim_response_start);
        assert!(timing.response_end > timing.final_response_start);
        assert_eq!(timing.encoded_body_size, encoded_size);
        assert_eq!(timing.decoded_body_size, payload.len());
        let data = timing.navigation_data("http://localhost/original");
        assert_eq!(data["responseStart"], data["firstInterimResponseStart"]);
        assert!(
            data["finalResponseHeadersStart"].as_f64().unwrap()
                > data["responseStart"].as_f64().unwrap()
        );
        assert_eq!(data["transferSize"], encoded_size + 300);
        assert_eq!(data["secureConnectionStart"], 0.0);
    }

    #[tokio::test]
    async fn navigation_timing_redirect_details_obey_navigation_tao() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let port = address.port();
        let server = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                    let mut bytes = [0; 1024];
                    let count = socket.read(&mut bytes).await.unwrap();
                    if count == 0 {
                        break;
                    }
                    request.extend_from_slice(&bytes[..count]);
                }
                let request = String::from_utf8_lossy(&request);
                let path = request.split_whitespace().nth(1).unwrap_or("/");
                let tao = if path.contains("tao") {
                    "Timing-Allow-Origin: *\r\n"
                } else {
                    ""
                };
                let response = if path.starts_with("/start/") {
                    let host = if path.contains("same") {
                        "127.0.0.1"
                    } else {
                        "localhost"
                    };
                    format!(
                        "HTTP/1.1 302 Found\r\nLocation: http://{host}:{port}/final/{path}\r\n{tao}Content-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                } else {
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n{tao}Content-Length: 2\r\nConnection: close\r\n\r\nok"
                    )
                };
                let _ = socket.write_all(response.as_bytes()).await;
            }
        });
        for (path, allowed, no_referrer) in [
            ("same", true, false),
            ("cross", false, false),
            ("tao", true, false),
            ("no-referrer-tao", false, true),
        ] {
            let url = parse_url(&format!("http://{address}/start/{path}")).unwrap();
            let mut request = Request::get(url.clone());
            set_navigation_metadata(&mut request, if no_referrer { Some(&url) } else { None });
            let response = fetch(&request).await.unwrap();
            let timing = response.timing.unwrap();
            assert!(timing.redirect_end > timing.start_time);
            assert!(timing.fetch_start <= timing.request_start);
            let data = timing.navigation_data(response.url.as_str());
            assert_eq!(data["redirectCount"], u16::from(allowed), "{path}");
            if allowed {
                assert!(data["redirectEnd"].as_f64().unwrap() > 0.0, "{path}");
            } else {
                assert_eq!(data["redirectStart"], 0.0);
                assert_eq!(data["redirectEnd"], 0.0);
            }
        }
        server.abort();
    }

    #[tokio::test]
    async fn navigation_timing_is_available_to_the_first_document_script() {
        // Navigation Timing #dfn-create-the-navigation-timing-entry and HTML
        // #initialise-the-document-object: expose the real navigation before
        // author scripts run, not an end-of-load snapshot or fabricated marks.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                let mut bytes = [0; 1024];
                let count = socket.read(&mut bytes).await.unwrap();
                assert!(count > 0);
                request.extend_from_slice(&bytes[..count]);
            }
            let body = r#"<!doctype html><body><script>
                const navigation = performance.getEntriesByType('navigation');
                document.body.setAttribute('data-navigation-count', String(navigation.length));
                if (navigation.length === 1) {
                    const entry = navigation[0];
                    const valid = entry instanceof PerformanceNavigationTiming &&
                        entry instanceof PerformanceResourceTiming && entry instanceof PerformanceEntry &&
                        entry.name === location.href && entry.entryType === 'navigation' &&
                        entry.startTime === 0 && entry.duration === 0 &&
                        entry.responseEnd >= entry.responseStart && entry.responseStart >= entry.requestStart &&
                        entry.requestStart >= entry.connectEnd && entry.connectEnd >= entry.fetchStart &&
                        entry.responseEnd > 0 && entry.responseEnd <= performance.now() &&
                        entry.domInteractive === 0 && entry.domComplete === 0 && entry.loadEventEnd === 0;
                    document.body.setAttribute('data-navigation-valid', String(valid));
                    document.addEventListener('DOMContentLoaded', () => {
                        document.body.setAttribute('data-navigation-dom-event', String(
                            document.readyState === 'interactive' && entry.domInteractive > 0 &&
                            entry.domContentLoadedEventStart >= entry.domInteractive &&
                            entry.domContentLoadedEventEnd === 0 && entry.domComplete === 0));
                    });
                    addEventListener('load', () => {
                        document.body.setAttribute('data-navigation-load-event', String(
                            document.readyState === 'complete' && entry.domComplete >= entry.domContentLoadedEventEnd &&
                            entry.loadEventStart >= entry.domComplete && entry.loadEventEnd === 0 && entry.duration === 0));
                        setTimeout(() => document.body.setAttribute('data-navigation-final', String(
                            entry.loadEventEnd >= entry.loadEventStart && entry.duration === entry.loadEventEnd &&
                            entry.loadEventEnd <= performance.now())), 0);
                    });
                }
            </script></body>"#;
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            socket.write_all(header.as_bytes()).await.unwrap();
            socket.write_all(body.as_bytes()).await.unwrap();
        });
        let url = parse_url(&format!("http://{address}/navigation")).unwrap();
        let response = fetch_web_default(&url).await.unwrap();
        server.await.unwrap();
        let response = execute_js(response, (80, 24), (8, 16), Default::default()).await;
        assert!(
            response
                .js
                .as_ref()
                .is_some_and(|outcome| outcome.errors.is_empty()),
            "navigation script errors: {:?}",
            response.js.as_ref().map(|outcome| &outcome.errors)
        );
        let html = navigation_timing_snapshot_after(response, "data-navigation-final=").await;
        assert!(
            html.contains("data-navigation-count=\"1\""),
            "missing first-script entry: {html}"
        );
        assert!(
            html.contains("data-navigation-valid=\"true\""),
            "invalid first-script timing: {html}"
        );
        for phase in ["dom-event", "load-event", "final"] {
            assert!(
                html.contains(&format!("data-navigation-{phase}=\"true\"")),
                "invalid {phase}: {html}"
            );
        }
    }

    pub(super) async fn navigation_timing_snapshot_after(
        mut response: Response,
        marker: &str,
    ) -> String {
        let initial = String::from_utf8_lossy(&response.body).into_owned();
        if initial.contains(marker) {
            return initial;
        }
        let mut live = response
            .live
            .take()
            .expect("pending timing tasks retain their page actor");
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                match live.events.recv().await {
                    Some(
                        crate::js::PageEvt::Updated { html, outcome }
                        | crate::js::PageEvt::Static { html, outcome },
                    ) => {
                        assert!(
                            outcome.errors.is_empty(),
                            "timing actor errors: {:?}",
                            outcome.errors
                        );
                        if html.contains(marker) {
                            break html;
                        }
                    }
                    Some(crate::js::PageEvt::Patched { patches, outcome }) => {
                        assert!(
                            outcome.errors.is_empty(),
                            "timing actor errors: {:?}",
                            outcome.errors
                        );
                        if let Some(patch) = patches
                            .into_iter()
                            .find(|patch| patch.html.contains(marker))
                        {
                            break patch.html;
                        }
                    }
                    Some(crate::js::PageEvt::Trouble(errors)) => {
                        panic!("timing task failed: {errors:?}")
                    }
                    Some(_) => {}
                    None => panic!("timing page closed before {marker}; initial {initial}"),
                }
            }
        })
        .await
        .expect("timing lifecycle completion timed out")
    }

    #[tokio::test]
    async fn navigation_timing_follows_real_iframe_navigation() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                    let mut bytes = [0; 1024];
                    let count = socket.read(&mut bytes).await.unwrap();
                    if count == 0 {
                        break;
                    }
                    request.extend_from_slice(&bytes[..count]);
                }
                let (mime, body) = if request.starts_with(b"GET /child ") {
                    (
                        "text/html",
                        r#"<!doctype html><body>
                        <script defer src="/deferred.js"></script><script>
                        const n = performance.getEntriesByType('navigation')[0];
                        parent.document.body.setAttribute('data-child-first', String(
                            !!n && n.name === location.href && n instanceof PerformanceNavigationTiming &&
                            document.readyState === 'loading' && n.domInteractive === 0 &&
                            n.duration === 0 && n.loadEventEnd === 0 && n.responseEnd > 0 &&
                            n.responseEnd <= performance.now()));
                        </script></body>"#,
                    )
                } else if request.starts_with(b"GET /deferred.js ") {
                    (
                        "text/javascript",
                        r#"const deferredNav = performance.getEntriesByType('navigation')[0];
                        parent.document.body.setAttribute('data-child-deferred', String(
                            document.readyState === 'interactive' && deferredNav.domInteractive > 0 &&
                            deferredNav.domContentLoadedEventStart === 0));"#,
                    )
                } else {
                    (
                        "text/html",
                        r#"<!doctype html><body><script>
                        const frame = document.createElement('iframe'); document.body.appendChild(frame);
                        const initialWindowArray = frame.contentWindow.Array;
                        const initialPerformance = frame.contentWindow.performance;
                        const initialOrigin = initialPerformance.timeOrigin;
                        setTimeout(() => {
                            frame.onload = () => {
                                setTimeout(() => {
                                    const p = frame.contentWindow.performance, n = p.getEntriesByType('navigation')[0];
                                    document.body.setAttribute('data-child-final', String(
                                        !!n && p.timeOrigin > initialOrigin &&
                                        p === initialPerformance && frame.contentWindow.Array === initialWindowArray &&
                                        n.domComplete > 0 && n.loadEventEnd >= n.domComplete && n.duration === n.loadEventEnd &&
                                        performance.getEntriesByType('navigation')[0].name === location.href));
                                }, 0);
                            };
                            frame.src = '/child';
                        }, 5);
                    </script></body>"#,
                    )
                };
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = socket.write_all(header.as_bytes()).await;
                let _ = socket.write_all(body.as_bytes()).await;
            }
        });
        let url = parse_url(&format!("http://{address}/parent")).unwrap();
        let response = fetch_web_default(&url).await.unwrap();
        let response = execute_js(response, (80, 24), (8, 16), Default::default()).await;
        assert!(
            response
                .js
                .as_ref()
                .is_some_and(|outcome| outcome.errors.is_empty()),
            "iframe errors: {:?}",
            response.js.as_ref().map(|outcome| &outcome.errors)
        );
        let html = navigation_timing_snapshot_after(response, "data-child-final=").await;
        server.abort();
        for phase in ["first", "deferred", "final"] {
            assert!(
                html.contains(&format!("data-child-{phase}=\"true\"")),
                "invalid child {phase}: {html}"
            );
        }
    }

    // Same safe-across-await rationale as
    // document_cookie_reflects_captured_set_cookie.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn redirect_sends_captured_host_cookie() {
        let _guard = COOKIE_TEST_LOCK.lock().unwrap();
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        set_cookies_enabled(true);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let mut req = Vec::new();
                let mut buf = [0u8; 1024];
                while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                    match sock.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => req.extend_from_slice(&buf[..n]),
                    }
                }
                let text = String::from_utf8_lossy(&req);
                let first = text.starts_with("GET /start ");
                let has_cookie = text.contains("Cookie:") && text.contains("redirjar=ok");
                let reply = if first {
                    "HTTP/1.1 302 Found\r\nLocation: /final\r\nSet-Cookie: redirjar=ok; Path=/\r\nConnection: close\r\n\r\n"
                        .to_string()
                } else if has_cookie {
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 9\r\nConnection: close\r\n\r\ncookie ok"
                        .to_string()
                } else {
                    "HTTP/1.1 302 Found\r\nLocation: /start\r\nConnection: close\r\n\r\n"
                        .to_string()
                };
                let _ = sock.write_all(reply.as_bytes()).await;
            }
        });

        let url = parse_url(&format!("http://127.0.0.1:{port}/start")).unwrap();
        let response = fetch(&Request::get(url)).await.unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"cookie ok");
        server.abort();
    }

    /// Three GETs ride one connection; a POST always dials fresh (a
    /// stale-pool retry must never double-submit).
    #[tokio::test]
    async fn keep_alive_reuses_connections() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let accepts = Arc::new(AtomicUsize::new(0));
        let count = accepts.clone();
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                count.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let mut req: Vec<u8> = Vec::new();
                    loop {
                        let head_end = loop {
                            if let Some(p) = req.windows(4).position(|w| w == b"\r\n\r\n") {
                                break p + 4;
                            }
                            match sock.read(&mut buf).await {
                                Ok(0) | Err(_) => return,
                                Ok(n) => req.extend_from_slice(&buf[..n]),
                            }
                        };
                        let head = String::from_utf8_lossy(&req[..head_end]).into_owned();
                        let body_len = head
                            .lines()
                            .find_map(|l| {
                                let (k, v) = l.split_once(':')?;
                                k.eq_ignore_ascii_case("content-length")
                                    .then(|| v.trim().parse::<usize>().ok())?
                            })
                            .unwrap_or(0);
                        while req.len() < head_end + body_len {
                            match sock.read(&mut buf).await {
                                Ok(0) | Err(_) => return,
                                Ok(n) => req.extend_from_slice(&buf[..n]),
                            }
                        }
                        req.drain(..head_end + body_len);
                        let reply = b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\
                                      Content-Length: 2\r\n\r\nok";
                        if sock.write_all(reply).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });

        for path in ["/a", "/b", "/c"] {
            let url = parse_url(&format!("http://127.0.0.1:{port}{path}")).unwrap();
            let resp = fetch(&Request::get(url)).await.unwrap();
            assert_eq!(resp.body, b"ok");
        }
        assert_eq!(
            accepts.load(Ordering::SeqCst),
            1,
            "three GETs, one connection"
        );

        let url = parse_url(&format!("http://127.0.0.1:{port}/post")).unwrap();
        let request = Request {
            method: String::from("POST"),
            url,
            body: Some((String::from("text/plain"), b"hi".to_vec())),
            headers: Vec::new(),
            fetch_metadata: None,
            timing_client: None,
            fetch_policy: None,
        };
        let resp = fetch(&request).await.unwrap();
        assert_eq!(resp.body, b"ok");
        assert_eq!(
            accepts.load(Ordering::SeqCst),
            2,
            "POSTs never reuse pooled connections"
        );
        server.abort();
    }

    #[tokio::test]
    async fn request_headers_reach_the_wire_but_managed_ones_cannot_be_spoofed() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        // A page's request headers (XHR setRequestHeader / fetch init.headers)
        // must reach the server — `X-Requested-With` is what `$request->ajax()`
        // reads (erome's disclaimer accept needs it). But a page must NOT be
        // able to spoof transport/identity headers (Host, Cookie, …), and a
        // page-supplied `Accept` overrides our default without duplicating it.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let captured = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let cap2 = captured.clone();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut req = Vec::new();
            let mut buf = [0u8; 2048];
            while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                match sock.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => req.extend_from_slice(&buf[..n]),
                }
            }
            *cap2.lock().unwrap() = String::from_utf8_lossy(&req).into_owned();
            let _ = sock
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                .await;
        });
        let url = parse_url(&format!("http://127.0.0.1:{port}/x")).unwrap();
        let request = Request {
            method: String::from("GET"),
            url,
            body: None,
            headers: vec![
                ("X-Requested-With".into(), "XMLHttpRequest".into()),
                ("Authorization".into(), "Bearer tok".into()),
                ("Accept".into(), "application/json".into()),
                ("Accept-Language".into(), "fr-CA,fr;q=0.8".into()),
                ("Host".into(), "evil.example".into()),
                ("Cookie".into(), "spoof=1".into()),
                ("Sec-GPC".into(), "0".into()),
                ("Sec-Fetch-Site".into(), "none".into()),
                ("Upgrade-Insecure-Requests".into(), "0".into()),
            ],
            fetch_metadata: None,
            timing_client: None,
            fetch_policy: None,
        };
        let resp = fetch(&request).await.unwrap();
        assert_eq!(resp.body, b"ok");
        server.abort();
        let head = captured.lock().unwrap().clone();
        assert!(
            head.contains("X-Requested-With: XMLHttpRequest"),
            "X-Requested-With forwarded: {head}"
        );
        assert!(
            head.contains("Authorization: Bearer tok"),
            "Authorization forwarded: {head}"
        );
        assert!(
            head.contains("Accept: application/json"),
            "page Accept overrides default: {head}"
        );
        assert_eq!(
            head.matches("Accept:").count(),
            1,
            "no duplicate Accept header: {head}"
        );
        assert!(
            head.contains("Accept-Language: fr-CA,fr;q=0.8"),
            "page Accept-Language overrides the UA default: {head}"
        );
        assert_eq!(
            head.matches("Accept-Language:").count(),
            1,
            "no duplicate Accept-Language header: {head}"
        );
        assert!(
            head.contains("Accept-Encoding: gzip, deflate"),
            "supported response compression is advertised: {head}"
        );
        assert!(
            head.contains("Sec-GPC: 1\r\n"),
            "the user-agent GPC preference reaches the wire: {head}"
        );
        assert_eq!(head.matches("Sec-GPC:").count(), 1);
        assert!(
            !head.contains("evil.example"),
            "Host cannot be spoofed: {head}"
        );
        assert!(
            !head.contains("Sec-Fetch-Site:"),
            "Fetch Metadata cannot be forged by page headers: {head}"
        );
        assert!(
            !head.contains("Upgrade-Insecure-Requests:"),
            "UIR cannot be forged by page headers: {head}"
        );
        assert!(
            !head.contains("spoof=1"),
            "Cookie cannot be spoofed: {head}"
        );
    }

    #[tokio::test]
    async fn default_accept_language_reaches_the_wire() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (head_tx, head_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut req = Vec::new();
            let mut buf = [0u8; 2048];
            while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                match sock.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => req.extend_from_slice(&buf[..n]),
                }
            }
            let _ = head_tx.send(String::from_utf8_lossy(&req).into_owned());
            let _ = sock
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .await;
        });

        // A normal navigation has no page-authored Accept-Language; the
        // Fetch user-agent default must be emitted exactly once.
        let url = parse_url(&format!("http://127.0.0.1:{port}/reddit")).unwrap();
        let response = fetch(&Request::get(url)).await.unwrap();
        assert_eq!(response.body, b"ok");
        let head = head_rx.await.unwrap();
        assert!(
            head.contains(&format!(
                "Accept-Language: {}\r\n",
                crate::locale::ACCEPT_LANGUAGE
            )),
            "default language preference reaches the wire: {head}"
        );
        assert_eq!(head.matches("Accept-Language:").count(), 1);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn top_level_navigation_metadata_reaches_the_wire() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (head_tx, head_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut req = Vec::new();
            let mut buf = [0u8; 2048];
            while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                match sock.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => req.extend_from_slice(&buf[..n]),
                }
            }
            let _ = head_tx.send(String::from_utf8_lossy(&req).into_owned());
            let _ = sock
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .await;
        });

        let url = parse_url(&format!("http://127.0.0.1:{port}/page")).unwrap();
        let mut request = Request::get(url.clone());
        set_navigation_metadata(&mut request, None);
        let response = fetch(&request).await.unwrap();
        assert_eq!(response.body, b"ok");
        let head = head_rx.await.unwrap();
        for expected in [
            "Sec-Fetch-Dest: document\r\n",
            "Sec-Fetch-Mode: navigate\r\n",
            "Sec-Fetch-Site: none\r\n",
            "Sec-Fetch-User: ?1\r\n",
            "Upgrade-Insecure-Requests: 1\r\n",
        ] {
            assert!(
                head.contains(expected),
                "{expected:?} reaches the wire: {head}"
            );
            assert_eq!(head.matches(expected.trim_end()).count(), 1);
        }
        server.await.unwrap();

        // A page cannot smuggle a forged Fetch Metadata value through the
        // ordinary header list; only the user-agent context controls it.
        let same_origin = Url::parse(&format!("http://127.0.0.1:{port}/from")).unwrap();
        let mut same_origin_request = Request::get(same_origin.clone());
        set_navigation_metadata(&mut same_origin_request, Some(&same_origin));
        assert_eq!(
            same_origin_request.fetch_metadata.map(|m| m.site),
            Some(FetchSite::SameOrigin)
        );
    }

    #[tokio::test]
    async fn large_response_bodies_preserve_bytes_for_every_framing() {
        // RFC 9112 §6.3 / §7.1.3: framing determines the complete byte
        // sequence delivered to Fetch/XHR, including binary app resources
        // larger than the former 16 MiB script-oriented ceiling.
        let mut payload = vec![0xa5; 17 * 1024 * 1024];
        payload[..4].copy_from_slice(b"PK\x03\x04");
        *payload.last_mut().unwrap() = 0x5a;
        for framing in ["length", "chunked", "eof"] {
            let mut raw = b"HTTP/1.1 200 OK\r\nContent-Type: application/zip\r\n".to_vec();
            match framing {
                "length" => {
                    raw.extend_from_slice(
                        format!("Content-Length: {}\r\n\r\n", payload.len()).as_bytes(),
                    );
                    raw.extend_from_slice(&payload);
                }
                "chunked" => {
                    raw.extend_from_slice(b"Transfer-Encoding: chunked\r\n\r\n");
                    // Cross the old cap cumulatively, not in one large chunk.
                    for chunk in payload.chunks(1024 * 1024) {
                        raw.extend_from_slice(format!("{:x}\r\n", chunk.len()).as_bytes());
                        raw.extend_from_slice(chunk);
                        raw.extend_from_slice(b"\r\n");
                    }
                    raw.extend_from_slice(b"0\r\n\r\n");
                }
                _ => {
                    raw.extend_from_slice(b"Connection: close\r\n\r\n");
                    raw.extend_from_slice(&payload);
                }
            }
            let (status, _, body, reusable, _) =
                read_response(&mut BufReader::new(raw.as_slice()), false)
                    .await
                    .unwrap_or_else(|error| panic!("{framing}: {error}"));
            assert_eq!(status, 200);
            assert_eq!(body.len(), payload.len(), "{framing}");
            assert!(body == payload, "{framing} must preserve every byte");
            assert_eq!(reusable, framing != "eof", "{framing}");
        }
    }

    #[tokio::test]
    async fn large_response_gzip_preserves_the_complete_decoded_body() {
        use flate2::{Compression, write::GzEncoder};
        use std::io::Write as _;
        let payload = vec![0xa5; 17 * 1024 * 1024];
        let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(&payload).unwrap();
        let compressed = encoder.finish().unwrap();
        let mut raw = format!(
            "HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\n\r\n",
            compressed.len()
        )
        .into_bytes();
        raw.extend_from_slice(&compressed);
        let (_, _, body, reusable, _) = read_response(&mut BufReader::new(raw.as_slice()), false)
            .await
            .unwrap();
        assert_eq!(body.len(), payload.len());
        assert!(body == payload);
        assert!(reusable);
    }

    #[tokio::test]
    async fn large_response_limit_still_rejects_oversized_framing() {
        // Reject before allocating the advertised body, and retain HEAD's
        // no-body precedence even when Content-Length exceeds the ceiling.
        let raw = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
            MAX_BODY + 1
        );
        let error = read_response(&mut BufReader::new(raw.as_bytes()), false)
            .await
            .unwrap_err();
        assert!(error.contains("cap"), "{error}");
        let (_, _, body, reusable, _) = read_response(&mut BufReader::new(raw.as_bytes()), true)
            .await
            .unwrap();
        assert!(body.is_empty());
        assert!(reusable);

        let raw = format!(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n",
            MAX_BODY + 1
        );
        let error = read_response(&mut BufReader::new(raw.as_bytes()), false)
            .await
            .unwrap_err();
        assert!(error.contains("cap"), "{error}");
    }

    #[tokio::test]
    async fn reads_chunked_bodies() {
        let raw: &[u8] =
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n";
        let (status, _, body, reusable, _) = read_response(&mut BufReader::new(raw), false)
            .await
            .unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, b"Wikipedia");
        assert!(reusable, "complete chunked response is reusable");

        // Extensions after the size are valid, but an incomplete chunked
        // message is a network error (RFC 9112 §7.1.3), not a partial body.
        let raw: &[u8] =
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4;name=v\r\nWiki\r\n5\r\npedia";
        assert!(
            read_response(&mut BufReader::new(raw), false)
                .await
                .is_err(),
            "missing terminator must fail rather than expose a partial body"
        );
    }

    #[tokio::test]
    async fn navigation_defers_unsupported_body_before_page_memory_cap() {
        let raw: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: application/pdf\r\nContent-Length: 999999999\r\n\r\n%PDF";
        let (status, headers, body, reusable, _) =
            read_response_with_policy(&mut BufReader::new(raw), false, true)
                .await
                .unwrap();
        assert_eq!(status, 200);
        assert_eq!(headers["content-type"], "application/pdf");
        assert!(
            body.is_empty(),
            "the file body belongs to the download task"
        );
        assert!(!reusable, "the deliberately unread response is not pooled");
    }

    #[tokio::test]
    async fn reads_delimited_responses() {
        // Content-Length delimits even with pipelined junk behind it.
        let raw: &[u8] =
            b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 5\r\n\r\nhellojunk";
        let (status, headers, body, reusable, _) = read_response(&mut BufReader::new(raw), false)
            .await
            .unwrap();
        assert_eq!(status, 200);
        assert_eq!(headers["content-type"], "text/html");
        assert_eq!(body, b"hello");
        assert!(reusable);

        // Connection: close means don't pool; no delimiter means EOF.
        let raw: &[u8] = b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\nstuff";
        let (_, _, body, reusable, _) = read_response(&mut BufReader::new(raw), false)
            .await
            .unwrap();
        assert_eq!(body, b"stuff");
        assert!(!reusable);

        // HTTP/1.0 is never pooled.
        let raw: &[u8] = b"HTTP/1.0 200 OK\r\nContent-Length: 2\r\n\r\nok";
        let (_, _, body, reusable, _) = read_response(&mut BufReader::new(raw), false)
            .await
            .unwrap();
        assert_eq!(body, b"ok");
        assert!(!reusable);
    }

    #[tokio::test]
    async fn bodyless_status_takes_precedence_over_framing_headers() {
        use tokio::io::AsyncReadExt as _;

        // RFC 9112 §6.3 item 1 precedes TE/CL validation. Neither an
        // advertised length nor a chunk terminator can become a 204 body.
        for status in [204, 304, 101] {
            for fields in [
                "Content-Length: 0\r\n",
                "Content-Length: 9\r\n",
                "Content-Length: invalid\r\n",
                "Transfer-Encoding: chunked\r\n",
                "Transfer-Encoding: chunked\r\nContent-Length: 0\r\n",
            ] {
                let raw = format!("HTTP/1.1 {status} Response\r\n{fields}\r\nunread");
                let mut reader = BufReader::new(raw.as_bytes());
                let (actual, _, body, reusable, _) = read_response(&mut reader, false)
                    .await
                    .expect("bodyless status wins over framing fields");
                assert_eq!(actual, status);
                assert!(body.is_empty());
                if status != 304 || fields.contains("invalid") || fields.matches(':').count() > 1 {
                    assert!(
                        !reusable,
                        "do not pool contradictory/upgraded response: {raw}"
                    );
                } else {
                    assert!(reusable, "valid 304 representation metadata stays poolable");
                }
                let mut tail = String::new();
                reader.read_to_string(&mut tail).await.unwrap();
                assert_eq!(tail, "unread");
            }
        }
        let raw =
            b"HTTP/1.1 103 Early Hints\r\nContent-Length: 0\r\n\r\nHTTP/1.1 204 No Content\r\n\r\n";
        let (status, _, body, reusable, _) =
            read_response(&mut BufReader::new(raw.as_slice()), false)
                .await
                .unwrap();
        assert_eq!(status, 204);
        assert!(body.is_empty());
        assert!(
            !reusable,
            "framing violation on an interim response also retires the connection"
        );
    }

    #[tokio::test]
    async fn bodyless_post_success_preserves_cookie_and_retires_connection() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let cookie = format!("bodyless_consent_{}=saved", address.port());
        let expected_cookie = cookie.clone();
        let server = tokio::spawn(async move {
            for index in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut buffer = [0; 2048];
                while !request.windows(4).any(|part| part == b"\r\n\r\n") {
                    let read = socket.read(&mut buffer).await.unwrap();
                    assert_ne!(read, 0);
                    request.extend_from_slice(&buffer[..read]);
                }
                let head = String::from_utf8(request).unwrap();
                if index == 0 {
                    assert!(head.starts_with("POST /save "));
                    socket.write_all(format!("HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nSet-Cookie: {cookie}; Path=/\r\n\r\n").as_bytes()).await.unwrap();
                    // The client must not put this connection back in the pool.
                    assert_eq!(socket.read(&mut buffer).await.unwrap(), 0);
                } else {
                    assert!(head.starts_with("GET /check "));
                    assert!(
                        head.lines()
                            .any(|line| line.starts_with("Cookie: ") && line.contains(&cookie)),
                        "{head}"
                    );
                    socket
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                        )
                        .await
                        .unwrap();
                }
            }
        });
        tokio::time::timeout(Duration::from_secs(5), async {
            let url = Url::parse(&format!("http://{address}/save")).unwrap();
            let mut request = Request::get(url.clone());
            request.method = String::from("POST");
            let response = fetch(&request)
                .await
                .expect("204 plus zero length is a successful save");
            assert_eq!(response.status, 204);
            assert!(response.body.is_empty());
            assert!(cookies_for_request(&url).contains(&expected_cookie));
            let response = fetch(&Request::get(url.join("/check").unwrap()))
                .await
                .unwrap();
            assert_eq!(response.body, b"ok");
            server.await.unwrap();
        })
        .await
        .expect("bodyless responses must not hang on a keep-alive connection");
    }

    #[tokio::test]
    async fn rejects_incomplete_and_malformed_response_framing() {
        // Informational responses are consumed before the final response and
        // never become a pooled response for the next request (RFC 9112 §6.2).
        let raw: &[u8] = b"HTTP/1.1 103 Early Hints\r\nLink: </app.js>\r\n\r\nHTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello";
        let (status, _, body, _, _) = read_response(&mut BufReader::new(raw), false)
            .await
            .unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, b"hello");

        let raw: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nno";
        assert!(
            read_response(&mut BufReader::new(raw), false)
                .await
                .is_err()
        );
        let raw: &[u8] =
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabcX\r\n0\r\n\r\n";
        assert!(
            read_response(&mut BufReader::new(raw), false)
                .await
                .is_err()
        );
        let raw: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: not-a-number\r\n\r\nbody";
        assert!(
            read_response(&mut BufReader::new(raw), false)
                .await
                .is_err()
        );
        let raw: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: +1\r\n\r\nx";
        assert!(
            read_response(&mut BufReader::new(raw), false)
                .await
                .is_err(),
            "Content-Length is decimal DIGITs, not a signed integer"
        );
        let raw: &[u8] = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip\r\n\r\nbody";
        assert!(
            read_response(&mut BufReader::new(raw), false)
                .await
                .is_err()
        );
        // Bodyless statuses are covered separately: RFC 9112 §6.3 item 1
        // wins over framing validation, with unsafe connections retired.
        // A 304 may carry transfer metadata describing the representation that
        // would have been sent by an unconditional GET, but it has no body of
        // its own (RFC 9112 §6.1 and §6.3).
        let raw = b"HTTP/1.1 304 Not Modified\r\nTransfer-Encoding: chunked\r\n\r\n";
        let (status, _, body, _, _) = read_response(&mut BufReader::new(raw.as_slice()), false)
            .await
            .unwrap();
        assert_eq!(status, 304);
        assert!(body.is_empty());

        let mut raw = b"HTTP/1.1 200 OK\r\n".to_vec();
        for _ in 0..257 {
            raw.extend_from_slice(b"X-Repeat: value\r\n");
        }
        raw.extend_from_slice(b"\r\n");
        assert!(
            read_response(&mut BufReader::new(raw.as_slice()), false)
                .await
                .is_err()
        );

        let mut raw = b"HTTP/1.1 200 OK\r\nX-Long: ".to_vec();
        raw.extend(std::iter::repeat_n(b'a', 64 * 1024));
        raw.extend_from_slice(b"\r\n\r\n");
        assert!(
            read_response(&mut BufReader::new(raw.as_slice()), false)
                .await
                .is_err()
        );

        for raw in [
            b"HTTP/2 200 OK\r\nContent-Length: 0\r\n\r\n".as_slice(),
            b"HTTP/1.1 200 OK\r\nBad Header: value\r\n\r\n".as_slice(),
            b"HTTP/1.1 200 OK\r\nContent-Length : 0\r\n\r\n".as_slice(),
            b"HTTP/1.1 200 OK\r\nX-Bad: a\rb\r\n\r\n".as_slice(),
        ] {
            assert!(
                read_response(&mut BufReader::new(raw), false)
                    .await
                    .is_err(),
                "malformed response metadata must be rejected"
            );
        }
    }

    #[tokio::test]
    async fn document_task_scope_aborts_owned_work_and_rejects_new_work() {
        use std::sync::atomic::{AtomicBool, Ordering};

        struct Dropped(std::sync::Arc<AtomicBool>);
        impl Drop for Dropped {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }

        let scope = std::sync::Arc::new(PageTaskScope::default());
        let dropped = std::sync::Arc::new(AtomicBool::new(false));
        let marker = dropped.clone();
        scope.spawn(&tokio::runtime::Handle::current(), async move {
            let _guard = Dropped(marker);
            std::future::pending::<()>().await;
        });
        tokio::task::yield_now().await;
        scope.cancel();
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !dropped.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        let started = std::sync::Arc::new(AtomicBool::new(false));
        let marker = started.clone();
        scope.spawn(&tokio::runtime::Handle::current(), async move {
            marker.store(true, Ordering::Release);
        });
        tokio::task::yield_now().await;
        assert!(!started.load(Ordering::Acquire));

        let (abort, registration) = futures::future::AbortHandle::new_pair();
        let fresh_scope = PageTaskScope::default();
        fresh_scope.track_fetch(abort);
        let waiting = tokio::spawn(futures::future::Abortable::new(
            std::future::pending::<()>(),
            registration,
        ));
        fresh_scope.cancel();
        assert!(waiting.await.unwrap().is_err());
    }

    /// RFC 9112 §6.3: a response to a HEAD request never has a body, even
    /// though it advertises the `Content-Length`/`Transfer-Encoding` a GET
    /// would return. Reading one blocks forever on bytes that never arrive
    /// (a keep-alive socket stays open), which hung whole page loads until
    /// the JS budget expired — itsfoss.com's ad framework HEADs a 1×1 probe
    /// image. With `is_head`, the body is empty at the header boundary and
    /// the connection stays poolable — no read into a body that isn't there.
    #[tokio::test]
    async fn head_response_has_no_body_despite_content_length() {
        // Content-Length says 95, but a HEAD carries no body. The bytes
        // after the header block belong to the NEXT pipelined response, and
        // must not be consumed as this one's body.
        let raw: &[u8] =
            b"HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: 95\r\n\r\n";
        let (status, headers, body, reusable, _) =
            read_response(&mut BufReader::new(raw), true).await.unwrap();
        assert_eq!(status, 200);
        assert_eq!(headers["content-length"], "95");
        assert!(body.is_empty(), "HEAD body is always empty: {body:?}");
        assert!(
            reusable,
            "a keep-alive HEAD is at a clean boundary, poolable"
        );

        // Same for a chunked-advertising HEAD: no chunk data follows.
        let raw: &[u8] = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n";
        let (_, _, body, reusable, _) =
            read_response(&mut BufReader::new(raw), true).await.unwrap();
        assert!(body.is_empty(), "HEAD ignores Transfer-Encoding: {body:?}");
        assert!(reusable);
    }

    #[test]
    fn decodes_latin1() {
        let body = [b'c', b'a', b'f', 0xe9];
        assert_eq!(decode_body("text/html; charset=ISO-8859-1", &body), "café");
        assert_eq!(decode_body("text/html", "café".as_bytes()), "café");
    }

    #[tokio::test]
    async fn decompresses_an_unsolicited_gzip_body() {
        use flate2::{Compression, write::GzEncoder};
        use std::io::Write as _;
        // A server's gzip response exercises the same path as the advertised
        // `Accept-Encoding: gzip, deflate` request.
        let mut e = GzEncoder::new(Vec::new(), Compression::default());
        e.write_all(b"<html>hello compressed</html>").unwrap();
        let gz = e.finish().unwrap();
        let mut raw = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\
             Content-Encoding: gzip\r\nContent-Length: {}\r\n\r\n",
            gz.len()
        )
        .into_bytes();
        raw.extend_from_slice(&gz);
        let (status, headers, body, reusable, _) =
            read_response(&mut BufReader::new(&raw[..]), false)
                .await
                .unwrap();
        assert_eq!(status, 200);
        assert_eq!(headers["content-encoding"], "gzip");
        assert_eq!(body, b"<html>hello compressed</html>");
        // Decoding is a body transform after framing — the conn stays poolable.
        assert!(reusable, "intact framing ⇒ still reusable after decode");
    }

    #[test]
    fn decodes_deflate_as_zlib_or_raw() {
        use flate2::{
            Compression,
            write::{DeflateEncoder, ZlibEncoder},
        };
        use std::io::Write as _;
        let payload = b"deflate me, twice over, for a little entropy".to_vec();
        let mut h = Headers::new();
        h.insert("content-encoding".into(), "deflate".into());

        // The spec form: a zlib-wrapped DEFLATE stream.
        let mut z = ZlibEncoder::new(Vec::new(), Compression::default());
        z.write_all(&payload).unwrap();
        assert_eq!(decode_content_encoding(&h, z.finish().unwrap()), payload);

        // The common mislabelled form: a bare DEFLATE stream. The zlib decode
        // yields nothing, so we fall back to raw.
        let mut d = DeflateEncoder::new(Vec::new(), Compression::default());
        d.write_all(&payload).unwrap();
        assert_eq!(decode_content_encoding(&h, d.finish().unwrap()), payload);
    }

    #[test]
    fn tolerates_a_truncated_gzip_stream() {
        use flate2::{Compression, write::GzEncoder};
        use std::io::Write as _;
        let payload: Vec<u8> = (0..50_000u32).map(|i| (i % 251) as u8).collect();
        let mut e = GzEncoder::new(Vec::new(), Compression::default());
        e.write_all(&payload).unwrap();
        let mut gz = e.finish().unwrap();
        // Cut into the DEFLATE stream itself, not just the trailer.
        gz.truncate(gz.len().saturating_sub(100));
        let mut h = Headers::new();
        h.insert("content-encoding".into(), "gzip".into());
        let out = decode_content_encoding(&h, gz);
        // Keep whatever decoded before the stream ran out: a non-empty prefix,
        // never a panic.
        assert!(!out.is_empty(), "partial stream still yields its prefix");
        assert!(payload.starts_with(&out), "decoded bytes are a prefix");
        assert!(out.len() < payload.len(), "and it really is truncated");
    }

    #[test]
    fn passes_through_identity_and_undecodable_encodings() {
        let raw = b"plain bytes, untouched".to_vec();
        // No Content-Encoding header.
        assert_eq!(decode_content_encoding(&Headers::new(), raw.clone()), raw);
        // identity is a no-op.
        let mut h = Headers::new();
        h.insert("content-encoding".into(), "identity".into());
        assert_eq!(decode_content_encoding(&h, raw.clone()), raw);
        // br/zstd we don't decode (never advertised) — hand the bytes through.
        let mut h = Headers::new();
        h.insert("content-encoding".into(), "br".into());
        assert_eq!(decode_content_encoding(&h, raw.clone()), raw);
    }

    #[test]
    fn renders_html_into_rows() {
        use crate::layout2::ItemKind;
        let base = Url::parse("https://example.com/dir/page.html").unwrap();
        let html = r#"
            <html><body>
            <h1>Big Title</h1>
            <p>Plain paragraph text.</p>
            <p><a href="other.html">A relative link</a></p>
            <p>Multi: <a href="/one">first</a> and <a href="https://other.example/">second</a>.</p>
            <pre>preformatted   text</pre>
            <p><img src="/cat.png" alt="a cat"></p>
            </body></html>"#;
        let doc = parse(
            &base,
            "text/html",
            html.as_bytes(),
            60,
            0,
            &Default::default(),
        );
        assert_eq!(item(&doc, "Big Title").kind, ItemKind::Heading(1));
        assert!(item(&doc, "Plain paragraph").link.is_none());

        assert_eq!(
            item(&doc, "A relative link").link,
            Some(Link::Http(
                Url::parse("https://example.com/dir/other.html").unwrap()
            ))
        );

        // Multi-link: both anchors are separate, selectable items.
        assert_eq!(
            item(&doc, "first").link,
            Some(Link::Http(Url::parse("https://example.com/one").unwrap()))
        );
        assert!(item(&doc, "second").link.is_some());

        assert_eq!(item(&doc, "preformatted   text").kind, ItemKind::Pre);
        // The image renders its alt text (real pixels arrive in L3).
        assert_eq!(item(&doc, "a cat").kind, ItemKind::Image);
    }

    /// The shape of rubymaelstrom.com/chat: a POST form with a hidden
    /// session, a text input, and a submit button.
    const CHAT_PAGE: &str = r#"
        <html><body>
        <p>Talkie says hello.</p>
        <form method="POST" action="/chat">
          <input type="hidden" name="session" value="cafe123">
          <input type="text" name="msg" placeholder="Type a message...">
          <button type="submit">Send</button>
        </form>
        </body></html>"#;

    #[test]
    fn parses_forms_into_widgets() {
        let base = Url::parse("https://example.com/chat").unwrap();
        let doc = parse(
            &base,
            "text/html",
            CHAT_PAGE.as_bytes(),
            60,
            0,
            &Default::default(),
        );

        assert_eq!(doc.forms.len(), 1);
        let form = &doc.forms[0];
        assert_eq!(form.method, FormMethod::Post);
        assert_eq!(form.action.as_str(), "https://example.com/chat");
        let names: Vec<&str> = form.fields.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["session", "msg", ""]);
        assert_eq!(form.fields[0].kind, FieldKind::Hidden);
        assert_eq!(form.fields[1].kind, FieldKind::Text);
        assert_eq!(form.fields[2].kind, FieldKind::Submit);
        assert_eq!(form.fields[2].label, "Send");

        // The hidden field never renders; the others are selectable Form
        // items with the right control links. (The text input pads to its used
        // width — the UA-default 20ch box — so match the label, not `[label]`.)
        assert!(!has_item(&doc, "cafe123"));
        let input = item(&doc, "Type a message...");
        assert_eq!(input.kind, crate::layout2::ItemKind::Form);
        assert_eq!(input.link, Some(Link::Form { form: 0, field: 1 }));
        let button = item(&doc, "[ Send ]");
        assert_eq!(button.kind, crate::layout2::ItemKind::Form);
        assert_eq!(button.link, Some(Link::Form { form: 0, field: 2 }));
    }

    #[test]
    fn parses_google_consent_accept_and_decline_as_post_forms() {
        // The consent page is intentionally script-free: desktop activation
        // must use the native form path, retain hidden state, and submit the
        // selected input[type=submit] rather than treating its aria-label as
        // a second visual control.
        let base = Url::parse("https://consent.google.ca/ml").unwrap();
        let html = r#"
            <form action="https://consent.google.ca/save" method="POST">
              <input type="hidden" name="gl" value="GB">
              <input type="hidden" name="continue" value="https://www.google.ca/">
              <input type="submit" value="Reject all" aria-label="Reject all">
            </form>
            <form action="https://consent.google.ca/save" method="POST">
              <input type="hidden" name="gl" value="GB">
              <input type="hidden" name="continue" value="https://www.google.ca/">
              <input type="submit" value="Accept all" aria-label="Accept all">
            </form>"#;
        let doc = parse(
            &base,
            "text/html",
            html.as_bytes(),
            80,
            0,
            &Default::default(),
        );
        assert_eq!(doc.forms.len(), 2);
        assert_eq!(doc.forms[0].fields.len(), 3);
        assert_eq!(
            doc.forms[0].encode(Some(2)),
            "gl=GB&continue=https%3A%2F%2Fwww.google.ca%2F"
        );
        assert_eq!(
            doc.forms[1].encode(Some(2)),
            "gl=GB&continue=https%3A%2F%2Fwww.google.ca%2F"
        );
        assert!(has_item(&doc, "[ Reject all ]"));
        assert!(has_item(&doc, "[ Accept all ]"));
    }

    #[test]
    fn pending_plain_edit_paints_immediately_without_replacing_the_authored_surface() {
        let base = Url::parse("https://example.test/").unwrap();
        let dom = crate::dom::Dom::parse_document(
            r#"<style>
          body{margin:0} #editor{width:240px;color:white;background:#123456}
          #editor p{margin:0;white-space:pre-wrap;font-size:17px}
          #editor p:empty:before{content:'Placeholder';color:gray}
          </style><div id=editor contenteditable=true><p></p></div>
          <div id=rich contenteditable=true><b>Bold text</b></div>"#,
        );
        let rendered = render_arena(
            &dom,
            &base,
            crate::layout2::Viewport::new(640., 480.),
            1.,
            None,
            &Default::default(),
        );
        let node = dom.get_by_id("editor").unwrap();
        let presentation = &rendered.rich_editors[&node];
        assert!(presentation.pending_text.is_some());
        assert!(
            rendered.rich_editors[&dom.get_by_id("rich").unwrap()]
                .pending_text
                .is_none()
        );
        let mut paint = rendered.layout.paint.primitives.clone();
        let surfaces = paint
            .iter()
            .filter(|p| {
                matches!(
                    p,
                    crate::render::DisplayCommand::Fill { .. }
                        | crate::render::DisplayCommand::FillRect { .. }
                )
            })
            .count();
        assert!(crate::render::paint_pending_editor_text(
            &mut paint,
            "Typed before JS finishes",
            crate::render::CssRect::new(0., 0., 240., 60.),
            presentation
        ));
        let texts: Vec<_> = paint
            .iter()
            .filter_map(|p| match p {
                crate::render::DisplayCommand::GlyphRun { shaped, .. } => {
                    Some(shaped.text.as_str())
                }
                _ => None,
            })
            .collect();
        assert!(texts.iter().any(|t| t.contains("Typed before")));
        assert!(!texts.iter().any(|t| t.contains("Placeholder")));
        assert!(texts.iter().any(|t| t.contains("Bold text")));
        assert_eq!(
            surfaces,
            paint
                .iter()
                .filter(|p| matches!(
                    p,
                    crate::render::DisplayCommand::Fill { .. }
                        | crate::render::DisplayCommand::FillRect { .. }
                ))
                .count()
        );
    }

    #[test]
    fn contenteditable_preserves_authored_content_and_control_binding() {
        // HTML #attr-contenteditable + CSS UI #appearance-switching. The
        // synthetic form binding is interaction metadata, not a replaced box.
        let base = Url::parse("https://example.test/").unwrap();
        let mut dom = crate::dom::Dom::parse_document(
            r#"<style>
            body{margin:0} #editor{width:300px;color:blue;caret-color:lime}
            #editor p{margin:0;line-height:30px;font-size:20px} #editor b{color:red}
            #empty:before{content:attr(data-placeholder);color:green}
            </style><div id=editor contenteditable=true><p><b>Rich</b> text</p><p>Second line</p></div>
            <div id=empty contenteditable=true data-placeholder="Ask anything"></div>"#,
        );
        dom.set_render_clickables(Default::default(), true);
        let rendered = render_arena(
            &dom,
            &base,
            crate::layout2::Viewport::new(640., 480.),
            1.,
            None,
            &Default::default(),
        );
        let editor = dom.get_by_id("editor").unwrap();
        assert!(rendered.controls.contains_key(&editor));
        let presentation = &rendered.rich_editors[&editor];
        assert_eq!(presentation.style.size, 20.0);
        assert_eq!(
            presentation.style.line_height,
            crate::text::CssLineHeight::Length(30.0)
        );
        assert_eq!(
            presentation.caret_color,
            crate::render::PaintColor::Rgba(0, 255, 0, 255)
        );
        let mut overlay = Vec::new();
        crate::render::paint_rich_editor_overlay(
            &mut overlay,
            &crate::render::EditorVisual {
                text: "must not repaint these glyphs".into(),
                selection: vec![],
                caret: Some(crate::render::CssRect::new(15.0, 0.0, 1.5, 30.0)),
            },
            crate::render::CssRect::new(0.0, 0.0, 300.0, 60.0),
            presentation,
        );
        assert_eq!(
            overlay.len(),
            3,
            "only a clipped caret, no opaque widget or duplicated text"
        );
        assert!(matches!(
            overlay[1],
            crate::render::DisplayCommand::FillRect {
                color: crate::render::PaintColor::Rgba(0, 255, 0, 255),
                ..
            }
        ));
        let paint = &rendered.layout.paint.primitives;
        let glyphs = paint
            .iter()
            .filter_map(|p| match p {
                crate::render::DisplayCommand::GlyphRun { shaped, .. } => {
                    Some(shaped.text.as_str())
                }
                _ => None,
            })
            .collect::<String>();
        assert!(
            glyphs.contains("Rich")
                && glyphs.contains("Second line")
                && glyphs.contains("Ask anything"),
            "{glyphs}"
        );
        assert!(paint.iter().any(|p|matches!(p,crate::render::DisplayCommand::GlyphRun{color:crate::render::PaintColor::Rgba(255,0,0,255),shaped,..} if shaped.text.contains("Rich"))));
        assert!(paint.iter().any(|p|matches!(p,crate::render::DisplayCommand::GlyphRun{color:crate::render::PaintColor::Rgba(0,128,0,255),shaped,..} if shaped.text.contains("Ask anything"))), "generated placeholder uses its own pseudo color");
    }

    #[test]
    fn contenteditable_host_becomes_an_editable_field() {
        // A `contenteditable` div (a rich-text editor root — ProseMirror/TipTap,
        // a comment box) is surfaced as a synthetic, un-submitted Textarea field
        // in an implicit form, so it rides the existing editable machinery. Its
        // placeholder (here `data-placeholder`) is the widget label, and the
        // whitespace-only initial content reads as an empty editor.
        let base = Url::parse("https://example.com/").unwrap();
        let doc = parse(
            &base,
            "text/html",
            b"<body><div contenteditable=\"true\" data-placeholder=\"Type here\">\n</div></body>",
            60,
            0,
            &Default::default(),
        );
        assert_eq!(doc.forms.len(), 1);
        let field = &doc.forms[0].fields[0];
        assert_eq!(field.kind, FieldKind::Textarea);
        assert!(
            field.name.is_empty(),
            "an editor is not a submitted control"
        );
        assert!(field.value.is_empty(), "whitespace-only is an empty editor");
        // data-placeholder is application metadata, not a native HTML
        // placeholder attribute. It must not replace the authored subtree.
        assert_eq!(field.label, "Type here");
        assert!(
            !doc.rows
                .iter()
                .flat_map(|r| &r.items)
                .any(|i| i.text.contains("Type here"))
        );

        // `contenteditable="false"` is explicitly NOT editable.
        let doc2 = parse(
            &base,
            "text/html",
            b"<body><div contenteditable=\"false\">x</div></body>",
            60,
            0,
            &Default::default(),
        );
        assert!(
            doc2.forms.is_empty(),
            "contenteditable=false is not a field"
        );
    }

    #[test]
    fn seeds_form_values_across_reparse() {
        let base = Url::parse("https://example.com/chat").unwrap();
        let mut doc = parse(
            &base,
            "text/html",
            CHAT_PAGE.as_bytes(),
            60,
            0,
            &Default::default(),
        );
        doc.forms[0].fields[1].value = String::from("hello there");

        // A resize-style re-parse at another width keeps the value.
        let rewrapped = parse_seeded(
            &base,
            "text/html",
            CHAT_PAGE.as_bytes(),
            40,
            0,
            (8, 16),
            Some(&doc.forms),
            &Default::default(),
            no_alpha(),
        );
        assert_eq!(rewrapped.forms[0].fields[1].value, "hello there");
        assert!(
            // The widget pads to its used-width box, so the value no longer
            // hugs the closing bracket — match the value itself.
            has_item(&rewrapped, "hello there"),
            "widget item shows the typed value"
        );
    }

    #[test]
    fn renders_get_forms_with_selects_and_boxes() {
        let base = Url::parse("http://search.example/").unwrap();
        let html = r#"
            <form action="lite/search">
              <input type="text" name="q">
              <select name="region">
                <option value="all" selected>Everywhere</option>
                <option value="us">United States</option>
              </select>
              <input type="checkbox" name="safe" checked>
              <input type="submit" value="Search">
            </form>"#;
        let doc = parse(
            &base,
            "text/html",
            html.as_bytes(),
            60,
            0,
            &Default::default(),
        );
        let form = &doc.forms[0];
        assert_eq!(form.method, FormMethod::Get);
        assert_eq!(form.action.as_str(), "http://search.example/lite/search");
        assert_eq!(
            form.fields[1].kind,
            FieldKind::Select(vec![
                (String::from("Everywhere"), String::from("all")),
                (String::from("United States"), String::from("us")),
            ])
        );
        assert_eq!(form.fields[1].value, "all");
        // Each control is a Form item showing its widget label (adjacent
        // inline widgets may carry a trailing separator space).
        assert!(has_item(&doc, "[Everywhere ▾]"));
        assert!(has_item(&doc, "[x] safe"));
        assert!(
            has_item(&doc, "Search"),
            "terminal submit label was lost: {:?}",
            doc.rows
        );
    }

    #[test]
    fn forms_without_submit_do_not_gain_a_rendered_control() {
        let base = Url::parse("http://example.com/").unwrap();
        let html = r#"<form action="/go"><input type="text" name="q"></form>"#;
        let doc = parse(
            &base,
            "text/html",
            html.as_bytes(),
            60,
            0,
            &Default::default(),
        );
        assert_eq!(doc.forms[0].fields.len(), 1);
        assert_eq!(doc.forms[0].fields[0].kind, FieldKind::Text);
        assert!(!has_item(&doc, "[ Submit ]"));
    }
}
