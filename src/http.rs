//! A deliberately small HTTP/1.1 client for the text web.
//!
//! Persistent connections (a RAM-only keep-alive pool — measured
//! 2026-06-12: serial fresh-TLS-per-request was 85% of a page load),
//! responses delimited precisely (Content-Length / chunked / EOF), no
//! compression (`Accept-Encoding: identity`), redirects followed here.
//! HTTPS uses standard WebPKI validation (`tls::webpki_connector`),
//! not TOFU. HTML renders through our own arena DOM (`dom.rs`) laid out
//! into positioned rows (`layout.rs`); forms are extracted from that
//! same arena. With JS on, `execute_js` first runs the page's scripts
//! against the DOM (js.rs) and lays out what they built.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use futures::StreamExt as _;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use url::Url;

use crate::doc::{Doc, DocLine, Field, FieldKind, Form, FormMethod, Kind, Link};
use crate::tls;

// Per-response ceiling — a memory guard, not a correctness limit. The big
// web ships large app bundles: YouTube's `kevlar_base` is ~10.5 MB of
// minified JS, so 5 MB silently dropped it. 16 MB clears today's giants with
// headroom while staying bounded (bodies are transient: parsed, then only the
// post-JS HTML is retained).
const MAX_BODY: usize = 16 * 1024 * 1024;
// A single request's wall cap. Generous because a streamed LLM chat completion
// (Open WebUI → llama.cpp) holds the connection open for the WHOLE generation —
// a reasoning model can think for a minute or more before the body closes. The
// real per-context bound is the JS budget (a page LOAD is still capped by
// `WALL_BUDGET`; an interactive dispatch extends only while a fetch is in
// flight), so this only needs to be long enough not to sever a working stream.
const FETCH_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_REDIRECTS: usize = 10;
pub(crate) const USER_AGENT: &str = "TRust/0.1";

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
}

impl Request {
    pub fn get(url: Url) -> Self {
        Self {
            method: String::from("GET"),
            url,
            body: None,
            headers: Vec::new(),
        }
    }
}

#[derive(Debug)]
pub struct Response {
    /// The URL that finally answered, after redirects.
    pub url: Url,
    pub status: u16,
    pub content_type: String,
    pub body: Vec<u8>,
    /// What the page's JavaScript did, when `execute_js` ran it.
    pub js: Option<crate::js::Outcome>,
    /// The living page behind this response, when its JS left
    /// something to interact with.
    pub live: Option<LivePage>,
    /// Set when the response is a bot-mitigation interstitial (AWS WAF,
    /// Cloudflare, …) rather than the real page — a short human-readable
    /// label like `"AWS WAF (challenge)"`. These walls serve a JS
    /// proof-of-work / fingerprint challenge a non-browser client can't
    /// pass, so what we render is an empty shell; the label lets the UI
    /// say so instead of showing a blank page. See `detect_challenge`.
    pub challenge: Option<String>,
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
    pub body: Vec<u8>,
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
pub struct PageCache {
    map: std::sync::Mutex<HashMap<String, SharedFetch>>,
}

impl std::fmt::Debug for PageCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let n = self.map.lock().map(|m| m.len()).unwrap_or(0);
        write!(f, "PageCache({n} entries)")
    }
}

impl PageCache {
    /// Get-or-start the shared fetch for `url`. The first caller spawns it
    /// (driven concurrently on the runtime, so speculative prefetch makes
    /// progress before anyone awaits); every later caller shares that one
    /// request. The caller is responsible for cap/`subresource_allowed`
    /// gating BEFORE starting a brand-new fetch (see `page_net_prepare`).
    pub fn fetch(&self, handle: &tokio::runtime::Handle, url: Url) -> SharedFetch {
        use futures::future::FutureExt as _;
        let key = url.to_string();
        let mut map = self.map.lock().unwrap();
        if let Some(f) = map.get(&key) {
            return f.clone();
        }
        let fut = async move {
            match fetch(&Request::get(url)).await {
                Ok(r) => Ok(std::sync::Arc::new(CachedResp {
                    status: r.status,
                    content_type: r.content_type,
                    body: r.body,
                })),
                Err(_) => Err(()),
            }
        }
        .boxed()
        .shared();
        map.insert(key, fut.clone());
        // Drive it now (dropping the JoinHandle doesn't cancel the task):
        // speculation overlaps with everything, even with no awaiter yet.
        handle.spawn(fut.clone());
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

    /// An existing entry (in-flight or done) for `url`, or None. The
    /// page's own `fetch()` uses this to join a known subresource request
    /// WITHOUT caching arbitrary API GETs — a miss falls through to a
    /// normal, uncached fetch so polling stays honest.
    pub fn peek(&self, url: &Url) -> Option<SharedFetch> {
        self.map.lock().unwrap().get(url.as_str()).cloned()
    }

    /// Block the calling (page) thread on a shared fetch. The module
    /// loader needs this: it must NOT `.await` (yielding would let Boa
    /// interleave another module load and cross up its per-referrer
    /// `loaded_modules`), and it can't `block_on` (it already runs inside
    /// one). With a runtime `handle` the fetch is driven on the runtime
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

    /// Seed an already-fetched body (the initial `execute_js` prefetch).
    pub fn seed(&self, url: String, status: u16, content_type: String, body: Vec<u8>) {
        use futures::future::FutureExt as _;
        let resp = std::sync::Arc::new(CachedResp {
            status,
            content_type,
            body,
        });
        let fut = async move { Ok(resp) }.boxed().shared();
        self.map.lock().unwrap().insert(url, fut);
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
    let first = fetch(&Request::get(url.clone())).await;
    if first.is_ok() || url.scheme() != "https" {
        return first;
    }
    let http_str = format!("http://{}", &url.as_str()["https://".len()..]);
    let Some(http_url) = parse_url(&http_str) else {
        return first;
    };
    match fetch(&Request::get(http_url)).await {
        Ok(response) => Ok(response),
        Err(_) => first,
    }
}

/// A process-global monotonic origin shared by every trace line (net
/// requests in http.rs, JS phase markers in js.rs) so a single load's
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
/// 301/302/303 turn into GET (dropping the body); 307/308 keep both.
/// `TRUST_NET_TRACE=1` prints one timing line per request to stderr —
/// the diagnostic for "where did the page-load time go". Each line shows
/// `@<start>ms +<duration>ms` against the shared `trace_origin`, so the
/// timeline (which requests overlap, where the gaps are) is reconstructable.
pub async fn fetch(request: &Request) -> Result<Response, String> {
    if std::env::var_os("TRUST_NET_TRACE").is_none() {
        return fetch_redirecting(request).await;
    }
    let at = trace_ms();
    let started = std::time::Instant::now();
    let result = fetch_redirecting(request).await;
    let ms = started.elapsed().as_millis();
    match &result {
        Ok(r) => eprintln!(
            "net: @{at:>6}ms +{ms:>5}ms {} {}B {}",
            r.status,
            r.body.len(),
            request.url
        ),
        Err(e) => eprintln!("net: @{at:>6}ms +{ms:>5}ms ERR {} ({e})", request.url),
    }
    result
}

async fn fetch_redirecting(request: &Request) -> Result<Response, String> {
    let mut request = request.clone();
    for _ in 0..=MAX_REDIRECTS {
        let response = tokio::time::timeout(FETCH_TIMEOUT, fetch_once(&request))
            .await
            .map_err(|_| String::from("timed out"))??;
        match response.status {
            301 | 302 | 303 | 307 | 308 => {}
            _ => return Ok(response),
        }
        // Redirect: fetch_once stashes the Location header in
        // content_type for 3xx (their bodies are never rendered).
        let target = request
            .url
            .join(response.content_type.trim())
            .map_err(|e| format!("bad redirect location: {e}"))?;
        match target.scheme() {
            "http" | "https" => {}
            other => return Err(format!("redirect leaves the web: {other}://")),
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
                .and_then(|src| referrer_for(&src, &target))
            {
                Some(v) => request.headers[pos].1 = v,
                None => {
                    request.headers.remove(pos);
                }
            }
        }
        if matches!(response.status, 301..=303) {
            request.method = String::from("GET");
            request.body = None;
        }
        request.url = target;
    }
    Err(format!("too many redirects (>{MAX_REDIRECTS})"))
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
    io: BufReader<Conn>,
    since: Instant,
}

/// The keep-alive pool: idle connections per (scheme, host, port),
/// RAM-only, newest-first reuse. A page load that used to pay a fresh
/// DNS+TCP+TLS per subresource (~500ms each on the wide net) now pays
/// it once per host.
static POOL: std::sync::LazyLock<std::sync::Mutex<HashMap<PoolKey, Vec<IdleConn>>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));

