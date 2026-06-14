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
use std::time::{Duration, Instant};

use futures::StreamExt as _;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use url::Url;

use crate::doc::{Doc, DocLine, Field, FieldKind, Form, FormMethod, Kind, Link};
use crate::tls;

const MAX_BODY: usize = 5 * 1024 * 1024;
const FETCH_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_REDIRECTS: usize = 10;
const USER_AGENT: &str = "TRust/0.1";

/// An HTTP request as the app sees it: method plus optional body.
#[derive(Clone, Debug)]
pub struct Request {
    pub method: String,
    pub url: Url,
    /// (content-type, payload) for POST and friends.
    pub body: Option<(String, Vec<u8>)>,
}

impl Request {
    pub fn get(url: Url) -> Self {
        Self {
            method: String::from("GET"),
            url,
            body: None,
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
}

/// A page kept alive for interaction: commands in, renders out.
#[derive(Debug)]
pub struct LivePage {
    pub handle: crate::js::PageHandle,
    pub events: tokio::sync::mpsc::Receiver<crate::js::PageEvt>,
}

/// Parse an absolute http(s) URL.
pub fn parse_url(s: &str) -> Option<Url> {
    if !(s.starts_with("http://") || s.starts_with("https://")) {
        return None;
    }
    Url::parse(s).ok()
}

/// Fetch a request, following up to `MAX_REDIRECTS` redirects.
/// 301/302/303 turn into GET (dropping the body); 307/308 keep both.
/// `TRUST_NET_TRACE=1` prints one timing line per request to stderr —
/// the diagnostic for "where did the page-load time go".
pub async fn fetch(request: &Request) -> Result<Response, String> {
    if std::env::var_os("TRUST_NET_TRACE").is_none() {
        return fetch_redirecting(request).await;
    }
    let started = std::time::Instant::now();
    let result = fetch_redirecting(request).await;
    let ms = started.elapsed().as_millis();
    match &result {
        Ok(r) => eprintln!(
            "net: {ms:>5}ms {} {}B {}",
            r.status,
            r.body.len(),
            request.url
        ),
        Err(e) => eprintln!("net: {ms:>5}ms ERR {} ({e})", request.url),
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
enum Conn {
    Plain(TcpStream),
    Tls(Box<tokio_rustls::client::TlsStream<TcpStream>>),
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

// ---- Read-only RAM cookie jar -----------------------------------------
//
// We CAPTURE `Set-Cookie` from responses and expose them to page JS via
// `document.cookie` (lots of sites' logged-out code reads a session
// cookie and crashes if it's absent). We deliberately DO NOT send cookies
// back on requests — that's the read-only stance: no cross-request
// tracking, RAM-only, dies with the process. Sending them back is a
// future opt-in, not a default. Subset of RFC 6265: name=value plus
// Domain/Path/Secure/HttpOnly/Max-Age(=0 deletes); Expires/SameSite
// ignored.

#[derive(Clone)]
struct Cookie {
    name: String,
    value: String,
    domain: String, // lowercased, no leading dot
    host_only: bool,
    path: String,
    secure: bool,
    http_only: bool,
}

static COOKIE_JAR: std::sync::LazyLock<std::sync::Mutex<Vec<Cookie>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(Vec::new()));

const COOKIE_JAR_MAX: usize = 1000;

/// Store a `Set-Cookie` header value against the response URL. `from_js`
/// (a `document.cookie` write) forces off HttpOnly, as the platform does.
fn store_cookie(url: &Url, line: &str, from_js: bool) {
    let (nv, rest) = line.split_once(';').unwrap_or((line, ""));
    let Some((name, value)) = nv.split_once('=') else {
        return;
    };
    let (name, value) = (name.trim().to_string(), value.trim().to_string());
    if name.is_empty() {
        return;
    }
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    let (mut domain, mut host_only, mut path) = (host.clone(), true, String::from("/"));
    let (mut secure, mut http_only, mut max_age) = (false, false, None::<i64>);
    for attr in rest.split(';') {
        let attr = attr.trim();
        let (k, v) = attr
            .split_once('=')
            .map_or((attr.to_ascii_lowercase(), String::new()), |(k, v)| {
                (k.trim().to_ascii_lowercase(), v.trim().to_string())
            });
        match k.as_str() {
            "domain" if !v.is_empty() => {
                domain = v.trim_start_matches('.').to_ascii_lowercase();
                host_only = false;
            }
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
    // A response can't set a cookie for an unrelated domain.
    if !(host_only || host == domain || host.ends_with(&format!(".{domain}"))) {
        return;
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
        host_only,
        path,
        secure,
        http_only,
    });
    if jar.len() > COOKIE_JAR_MAX {
        jar.remove(0);
    }
}

fn cookie_domain_match(host: &str, c: &Cookie) -> bool {
    if c.host_only {
        host == c.domain
    } else {
        host == c.domain || host.ends_with(&format!(".{}", c.domain))
    }
}

/// The `document.cookie` string for a page: name=value pairs for every
/// jar cookie that domain/path/secure-matches, excluding HttpOnly (which
/// JS can never read).
pub(crate) fn cookies_for_js(page: &Url) -> String {
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

/// A `document.cookie = "..."` write from page JS. Never sent to a server
/// (read-only jar); just readable by later `document.cookie` reads.
pub(crate) fn set_cookie_from_js(page: &Url, line: &str) {
    store_cookie(page, line, true);
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
    // Read-only cookie jar: capture Set-Cookie, never send it back.
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
    let mut head = format!(
        "{} {} HTTP/1.1\r\n\
         Host: {}\r\n\
         User-Agent: {}\r\n\
         Accept: text/html, text/*;q=0.8, */*;q=0.1\r\n\
         Accept-Encoding: identity\r\n\
         Connection: keep-alive\r\n",
        request.method, path, host_header, USER_AGENT,
    );
    if let Some((content_type, payload)) = &request.body {
        head.push_str(&format!(
            "Content-Type: {}\r\nContent-Length: {}\r\n",
            content_type,
            payload.len()
        ));
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
            return Err(String::from("response exceeds 5 MB cap"));
        }
        let (body, complete) = read_exactly(io, len).await?;
        reusable &= complete;
        body
    } else {
        // No delimiter: the old read-to-EOF world. Never reusable.
        reusable = false;
        read_to_eof(io).await?
    };
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
            return Err(String::from("response exceeds 5 MB cap"));
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
            return Err(String::from("response exceeds 5 MB cap"));
        }
    }
}

/// Decode the body per the content-type charset: UTF-8 by default,
/// Latin-1 (and its windows-1252 sibling, near enough) by byte map.
fn decode_body(content_type: &str, body: &[u8]) -> String {
    let charset = content_type
        .split(';')
        .find_map(|p| p.trim().strip_prefix("charset="))
        .map(|c| c.trim_matches('"').to_ascii_lowercase());
    match charset.as_deref() {
        Some("iso-8859-1" | "latin1" | "windows-1252") => body.iter().map(|&b| b as char).collect(),
        _ => String::from_utf8_lossy(body).into_owned(),
    }
}

/// Most external scripts fetched for one page.
const MAX_PAGE_SCRIPTS: usize = 16;

/// External stylesheets fetched for the visibility cascade; pages
/// rarely carry more than a handful of `<link rel=stylesheet>`.
const MAX_PAGE_SHEETS: usize = 16;

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
    // Cheap pre-filter: no script tag, no engine spin-up at all.
    if !html.to_ascii_lowercase().contains("<script") {
        return response;
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
    let results = futures::stream::iter(jobs.into_iter().map(|(kind, raw)| {
        let base = response.url.clone();
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
            (kind, raw, resolved, resp)
        }
    }))
    // `buffered` keeps list order: scripts execute and sheets cascade
    // in document order regardless of arrival order.
    .buffered(PREFETCH_CONCURRENCY)
    .collect::<Vec<_>>()
    .await;

    let mut externals = Vec::new();
    let mut sheets = Vec::new();
    let mut preloaded = Vec::new();
    for (kind, raw, resolved, resp) in results {
        match kind {
            Kind::Script => externals.push((raw, resp.map(|r| r.body))),
            Kind::Sheet => {
                // A failed sheet is simply absent: fail-open, nothing
                // gets hidden.
                if let Some(r) = resp {
                    sheets.push((raw, decode_body(&r.content_type, &r.body)));
                }
            }
            Kind::Preload => {
                if let (Some(u), Some(r)) = (resolved, resp) {
                    preloaded.push((u.to_string(), r.body));
                }
            }
        }
    }
    let env = crate::js::PageEnv {
        url: response.url.to_string(),
        viewport,
        externals,
        sheets,
        preloaded,
        net: Some(tokio::runtime::Handle::current()),
        storage: Some(storage),
    };
    // The page actor owns the engine on its own wide-stack thread (Boa's
    // parser recursion — see CLAUDE.md). Its first event is `Static`
    // (nothing to interact with: actor already gone, free efficiency)
    // or `Updated` (alive: hand the channels to the app).
    let (handle, mut events) = crate::js::spawn_page(html, env);
    let first = tokio::time::timeout(Duration::from_secs(60), events.recv()).await;
    let (out, outcome, live) = match first {
        Ok(Some(crate::js::PageEvt::Static { html, outcome })) => (html, outcome, None),
        Ok(Some(crate::js::PageEvt::Updated { html, outcome })) => {
            (html, outcome, Some(LivePage { handle, events }))
        }
        // Died, hung, or spoke out of turn: render the page as fetched.
        _ => return response,
    };
    response.body = out.into_bytes();
    // The serializer emits UTF-8 regardless of the original charset;
    // re-parses (resize re-wraps) must read it as such.
    response.content_type = String::from("text/html; charset=utf-8");
    response.js = Some(outcome);
    response.live = live;
    response
}

/// A public page must not pivot us into fetching subresources (scripts,
/// page-initiated fetch/XHR) from private address space; same-host is
/// always fine (localhost dev included).
pub(crate) fn subresource_allowed(page: &Url, script: &Url) -> bool {
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
        rows = crate::layout::lay_out(&dom, url, width, &forms, &controls, images);
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
    }
}

/// The absolute http(s) URLs of every `<img src>` in document order,
/// de-duplicated (the decode pipeline fetches each once).
fn collect_image_urls(dom: &crate::dom::Dom, base: &Url) -> Vec<String> {
    let mut urls = Vec::new();
    for id in dom.descendants(crate::dom::DOCUMENT) {
        if dom.tag_name(id) != Some("img") {
            continue;
        }
        let Some(src) = dom.attr(id, "src").map(str::trim).filter(|s| !s.is_empty()) else {
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
    walk_forms_arena(dom, crate::dom::DOCUMENT, None, base, &mut forms, &mut map);

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
                });
                let form = forms.len() - 1;
                walk_forms_arena(dom, child, Some(form), base, forms, map);
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
                    });
                    map.insert(child, (form, forms[form].fields.len() - 1));
                }
            }
            Some(tag @ ("input" | "button" | "select" | "textarea")) => {
                let Some(form) = current else { continue };
                let Some(field) = field_from_arena(dom, child, tag) else {
                    continue;
                };
                let renders = field.kind != FieldKind::Hidden;
                forms[form].fields.push(field);
                if renders {
                    map.insert(child, (form, forms[form].fields.len() - 1));
                }
            }
            _ => walk_forms_arena(dom, child, current, base, forms, map),
        }
    }
}

