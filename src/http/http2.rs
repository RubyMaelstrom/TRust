//! HTTPS HTTP/2 transport, not a second browser-policy implementation.
//!
//! RFC 9113 §§3.2, 5.2, 6.8, 8, 9.1; RFC 7541 §§6.2.3, 7 (official
//! RFC Editor snapshot 2026-09-06). `h2` owns framing, HPACK, SETTINGS and
//! stream state. We own ALPN, partitioned pooling, bounded consumption,
//! cancellation and retry decisions. Cleartext HTTP and WebSocket Upgrade
//! remain HTTP/1.1. Server push is explicitly disabled (§6.5.2).

use std::collections::{HashMap, VecDeque};
use std::future::{Future, poll_fn};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex, Weak};
use std::task::{Context, Poll};
use std::time::Instant;

use bytes::Bytes;
use h2::{Reason, RecvStream, SendStream, client};
use http_wire::{HeaderMap, HeaderName, HeaderValue, Version};
use tokio::io::{AsyncRead, AsyncWrite, BufReader, ReadBuf};
use tokio::sync::Mutex as AsyncMutex;

use super::{Conn, FetchTiming, Headers, PoolKey, Request, ResponseParts};

const HEADER_BYTES: u32 = 256 * 1024;
const HEADER_FIELDS: usize = 256;
const STREAM_WINDOW: u32 = 1024 * 1024;
const CONNECTION_WINDOW: u32 = 8 * 1024 * 1024;
const UPLOAD_CHUNK: usize = 64 * 1024;
const MAX_INTERIM_RESPONSES: usize = 128;

/// Fetch #http-network-fetch records the FIRST frame-header byte, not the
/// instant HPACK finishes decoding a possibly fragmented header block. Only
/// frame boundaries are observed here; h2 remains the sole protocol parser.
#[derive(Default)]
struct ResponseClock(Mutex<HashMap<u32, VecDeque<f64>>>);

impl ResponseClock {
    fn take(&self, stream: u32) -> f64 {
        self.0
            .lock()
            .unwrap()
            .get_mut(&stream)
            .and_then(VecDeque::pop_front)
            .unwrap_or_else(crate::performance::now_ms)
    }
}

struct TimedIo<T> {
    inner: T,
    clock: Arc<ResponseClock>,
    header: [u8; 9],
    used: usize,
    remaining: usize,
    started: f64,
}

impl<T> TimedIo<T> {
    fn observe(&mut self, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            if self.remaining != 0 {
                let count = self.remaining.min(bytes.len());
                self.remaining -= count;
                bytes = &bytes[count..];
                continue;
            }
            if self.used == 0 {
                self.started = crate::performance::now_ms();
            }
            let count = (9 - self.used).min(bytes.len());
            self.header[self.used..self.used + count].copy_from_slice(&bytes[..count]);
            self.used += count;
            bytes = &bytes[count..];
            if self.used != 9 {
                continue;
            }
            self.remaining =
                u32::from_be_bytes([0, self.header[0], self.header[1], self.header[2]]) as usize;
            self.used = 0;
            if self.header[3] == 1 {
                let id = u32::from_be_bytes(self.header[5..9].try_into().unwrap()) & 0x7fffffff;
                if let Some(times) = self.clock.0.lock().unwrap().get_mut(&id)
                    && times.len() <= MAX_INTERIM_RESPONSES
                {
                    times.push_back(self.started);
                }
            }
        }
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for TimedIo<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let result = Pin::new(&mut self.inner).poll_read(cx, buf);
        if matches!(result, Poll::Ready(Ok(()))) {
            self.observe(&buf.filled()[before..]);
        }
        result
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for TimedIo<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, bytes)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[std::io::IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write_vectored(cx, bytes)
    }
}

pub(super) enum Transport {
    Http1(BufReader<Conn>),
    Http2(Lease),
}

struct Origin {
    connector: tokio_rustls::TlsConnector,
    http1_required: AtomicBool,
    // Held through dialing/handshake only. Concurrent first requests share
    // one connection rather than racing N TCP/TLS handshakes (§9.1).
    session: AsyncMutex<Option<Arc<Session>>>,
}

struct Session {
    // Serialize only stream creation, NOT response processing. Keeping the
    // same SendRequest preserves its bounded pending-open queue and readiness.
    sender: AsyncMutex<client::SendRequest<Bytes>>,
    closed: Arc<AtomicBool>,
    clock: Arc<ResponseClock>,
}

#[derive(Clone)]
pub(super) struct Lease {
    origin: Arc<Origin>,
    session: Arc<Session>,
}

impl Lease {
    pub(super) async fn require_http1(&self) {
        self.origin.http1_required.store(true, Ordering::Release);
        self.retire().await;
    }

    pub(super) async fn retire(&self) {
        let mut current = self.origin.session.lock().await;
        if current
            .as_ref()
            .is_some_and(|session| Arc::ptr_eq(session, &self.session))
        {
            // Existing streams keep their session while it drains. No new
            // request may use a GOAWAY connection (RFC 9113 §6.8).
            *current = None;
        }
    }
}

#[derive(Default)]
struct Pool {
    live: HashMap<PoolKey, Weak<Origin>>,
    recent: VecDeque<(Instant, Arc<Origin>)>,
}

impl Pool {
    fn prune(&mut self, now: Instant) {
        self.recent
            .retain(|(at, _)| now.saturating_duration_since(*at) < super::POOL_IDLE_TTL);
        while self.recent.len() > super::POOL_MAX_IDLE {
            self.recent.pop_front();
        }
        self.live.retain(|_, origin| origin.strong_count() != 0);
    }

    fn origin(&mut self, key: &PoolKey, now: Instant) -> Arc<Origin> {
        self.prune(now);
        let origin = self
            .live
            .get(key)
            .and_then(Weak::upgrade)
            .unwrap_or_else(|| {
                let origin = Arc::new(Origin {
                    connector: crate::tls::http_connector(),
                    http1_required: AtomicBool::new(false),
                    session: AsyncMutex::new(None),
                });
                self.live.insert(key.clone(), Arc::downgrade(&origin));
                origin
            });
        self.recent
            .retain(|(_, entry)| !Arc::ptr_eq(entry, &origin));
        self.recent.push_back((now, origin.clone()));
        self.prune(now);
        origin
    }
}

static POOL: LazyLock<Mutex<Pool>> = LazyLock::new(|| Mutex::new(Pool::default()));

pub(super) fn prune_idle() {
    if let Ok(mut pool) = POOL.lock() {
        pool.prune(Instant::now());
    }
}

// Also runs when the owning runtime cancels the driver during shutdown.
struct DriverClosed(Arc<AtomicBool>);
impl Drop for DriverClosed {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

async fn handshake<T>(io: T) -> Result<Arc<Session>, String>
where
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    // Windows are credits, not eager allocations. Enough in-flight data for
    // high-bandwidth paths, while unread response buffers remain bounded.
    // h2 independently bounds HPACK, frame sizes and reset bookkeeping.
    let clock = Arc::new(ResponseClock::default());
    let io = TimedIo {
        inner: io,
        clock: clock.clone(),
        header: [0; 9],
        used: 0,
        remaining: 0,
        started: 0.0,
    };
    let (sender, connection) = client::Builder::new()
        .enable_push(false)
        .max_header_list_size(HEADER_BYTES)
        .initial_window_size(STREAM_WINDOW)
        .initial_connection_window_size(CONNECTION_WINDOW)
        .max_send_buffer_size(UPLOAD_CHUNK)
        .handshake::<_, Bytes>(io)
        .await
        .map_err(|error| format!("HTTP/2 handshake: {error}"))?;
    let closed = Arc::new(AtomicBool::new(false));
    let guard = DriverClosed(closed.clone());
    tokio::spawn(async move {
        let _guard = guard;
        // Dropping the last sender and stream drives graceful GOAWAY and
        // transport shutdown in h2. No detached copy of the sender lives here.
        let _ = connection.await;
    });
    Ok(Arc::new(Session {
        sender: AsyncMutex::new(sender),
        closed,
        clock,
    }))
}