/// Servers commonly drop idle connections after ~60s; don't bother
/// trying one older than this.
const POOL_IDLE_TTL: Duration = Duration::from_secs(30);
const POOL_MAX_IDLE_PER_KEY: usize = 8;

fn pool_get(key: &PoolKey) -> Option<BufReader<Conn>> {
    let mut pool = POOL.lock().ok()?;
    let idle = pool.get_mut(key)?;
    while let Some(conn) = idle.pop() {
        if conn.since.elapsed() < POOL_IDLE_TTL {
            return Some(conn.io);
        }
    }
    None
}

fn pool_put(key: PoolKey, io: BufReader<Conn>) {
    if let Ok(mut pool) = POOL.lock() {
        let idle = pool.entry(key).or_default();
        if idle.len() < POOL_MAX_IDLE_PER_KEY {
            idle.push(IdleConn {
                io,
                since: Instant::now(),
            });
        }
    }
}

// ---- RAM-only cookie jar ----------------------------------------------
//
// Cookies are ON by default, RAM-only, and never persisted. We CAPTURE
// `Set-Cookie`, expose non-HttpOnly matches to page JS via
// `document.cookie`, and send matching cookies back on requests. The
// privacy line is exact-host isolation: `Domain=` is ignored, so a cookie
// set by `shop.example` is only for `shop.example`, never `example` or
// `other.example`. `set cookies off` disables capture, sends, and
// document.cookie exposure without deleting the in-memory jar. Subset of
// RFC 6265: name=value plus Path/Secure/HttpOnly/Max-Age(=0 deletes);
// Domain/Expires/SameSite ignored.

#[derive(Clone)]
struct Cookie {
    name: String,
    value: String,
    domain: String, // exact lowercased host that created it
    path: String,
    secure: bool,
    http_only: bool,
}

static COOKIE_JAR: std::sync::LazyLock<std::sync::Mutex<Vec<Cookie>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(Vec::new()));
static COOKIES_ENABLED: AtomicBool = AtomicBool::new(true);

const COOKIE_JAR_MAX: usize = 1000;

#[cfg(test)]
pub(crate) static COOKIE_TEST_LOCK: std::sync::LazyLock<std::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(()));

pub(crate) fn set_cookies_enabled(enabled: bool) {
    COOKIES_ENABLED.store(enabled, Ordering::Relaxed);
}

pub(crate) fn cookies_enabled() -> bool {
    COOKIES_ENABLED.load(Ordering::Relaxed)
}

/// Store a `Set-Cookie` header value against the response URL. `from_js`
/// (a `document.cookie` write) forces off HttpOnly, as the platform does.
fn store_cookie(url: &Url, line: &str, from_js: bool) {
    if !cookies_enabled() {
        return;
    }
    let (nv, rest) = line.split_once(';').unwrap_or((line, ""));
    let Some((name, value)) = nv.split_once('=') else {
        return;
    };
    let (name, value) = (name.trim().to_string(), value.trim().to_string());
    if name.is_empty() {
        return;
    }
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    let (domain, mut path) = (host.clone(), String::from("/"));
    let (mut secure, mut http_only, mut max_age) = (false, false, None::<i64>);
    for attr in rest.split(';') {
        let attr = attr.trim();
        let (k, v) = attr
            .split_once('=')
            .map_or((attr.to_ascii_lowercase(), String::new()), |(k, v)| {
                (k.trim().to_ascii_lowercase(), v.trim().to_string())
            });
        match k.as_str() {
            // Deliberately ignored: cookies are exact-host only in TRust.
            "domain" => {}
            "path" if v.starts_with('/') => path = v,
            "secure" => secure = true,
            "httponly" => http_only = true,
            "max-age" => max_age = v.parse().ok(),
            _ => {}
        }
    }
    if from_js {
        http_only = false;
    }
    let mut jar = COOKIE_JAR.lock().unwrap();
    jar.retain(|c| !(c.name == name && c.domain == domain && c.path == path));
    if max_age.is_some_and(|m| m <= 0) {
        return; // deletion (the retain above removed it)
    }
    jar.push(Cookie {
        name,
        value,
        domain,
        path,
        secure,
        http_only,
    });
    if jar.len() > COOKIE_JAR_MAX {
        jar.remove(0);
    }
}

fn cookie_domain_match(host: &str, c: &Cookie) -> bool {
    host == c.domain
}

/// The `document.cookie` string for a page: name=value pairs for every
/// jar cookie that domain/path/secure-matches, excluding HttpOnly (which
/// JS can never read).
pub(crate) fn cookies_for_js(page: &Url) -> String {
    if !cookies_enabled() {
        return String::new();
    }
    let host = page.host_str().unwrap_or_default().to_ascii_lowercase();
    let path = page.path();
    let https = page.scheme() == "https";
    let jar = COOKIE_JAR.lock().unwrap();
    jar.iter()
        .filter(|c| !c.http_only)
        .filter(|c| !c.secure || https)
        .filter(|c| cookie_domain_match(&host, c))
        .filter(|c| path == c.path || path.starts_with(&c.path))
        .map(|c| format!("{}={}", c.name, c.value))
        .collect::<Vec<_>>()
        .join("; ")
}

/// A `document.cookie = "..."` write from page JS. Stored in the same
/// RAM-only, exact-host jar used for requests.
pub(crate) fn set_cookie_from_js(page: &Url, line: &str) {
    store_cookie(page, line, true);
}

pub(crate) fn cookies_for_request(url: &Url) -> String {
    if !cookies_enabled() {
        return String::new();
    }
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    let path = url.path();
    let https = url.scheme() == "https";
    let jar = COOKIE_JAR.lock().unwrap();
    jar.iter()
        .filter(|c| !c.secure || https)
        .filter(|c| cookie_domain_match(&host, c))
        .filter(|c| path == c.path || path.starts_with(&c.path))
        .map(|c| format!("{}={}", c.name, c.value))
        .collect::<Vec<_>>()
        .join("; ")
}

async fn dial(scheme: &str, host: &str, port: u16) -> Result<BufReader<Conn>, String> {
    let stream = TcpStream::connect((host, port))
        .await
        .map_err(|e| e.to_string())?;
    let _ = stream.set_nodelay(true);
    let conn = if scheme == "https" {
        let name = tls::server_name(host)?;
        let stream = tls::webpki_connector()
            .connect(name, stream)
            .await
            .map_err(|e| format!("TLS: {e}"))?;
        Conn::Tls(Box::new(stream))
    } else {
        Conn::Plain(stream)
    };
    Ok(BufReader::new(conn))
}

async fn fetch_once(request: &Request) -> Result<Response, String> {
    let url = &request.url;
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
            if let Ok(parts) = exchange(&mut io, request, &host, port).await {
                return finish_response(request, parts, io, key);
            }
        }
    }
    let mut io = dial(url.scheme(), &host, port).await?;
    let parts = exchange(&mut io, request, &host, port).await?;
    finish_response(request, parts, io, key)
}