/// Build a `Field` from an arena control element (mirrors `field_from`
/// but over our own DOM), or `None` for controls we drop.
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
        let response = execute_js(response, (80, 24), Default::default()).await;
        let body = String::from_utf8_lossy(&response.body);
        assert!(body.contains("external ran + inline"), "{body}");
        assert!(!body.contains("js is off"), "{body}");
        assert!(!body.contains("<script"), "{body}");
        assert_eq!(response.content_type, "text/html; charset=utf-8");
        let outcome = response.js.expect("outcome recorded");
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
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
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        const N: usize = 12;
        const DELAY_MS: u64 = 120;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                // One task per connection: the server must not serialize
                // the responses itself, or it would mask client-side
                // concurrency and the test would prove nothing.
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
                        tokio::time::sleep(std::time::Duration::from_millis(DELAY_MS)).await;
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
        let response = execute_js(response, (80, 24), Default::default()).await;
        let elapsed = started.elapsed();
        eprintln!("page_fetches_run_concurrently: {N} fetches @ {DELAY_MS}ms took {elapsed:?}");
        let body = String::from_utf8_lossy(&response.body);
        assert!(
            body.contains(&format!("got {N}")),
            "all fetches resolved: {body}"
        );
        let serial = std::time::Duration::from_millis(DELAY_MS * N as u64);
        assert!(
            elapsed < serial / 2,
            "fetches did not overlap: {elapsed:?} (serial would be ~{serial:?})"
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
        let response = execute_js(response, (80, 24), storage.clone()).await;
        let body = String::from_utf8_lossy(&response.body);
        assert!(body.contains("fetched trust"), "{body}");
        assert!(body.contains(">blocked<"), "{body}");
        let outcome = response.js.expect("outcome");
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert_eq!(outcome.fetches, 1); // the blocked probe never counted

        // Page 2: async XHR POST + storage written by page 1.
        let url = parse_url(&format!("http://127.0.0.1:{port}/page2")).unwrap();
        let response = fetch(&Request::get(url)).await.unwrap();
        let response = execute_js(response, (80, 24), storage.clone()).await;
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
        let mut response = execute_js(response, (80, 24), Default::default()).await;
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
        let mut response = execute_js(response, (80, 24), Default::default()).await;
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

    /// Diagnostic: fetch a REAL url through the full JS pipeline and
    /// report. `TRUST_NET_DIAG=https://… cargo test net_diag -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "manual diagnostic, needs TRUST_NET_DIAG=<url>"]
    async fn net_diag() {
        let Ok(target) = std::env::var("TRUST_NET_DIAG") else {
            eprintln!("set TRUST_NET_DIAG to a URL");
            return;
        };
        let url = parse_url(&target).expect("absolute http(s) url");
        let response = fetch(&Request::get(url)).await.unwrap();
        eprintln!(
            "fetched: status={} content_type={:?} body={}B",
            response.status,
            response.content_type,
            response.body.len()
        );
        let mut response = execute_js(response, (80, 24), Default::default()).await;
        eprintln!("js outcome: {:?}", response.js);
        eprintln!("live: {}", response.live.is_some());
        eprintln!(
            "body after: {}",
            String::from_utf8_lossy(&response.body[..response.body.len().min(1200)])
        );
        if let Ok(out) = std::env::var("TRUST_NET_DIAG_OUT") {
            std::fs::write(&out, &response.body).unwrap();
            eprintln!("full post-JS body ({}B) -> {out}", response.body.len());
        }
        drop(response.live.take());
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
        let response = execute_js(response, (80, 24), Default::default()).await;
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
                    } else if text.starts_with("GET /m/a.js ") {
                        ("text/javascript", "import './shared.js';")
                    } else if text.starts_with("GET /m/b.js ") {
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
        let response = execute_js(response, (80, 24), Default::default()).await;
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
        let response = execute_js(response, (80, 24), Default::default()).await;
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
        let mut response = execute_js(response, (80, 24), Default::default()).await;
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
        };
        let response = fetch(&request).await.unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"posted ok");

        server.abort();
    }

    #[test]
    fn cookie_jar_parses_matches_and_stays_read_only() {
        // Unique domain so the process-global jar doesn't collide with
        // other tests (same caveat as the TOFU pins).
        let resp = parse_url("https://shop.ckjar-test.example/p").unwrap();
        store_cookie(
            &resp,
            "sid=abc; Domain=ckjar-test.example; Path=/; Secure",
            false,
        );
        store_cookie(&resp, "secret=xyz; HttpOnly", false); // host-only, hidden from JS
        store_cookie(&resp, "pref=dark", false); // host-only, readable

        let page = parse_url("https://shop.ckjar-test.example/foo").unwrap();
        let c = cookies_for_js(&page);
        assert!(
            c.contains("sid=abc"),
            "domain+secure cookie over https: {c}"
        );
        assert!(c.contains("pref=dark"), "host cookie: {c}");
        assert!(!c.contains("secret"), "HttpOnly hidden from JS: {c}");

        // A sibling host sees the Domain cookie but not host-only ones.
        let sib = parse_url("https://other.ckjar-test.example/").unwrap();
        let cs = cookies_for_js(&sib);
        assert!(
            cs.contains("sid=abc"),
            "domain cookie reaches subdomain: {cs}"
        );
        assert!(!cs.contains("pref"), "host-only not cross-host: {cs}");

        // Secure cookies don't surface over http.
        let http = parse_url("http://shop.ckjar-test.example/").unwrap();
        assert!(
            !cookies_for_js(&http).contains("sid=abc"),
            "secure hidden on http"
        );

        // Max-Age=0 deletes; a JS write is never HttpOnly and is readable.
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
    }

    /// A page reads back the `Set-Cookie` from its own response via
    /// `document.cookie` (read-only jar). The request never sends it back.
    #[tokio::test]
    async fn document_cookie_reflects_captured_set_cookie() {
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
        let response = execute_js(response, (80, 24), Default::default()).await;
        let out = String::from_utf8_lossy(&response.body);
        assert!(out.contains("tjar=ckval123"), "cookie visible to JS: {out}");
        assert!(
            !out.contains("tj_secret"),
            "HttpOnly cookie hidden from JS: {out}"
        );
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
        let input = item(&doc, "[msg: Type a message...]");
        assert_eq!(input.kind, crate::layout::ItemKind::Form);
        assert_eq!(input.link, Some(Link::Form { form: 0, field: 1 }));
        let button = item(&doc, "[ Send ]");
        assert_eq!(button.kind, crate::layout::ItemKind::Form);
        assert_eq!(button.link, Some(Link::Form { form: 0, field: 2 }));
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
            has_item(&rewrapped, "[msg: hello there]"),
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
        assert!(has_item(&doc, "[region: Everywhere ▾]"));
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