pub(super) async fn acquire(key: &PoolKey, timing: &mut FetchTiming) -> Result<Transport, String> {
    if key.scheme != "https" {
        return super::dial_with_timing(&key.scheme, &key.host, key.port, Some(timing))
            .await
            .map(Transport::Http1);
    }
    let origin = POOL
        .lock()
        .map_err(|_| "HTTP/2 pool unavailable")?
        .origin(key, Instant::now());
    let mut current = origin.session.lock().await;
    if origin.http1_required.load(Ordering::Acquire) {
        // A selected/required HTTP/1.1 transport cannot multiplex. Release
        // the single-flight H2 gate BEFORE dialing: H1 connections must be
        // able to establish concurrently, including slow TLS handshakes.
        drop(current);
        let mut config = (**origin.connector.config()).clone();
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        return super::dial_with_connector(
            &key.scheme,
            &key.host,
            key.port,
            &tokio_rustls::TlsConnector::from(Arc::new(config)),
            Some(timing),
        )
        .await
        .map(Transport::Http1);
    }
    if let Some(session) = current
        .as_ref()
        .filter(|session| !session.closed.load(Ordering::Acquire))
    {
        timing.reused_connection(true);
        timing.next_hop_protocol = "h2";
        return Ok(Transport::Http2(Lease {
            origin: origin.clone(),
            session: session.clone(),
        }));
    }
    let io = super::dial_with_connector(
        &key.scheme,
        &key.host,
        key.port,
        &origin.connector,
        Some(timing),
    )
    .await?;
    if timing.next_hop_protocol != "h2" {
        // ALPN selection is definitive. No ALPN or http/1.1 uses the existing
        // parser; never send an HTTP/1.1 request over a negotiated h2 socket.
        origin.http1_required.store(true, Ordering::Release);
        return Ok(Transport::Http1(io));
    }
    let session = handshake(io).await?;
    *current = Some(session.clone());
    Ok(Transport::Http2(Lease {
        origin: origin.clone(),
        session,
    }))
}

#[derive(Debug)]
pub(super) struct Error {
    message: String,
    retry: bool,
    retire: bool,
    http1_required: bool,
    local_protocol: bool,
}

impl Error {
    fn wire(error: h2::Error, unsent: bool) -> Self {
        // RFC 9113 §8.7. In h2, a received GOAWAY is delivered to streams
        // ABOVE last-stream-id; lower IDs remain live or get a later I/O
        // failure. A remote REFUSED_STREAM also guarantees no processing.
        // Never retry an ambiguous POST on EOF, timeout or other reset.
        let retry = unsent
            || (error.is_remote()
                && (error.is_go_away() || error.reason() == Some(Reason::REFUSED_STREAM)));
        Self {
            message: format!("HTTP/2: {error}"),
            retry,
            retire: unsent || error.is_go_away() || error.is_io(),
            http1_required: error.is_remote() && error.reason() == Some(Reason::HTTP_1_1_REQUIRED),
            local_protocol: false,
        }
    }
    pub(super) fn retry_safe(&self) -> bool {
        self.retry
    }
    pub(super) fn retire_connection(&self) -> bool {
        self.retire
    }
    pub(super) fn requires_http1(&self) -> bool {
        self.http1_required
    }
    fn limit(message: &str) -> Self {
        Self {
            message: message.into(),
            retry: false,
            retire: false,
            http1_required: false,
            local_protocol: false,
        }
    }
}

impl From<String> for Error {
    fn from(message: String) -> Self {
        Self {
            message,
            retry: false,
            retire: false,
            http1_required: false,
            local_protocol: true,
        }
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.message.fmt(f)
    }
}

fn wire_request(
    request: &Request,
    origin: Option<&str>,
    download: bool,
) -> Result<http_wire::Request<()>, Error> {
    // Strip fragment and userinfo without decoding/re-encoding the path.
    // h2 derives :method/:scheme/:authority/:path from this HTTP/2 request.
    let url = &request.url;
    let uri = format!(
        "{}://{}{}",
        url.scheme(),
        &url[url::Position::BeforeHost..url::Position::AfterPort],
        &url[url::Position::BeforePath..url::Position::AfterQuery]
    );
    let mut wire = http_wire::Request::builder()
        .method(request.method.as_str())
        .uri(uri)
        .version(Version::HTTP_2)
        .body(())
        .map_err(|error| Error::from(format!("invalid HTTP/2 request: {error}")))?;
    for (name, value) in super::request_headers(request, origin) {
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| Error::from("invalid HTTP request header name".to_string()))?;
        let value = if download && name == "accept-encoding" {
            "identity"
        } else {
            value.trim_matches([' ', '\t'])
        };
        let mut value = HeaderValue::from_str(value)
            .map_err(|_| Error::from("invalid HTTP request header value".to_string()))?;
        // RFC 7541 §7.1.3: sensitive values never enter the HPACK table.
        value.set_sensitive(matches!(
            name.as_str(),
            "cookie" | "authorization" | "proxy-authorization"
        ));
        wire.headers_mut().append(name, value);
    }
    Ok(wire)
}

/// Dropping a fetch future (navigation, AbortController, timeout) must reset
/// only its stream. Fetch #http-network-fetch; RFC 9113 §§5.4.2, 6.4.
struct ResetOnDrop {
    send: SendStream<Bytes>,
    armed: bool,
    clock: Arc<ResponseClock>,
    id: u32,
}
impl ResetOnDrop {
    fn fail(&mut self, error: &Error) {
        if error.local_protocol {
            // RFC 9113 §8.1.1: a malformed message is a PROTOCOL_ERROR,
            // distinct from cancellation or a local resource limit.
            self.send.send_reset(Reason::PROTOCOL_ERROR);
            self.armed = false;
        }
    }
}
impl Drop for ResetOnDrop {
    fn drop(&mut self) {
        self.clock.0.lock().unwrap().remove(&self.id);
        if self.armed {
            self.send.send_reset(Reason::CANCEL);
        }
    }
}

async fn start(
    lease: &Lease,
    wire: http_wire::Request<()>,
    empty: bool,
    timing: &mut FetchTiming,
) -> Result<(client::ResponseFuture, ResetOnDrop), Error> {
    let mut sender = lease.session.sender.lock().await;
    poll_fn(|cx| sender.poll_ready(cx))
        .await
        .map_err(|e| Error::wire(e, true))?;
    timing.request_start = crate::performance::now_ms();
    // Register atomically with queuing HEADERS, before a different runtime
    // thread can observe a response. The I/O observer never locks h2 state.
    let mut clocks = lease.session.clock.0.lock().unwrap();
    let (response, send) = sender
        .send_request(wire, empty)
        .map_err(|e| Error::wire(e, true))?;
    let id = response.stream_id().as_u32();
    clocks.insert(id, VecDeque::new());
    Ok((
        response,
        ResetOnDrop {
            send,
            armed: true,
            clock: lease.session.clock.clone(),
            id,
        },
    ))
}