/// Build the Response and return a still-healthy connection to the
/// pool.
/// Recognise a bot-mitigation interstitial from response headers. Walls
/// like AWS WAF and Cloudflare answer a normal request with a JS challenge
/// page (proof-of-work + browser fingerprint) instead of the real content;
/// a non-browser client can't pass it, so the body we'd render is an empty
/// shell. Detecting it from the response header — host-agnostic, no
/// site-sniffing — lets the UI tell the user what happened rather than
/// showing a blank page. Returns a short label, e.g. `"AWS WAF (challenge)"`.
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
) -> Result<Response, String> {
    if reusable {
        pool_put(key, io);
    }
    let url = &request.url;
    for line in &set_cookies {
        store_cookie(url, line, false);
    }
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
            body: Vec::new(),
            js: None,
            live: None,
            challenge: None,
        });
    }
    let content_type = headers
        .get("content-type")
        .cloned()
        .unwrap_or_else(|| String::from("text/html"));
    Ok(Response {
        url: url.clone(),
        status,
        content_type,
        body,
        js: None,
        live: None,
        challenge: detect_challenge(&headers),
    })
}

type Headers = HashMap<String, String>;

/// Write the request, read exactly one response. The bool says the
/// connection is positioned at the next message boundary — safe to
/// pool. Truncation (server hung up early, missing TLS close_notify)
/// is tolerated as on the small net: keep what arrived, don't reuse.
async fn exchange(
    io: &mut BufReader<Conn>,
    request: &Request,
    host: &str,
    port: u16,
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
    // `accept` is the exception: a page (XHR/fetch) may legitimately ask for
    // `application/json`, so its value overrides our HTML default.
    const MANAGED: &[&str] = &[
        "host",
        "user-agent",
        "content-length",
        "content-type",
        "connection",
        "cookie",
        "accept-encoding",
    ];
    let page_accept = request
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("accept"))
        .map(|(_, v)| v.as_str());
    let mut head = format!(
        "{} {} HTTP/1.1\r\n\
         Host: {}\r\n\
         User-Agent: {}\r\n\
         Accept: {}\r\n\
         Accept-Encoding: identity\r\n\
         Connection: keep-alive\r\n",
        request.method,
        path,
        host_header,
        USER_AGENT,
        page_accept.unwrap_or("text/html, text/*;q=0.8, */*;q=0.1"),
    );
    let cookie = cookies_for_request(url);
    if !cookie.is_empty() {
        head.push_str(&format!("Cookie: {cookie}\r\n"));
    }
    if let Some((content_type, payload)) = &request.body {
        head.push_str(&format!(
            "Content-Type: {}\r\nContent-Length: {}\r\n",
            content_type,
            payload.len()
        ));
    }
    // Page-supplied headers (X-Requested-With — which servers read as
    // `$request->ajax()` — Authorization, X-CSRF-TOKEN, …), minus the managed
    // set and `accept` (already folded in above). A header with no value or a
    // CR/LF (injection) is dropped.
    for (k, v) in &request.headers {
        let lk = k.to_ascii_lowercase();
        if MANAGED.contains(&lk.as_str())
            || lk == "accept"
            || k.is_empty()
            || k.contains(['\r', '\n', ':'])
            || v.contains(['\r', '\n'])
        {
            continue;
        }
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");

    io.write_all(head.as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    if let Some((_, payload)) = &request.body {
        io.write_all(payload).await.map_err(|e| e.to_string())?;
    }
    io.flush().await.map_err(|e| e.to_string())?;

    read_response(io).await
}

/// One CRLF-terminated line, sans terminator. Err on EOF-before-line.
async fn read_line<R: tokio::io::AsyncBufRead + Unpin>(io: &mut R) -> Result<String, String> {
    let mut buf = Vec::new();
    loop {
        let n = io
            .read_until(b'\n', &mut buf)
            .await
            .map_err(|e| e.to_string())?;
        if n == 0 && buf.is_empty() {
            return Err(String::from("connection closed"));
        }
        if n == 0 || buf.last() == Some(&b'\n') {
            while buf.last().is_some_and(|&b| b == b'\n' || b == b'\r') {
                buf.pop();
            }
            return Ok(String::from_utf8_lossy(&buf).into_owned());
        }
        if buf.len() > 64 * 1024 {
            return Err(String::from("header line exceeds 64 KB"));
        }
    }
}

/// Read one response off the stream: status line, headers, then a body
/// delimited by Content-Length, chunked encoding, or (last resort)
/// EOF. Returns (status, headers, body, reusable).
async fn read_response<R: AsyncRead + Unpin>(
    io: &mut BufReader<R>,
) -> Result<(u16, Headers, Vec<u8>, bool, Vec<String>), String> {
    let status_line = read_line(io).await?;
    let http11 = status_line.starts_with("HTTP/1.1");
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| format!("malformed status line: {status_line:?}"))?;
    let mut headers = HashMap::new();
    // Set-Cookie is multi-valued; the dedup'ing HashMap would lose all but
    // the last, so collect them separately for the cookie jar.
    let mut set_cookies = Vec::new();
    loop {
        let line = read_line(io).await?;
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            let name = name.trim().to_ascii_lowercase();
            if name == "set-cookie" {
                set_cookies.push(value.trim().to_string());
            }
            headers.insert(name, value.trim().to_string());
        }
        if headers.len() > 256 {
            return Err(String::from("too many response headers"));
        }
    }

    let mut reusable = http11
        && !headers
            .get("connection")
            .is_some_and(|c| c.to_ascii_lowercase().contains("close"));

    // TRust never plays video/audio — video is mpv's job (the `v` key /
    // YouTube auto-route). Downloading media bodies is pure waste: they're
    // large and a real budget sink (YouTube prefetches feed-tile video
    // previews via fetch() this way, which starves the actual page render).
    // Skip the body entirely so a page falls back to its static poster /
    // thumbnail. The unread body means this socket can't be pooled. General
    // policy, not a site rule — any video/audio response anywhere is dropped.
    if headers.get("content-type").is_some_and(|c| {
        let c = c.trim_start().to_ascii_lowercase();
        c.starts_with("video/") || c.starts_with("audio/")
    }) {
        return Ok((status, headers, Vec::new(), false, set_cookies));
    }

    let body = if matches!(status, 204 | 304) || matches!(status, 100..=199) {
        Vec::new()
    } else if headers
        .get("transfer-encoding")
        .is_some_and(|t| t.to_ascii_lowercase().contains("chunked"))
    {
        let (body, complete) = read_chunked(io).await?;
        reusable &= complete;
        body
    } else if let Some(len) = headers.get("content-length").and_then(|l| l.parse().ok()) {
        if len > MAX_BODY {
            return Err(format!(
                "response exceeds {} MB cap",
                MAX_BODY / (1024 * 1024)
            ));
        }
        let (body, complete) = read_exactly(io, len).await?;
        reusable &= complete;
        body
    } else {
        // No delimiter: the old read-to-EOF world. Never reusable.
        reusable = false;
        read_to_eof(io).await?
    };
    // Undo any (unsolicited) Content-Encoding now the body is fully framed.
    let body = decode_content_encoding(&headers, body);
    Ok((status, headers, body, reusable, set_cookies))
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
        let Ok(size) = usize::from_str_radix(line.split(';').next().unwrap_or("").trim(), 16)
        else {
            return Ok((out, false));
        };
        if size == 0 {
            // Trailers, if any, end at an empty line.
            loop {
                match read_line(io).await {
                    Ok(l) if l.is_empty() => return Ok((out, true)),
                    Ok(_) => {}
                    Err(_) => return Ok((out, false)),
                }
            }
        }
        if out.len() + size > MAX_BODY {
            return Err(format!(
                "response exceeds {} MB cap",
                MAX_BODY / (1024 * 1024)
            ));
        }
        let start = out.len();
        out.resize(start + size, 0);
        if !fill(io, &mut out[start..]).await {
            out.truncate(start);
            return Ok((out, false));
        }
        // The CRLF after the chunk data.
        let mut crlf = [0u8; 2];
        if !fill(io, &mut crlf).await {
            return Ok((out, false));
        }
    }
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