fn response_headers(fields: &HeaderMap) -> Result<(Headers, Vec<String>), Error> {
    if fields.len() > HEADER_FIELDS {
        return Err(Error::limit("too many HTTP/2 response headers"));
    }
    let mut headers = Headers::new();
    let mut cookies = Vec::new();
    let mut bytes = 0usize;
    for (name, value) in fields {
        let name = name.as_str();
        let raw = value.as_bytes();
        // h2 checks pseudo-headers and frame ordering; enforce field-value
        // constraints and connection-specific fields at the HTTP boundary.
        if matches!(
            name,
            "connection"
                | "proxy-connection"
                | "keep-alive"
                | "transfer-encoding"
                | "upgrade"
                | "te"
        ) || raw.first().is_some_and(|b| matches!(b, b' ' | b'\t'))
            || raw.last().is_some_and(|b| matches!(b, b' ' | b'\t'))
            || raw.iter().any(|b| (*b < 0x20 && *b != b'\t') || *b == 0x7f)
        {
            return Err("malformed HTTP/2 response header".to_string().into());
        }
        bytes = bytes.saturating_add(name.len() + raw.len() + 32);
        if bytes > HEADER_BYTES as usize {
            return Err(Error::limit("HTTP/2 response headers exceed size limit"));
        }
        let value = String::from_utf8_lossy(raw);
        if name == "set-cookie" {
            cookies.push(value.to_string());
        }
        headers
            .entry(name.to_string())
            .and_modify(|existing: &mut String| {
                existing.push_str(", ");
                existing.push_str(&value);
            })
            .or_insert_with(|| value.into_owned());
    }
    Ok((headers, cookies))
}

async fn response_head(
    mut response: client::ResponseFuture,
    timing: &mut FetchTiming,
    clock: &ResponseClock,
) -> Result<(u16, Headers, Vec<String>, RecvStream), Error> {
    // Drain interim responses before the final response, including when both
    // arrived in the same read. They never supply cookies or final metadata.
    let id = response.stream_id().as_u32();
    let mut interim_count = 0;
    let response = poll_fn(|cx| {
        loop {
            match response.poll_informational(cx) {
                Poll::Ready(Some(Ok(interim))) => {
                    interim_count += 1;
                    if interim_count > MAX_INTERIM_RESPONSES {
                        return Poll::Ready(Err(Error::limit(
                            "too many HTTP/2 informational responses",
                        )));
                    }
                    let started = clock.take(id);
                    response_headers(interim.headers())?;
                    if interim.status().as_u16() == 101 {
                        return Poll::Ready(Err("HTTP/2 cannot switch protocols"
                            .to_string()
                            .into()));
                    }
                    if timing.first_interim_response_start == 0.0 {
                        timing.first_interim_response_start = started;
                    }
                }
                Poll::Ready(Some(Err(error))) => {
                    return Poll::Ready(Err(Error::wire(error, false)));
                }
                Poll::Ready(None) => break,
                Poll::Pending => return Poll::Pending,
            }
        }
        Pin::new(&mut response)
            .poll(cx)
            .map_err(|e| Error::wire(e, false))
    })
    .await?;
    timing.final_response_start = clock.take(id);
    let status = response.status().as_u16();
    let (headers, cookies) = response_headers(response.headers())?;
    Ok((status, headers, cookies, response.into_body()))
}

async fn upload(send: &mut SendStream<Bytes>, payload: &[u8]) -> Result<(), h2::Error> {
    let mut position = 0;
    while position < payload.len() {
        send.reserve_capacity((payload.len() - position).min(UPLOAD_CHUNK));
        let available = poll_fn(|cx| send.poll_capacity(cx))
            .await
            .ok_or_else(|| h2::Error::from(Reason::CANCEL))??;
        let count = available.min(UPLOAD_CHUNK).min(payload.len() - position);
        if count == 0 {
            continue;
        }
        let bytes = Bytes::copy_from_slice(&payload[position..position + count]);
        position += count;
        send.send_data(bytes, position == payload.len())?;
    }
    Ok(())
}

struct Incoming {
    recv: RecvStream,
    length: Option<usize>,
    received: usize,
    limit: usize,
    no_content: bool,
}

impl Incoming {
    fn new(
        recv: RecvStream,
        headers: &Headers,
        no_content: bool,
        limit: usize,
    ) -> Result<Self, Error> {
        let length = headers
            .get("content-length")
            .map(|v| super::parse_content_length(v))
            .transpose()?
            .flatten();
        if !no_content && length.is_some_and(|length| length > limit) {
            return Err(Error::limit("HTTP/2 response exceeds body size limit"));
        }
        Ok(Self {
            recv,
            length,
            received: 0,
            limit,
            no_content,
        })
    }

    async fn next(&mut self) -> Result<Option<Bytes>, Error> {
        if let Some(data) = self.recv.data().await {
            let data = data.map_err(|e| Error::wire(e, false))?;
            self.received = self.received.saturating_add(data.len());
            if self.received > self.limit {
                return Err(Error::limit("HTTP/2 response exceeds body size limit"));
            }
            if self.no_content && !data.is_empty() {
                return Err("HTTP/2 body is forbidden for this response"
                    .to_string()
                    .into());
            }
            return Ok(Some(data));
        }
        if let Some(trailers) = self
            .recv
            .trailers()
            .await
            .map_err(|e| Error::wire(e, false))?
        {
            // Validate but do not merge trailers into headers or cookie state.
            response_headers(&trailers)?;
        }
        if !self.no_content && self.length.is_some_and(|length| length != self.received) {
            return Err("HTTP/2 Content-Length does not match response body"
                .to_string()
                .into());
        }
        Ok(None)
    }

    fn consumed(&mut self, count: usize) -> Result<(), Error> {
        self.recv
            .flow_control()
            .release_capacity(count)
            .map_err(|e| Error::wire(e, false))
    }
}

async fn receive(
    response: client::ResponseFuture,
    request: &Request,
    timing: &mut FetchTiming,
    clock: &ResponseClock,
) -> Result<(ResponseParts, bool), Error> {
    let (status, headers, cookies, recv) = response_head(response, timing, clock).await?;
    let no_content =
        request.method.eq_ignore_ascii_case("HEAD") || matches!(status, 204 | 205 | 304);
    let navigation_get = request.method.eq_ignore_ascii_case("GET")
        && request
            .fetch_metadata
            .is_some_and(|m| m.destination == "document");
    let skip = !no_content
        && ((navigation_get && super::navigation_headers_need_download(&headers))
            || headers.get("content-type").is_some_and(|c| {
                let c = c.trim_start().to_ascii_lowercase();
                c.starts_with("video/") || c.starts_with("audio/")
            }));
    if skip {
        timing.response_end = crate::performance::now_ms();
        return Ok(((status, headers, Vec::new(), cookies), false));
    }
    let mut incoming = Incoming::new(recv, &headers, no_content, super::MAX_BODY)?;
    let mut body = Vec::new();
    while let Some(chunk) = incoming.next().await? {
        body.extend_from_slice(&chunk);
        incoming.consumed(chunk.len())?;
    }
    timing.response_end = crate::performance::now_ms();
    timing.encoded_body_size = body.len();
    let body = super::decode_content_encoding(&headers, body);
    timing.decoded_body_size = body.len();
    Ok(((status, headers, body, cookies), true))
}

pub(super) async fn exchange(
    lease: &Lease,
    request: &Request,
    origin: Option<&str>,
    timing: &mut FetchTiming,
) -> Result<ResponseParts, Error> {
    let payload = request
        .body
        .as_ref()
        .map_or(&[][..], |(_, bytes)| bytes.as_slice());
    let wire = wire_request(request, origin, false)?;
    let (response, mut reset) = start(lease, wire, payload.is_empty(), timing).await?;
    let receive = receive(response, request, timing, &lease.session.clock);
    tokio::pin!(receive);
    let result: Result<_, Error> = async {
        let result = if payload.is_empty() {
            (receive.await?, true)
        } else {
            // HTTP is full duplex. An early final response (e.g. 413) must not
            // deadlock behind an upload for which the server stopped giving credit.
            tokio::select! {
                biased;
                result = &mut receive => (result?, false),
                result = upload(&mut reset.send, payload) => {
                    match result {
                        Ok(()) => (receive.await?, true),
                        Err(error) if error.reason() == Some(Reason::NO_ERROR) => (receive.await?, false),
                        Err(error) => return Err(Error::wire(error, false)),
                    }
                }
            }
        };
        Ok(result)
    }.await;
    let ((parts, complete), uploaded) = result.inspect_err(|error| reset.fail(error))?;
    if complete {
        // Harmless if the upload already ended; closes a remaining send half
        // after an early complete response without discarding that response.
        if !uploaded {
            reset.send.send_reset(Reason::NO_ERROR);
        }
        reset.armed = false;
    }
    Ok(parts)
}

/// Streaming downloads hold flow-control credits until bytes reach disk.
/// Their 2 GiB file limit is independent of the buffered page-body limit.
pub(crate) struct DownloadResponse {
    pub(crate) status: u16,
    pub(crate) headers: Headers,
    incoming: Incoming,
    reset: ResetOnDrop,
    _lease: Lease,
}

impl DownloadResponse {
    pub(crate) async fn write_to(&mut self, file: &mut tokio::fs::File) -> Result<u64, String> {
        use tokio::io::AsyncWriteExt;
        while let Some(chunk) = self.incoming.next().await.map_err(|error| {
            self.reset.fail(&error);
            error.to_string()
        })? {
            file.write_all(&chunk).await.map_err(|e| e.to_string())?;
            self.incoming
                .consumed(chunk.len())
                .map_err(|e| e.to_string())?;
        }
        self.reset.armed = false;
        Ok(self.incoming.received as u64)
    }
}