/// Undo `Content-Encoding` on a fully-read body. We never advertise an
/// encoding (`Accept-Encoding: identity`), but some servers compress anyway
/// (archive.org sends `Content-Encoding: gzip` unsolicited); a browser
/// decodes it regardless, so we do too. Layering: this runs AFTER framing
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

/// External classic scripts prefetched in parallel for one page. A browser has
/// no such cap; it's only a parallelism lid (politeness toward one host) and a
/// hostile-page lid. It must clear a real code-split SPA's chunk count — a
/// webpack `cr-acquisition`-style app ships ~24 `<script src>` chunks, so at the
/// old 16 the app's own bundle was truncated (the trailing chunks errored "not
/// fetched") and the page never mounted. Matches `MAX_PAGE_PRELOADS` in spirit
/// (both are "app code"). It is NOT a correctness cliff anymore: a classic
/// script the execution loop reaches that wasn't prefetched is fetched on
/// demand (see js.rs), bounded by `MAX_PAGE_FETCHES`.
const MAX_PAGE_SCRIPTS: usize = 96;

/// External stylesheets fetched for the cascade. A browser has no such cap;
/// it's only a lid on hostile pages. It must clear a real design system's
/// sheet count — GitHub links ~33 distinct sheets (Primer, theme variants,
/// per-view + per-component CSS modules), with structural sheets (the nav, the
/// repo layout) LAST. At the old 16 those were dropped after the leading color-
/// theme sheets, so menus rendered un-collapsed and grids lost their tracks.
const MAX_PAGE_SHEETS: usize = 48;

/// Module-graph prefetches (`<link rel=modulepreload>` + module entry
/// srcs); archive.org announces ~32. Matches MAX_PAGE_FETCHES in
/// spirit: enough for real apps, a lid on hostile pages.
const MAX_PAGE_PRELOADS: usize = 96;

/// Concurrent subresource fetches per page load — browser-ish
/// politeness toward one host, and the pool holds about this many
/// idle connections anyway.
const PREFETCH_CONCURRENCY: usize = 8;