pub(super) async fn download(
    lease: &Lease,
    request: &Request,
    limit: usize,
    timing: &mut FetchTiming,
) -> Result<DownloadResponse, Error> {
    let wire = wire_request(request, None, true)?;
    let (response, mut reset) = start(lease, wire, true, timing).await?;
    let (status, headers, cookies, recv) = response_head(response, timing, &lease.session.clock)
        .await
        .inspect_err(|error| reset.fail(error))?;
    for cookie in cookies {
        super::response_cookie(request, &cookie);
    }
    let no_content = matches!(status, 204 | 205 | 304);
    let incoming =
        Incoming::new(recv, &headers, no_content, limit).inspect_err(|error| reset.fail(error))?;
    Ok(DownloadResponse {
        status,
        headers,
        incoming,
        reset,
        _lease: lease.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio_rustls::{TlsAcceptor, TlsConnector, rustls};

    struct Site {
        request: Request,
        listener: TcpListener,
        acceptor: TlsAcceptor,
        connector: TlsConnector,
    }

    impl Site {
        async fn new(protocols: &[&[u8]]) -> Self {
            crate::tls::ensure_provider();
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let signed = rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()]).unwrap();
            let mut roots = rustls::RootCertStore::empty();
            roots.add(signed.cert.der().clone()).unwrap();
            let key =
                rustls::pki_types::PrivateKeyDer::try_from(signed.signing_key.serialize_der())
                    .unwrap();
            let mut server = rustls::ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(vec![signed.cert.der().clone()], key)
                .unwrap();
            server.alpn_protocols = protocols.iter().map(|p| p.to_vec()).collect();
            let mut client = rustls::ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth();
            client.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
            let request = Request::get(
                url::Url::parse(&format!("https://{}/", listener.local_addr().unwrap())).unwrap(),
            );
            let site = Self {
                request,
                listener,
                acceptor: TlsAcceptor::from(Arc::new(server)),
                connector: TlsConnector::from(Arc::new(client)),
            };
            site.install(&site.request);
            site
        }

        fn install(&self, request: &Request) {
            let origin = Arc::new(Origin {
                connector: self.connector.clone(),
                http1_required: AtomicBool::new(false),
                session: AsyncMutex::new(None),
            });
            let mut pool = POOL.lock().unwrap();
            pool.live.insert(
                PoolKey::for_request(request).unwrap(),
                Arc::downgrade(&origin),
            );
            pool.recent.push_back((Instant::now(), origin));
        }

        fn serve<F, Fut>(self, handler: F) -> Server
        where
            F: Fn(http_wire::Request<RecvStream>, h2::server::SendResponse<Bytes>) -> Fut
                + Send
                + Sync
                + 'static,
            Fut: Future<Output = ()> + Send + 'static,
        {
            let count = Arc::new(AtomicUsize::new(0));
            let connections = count.clone();
            let handler = Arc::new(handler);
            let task = tokio::spawn(async move {
                let mut children = tokio::task::JoinSet::new();
                loop {
                    let (tcp, _) = self.listener.accept().await.unwrap();
                    count.fetch_add(1, Ordering::SeqCst);
                    let acceptor = self.acceptor.clone();
                    let handler = handler.clone();
                    children.spawn(async move {
                        let tls = acceptor.accept(tcp).await.unwrap();
                        assert_eq!(tls.get_ref().1.alpn_protocol(), Some(&b"h2"[..]));
                        let mut connection = h2::server::handshake(tls).await.unwrap();
                        let mut requests = tokio::task::JoinSet::new();
                        while let Some(Ok((request, respond))) = connection.accept().await {
                            requests.spawn(handler(request, respond));
                        }
                    });
                }
            });
            Server { task, connections }
        }
    }

    struct Server {
        task: tokio::task::JoinHandle<()>,
        connections: Arc<AtomicUsize>,
    }
    impl Drop for Server {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn fetch(request: &Request) -> Result<super::super::Response, String> {
        tokio::time::timeout(Duration::from_secs(10), super::super::fetch(request))
            .await
            .expect("request must not hang")
    }

    fn ok(respond: &mut h2::server::SendResponse<Bytes>, bytes: &'static [u8]) {
        let response = http_wire::Response::builder()
            .header("content-type", "text/plain")
            .header("content-length", bytes.len())
            .body(())
            .unwrap();
        respond
            .send_response(response, bytes.is_empty())
            .unwrap()
            .send_data(Bytes::from_static(bytes), true)
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn http2_negotiates_and_multiplexes_concurrent_first_requests() {
        let site = Site::new(&[b"h2", b"http/1.1"]).await;
        let request = site.request.clone();
        let barrier = Arc::new(tokio::sync::Barrier::new(12));
        let server = site.serve(move |request, mut respond| {
            let barrier = barrier.clone();
            async move {
                assert_eq!(request.version(), Version::HTTP_2);
                assert!(request.uri().authority().is_some());
                assert!(!request.headers().contains_key("connection"));
                assert!(!request.headers().contains_key("host"));
                assert_eq!(request.headers()["user-agent"], super::super::USER_AGENT);
                barrier.wait().await;
                ok(&mut respond, b"multiplexed");
            }
        });
        let results = futures::future::join_all((0..12).map(|_| fetch(&request))).await;
        for response in results {
            let response = response.unwrap();
            assert_eq!(response.body, b"multiplexed");
            assert_eq!(response.timing.unwrap().next_hop_protocol, "h2");
        }
        assert_eq!(server.connections.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn http2_flow_control_upload_download_and_reuse() {
        let site = Site::new(&[b"h2"]).await;
        let mut request = site.request.clone();
        request.method = "POST".into();
        let payload = vec![0x5a; STREAM_WINDOW as usize * 3 + 37];
        request.body = Some(("application/octet-stream".into(), payload.clone()));
        let server = site.serve(move |request, mut respond| async move {
            let mut body = request.into_body();
            let mut received = Vec::new();
            while let Some(chunk) = body.data().await {
                let chunk = chunk.unwrap();
                received.extend_from_slice(&chunk);
                body.flow_control().release_capacity(chunk.len()).unwrap();
            }
            let head = http_wire::Response::builder()
                .header("content-type", "application/octet-stream")
                .header("content-length", received.len())
                .body(())
                .unwrap();
            let mut send = respond.send_response(head, false).unwrap();
            upload(&mut send, &received).await.unwrap();
        });
        for reused in [false, true] {
            let response = fetch(&request).await.unwrap();
            assert_eq!(response.body, payload);
            let timing = response.timing.unwrap();
            assert_eq!(timing.connection_reused, reused);
            assert_eq!(timing.next_hop_protocol, "h2");
            assert_eq!(timing.encoded_body_size, payload.len());
        }
        assert_eq!(server.connections.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn http2_early_final_response_does_not_wait_for_blocked_upload() {
        let site = Site::new(&[b"h2"]).await;
        let mut request = site.request.clone();
        request.method = "POST".into();
        request.body = Some((String::new(), vec![1; STREAM_WINDOW as usize * 4]));
        let _server = site.serve(|request, mut respond| async move {
            let head = http_wire::Response::builder()
                .status(413)
                .header("content-length", "0")
                .body(())
                .unwrap();
            let _send = respond.send_response(head, true).unwrap();
            // Retain the unread upload without granting any more credit.
            tokio::time::sleep(Duration::from_secs(1)).await;
            drop(request);
        });
        let response = tokio::time::timeout(Duration::from_millis(800), fetch(&request))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(response.status, 413);
        assert!(response.body.is_empty());
    }

    #[tokio::test]
    async fn http2_cancellation_resets_only_its_stream() {
        let site = Site::new(&[b"h2"]).await;
        let mut request = site.request.clone();
        request.url.set_path("/cancel");
        let (seen_tx, mut seen_rx) = tokio::sync::mpsc::unbounded_channel();
        let (reset_tx, mut reset_rx) = tokio::sync::mpsc::unbounded_channel();
        let server = site.serve(move |request, mut respond| {
            let seen = seen_tx.clone();
            let reset = reset_tx.clone();
            async move {
                if request.uri().path() == "/cancel" {
                    let mut send = respond
                        .send_response(http_wire::Response::new(()), false)
                        .unwrap();
                    seen.send(()).unwrap();
                    let reason = poll_fn(|cx| send.poll_reset(cx)).await.unwrap();
                    reset.send(reason).unwrap();
                } else {
                    ok(&mut respond, b"still alive");
                }
            }
        });
        let pending = request.clone();
        let task = tokio::spawn(async move { fetch(&pending).await });
        seen_rx.recv().await.unwrap();
        task.abort();
        let _ = task.await;
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), reset_rx.recv())
                .await
                .unwrap(),
            Some(Reason::CANCEL)
        );
        request.url.set_path("/healthy");
        assert_eq!(fetch(&request).await.unwrap().body, b"still alive");
        assert_eq!(server.connections.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn http2_refused_post_is_retried_but_other_resets_are_not() {
        for reason in [
            Reason::REFUSED_STREAM,
            Reason::CANCEL,
            Reason::INTERNAL_ERROR,
        ] {
            let site = Site::new(&[b"h2"]).await;
            let mut request = site.request.clone();
            request.method = "POST".into();
            request.body = Some((String::new(), b"submit once".to_vec()));
            let count = Arc::new(AtomicUsize::new(0));
            let attempts = count.clone();
            let _server = site.serve(move |_request, mut respond| {
                let attempt = attempts.fetch_add(1, Ordering::SeqCst);
                async move {
                    if attempt == 0 {
                        respond.send_reset(reason);
                    } else {
                        ok(&mut respond, b"accepted");
                    }
                }
            });
            let result = fetch(&request).await;
            if reason == Reason::REFUSED_STREAM {
                assert_eq!(result.unwrap().body, b"accepted");
                assert_eq!(count.load(Ordering::SeqCst), 2);
            } else {
                assert!(result.is_err());
                assert_eq!(count.load(Ordering::SeqCst), 1);
            }
        }
    }

    #[tokio::test]
    async fn http2_content_length_and_bodyless_responses() {
        let site = Site::new(&[b"h2"]).await;
        let mut request = site.request.clone();
        let _server = site.serve(|request, mut respond| async move {
            match request.uri().path() {
                "/bad" => {
                    let response = http_wire::Response::builder()
                        .header("content-length", "100")
                        .body(())
                        .unwrap();
                    let mut body = respond.send_response(response, false).unwrap();
                    body.send_data(Bytes::from_static(b"short"), true).unwrap();
                }
                "/head" => {
                    let response = http_wire::Response::builder()
                        .header("content-length", "123456789")
                        .body(())
                        .unwrap();
                    respond.send_response(response, true).unwrap();
                }
                "/304" => {
                    let response = http_wire::Response::builder()
                        .status(304)
                        .header("content-length", "99")
                        .body(())
                        .unwrap();
                    respond.send_response(response, true).unwrap();
                }
                _ => {
                    ok(&mut respond, b"valid");
                }
            }
        });
        request.url.set_path("/bad");
        assert!(fetch(&request).await.is_err());
        request.url.set_path("/head");
        request.method = "HEAD".into();
        assert!(fetch(&request).await.unwrap().body.is_empty());
        request.url.set_path("/304");
        request.method = "GET".into();
        assert_eq!(fetch(&request).await.unwrap().status, 304);
        request.url.set_path("/good");
        assert_eq!(fetch(&request).await.unwrap().body, b"valid");
    }

    #[tokio::test]
    async fn http2_download_streams_to_disk_on_shared_connection() {
        let site = Site::new(&[b"h2"]).await;
        let request = site.request.clone();
        let content = Arc::new(vec![0x6b; STREAM_WINDOW as usize * 2 + 19]);
        let expected = content.clone();
        let server = site.serve(move |request, mut respond| {
            let content = content.clone();
            async move {
                if request.uri().path() == "/" {
                    ok(&mut respond, b"page");
                    return;
                }
                assert_eq!(request.headers()["accept-encoding"], "identity");
                let response = http_wire::Response::builder()
                    .header("content-type", "application/octet-stream")
                    .header("content-length", content.len())
                    .body(())
                    .unwrap();
                let mut body = respond.send_response(response, false).unwrap();
                upload(&mut body, &content).await.unwrap();
            }
        });
        assert_eq!(fetch(&request).await.unwrap().body, b"page");
        let mut offer = crate::download::DownloadOffer::from_bytes(
            request.url.join("file").unwrap(),
            "file.bin".into(),
            vec![],
        );
        offer.fetch_body = true;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("download.bin");
        let length = tokio::time::timeout(
            Duration::from_secs(10),
            crate::download::save(&offer, &path),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(length, expected.len() as u64);
        assert_eq!(tokio::fs::read(&path).await.unwrap(), *expected);
        assert_eq!(fetch(&request).await.unwrap().body, b"page");
        assert_eq!(server.connections.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn http2_negotiation_falls_back_to_http1_with_or_without_alpn() {
        for protocols in [vec![&b"http/1.1"[..]], vec![]] {
            let site = Site::new(&protocols).await;
            let request = site.request.clone();
            let server = tokio::spawn(async move {
                let (tcp, _) = site.listener.accept().await.unwrap();
                let mut tls = site.acceptor.accept(tcp).await.unwrap();
                assert_ne!(tls.get_ref().1.alpn_protocol(), Some(&b"h2"[..]));
                let mut head = Vec::new();
                let mut byte = [0];
                while !head.ends_with(b"\r\n\r\n") {
                    tls.read_exact(&mut byte).await.unwrap();
                    head.push(byte[0]);
                }
                assert!(head.starts_with(b"GET / HTTP/1.1\r\n"));
                tls.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\nfallback",
                )
                .await
                .unwrap();
                tls.shutdown().await.unwrap();
            });
            let response = fetch(&request).await.unwrap();
            assert_eq!(response.body, b"fallback");
            assert_eq!(response.timing.unwrap().next_hop_protocol, "http/1.1");
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn http2_http1_fallback_does_not_serialize_subsequent_tls_handshakes() {
        let site = Site::new(&[b"http/1.1"]).await;
        let request = site.request.clone();
        let server = tokio::spawn(async move {
            async fn reply(mut tls: tokio_rustls::server::TlsStream<tokio::net::TcpStream>) {
                let mut head = Vec::new();
                let mut byte = [0];
                while !head.ends_with(b"\r\n\r\n") {
                    tls.read_exact(&mut byte).await.unwrap();
                    head.push(byte[0]);
                }
                tls.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                )
                .await
                .unwrap();
                tls.shutdown().await.unwrap();
            }
            let (tcp, _) = site.listener.accept().await.unwrap();
            reply(site.acceptor.accept(tcp).await.unwrap()).await;
            let barrier = Arc::new(tokio::sync::Barrier::new(4));
            let mut peers = tokio::task::JoinSet::new();
            for _ in 0..4 {
                let (tcp, _) = site.listener.accept().await.unwrap();
                let barrier = barrier.clone();
                let acceptor = site.acceptor.clone();
                peers.spawn(async move {
                    barrier.wait().await; // all TCP dials precede ANY TLS completion
                    reply(acceptor.accept(tcp).await.unwrap()).await;
                });
            }
            while let Some(peer) = peers.join_next().await {
                peer.unwrap();
            }
        });
        assert_eq!(fetch(&request).await.unwrap().body, b"ok");
        for result in futures::future::join_all((0..4).map(|_| fetch(&request))).await {
            assert_eq!(result.unwrap().body, b"ok");
        }
        server.await.unwrap();
    }

    #[test]
    fn http2_headers_authority_sensitive_fields_and_validation() {
        let mut request = Request::get(
            url::Url::parse("https://user:password@[::1]:8443/a%2Fb?q=x%20y#private").unwrap(),
        );
        request.method = "POST".into();
        request.headers = vec![
            ("Authorization".into(), "Bearer secret".into()),
            ("Connection".into(), "upgrade".into()),
            ("Transfer-Encoding".into(), "chunked".into()),
        ];
        let wire = wire_request(&request, Some("https://example.test"), false).unwrap();
        assert_eq!(wire.version(), Version::HTTP_2);
        assert_eq!(wire.uri().to_string(), "https://[::1]:8443/a%2Fb?q=x%20y");
        assert_eq!(wire.headers()["content-length"], "0");
        assert!(wire.headers()["authorization"].is_sensitive());
        assert!(!wire.headers().contains_key("connection"));
        assert!(!wire.headers().contains_key("transfer-encoding"));
        for (name, value) in [
            ("connection", "close"),
            ("te", "trailers"),
            ("x-test", " leading"),
            ("x-test", "trailing\t"),
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(
                HeaderName::from_static(name),
                HeaderValue::from_str(value).unwrap(),
            );
            assert!(response_headers(&headers).is_err(), "{name}: {value}");
        }
        let mut headers = HeaderMap::new();
        headers.append("set-cookie", HeaderValue::from_static("a=1"));
        headers.append("set-cookie", HeaderValue::from_static("b=2"));
        assert_eq!(response_headers(&headers).unwrap().1, ["a=1", "b=2"]);
    }

    async fn frame<T: tokio::io::AsyncWrite + Unpin>(
        io: &mut T,
        kind: u8,
        flags: u8,
        id: u32,
        payload: &[u8],
    ) {
        let length = payload.len() as u32;
        let mut bytes = Vec::with_capacity(payload.len() + 9);
        bytes.extend_from_slice(&length.to_be_bytes()[1..]);
        bytes.extend_from_slice(&[kind, flags]);
        bytes.extend_from_slice(&id.to_be_bytes());
        bytes.extend_from_slice(payload);
        io.write_all(&bytes).await.unwrap();
        io.flush().await.unwrap();
    }

    async fn raw_request<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(
        io: &mut T,
    ) -> u32 {
        let mut preface = [0; 24];
        io.read_exact(&mut preface).await.unwrap();
        assert_eq!(&preface, b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n");
        frame(io, 4, 0, 0, &[]).await;
        loop {
            let mut head = [0; 9];
            io.read_exact(&mut head).await.unwrap();
            let length = u32::from_be_bytes([0, head[0], head[1], head[2]]) as usize;
            assert!(length <= 65536);
            let mut payload = vec![0; length];
            io.read_exact(&mut payload).await.unwrap();
            if head[3] == 4 && head[4] & 1 == 0 {
                frame(io, 4, 1, 0, &[]).await;
            }
            if head[3] == 1 {
                return u32::from_be_bytes(head[5..9].try_into().unwrap()) & 0x7fffffff;
            }
        }
    }

    #[tokio::test]
    async fn http2_goaway_retries_only_definitely_unprocessed_posts() {
        for processed in [false, true] {
            let site = Site::new(&[b"h2"]).await;
            let mut request = site.request.clone();
            request.method = "POST".into();
            request.body = Some((String::new(), b"must not duplicate".to_vec()));
            let count = Arc::new(AtomicUsize::new(0));
            let attempts = count.clone();
            let server = tokio::spawn(async move {
                let (tcp, _) = site.listener.accept().await.unwrap();
                attempts.fetch_add(1, Ordering::SeqCst);
                let mut first = site.acceptor.accept(tcp).await.unwrap();
                let id = raw_request(&mut first).await;
                let last_id = if processed { id } else { 0u32 };
                let payload = [last_id.to_be_bytes(), 0u32.to_be_bytes()].concat();
                frame(&mut first, 7, 0, 0, &payload).await;
                first.shutdown().await.unwrap();
                // Drain outstanding upload bytes so closing TCP cannot lose
                // the GOAWAY through a reset caused by unread request bytes.
                let drain = tokio::spawn(async move {
                    let mut discarded = Vec::new();
                    let _ = tokio::time::timeout(
                        Duration::from_secs(2),
                        first.read_to_end(&mut discarded),
                    )
                    .await;
                });
                if processed {
                    assert!(
                        tokio::time::timeout(Duration::from_millis(250), site.listener.accept())
                            .await
                            .is_err(),
                        "a possibly processed POST must not open a retry connection"
                    );
                } else {
                    let (tcp, _) = site.listener.accept().await.unwrap();
                    attempts.fetch_add(1, Ordering::SeqCst);
                    let mut second = site.acceptor.accept(tcp).await.unwrap();
                    let id = raw_request(&mut second).await;
                    frame(&mut second, 1, 4, id, &[0x88]).await;
                    frame(&mut second, 0, 1, id, b"retried safely").await;
                    second.shutdown().await.unwrap();
                }
                drain.abort();
            });
            let response = fetch(&request).await;
            if processed {
                assert!(response.is_err());
            } else {
                assert_eq!(response.unwrap().body, b"retried safely");
            }
            tokio::time::timeout(Duration::from_secs(3), server)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(count.load(Ordering::SeqCst), if processed { 1 } else { 2 });
        }
    }

    #[tokio::test]
    async fn http2_interim_responses_trailers_and_completed_response_reset() {
        let site = Site::new(&[b"h2"]).await;
        let mut request = site.request.clone();
        request.method = "POST".into();
        request.body = Some((String::new(), vec![0x42; STREAM_WINDOW as usize * 2]));
        let server = tokio::spawn(async move {
            let (tcp, _) = site.listener.accept().await.unwrap();
            let mut tls = site.acceptor.accept(tcp).await.unwrap();
            let id = raw_request(&mut tls).await;
            // HPACK indexed-name literal :status=103, then final 200.
            frame(&mut tls, 1, 4, id, &[0x08, 3, b'1', b'0', b'3']).await;
            frame(&mut tls, 1, 4, id, &[0x88]).await;
            frame(&mut tls, 0, 0, id, b"complete").await;
            // Legal trailer and NO_ERROR after END_STREAM: cancel the upload
            // but preserve the complete response (RFC 9113 §8.1).
            frame(&mut tls, 1, 5, id, &[0x00, 1, b'x', 1, b'y']).await;
            frame(&mut tls, 3, 0, id, &0u32.to_be_bytes()).await;
            let mut discarded = [0; 65536];
            let _ = tokio::time::timeout(Duration::from_millis(200), async {
                while tls.read(&mut discarded).await.unwrap_or(0) != 0 {}
            })
            .await;
        });
        let response = fetch(&request).await.unwrap();
        assert_eq!(response.body, b"complete");
        assert!(!response.headers.iter().any(|(name, _)| name == "x"));
        let timing = response.timing.unwrap();
        assert!(timing.first_interim_response_start >= timing.request_start);
        assert!(timing.final_response_start >= timing.first_interim_response_start);
        server.await.unwrap();
    }

    // Same serialization as the HTTP/1 cookie integration tests. The server
    // task does not acquire this test-only lock.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn http2_redirects_cookies_content_decoding_and_cors_use_shared_policy() {
        use std::io::Write;
        let _cookie_guard = super::super::COOKIE_TEST_LOCK.lock().unwrap();
        super::super::set_cookies_enabled(true);
        let site = Site::new(&[b"h2"]).await;
        let mut request = site.request.clone();
        super::super::set_navigation_metadata(&mut request, None);
        let expected_cookie = format!("h2_redirect_{}=yes", request.url.port().unwrap());
        let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gzip.write_all(b"decoded correctly").unwrap();
        let compressed = gzip.finish().unwrap();
        let _server = site.serve(move |request, mut respond| {
            let cookie = expected_cookie.clone();
            let compressed = compressed.clone();
            async move {
                if request.uri().path() == "/" {
                    let response = http_wire::Response::builder()
                        .status(302)
                        .header("location", "/final")
                        .header("set-cookie", format!("{cookie}; Path=/; Secure"))
                        .header("set-cookie", "h2_second_cookie=kept; Path=/; Secure")
                        .body(())
                        .unwrap();
                    respond.send_response(response, true).unwrap();
                } else {
                    let cookies = request.headers().get("cookie").unwrap().to_str().unwrap();
                    assert!(cookies.contains(&cookie));
                    assert!(cookies.contains("h2_second_cookie=kept"));
                    let response = http_wire::Response::builder()
                        .header("content-encoding", "gzip")
                        .header("content-type", "text/plain")
                        .header("content-length", compressed.len())
                        .body(())
                        .unwrap();
                    respond
                        .send_response(response, false)
                        .unwrap()
                        .send_data(Bytes::from(compressed), true)
                        .unwrap();
                }
            }
        });
        let response = fetch(&request).await.unwrap();
        assert_eq!(response.url.path(), "/final");
        assert_eq!(response.body, b"decoded correctly");
        let timing = response.timing.unwrap();
        assert_eq!(timing.next_hop_protocol, "h2");
        assert!(timing.connection_reused);
        assert_eq!(timing.decoded_body_size, b"decoded correctly".len());
        // Cross-origin script access still fails without ACAO, despite a
        // successful transport and correctly decoded response body.
        request.url.set_path("/final");
        let mut client = request.url.clone();
        client.set_port(Some(1)).unwrap();
        request.fetch_policy = Some(super::super::FetchPolicy {
            origin: client,
            mode: super::super::RequestMode::Cors,
            credentials: super::super::CredentialsMode::Include,
        });
        assert!(
            super::super::fetch_script(&request)
                .await
                .unwrap_err()
                .contains("CORS")
        );
    }

    #[tokio::test]
    async fn http2_respects_server_concurrent_stream_limit() {
        let site = Site::new(&[b"h2"]).await;
        let request = site.request.clone();
        let server = tokio::spawn(async move {
            let (tcp, _) = site.listener.accept().await.unwrap();
            let tls = site.acceptor.accept(tcp).await.unwrap();
            let mut connection = h2::server::Builder::new()
                .max_concurrent_streams(1)
                .handshake::<_, Bytes>(tls)
                .await
                .unwrap();
            let mut replies = tokio::task::JoinSet::new();
            while let Some(Ok((_request, mut respond))) = connection.accept().await {
                replies.spawn(async move {
                    tokio::task::yield_now().await;
                    ok(&mut respond, b"limited");
                });
            }
        });
        assert_eq!(fetch(&request).await.unwrap().body, b"limited");
        for response in futures::future::join_all((0..8).map(|_| fetch(&request))).await {
            assert_eq!(response.unwrap().body, b"limited");
        }
        server.abort();
    }

    #[tokio::test]
    async fn http2_required_http1_uses_new_tls_connection_without_unsafe_post_replay() {
        for (method, goaway) in [("POST", true), ("GET", false), ("POST", false)] {
            let site = Site::new(&[b"h2", b"http/1.1"]).await;
            let mut request = site.request.clone();
            request.method = method.into();
            if method == "POST" {
                request.body = Some((String::new(), b"once".to_vec()));
            }
            let can_retry = goaway || method == "GET";
            let server = tokio::spawn(async move {
                let (tcp, _) = site.listener.accept().await.unwrap();
                let mut first = site.acceptor.accept(tcp).await.unwrap();
                let id = raw_request(&mut first).await;
                if goaway {
                    frame(&mut first, 7, 0, 0, &[0, 0, 0, 0, 0, 0, 0, 13]).await;
                } else {
                    frame(&mut first, 3, 0, id, &[0, 0, 0, 13]).await;
                }
                if !can_retry {
                    assert!(
                        tokio::time::timeout(Duration::from_millis(250), site.listener.accept())
                            .await
                            .is_err()
                    );
                    return;
                }
                let drain = tokio::spawn(async move {
                    let mut bytes = [0; 4096];
                    while first.read(&mut bytes).await.unwrap_or(0) != 0 {}
                });
                let (tcp, _) = site.listener.accept().await.unwrap();
                let mut second = site.acceptor.accept(tcp).await.unwrap();
                assert_eq!(second.get_ref().1.alpn_protocol(), Some(&b"http/1.1"[..]));
                let mut head = Vec::new();
                let mut byte = [0];
                while !head.ends_with(b"\r\n\r\n") {
                    second.read_exact(&mut byte).await.unwrap();
                    head.push(byte[0]);
                }
                assert!(head.starts_with(format!("{method} / HTTP/1.1\r\n").as_bytes()));
                if method == "POST" {
                    let mut body = [0; 4];
                    second.read_exact(&mut body).await.unwrap();
                    assert_eq!(&body, b"once");
                }
                second.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\nfallback").await.unwrap();
                second.shutdown().await.unwrap();
                drain.abort();
            });
            let result = fetch(&request).await;
            if can_retry {
                let response = result.unwrap();
                assert_eq!(response.body, b"fallback");
                assert_eq!(response.timing.unwrap().next_hop_protocol, "http/1.1");
            } else {
                assert!(result.is_err());
            }
            tokio::time::timeout(Duration::from_secs(3), server)
                .await
                .unwrap()
                .unwrap();
        }
    }

    #[tokio::test]
    async fn http2_misdirected_response_retries_once_on_new_connection() {
        let site = Site::new(&[b"h2"]).await;
        let request = site.request.clone();
        let count = Arc::new(AtomicUsize::new(0));
        let requests = count.clone();
        let server = site.serve(move |_request, mut respond| {
            let attempt = requests.fetch_add(1, Ordering::SeqCst);
            async move {
                let head = http_wire::Response::builder()
                    .status(if attempt == 0 { 421 } else { 200 })
                    .body(())
                    .unwrap();
                let mut body = respond.send_response(head, false).unwrap();
                body.send_data(
                    Bytes::from_static(if attempt == 0 {
                        b"misdirected"
                    } else {
                        b"correct"
                    }),
                    true,
                )
                .unwrap();
            }
        });
        let response = fetch(&request).await.unwrap();
        assert_eq!(response.body, b"correct");
        assert_eq!(response.timing.unwrap().encoded_body_size, 7);
        assert_eq!(server.connections.load(Ordering::SeqCst), 2);
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn http2_frame_clock_preserves_fragmented_header_start_and_skips_payload() {
        let clock = Arc::new(ResponseClock::default());
        clock.0.lock().unwrap().insert(1, VecDeque::new());
        let mut io = TimedIo {
            inner: (),
            clock: clock.clone(),
            header: [0; 9],
            used: 0,
            remaining: 0,
            started: 0.0,
        };
        io.observe(&[0]);
        io.started = 42.5;
        io.observe(&[0, 1, 1, 4, 0, 0, 0, 1, 0x88]);
        assert_eq!(clock.take(1), 42.5);
        // DATA payload that looks like a HEADERS frame must not be inspected.
        io.observe(&[0, 0, 9, 0, 0, 0, 0, 0, 1, 0, 0, 0, 1, 4, 0, 0, 0, 1]);
        assert!(clock.0.lock().unwrap()[&1].is_empty());
        io.observe(&[0, 0, 0, 1, 5, 0, 0, 0, 1]);
        assert_eq!(clock.0.lock().unwrap()[&1].len(), 1);
    }

    #[test]
    fn http2_tls_negotiation_does_not_change_websocket_connector() {
        assert!(
            crate::tls::webpki_connector()
                .config()
                .alpn_protocols
                .is_empty()
        );
        assert_eq!(
            crate::tls::http_connector().config().alpn_protocols,
            [b"h2".to_vec(), b"http/1.1".to_vec()]
        );
    }

    #[tokio::test]
    async fn http2_tls_still_rejects_untrusted_certificates() {
        let site = Site::new(&[b"h2"]).await;
        let port = site.request.url.port().unwrap();
        let server = tokio::spawn(async move {
            let (tcp, _) = site.listener.accept().await.unwrap();
            assert!(site.acceptor.accept(tcp).await.is_err());
        });
        let error = super::super::dial_with_connector(
            "https",
            "127.0.0.1",
            port,
            &crate::tls::http_connector(),
            None,
        )
        .await
        .err()
        .expect("the production WebPKI roots must reject the test certificate");
        assert!(error.contains("certificate"), "{error}");
        server.await.unwrap();
    }

    #[tokio::test]
    #[ignore = "live transport diagnostic; set TRUST_NET_DIAG to an HTTPS URL"]
    async fn http2_live_transport_probe() {
        let url = std::env::var("TRUST_NET_DIAG").expect("set TRUST_NET_DIAG");
        let mut request = Request::get(url::Url::parse(&url).unwrap());
        super::super::set_navigation_metadata(&mut request, None);
        for _ in 0..2 {
            let response = fetch(&request).await.unwrap();
            let timing = response.timing.unwrap();
            eprintln!(
                "transport: status={} protocol={} reused={} bytes={} challenge={} elapsed_ms={:.1}",
                response.status,
                timing.next_hop_protocol,
                timing.connection_reused,
                response.body.len(),
                response.challenge.is_some(),
                timing.response_end - timing.start_time
            );
            assert_eq!(timing.next_hop_protocol, "h2");
            assert_eq!(response.status, 200);
            assert!(response.challenge.is_none());
        }
    }

    #[tokio::test]
    async fn http2_pool_is_bounded_partitioned_and_keeps_live_origins_reachable() {
        let mut pool = Pool::default();
        let now = Instant::now();
        let mut request = Request::get(url::Url::parse("https://example.test/").unwrap());
        let key = PoolKey::for_request(&request).unwrap();
        let live = pool.origin(&key, now);
        for index in 0..100 {
            request.url = url::Url::parse(&format!("https://host-{index}.test/")).unwrap();
            pool.origin(&PoolKey::for_request(&request).unwrap(), now);
        }
        assert_eq!(pool.recent.len(), super::super::POOL_MAX_IDLE);
        assert!(Arc::ptr_eq(&live, &pool.origin(&key, now)));
        let mut partitioned = key.clone();
        partitioned.top_level_site = Some("https://different.test".into());
        assert!(!Arc::ptr_eq(&live, &pool.origin(&partitioned, now)));
        partitioned = key.clone();
        partitioned.credentials = !key.credentials;
        assert!(!Arc::ptr_eq(&live, &pool.origin(&partitioned, now)));
        pool.prune(now + super::super::POOL_IDLE_TTL);
        assert!(pool.recent.is_empty());
        assert_eq!(
            pool.live.len(),
            1,
            "only the externally live origin remains"
        );
    }
}