/// Run an HTML page's JavaScript (js.rs) and swap the body for the
/// post-JS document. External scripts are fetched here, with the same
/// caps and timeouts as pages — the page's own JS has no I/O at all.
/// Never fails: trouble lands in `response.js` and the original body
/// survives.
pub async fn execute_js(
    mut response: Response,
    viewport: (u16, u16),
    cell_px: (u16, u16),
    storage: crate::js::WebStorage,
) -> Response {
    let media = response
        .content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if !(media.is_empty() || media == "text/html" || media == "application/xhtml+xml") {
        return response;
    }
    let html = decode_body(&response.content_type, &response.body);
    // Cheap pre-filter: no script tag, no engine spin-up — but still bake the
    // page's CSS so a script-less page lays out per its stylesheets.
    if !html.to_ascii_lowercase().contains("<script") {
        return css_only(response, viewport, cell_px).await;
    }
    // All subresources — classic scripts, stylesheets, and the module
    // graph the page announces up front (modulepreload + module entry
    // srcs) — fetch CONCURRENTLY. With the keep-alive pool this turns
    // a page load from sum-of-latencies into max-of-latencies.
    enum Kind {
        Script,
        Sheet,
        Preload,
    }
    let jobs: Vec<(Kind, String)> = crate::js::external_scripts(&html)
        .into_iter()
        .take(MAX_PAGE_SCRIPTS)
        .map(|s| (Kind::Script, s))
        .chain(
            crate::js::external_stylesheets(&html)
                .into_iter()
                .take(MAX_PAGE_SHEETS)
                .map(|s| (Kind::Sheet, s)),
        )
        .chain(
            crate::js::module_preloads(&html)
                .into_iter()
                .take(MAX_PAGE_PRELOADS)
                .map(|s| (Kind::Preload, s)),
        )
        .collect();
    if std::env::var_os("TRUST_NET_TRACE").is_some() {
        eprintln!(
            "js : @{:>6}ms prefetch start ({} subresources)",
            trace_ms(),
            jobs.len()
        );
    }
    let results = futures::stream::iter(jobs.into_iter().map(|(kind, raw)| {
        let base = response.url.clone();
        async move {
            let resolved = base
                .join(&raw)
                .ok()
                .filter(|u| matches!(u.scheme(), "http" | "https"))
                .filter(|u| subresource_allowed(&base, u));
            let resp = match &resolved {
                Some(u) => {
                    if std::env::var_os("TRUST_NET_TRACE").is_some() {
                        eprintln!("src: @{:>6}ms PREFETCH {u}", trace_ms());
                    }
                    fetch(&Request::get(u.clone())).await.ok()
                }
                None => None,
            };
            (kind, raw, resolved, resp)
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
    let mut sheets = Vec::new();
    for (kind, raw, resolved, resp) in results {
        match kind {
            Kind::Script => {
                // Seed the cache too: the module entry is a classic-script
                // <src>, and the loader/bundler reach it through the cache.
                if let (Some(u), Some(r)) = (resolved.as_ref(), resp.as_ref()) {
                    cache.seed(
                        u.to_string(),
                        r.status,
                        r.content_type.clone(),
                        r.body.clone(),
                    );
                }
                externals.push((raw, resp.map(|r| r.body)));
            }
            Kind::Sheet => {
                // A failed sheet is simply absent: fail-open, nothing
                // gets hidden.
                if let Some(r) = resp {
                    sheets.push((raw, decode_body(&r.content_type, &r.body)));
                }
            }
            Kind::Preload => {
                if let (Some(u), Some(r)) = (resolved, resp) {
                    cache.seed(u.to_string(), r.status, r.content_type, r.body);
                }
            }
        }
    }
    let env = crate::js::PageEnv {
        url: response.url.to_string(),
        viewport,
        cell_px,
        externals,
        sheets,
        cache,
        net: Some(tokio::runtime::Handle::current()),
        storage: Some(storage),
    };
    // The page actor owns the engine on its own wide-stack thread (Boa's
    // parser recursion — see CLAUDE.md). Its first event is `Static`
    // (nothing to interact with: actor already gone, free efficiency)
    // or `Updated` (alive: hand the channels to the app).
    if std::env::var_os("TRUST_NET_TRACE").is_some() {
        eprintln!("js : @{:>6}ms prefetch done; spawning page", trace_ms());
    }
    let (handle, mut events) = crate::js::spawn_page(html, env);
    let first = tokio::time::timeout(Duration::from_secs(60), events.recv()).await;
    if std::env::var_os("TRUST_NET_TRACE").is_some() {
        eprintln!(
            "js : @{:>6}ms first PageEvt received (page rendered)",
            trace_ms()
        );
    }
    let (out, outcome, live) = match first {
        Ok(Some(crate::js::PageEvt::Static { html, outcome })) => (html, outcome, None),
        Ok(Some(crate::js::PageEvt::Updated { html, outcome })) => {
            (html, outcome, Some(LivePage { handle, events }))
        }
        // Died, hung, or spoke out of turn (most often: the page's JS is too
        // slow to first-paint within the timeout — a big GitHub code file).
        // Fall back to a CSS-only render so it still lays out per its own
        // stylesheets (flex gutter, collapsed menus) instead of UA defaults.
        _ => return css_only(response, viewport, cell_px).await,
    };
    response.body = out.into_bytes();
    // The serializer emits UTF-8 regardless of the original charset;
    // re-parses (resize re-wraps) must read it as such.
    response.content_type = String::from("text/html; charset=utf-8");
    response.js = Some(outcome);
    response.live = live;
    response
}

/// Fetch a page's external stylesheets — the same set + caps `execute_js`
/// uses — concurrently, returning `(href, css)` in document order.
async fn fetch_page_sheets(html: &str, base: &Url) -> Vec<(String, String)> {
    let jobs: Vec<String> = crate::js::external_stylesheets(html)
        .into_iter()
        .take(MAX_PAGE_SHEETS)
        .collect();
    futures::stream::iter(jobs.into_iter().map(|raw| {
        let base = base.clone();
        async move {
            let resolved = base
                .join(&raw)
                .ok()
                .filter(|u| matches!(u.scheme(), "http" | "https"))
                .filter(|u| subresource_allowed(&base, u));
            let resp = match &resolved {
                Some(u) => fetch(&Request::get(u.clone())).await.ok(),
                None => None,
            };
            (raw, resp)
        }
    }))
    .buffered(PREFETCH_CONCURRENCY)
    .collect::<Vec<_>>()
    .await
    .into_iter()
    .filter_map(|(raw, resp)| resp.map(|r| (raw, decode_body(&r.content_type, &r.body))))
    .collect()
}

/// Render an HTML page with ONLY its CSS cascade applied (no JS): fetch its
/// stylesheets and bake the cascade into the serialized DOM. The path for
/// every page JS won't transform — no `<script>`, `set js off`, and the
/// `execute_js` load-timeout/early-exit fallback — so the page still lays out
/// per its own CSS instead of UA defaults (see `crate::js::css_bake`).
pub async fn css_only(
    mut response: Response,
    viewport: (u16, u16),
    cell_px: (u16, u16),
) -> Response {
    let media = response
        .content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if !(media.is_empty() || media == "text/html" || media == "application/xhtml+xml") {
        return response;
    }
    let html = decode_body(&response.content_type, &response.body);
    let sheets = fetch_page_sheets(&html, &response.url).await;
    // The frame documents are fetched up front into a url→content map (Dom is
    // not `Send`, so it must never cross an `.await`), then installed into the
    // real arena synchronously.
    let base = base_with_doc_base(&html, &response.url);
    let frames = prefetch_frame_documents(&html, &base, &response.url).await;
    let mut dom = crate::js::css_prepare(&html, viewport, cell_px);
    if !frames.is_empty() {
        install_page_frames(&mut dom, &response.url, &frames);
    }
    response.body = crate::js::css_finish(dom, &sheets).into_bytes();
    response.content_type = String::from("text/html; charset=utf-8");
    response
}

/// Bounded nesting for the no-JS frame load: a frame whose document holds
/// more frames is followed this many levels deep, after which deeper frames
/// render empty (a hostile-page lid; the circular guard already stops a
/// self-embed at any depth).
const MAX_FRAME_DEPTH: usize = 8;
/// Total frame documents loaded per page — the script-less analogue of the
/// JS pipeline's `MAX_PAGE_FETCHES` ceiling, so a frame bomb can't fan out.
const MAX_FRAME_LOADS: usize = 32;

fn strip_fragment(u: &str) -> &str {
    u.split('#').next().unwrap_or(u)
}

/// The base URL for resolving a document's `src`/`href`: the document URL,
/// overridden by the first `<base href>` if present (mirrors `baseHref()` in
/// the JS pipeline). Takes the raw HTML so it works before the real arena
/// exists (the prefetch phase needs it too).
fn base_with_doc_base(html: &str, doc_url: &Url) -> Url {
    let dom = crate::dom::Dom::parse_document(html);
    for id in dom.descendants(crate::dom::DOCUMENT) {
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
/// gating in the prefetch and install passes: http(s) only (about:/data:/blob:
/// render nothing — a documented deviation), no private-network pivot, and the
/// spec circular-navigation guard (a frame may not load a URL already held by
/// an inclusive ancestor navigable). `None` means "don't load".
fn resolve_frame_src(src: &str, base: &Url, page_url: &Url, ancestors: &[String]) -> Option<Url> {
    let url = base.join(src.trim()).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    if !subresource_allowed(page_url, &url) {
        return None;
    }
    if ancestors.iter().any(|a| a == strip_fragment(url.as_str())) {
        return None;
    }
    Some(url)
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
    for id in dom.descendants(crate::dom::DOCUMENT) {
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
/// nested `src` frames. Bounded by depth and a total-frame cap; only 2xx
/// `text/html` responses are kept.
async fn prefetch_frame_documents(
    html: &str,
    base: &Url,
    page_url: &Url,
) -> std::collections::HashMap<String, String> {
    use std::collections::VecDeque;
    let mut map: HashMap<String, String> = HashMap::new();
    // (document markup, its base, fragment-stripped ancestor URLs, depth)
    let mut queue: VecDeque<(String, Url, Vec<String>, usize)> = VecDeque::new();
    queue.push_back((
        html.to_string(),
        base.clone(),
        vec![strip_fragment(page_url.as_str()).to_string()],
        0,
    ));
    let mut loaded = 0usize;

    while let Some((markup, base, ancestors, depth)) = queue.pop_front() {
        if depth >= MAX_FRAME_DEPTH || loaded >= MAX_FRAME_LOADS {
            continue;
        }
        let (srcs, srcdocs) = scan_frame_sources(&markup, &base, page_url, &ancestors);

        // Fetch this level's `src` documents concurrently.
        let fetched: Vec<Option<(Url, String)>> =
            futures::stream::iter(srcs.into_iter().map(|url| async move {
                let resp = fetch(&Request::get(url.clone())).await.ok()?;
                let media = resp
                    .content_type
                    .split(';')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_ascii_lowercase();
                let is_html =
                    media.is_empty() || media == "text/html" || media == "application/xhtml+xml";
                (resp.status >= 200 && resp.status < 300 && is_html)
                    .then(|| (url, decode_body(&resp.content_type, &resp.body)))
            }))
            .buffered(PREFETCH_CONCURRENCY)
            .collect()
            .await;

        for (url, body) in fetched.into_iter().flatten() {
            if loaded >= MAX_FRAME_LOADS {
                break;
            }
            loaded += 1;
            let mut child_ancestors = ancestors.clone();
            child_ancestors.push(strip_fragment(url.as_str()).to_string());
            // Nested frames in the fetched content resolve against ITS url.
            queue.push_back((body.clone(), url.clone(), child_ancestors, depth + 1));
            map.insert(url.to_string(), body);
        }
        // srcdoc bodies hold no URL of their own; recurse to load THEIR frames
        // (base/origin inherit the parent document, per about:srcdoc).
        for srcdoc in srcdocs {
            queue.push_back((srcdoc, base.clone(), ancestors.clone(), depth + 1));
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
    // (subtree root, base for that subtree, fragment-stripped ancestor URLs, depth)
    let mut queue: VecDeque<(NodeId, Url, Vec<String>, usize)> = VecDeque::new();
    queue.push_back((
        DOCUMENT,
        base,
        vec![strip_fragment(page_url.as_str()).to_string()],
        0,
    ));
    let mut loaded = 0usize;

    while let Some((root, base, ancestors, depth)) = queue.pop_front() {
        if depth >= MAX_FRAME_DEPTH || loaded >= MAX_FRAME_LOADS {
            continue;
        }
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
            if loaded >= MAX_FRAME_LOADS {
                break;
            }
            if let Some(body) = dom.install_frame_document(frame, &content, frame_base.as_str()) {
                loaded += 1;
                let mut child_ancestors = ancestors.clone();
                if let Some(u) = &frame_url {
                    child_ancestors.push(strip_fragment(u.as_str()).to_string());
                }
                queue.push_back((body, frame_base, child_ancestors, depth + 1));
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
        && a.host_str() == b.host_str()
        && a.port_or_known_default() == b.port_or_known_default()
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

/// Render a response body into a document. `images` maps already-decoded
/// image URLs to their cell box; pass an empty map on the first parse
/// (the app re-lays-out once its decode pipeline fills it).
pub fn parse(
    url: &Url,
    content_type: &str,
    body: &[u8],
    width: usize,
    images: &crate::layout::ImageSizes,
) -> Doc {
    parse_seeded(url, content_type, body, width, None, images)
}

/// Like `parse`, seeding form field values from a previous parse of the
/// same page (resize re-wraps and edits must not lose what was typed).
pub fn parse_seeded(
    url: &Url,
    content_type: &str,
    body: &[u8],
    width: usize,
    seed: Option<&[Form]>,
    images: &crate::layout::ImageSizes,
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
    let mut image_urls = Vec::new();
    let lines = if media.is_empty() || media == "text/html" || media == "application/xhtml+xml" {
        let html = decode_body(content_type, body);
        // The HTTP renderer: our own arena DOM laid out into rows of
        // positioned items (multi-link rows, real CSS, live form
        // controls). Forms are extracted from the SAME arena so the
        // control map's node ids line up with the layout pass. HTML no
        // longer uses the line model — `rows` is the whole story.
        let dom = crate::dom::Dom::parse_document(&html);
        let (found, controls) = extract_forms_arena(&dom, url, seed);
        forms = found;
        image_urls = collect_image_urls(&dom, url);
        let (laid, found_carousels) = crate::layout::lay_out_with_carousels(
            &dom,
            url,
            width,
            &forms,
            &controls,
            images,
            crate::layout::borders_enabled(),
        );
        rows = laid;
        carousels = found_carousels;
        Vec::new()
    } else if media.starts_with("text/") {
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
        raw: body.to_vec(),
        wrapped_to: width,
        cp437: false,
        meta: Some(content_type.to_string()),
        forms,
        rows,
        image_urls,
        carousels,
    }
}

/// The absolute http(s) URLs of every `<img src>` in document order,
/// de-duplicated (the decode pipeline fetches each once).
fn collect_image_urls(dom: &crate::dom::Dom, base: &Url) -> Vec<String> {
    let mut urls = Vec::new();
    for id in dom.descendants(crate::dom::DOCUMENT) {
        // `<img src>` and `<video poster>` (the poster renders as the video's
        // clickable thumbnail) both feed the decode pipeline.
        let src = match dom.tag_name(id) {
            Some("img") => dom.attr(id, "src"),
            Some("video") => dom.attr(id, "poster"),
            _ => None,
        };
        let Some(src) = src.map(str::trim).filter(|s| !s.is_empty()) else {
            continue;
        };
        if let Link::Http(u) = resolve(base, src) {
            let u = u.to_string();
            if !urls.contains(&u) {
                urls.push(u);
            }
        }
    }
    urls
}

/// Resolve an href against the page, mapping schemes to our link types.
pub(crate) fn resolve(base: &Url, target: &str) -> Link {
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
            "http" | "https" => Link::Http(joined),
            "gemini" => crate::gemini::GeminiUrl::parse(joined.as_str())
                .map(Link::Gemini)
                .unwrap_or_else(|| Link::External(joined.to_string())),
            "gopher" => crate::gopher::GopherUrl::parse(joined.as_str())
                .map(Link::Gopher)
                .unwrap_or_else(|| Link::External(joined.to_string())),
            "finger" | "whois" | "dict" => crate::oneshot::OneShotUrl::parse(joined.as_str())
                .map(Link::OneShot)
                .unwrap_or_else(|| Link::External(joined.to_string())),
            _ => Link::External(joined.to_string()),
        },
        Err(_) => Link::External(target.to_string()),
    }
}

/// Extract the page's forms from our own arena DOM (the layout path's
/// source of truth), returning the forms plus a map from each rendering
/// control's `NodeId` to its `(form, field)` indices so the layout can
/// make those items selectable `Link::Form`s. A form element whose node
/// is in the map carries its synthetic submit (button-less forms).
pub(crate) fn extract_forms_arena(
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
    for child in dom.children(id) {
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
                // A form with no submit control still needs a trigger: a
                // synthetic submit, surfaced as an item on the form node.
                if !forms[form].fields.is_empty()
                    && !forms[form]
                        .fields
                        .iter()
                        .any(|f| f.kind == FieldKind::Submit)
                {
                    forms[form].fields.push(Field {
                        name: String::new(),
                        value: String::new(),
                        checked: false,
                        label: String::from("Submit"),
                        kind: FieldKind::Submit,
                        live_node: live_node(dom, child),
                    });
                    map.insert(child, (form, forms[form].fields.len() - 1));
                }
            }
            Some(tag @ ("input" | "button" | "select" | "textarea")) => {
                let Some(field) = field_from_arena(dom, child, tag) else {
                    continue;
                };
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
                    value,
                    checked: false,
                    label: contenteditable_placeholder(dom, child),
                    kind: FieldKind::Textarea,
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
    dom.attr(id, "data-trust-node")?.parse().ok()
}

fn field_from_arena(dom: &crate::dom::Dom, id: usize, tag: &str) -> Option<Field> {
    let name = dom.attr(id, "name").unwrap_or("").to_string();
    let value = dom.attr(id, "value").unwrap_or("").to_string();
    let checked = dom.attr(id, "checked").is_some();
    let mut label = String::new();
    let kind = match tag {
        "input" => {
            let ty = dom.attr(id, "type").unwrap_or("").to_ascii_lowercase();
            match ty.as_str() {
                "hidden" => FieldKind::Hidden,
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
                "button" | "reset" | "file" => return None,
                _ => {
                    label = dom.attr(id, "placeholder").unwrap_or("").to_string();
                    FieldKind::Text
                }
            }
        }
        "button" => {
            let ty = dom.attr(id, "type").unwrap_or("").to_ascii_lowercase();
            if !(ty.is_empty() || ty == "submit") {
                return None;
            }
            let text = dom.text_content(id).trim().to_string();
            label = if !text.is_empty() {
                text
            } else if !value.is_empty() {
                value.clone()
            } else {
                String::from("Submit")
            };
            FieldKind::Submit
        }
        "textarea" => {
            return Some(Field {
                name,
                value: dom.text_content(id),
                checked: false,
                label,
                kind: FieldKind::Textarea,
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
                value,
                checked: false,
                label,
                kind: FieldKind::Select(options),
                live_node: live_node(dom, id),
            });
        }
        _ => return None,
    };
    Some(Field {
        name,
        value,
        checked,
        label,
        kind,
        live_node: live_node(dom, id),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Find the first laid-out item whose text contains `needle`.
    fn item<'a>(doc: &'a Doc, needle: &str) -> &'a crate::layout::Item {
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
        let body = String::from_utf8_lossy(&response.body);
        assert!(body.contains("external ran + inline"), "{body}");
        assert!(!body.contains("js is off"), "{body}");
        assert!(!body.contains("<script"), "{body}");
        assert_eq!(response.content_type, "text/html; charset=utf-8");
        let outcome = response.js.expect("outcome recorded");
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
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
        let response = execute_js(response, (80, 24), (8, 16), Default::default()).await;
        let body = String::from_utf8_lossy(&response.body);
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
                      <iframe src=\"/inner\"></iframe></body>"
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
        assert!(!body.contains("<iframe"), "iframe element survived: {body}");
        assert!(
            body.contains(&format!("http://127.0.0.1:{port}/deep.html")),
            "relative link not resolved against the frame url: {body}"
        );
        server.abort();
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
    // js.rs unit tests prove the cache mechanics directly.)
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
                    // Complete the RFC 6455 handshake (our client doesn't verify
                    // the accept key — it requires 101 + Upgrade), then push one
                    // unmasked text frame "hi" and hold the socket open.
                    let _ = sock
                        .write_all(
                            b"HTTP/1.1 101 Switching Protocols\r\n\
                              Upgrade: websocket\r\nConnection: Upgrade\r\n\
                              Sec-WebSocket-Accept: x\r\n\r\n\x81\x02hi",
                        )
                        .await;
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
        let response = execute_js(response, (80, 24), (8, 16), Default::default()).await;
        let elapsed = started.elapsed();
        let peak = peak.load(Relaxed);
        eprintln!(
            "page_fetches_run_concurrently: {N} fetches @ {DELAY_MS}ms, peak in-flight {peak}, took {elapsed:?}"
        );
        let body = String::from_utf8_lossy(&response.body);
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
        let response = execute_js(response, (80, 24), (8, 16), Default::default()).await;
        let body = String::from_utf8_lossy(&response.body);
        assert!(body.contains("sdk-ran"), "injected script ran: {body}");
        assert!(body.contains("sdk-ran loaded"), "load event fired: {body}");
        assert!(
            response.js.map(|j| j.errors.is_empty()).unwrap_or(true),
            "no JS errors"
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
        let response = execute_js(response, (80, 24), (8, 16), Default::default()).await;
        let body = String::from_utf8_lossy(&response.body);
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
    /// ahead of Boa's serial loader) instead of one-RTT-at-a-time. Mirrors
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
        let response = execute_js(response, (80, 24), (8, 16), Default::default()).await;
        let elapsed = started.elapsed();
        let peak = peak.load(Relaxed);
        let body = String::from_utf8_lossy(&response.body);
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
        let response = execute_js(response, (80, 24), (8, 16), Default::default()).await;
        let elapsed = started.elapsed();
        let peak = peak.load(Relaxed);
        let body = String::from_utf8_lossy(&response.body);
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
        let response = execute_js(response, (80, 24), (8, 16), storage.clone()).await;
        let body = String::from_utf8_lossy(&response.body);
        assert!(body.contains("fetched trust"), "{body}");
        assert!(body.contains(">blocked<"), "{body}");
        let outcome = response.js.expect("outcome");
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert_eq!(outcome.fetches, 1); // the blocked probe never counted

        // Page 2: async XHR POST + storage written by page 1.
        let url = parse_url(&format!("http://127.0.0.1:{port}/page2")).unwrap();
        let response = fetch(&Request::get(url)).await.unwrap();
        let response = execute_js(response, (80, 24), (8, 16), storage.clone()).await;
        let body = String::from_utf8_lossy(&response.body);
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
    /// the wire completes. Before this the dispatch BLOCKED on the fetch (up to
    /// `DISPATCH_NET_GRACE` = 300s), freezing the live engine. We prove the
    /// no-block contract by delaying `/data` so a "loading" render is forced out
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
            for _ in 0..6 {
                match tokio::time::timeout(Duration::from_secs(20), live.events.recv()).await {
                    Ok(Some(crate::js::PageEvt::Updated { html, outcome })) => {
                        eprintln!(
                            "SETTLE EVT errors={:?} console={:?}",
                            outcome.errors, outcome.console
                        );
                        body = html;
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
            live.handle
                .cmds
                .send(crate::js::PageCmd::Click(marker))
                .await
                .unwrap();
            // Drain a few events so we see Updated/Navigate/Settled, not just the
            // first. Time-bounded: after the dispatch settles no more events come,
            // so don't block waiting for the actor to time out.
            for _ in 0..4 {
                let ev =
                    match tokio::time::timeout(Duration::from_secs(8), live.events.recv()).await {
                        Ok(ev) => ev,
                        Err(_) => {
                            eprintln!("EVT <no more within 8s>");
                            break;
                        }
                    };
                match ev {
                    Some(crate::js::PageEvt::Updated { html, outcome }) => {
                        eprintln!("EVT Updated: errors={:?}", outcome.errors);
                        eprintln!("   console={:?}", outcome.console);
                        eprintln!(
                            "   probe {probe:?} present AFTER = {}",
                            html.contains(&probe)
                        );
                        last_html = Some(html);
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
        let mut images = crate::layout::ImageSizes::new();
        let mut srcs: Vec<(crate::dom::NodeId, url::Url)> = Vec::new();
        for id in dom.descendants(crate::dom::DOCUMENT) {
            if dom.tag_name(id) == Some("img")
                && let Some(src) = dom.attr(id, "src")
                && !src.trim_end().ends_with(".svg")
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
                // Store the CELL box the real pipeline lays out with (px→cells
                // via the font size, capped to 80x24 preserving aspect), NOT raw
                // pixels — raw px made every image read ~hundreds of cells wide
                // and clamp to `avail`, distorting flex-basis measurement.
                let nat = ratatui_image::Resize::natural_size(&im, (8, 16).into());
                let (cw, ch) = (nat.width.max(1) as f32, nat.height.max(1) as f32);
                let scale = (80.0 / cw).min(24.0 / ch).min(1.0);
                let w = (cw * scale).round().max(1.0) as u16;
                let h = (ch * scale).round().max(1.0) as u16;
                images.insert(u.to_string(), (w, h));
            }
        }
        eprintln!("decoded {} images", images.len());
        let (rows, _car) = crate::layout::lay_out_with_carousels(
            &dom,
            &url,
            vp.0 as usize,
            &[],
            &crate::layout::ControlMap::new(),
            &images,
            false,
        );
        for (y, row) in rows.iter().enumerate() {
            for it in &row.items {
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
                        "row{y:>3} col{:>3} {:>3}x{:<3} css(w={cw},h={ch}) [{cls}] {tail}",
                        it.col, it.width, it.height
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
        let mut response = fetch(&Request::get(url)).await.unwrap();
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
            for _ in 0..6 {
                match tokio::time::timeout(Duration::from_secs(20), live.events.recv()).await {
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
        if let Ok(out) = std::env::var("TRUST_NET_DIAG_OUT") {
            std::fs::write(&out, &response.body).unwrap();
            eprintln!("full post-JS body ({}B) -> {out}", response.body.len());
        }
        drop(response.live.take());
    }

    /// Lay out a (post-JS) HTML FILE and dump the rows + carousels, to see
    /// exactly what reaches the screen. `TRUST_LAYOUT_FILE=<html> [TRUST_DIAG_VP=WxH]
    /// [TRUST_LAYOUT_GREP=substr] cargo test layout_dump -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "manual diagnostic, needs TRUST_LAYOUT_FILE=<html>"]
    async fn layout_dump() {
        let Ok(path) = std::env::var("TRUST_LAYOUT_FILE") else {
            eprintln!("set TRUST_LAYOUT_FILE to a post-JS html file");
            return;
        };
        let html = std::fs::read(&path).unwrap();
        let w: usize = std::env::var("TRUST_DIAG_VP")
            .ok()
            .and_then(|s| s.split_once('x').and_then(|(w, _)| w.parse().ok()))
            .unwrap_or(80);
        let grep = std::env::var("TRUST_LAYOUT_GREP").ok();
        let url = parse_url("https://store.steampowered.com/").unwrap();
        let images = crate::layout::ImageSizes::new();
        let doc = parse_seeded(&url, "text/html", &html, w, None, &images);
        for (ri, row) in doc.rows.iter().enumerate() {
            let mut s = String::new();
            for it in &row.items {
                let t = it.text.replace('\n', "\\n");
                let tag = match &it.kind {
                    crate::layout::ItemKind::Image => "IMG",
                    crate::layout::ItemKind::Border => "BRD",
                    _ => "txt",
                };
                s.push_str(&format!(
                    "[c{} w{} h{} {tag} n{} {:?}{}] ",
                    it.col,
                    it.width,
                    it.height,
                    it.node,
                    t,
                    if it.link.is_some() { "*" } else { "" }
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
        let response = execute_js(response, (80, 24), (8, 16), Default::default()).await;
        let body = String::from_utf8_lossy(&response.body);
        let outcome = response.js.as_ref().expect("js ran");
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert_eq!(outcome.modules_skipped, 0);
        assert!(body.contains("modules drive TRust — dynamically"), "{body}");
        server.abort();
    }

    // A DIAMOND module graph (two siblings import one shared module)
    // served by a CONCURRENT, delaying server, so the two loads of the
    // shared module genuinely overlap. Before the loader serialized its
    // fetch+parse, this raced: duplicate `Module` objects tripped Boa's
    // loaded_modules assert (module/source.rs:420) or corrupted the graph
    // into a stack overflow — archive.org crashed reliably. The shared
    // module must load and EVALUATE exactly once.
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

    /// REAL Lit 3 (lit-core.min.js from target/canary, see js.rs canary
    /// docs) driving the full pipeline: reactive properties, tagged
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
                    assert!(text.contains("Connection: keep-alive"));
                    // Chunked HTML with a link.
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\
                      Transfer-Encoding: chunked\r\n\r\n\
                      1c\r\n<h1>Arrived</h1><p><a href=\"\r\n\
                      15\r\n/next\">onward</a></p>\r\n\
                      0\r\n\r\n"
                        .to_vec()
                } else if text.starts_with("POST /submit ") {
                    assert!(text.contains("Content-Type: application/x-www-form-urlencoded"));
                    assert!(text.ends_with("k=v&x=y"), "body arrived: {text:?}");
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\r\nposted ok".to_vec()
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
            &Default::default(),
        );
        assert_eq!(
            item(&doc, "Arrived").kind,
            crate::layout::ItemKind::Heading(1)
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
        };
        let response = fetch(&request).await.unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"posted ok");

        server.abort();
    }

    #[test]
    fn cookie_jar_is_exact_host_and_request_visible() {
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

        // Domain= is deliberately ignored: no sibling or parent host gets it.
        let sib = parse_url("https://other.ckjar-test.example/").unwrap();
        assert!(cookies_for_js(&sib).is_empty(), "sibling sees nothing");
        assert!(
            cookies_for_request(&sib).is_empty(),
            "sibling sends nothing"
        );
        let parent = parse_url("https://ckjar-test.example/").unwrap();
        assert!(
            cookies_for_request(&parent).is_empty(),
            "parent sends nothing"
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

        // Max-Age=0 deletes; a JS write is never HttpOnly and is readable/sent.
        store_cookie(&resp, "pref=; Max-Age=0", false);
        assert!(
            !cookies_for_js(&page).contains("pref"),
            "deleted by Max-Age=0"
        );
        set_cookie_from_js(&page, "fromjs=1; HttpOnly");
        assert!(
            cookies_for_js(&page).contains("fromjs=1"),
            "JS-set cookie readable"
        );
        assert!(
            cookies_for_request(&page).contains("fromjs=1"),
            "JS-set cookie sent to exact host"
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

    // Same safe-across-await rationale as
    // document_cookie_reflects_captured_set_cookie.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn redirect_sends_captured_exact_host_cookie() {
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
                ("Host".into(), "evil.example".into()),
                ("Cookie".into(), "spoof=1".into()),
            ],
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
            !head.contains("evil.example"),
            "Host cannot be spoofed: {head}"
        );
        assert!(
            !head.contains("spoof=1"),
            "Cookie cannot be spoofed: {head}"
        );
    }

    #[tokio::test]
    async fn reads_chunked_bodies() {
        let raw: &[u8] =
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n";
        let (status, _, body, reusable, _) = read_response(&mut BufReader::new(raw)).await.unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, b"Wikipedia");
        assert!(reusable, "complete chunked response is reusable");

        // Extensions after the size and truncated tails are tolerated —
        // but a truncated stream is never pooled.
        let raw: &[u8] =
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4;name=v\r\nWiki\r\n5\r\npedia";
        let (_, _, body, reusable, _) = read_response(&mut BufReader::new(raw)).await.unwrap();
        assert_eq!(body, b"Wikipedia");
        assert!(
            !reusable,
            "missing terminator: keep the data, drop the conn"
        );
    }

    #[tokio::test]
    async fn reads_delimited_responses() {
        // Content-Length delimits even with pipelined junk behind it.
        let raw: &[u8] =
            b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 5\r\n\r\nhellojunk";
        let (status, headers, body, reusable, _) =
            read_response(&mut BufReader::new(raw)).await.unwrap();
        assert_eq!(status, 200);
        assert_eq!(headers["content-type"], "text/html");
        assert_eq!(body, b"hello");
        assert!(reusable);

        // Connection: close means don't pool; no delimiter means EOF.
        let raw: &[u8] = b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\nstuff";
        let (_, _, body, reusable, _) = read_response(&mut BufReader::new(raw)).await.unwrap();
        assert_eq!(body, b"stuff");
        assert!(!reusable);

        // HTTP/1.0 is never pooled.
        let raw: &[u8] = b"HTTP/1.0 200 OK\r\nContent-Length: 2\r\n\r\nok";
        let (_, _, body, reusable, _) = read_response(&mut BufReader::new(raw)).await.unwrap();
        assert_eq!(body, b"ok");
        assert!(!reusable);
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
        // A server forces gzip even though we asked for `identity` — decode
        // it like a browser would, end to end through read_response.
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
            read_response(&mut BufReader::new(&raw[..])).await.unwrap();
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
        use crate::layout::ItemKind;
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
        let doc = parse(&base, "text/html", html.as_bytes(), 60, &Default::default());

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
        // items with the right control links.
        assert!(!has_item(&doc, "cafe123"));
        let input = item(&doc, "[Type a message...]");
        assert_eq!(input.kind, crate::layout::ItemKind::Form);
        assert_eq!(input.link, Some(Link::Form { form: 0, field: 1 }));
        let button = item(&doc, "[ Send ]");
        assert_eq!(button.kind, crate::layout::ItemKind::Form);
        assert_eq!(button.link, Some(Link::Form { form: 0, field: 2 }));
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
        let widget = item(&doc, "[Type here]");
        assert_eq!(widget.kind, crate::layout::ItemKind::Form);
        assert_eq!(widget.link, Some(Link::Form { form: 0, field: 0 }));

        // `contenteditable="false"` is explicitly NOT editable.
        let doc2 = parse(
            &base,
            "text/html",
            b"<body><div contenteditable=\"false\">x</div></body>",
            60,
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
            &Default::default(),
        );
        doc.forms[0].fields[1].value = String::from("hello there");

        // A resize-style re-parse at another width keeps the value.
        let rewrapped = parse_seeded(
            &base,
            "text/html",
            CHAT_PAGE.as_bytes(),
            40,
            Some(&doc.forms),
            &Default::default(),
        );
        assert_eq!(rewrapped.forms[0].fields[1].value, "hello there");
        assert!(
            has_item(&rewrapped, "[hello there]"),
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
        let doc = parse(&base, "text/html", html.as_bytes(), 60, &Default::default());
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
        assert!(has_item(&doc, "[ Search ]"));
    }

    #[test]
    fn forms_without_submit_get_a_synthetic_one() {
        let base = Url::parse("http://example.com/").unwrap();
        let html = r#"<form action="/go"><input type="text" name="q"></form>"#;
        let doc = parse(&base, "text/html", html.as_bytes(), 60, &Default::default());
        assert_eq!(doc.forms[0].fields.last().unwrap().kind, FieldKind::Submit);
        assert!(has_item(&doc, "[ Submit ]"));
    }
}
