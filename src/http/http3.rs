//! HTTP/3 over QUIC v1: RFC 9114, RFC 9000/9001/9002, RFC 9204 and
//! RFC 7838 (local RFC Editor snapshot 2026-09-06). Quinn owns QUIC/TLS,
//! `h3` owns framing, SETTINGS and stateless QPACK. TRust owns discovery,
//! partitioning, browser policy, resource bounds, cancellation and retries.
//! No 0-RTT, server push, cross-origin coalescing or extended CONNECT.

use super::{FetchTiming, Headers, PoolKey, Request, ResponseParts, alt_svc, http3_io as io, wire};
use bytes::{Buf, Bytes};
use h3::client::{RequestStream, SendRequest};
use h3::error::{Code, ConnectionError, StreamError};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime};
use tokio::sync::{Notify, Semaphore, oneshot};

const CONNECT_BUDGET: Duration = Duration::from_secs(3);
const FIRST_WAIT: Duration = Duration::from_millis(250);
const RETRY_DELAY: Duration = Duration::from_secs(60);
const CACHE_ENTRIES: usize = 128;
const UPLOAD_CHUNK: usize = 64 * 1024;

// Bound speculative work even if many origins advertise new alternatives
// while earlier handshakes are blackholed. Ordinary TCP work never waits on
// this budget; it is not a limit on active HTTP streams or browser workers.
static CONNECT_SLOTS: LazyLock<Arc<Semaphore>> = LazyLock::new(|| Arc::new(Semaphore::new(16)));

type Sender = SendRequest<io::Open<h3_quinn::OpenStreams>, Bytes>;
type Receive = RequestStream<io::Recv, Bytes>;
type Send = RequestStream<io::SendHalf, Bytes>;

enum State {
    Idle,
    Connecting,
    Ready(Arc<Session>),
    Failed(Instant),
}
struct Origin {
    alternative: alt_svc::Alternative,
    config: quinn::ClientConfig,
    state: Mutex<State>,
}
struct Cached {
    origin: Arc<Origin>,
    expires: Instant,
    used: Instant,
}
static POOL: LazyLock<Mutex<HashMap<PoolKey, Cached>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn prune(pool: &mut HashMap<PoolKey, Cached>, now: Instant) {
    pool.retain(|_, entry| entry.expires > now);
    for entry in pool.values() {
        let mut state = entry.origin.state.lock().unwrap();
        if matches!(&*state, State::Ready(session) if session.closed.load(Ordering::Acquire)
            && session.activity.count.load(Ordering::Acquire) == 0)
        {
            // Retain a fresh advertisement, not a closed QUIC endpoint and
            // its transport bookkeeping, for the lifetime of that mapping.
            *state = State::Idle;
        }
    }
}

fn client_config(
    config: tokio_rustls::rustls::ClientConfig,
) -> Result<quinn::ClientConfig, String> {
    let crypto =
        quinn::crypto::rustls::QuicClientConfig::try_from(config).map_err(|e| e.to_string())?;
    let mut config = quinn::ClientConfig::new(Arc::new(crypto));
    let mut transport = quinn::TransportConfig::default();
    transport
        .max_concurrent_bidi_streams(0u32.into())
        .max_concurrent_uni_streams(16u32.into())
        .stream_receive_window((1024 * 1024u32).into())
        .receive_window((8 * 1024 * 1024u32).into())
        .send_window(8 * 1024 * 1024)
        .max_idle_timeout(Some(Duration::from_secs(30).try_into().unwrap()))
        // Keep outstanding responses alive (RFC 9114 §5.1). The driver
        // closes an unused session after POOL_IDLE_TTL, even with keepalive.
        .keep_alive_interval(Some(Duration::from_secs(10)))
        .datagram_receive_buffer_size(None)
        .datagram_send_buffer_size(0);
    config.transport_config(Arc::new(transport));
    Ok(config)
}

pub(crate) fn remember(request: &Request, status: u16, headers: &Headers, timing: &FetchTiming) {
    if request.url.scheme() != "https" || status == 421 {
        return;
    }
    let Some(value) = headers.get("alt-svc") else {
        return;
    };
    let Ok(key) = PoolKey::for_request(request) else {
        return;
    };
    let age = alt_svc::response_age(headers, timing, SystemTime::now());
    let alternative = alt_svc::parse(value, &key.host, age);
    let now = Instant::now();
    let mut pool = POOL.lock().unwrap();
    prune(&mut pool, now);
    let Some((alternative, ttl)) = alternative else {
        pool.remove(&key);
        return;
    };
    if let Some(entry) = pool.get_mut(&key)
        && entry.origin.alternative == alternative
    {
        // Repeated advertisements must not undo a failed-path cooldown.
        entry.expires = now + ttl;
        return;
    }
    let Ok(config) = client_config(crate::tls::http3_config()) else {
        return;
    };
    if pool.len() >= CACHE_ENTRIES
        && let Some(oldest) = pool
            .iter()
            .min_by_key(|(_, entry)| entry.used)
            .map(|(key, _)| key.clone())
    {
        pool.remove(&oldest);
    }
    pool.insert(
        key,
        Cached {
            origin: Arc::new(Origin {
                alternative,
                config,
                state: Mutex::new(State::Idle),
            }),
            expires: now + ttl,
            used: now,
        },
    );
}

pub(super) fn prune_idle() {
    prune(&mut POOL.lock().unwrap(), Instant::now());
}

/// RFC 7838 §9.4: alternative routing information is site data too.
pub(crate) fn forget_site(site: &str) {
    POOL.lock().unwrap().retain(|key, _| {
        let domain = psl::domain_str(&key.host).unwrap_or(&key.host);
        domain != site
    });
}

#[derive(Default)]
struct Activity {
    count: AtomicUsize,
    changed: Notify,
}
struct Session {
    sender: Mutex<Sender>,
    connection: quinn::Connection,
    _endpoint: quinn::Endpoint,
    clock: Arc<io::ResponseClock>,
    closed: Arc<AtomicBool>,
    activity: Arc<Activity>,
    timing: FetchTiming,
    used: AtomicBool,
}

pub(super) struct Lease {
    origin: Arc<Origin>,
    session: Arc<Session>,
}
impl Clone for Lease {
    fn clone(&self) -> Self {
        self.session.activity.count.fetch_add(1, Ordering::AcqRel);
        Self {
            origin: self.origin.clone(),
            session: self.session.clone(),
        }
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        self.session.activity.count.fetch_sub(1, Ordering::AcqRel);
        self.session.activity.changed.notify_one();
    }
}
impl Lease {
    fn new(origin: Arc<Origin>, session: Arc<Session>, timing: &mut FetchTiming) -> Self {
        session.activity.count.fetch_add(1, Ordering::AcqRel);
        session.activity.changed.notify_one();
        if session.used.swap(true, Ordering::AcqRel)
            || session.timing.connect_start < timing.fetch_start
        {
            timing.reused_connection(true);
        } else {
            timing.connection_reused = false;
            timing.domain_lookup_start = session.timing.domain_lookup_start;
            timing.domain_lookup_end = session.timing.domain_lookup_end;
            timing.connect_start = session.timing.connect_start;
            timing.secure_connection_start = session.timing.secure_connection_start;
            timing.connect_end = session.timing.connect_end;
        }
        timing.next_hop_protocol = "h3";
        Self { origin, session }
    }
    pub(super) fn failed(&self) {
        let mut state = self.origin.state.lock().unwrap();
        if matches!(&*state, State::Ready(session) if Arc::ptr_eq(session, &self.session)) {
            // Keep active streams alive while this alternative cools down.
            *state = State::Failed(Instant::now() + RETRY_DELAY);
        }
    }
    pub(super) fn misdirected(&self, key: &PoolKey) {
        let mut pool = POOL.lock().unwrap();
        if pool
            .get(key)
            .is_some_and(|entry| Arc::ptr_eq(&entry.origin, &self.origin))
        {
            pool.remove(key);
        }
    }
}

/// Only connection establishment races TCP, NEVER a request. The first
/// caller gives QUIC a short head start. Concurrent callers use existing
/// TCP immediately; a successful background handshake serves later work.
pub(super) async fn acquire(key: &PoolKey, timing: &mut FetchTiming) -> Option<Lease> {
    if key.scheme != "https" {
        return None;
    }
    let now = Instant::now();
    let origin = {
        let mut pool = POOL.lock().unwrap();
        prune(&mut pool, now);
        let entry = pool.get_mut(key)?;
        entry.used = now;
        entry.origin.clone()
    };
    let done = {
        let mut state = origin.state.lock().unwrap();
        match &*state {
            State::Ready(session)
                if !session.closed.load(Ordering::Acquire)
                    && session.connection.close_reason().is_none() =>
            {
                return Some(Lease::new(origin.clone(), session.clone(), timing));
            }
            State::Connecting => return None,
            State::Failed(until) if *until > now => return None,
            _ => {}
        }
        let permit = CONNECT_SLOTS.clone().try_acquire_owned().ok()?;
        *state = State::Connecting;
        let (send, done) = oneshot::channel();
        let origin = origin.clone();
        let key = key.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let result = tokio::time::timeout(CONNECT_BUDGET, connect(&key, &origin)).await;
            if std::env::var_os("TRUST_NET_TRACE").is_some() {
                match &result {
                    Ok(Ok(_)) => eprintln!("net: h3-ready origin={}:{}", key.host, key.port),
                    Ok(Err(error)) => eprintln!(
                        "net: h3-unavailable origin={}:{} reason={error:?}",
                        key.host, key.port
                    ),
                    Err(_) => eprintln!(
                        "net: h3-unavailable origin={}:{} reason=handshake-timeout",
                        key.host, key.port
                    ),
                }
            }
            let mut state = origin.state.lock().unwrap();
            *state = match result {
                Ok(Ok(session)) => State::Ready(session),
                _ => State::Failed(Instant::now() + RETRY_DELAY),
            };
            let _ = send.send(());
        });
        done
    };
    let _ = tokio::time::timeout(FIRST_WAIT, done).await;
    let state = origin.state.lock().unwrap();
    match &*state {
        State::Ready(session) if !session.closed.load(Ordering::Acquire) => {
            Some(Lease::new(origin.clone(), session.clone(), timing))
        }
        _ => None,
    }
}

// Closes losing/canceled address attempts, including interrupted handshakes.
struct EndpointGuard(Option<quinn::Endpoint>);
impl Drop for EndpointGuard {
    fn drop(&mut self) {
        if let Some(endpoint) = &self.0 {
            endpoint.close(0u32.into(), b"connection attempt canceled");
        }
    }
}

async fn connect(key: &PoolKey, origin: &Origin) -> Result<Arc<Session>, String> {
    use futures::{StreamExt, stream::FuturesUnordered};
    let mut timing = FetchTiming::new();
    timing.domain_lookup_start = crate::performance::now_ms();
    let mut addresses: Vec<_> =
        tokio::net::lookup_host((origin.alternative.dns_host(), origin.alternative.port))
            .await
            .map_err(|e| e.to_string())?
            .take(8)
            .collect();
    timing.domain_lookup_end = crate::performance::now_ms();
    if addresses.is_empty() {
        return Err("HTTP/3 DNS returned no addresses".into());
    }
    // Alternate address families early when one route is unavailable.
    if let Some(other) = addresses
        .iter()
        .position(|a| a.is_ipv6() != addresses[0].is_ipv6())
    {
        addresses.swap(1, other);
    }
    let mut attempts = FuturesUnordered::new();
    for (index, address) in addresses.into_iter().enumerate() {
        let config = origin.config.clone();
        let host = key.host.trim_matches(['[', ']']).to_owned();
        attempts.push(async move {
            if index != 0 {
                tokio::time::sleep(FIRST_WAIT * index as u32).await;
            }
            let started = crate::performance::now_ms();
            let bind = if address.is_ipv6() {
                "[::]:0"
            } else {
                "0.0.0.0:0"
            };
            let endpoint =
                quinn::Endpoint::client(bind.parse().unwrap()).map_err(|e| e.to_string())?;
            let mut guard = EndpointGuard(Some(endpoint));
            let connection = guard
                .0
                .as_ref()
                .unwrap()
                .connect_with(config, address, &host)
                .map_err(|e| e.to_string())?
                .await
                .map_err(|e| e.to_string())?;
            let alpn = connection
                .handshake_data()
                .and_then(|data| data.downcast::<quinn::crypto::rustls::HandshakeData>().ok())
                .and_then(|data| data.protocol);
            if alpn.as_deref() != Some(b"h3") {
                return Err("HTTP/3 ALPN mismatch".into());
            }
            Ok((guard.0.take().unwrap(), connection, started))
        });
    }
    let mut last = "HTTP/3 connection failed".to_string();
    while let Some(result) = attempts.next().await {
        let (endpoint, connection, started) = match result {
            Ok(value) => value,
            Err(error) => {
                last = error;
                continue;
            }
        };
        timing.connect_start = started;
        timing.secure_connection_start = started;
        timing.connect_end = crate::performance::now_ms();
        timing.next_hop_protocol = "h3";
        return handshake(endpoint, connection, timing).await;
    }
    Err(last)
}

struct DriverClosed(Arc<AtomicBool>);
impl Drop for DriverClosed {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

async fn handshake(
    endpoint: quinn::Endpoint,
    quic: quinn::Connection,
    timing: FetchTiming,
) -> Result<Arc<Session>, String> {
    let clock = Arc::new(io::ResponseClock::default());
    let (mut driver, sender) = h3::client::builder()
        .max_field_section_size(u64::from(wire::HEADER_BYTES))
        .build(io::Open::new(
            h3_quinn::Connection::new(quic.clone()),
            clock.clone(),
        ))
        .await
        .map_err(|e| e.to_string())?;
    let closed = Arc::new(AtomicBool::new(false));
    let activity = Arc::new(Activity::default());
    let session = Arc::new(Session {
        sender: Mutex::new(sender),
        connection: quic.clone(),
        _endpoint: endpoint,
        clock,
        closed: closed.clone(),
        activity: activity.clone(),
        timing,
        used: AtomicBool::new(false),
    });
    let closed = DriverClosed(closed);
    tokio::spawn(async move {
        let _closed = closed;
        loop {
            let unused = activity.count.load(Ordering::Acquire) == 0;
            tokio::select! {
                _ = driver.wait_idle() => break,
                _ = activity.changed.notified() => {},
                _ = tokio::time::sleep(super::POOL_IDLE_TTL), if unused => {
                    if activity.count.load(Ordering::Acquire) == 0 {
                        let _ = tokio::time::timeout(FIRST_WAIT, driver.shutdown(0)).await;
                        quic.close(quinn::VarInt::from_u32(0x100), b"idle");
                        break;
                    }
                }
            }
        }
    });
    Ok(session)
}

pub(super) struct Error {
    message: String,
    retry: bool,
    network: bool,
    http1: bool,
    code: Code,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.message.fmt(f)
    }
}
impl Error {
    fn malformed(message: &str) -> Self {
        Self {
            message: message.into(),
            retry: false,
            network: false,
            http1: false,
            code: Code::H3_MESSAGE_ERROR,
        }
    }
    fn limit(message: &str) -> Self {
        Self {
            code: Code::H3_EXCESSIVE_LOAD,
            ..Self::malformed(message)
        }
    }
    pub(super) fn retry_safe(&self, method: &str) -> bool {
        self.retry
            || self.network
                && matches!(
                    method,
                    "GET" | "HEAD" | "PUT" | "DELETE" | "OPTIONS" | "TRACE"
                )
    }
    pub(super) fn requires_http1(&self) -> bool {
        self.http1
    }
}
impl From<wire::Error> for Error {
    fn from(error: wire::Error) -> Self {
        if error.malformed {
            Self::malformed(&error.message)
        } else {
            Self::limit(&error.message)
        }
    }
}
impl From<StreamError> for Error {
    fn from(error: StreamError) -> Self {
        let mut result = Self::malformed(&format!("HTTP/3: {error}"));
        result.code = Code::H3_REQUEST_CANCELLED;
        match error {
            StreamError::RemoteClosing => {
                result.retry = true;
                result.network = true;
            }
            StreamError::RemoteTerminate { code, .. } => {
                result.retry = code == Code::H3_REQUEST_REJECTED;
                result.http1 = code == Code::H3_VERSION_FALLBACK;
                result.network = result.retry
                    || result.http1
                    || matches!(code, Code::H3_REQUEST_CANCELLED | Code::H3_NO_ERROR);
            }
            StreamError::ConnectionError(ConnectionError::Remote(ref remote), ..) => {
                result.network = true;
                result.http1 = matches!(remote, h3::quic::ConnectionErrorIncoming::ApplicationClose { error_code } if *error_code == Code::H3_VERSION_FALLBACK.value());
            }
            StreamError::ConnectionError(ConnectionError::Timeout, ..) => result.network = true,
            StreamError::StreamError { code, .. } => result.code = code,
            StreamError::HeaderTooBig { .. } => result.code = Code::H3_EXCESSIVE_LOAD,
            _ => {}
        }
        result
    }
}

async fn start(
    lease: &Lease,
    request: &Request,
    origin: Option<&str>,
    download: bool,
    timing: &mut FetchTiming,
) -> Result<(Send, Receive), Error> {
    let mut wire = wire::request(request, origin, download, http_wire::Version::HTTP_3)?;
    wire.headers_mut().insert(
        "alt-used",
        lease
            .origin
            .alternative
            .authority()
            .parse()
            .map_err(|_| Error::malformed("invalid Alt-Used authority"))?,
    );
    // Cloning an opener permits concurrent stream creation without holding
    // a mutex across QUIC flow-control or header transmission waits.
    let mut sender = lease.session.sender.lock().unwrap().clone();
    timing.request_start = crate::performance::now_ms();
    Ok(sender
        .send_request(wire)
        .await
        .map_err(Error::from)?
        .split())
}

async fn response_head(
    recv: &mut Receive,
    timing: &mut FetchTiming,
    clock: &io::ResponseClock,
) -> Result<(u16, Headers, Vec<String>), Error> {
    for _ in 0..=io::MAX_INTERIM {
        let response = recv.recv_response().await.map_err(Error::from)?;
        let started = clock.take(recv.id().into_inner());
        let (headers, cookies) = wire::response_headers(response.headers())?;
        let status = response.status().as_u16();
        if status == 101 {
            return Err(Error::malformed("HTTP/3 cannot switch protocols"));
        }
        if status < 200 {
            if timing.first_interim_response_start == 0.0 {
                timing.first_interim_response_start = started;
            }
            continue;
        }
        timing.final_response_start = started;
        return Ok((status, headers, cookies));
    }
    Err(Error::limit("too many HTTP/3 informational responses"))
}

async fn upload(send: &mut Send, payload: &[u8]) -> Result<(), StreamError> {
    for chunk in payload.chunks(UPLOAD_CHUNK) {
        send.send_data(Bytes::copy_from_slice(chunk)).await?;
    }
    send.finish().await
}

struct Incoming {
    recv: Receive,
    length: Option<usize>,
    received: usize,
    limit: usize,
    no_content: bool,
}
impl Incoming {
    fn new(
        mut recv: Receive,
        headers: &Headers,
        no_content: bool,
        limit: usize,
    ) -> Result<Self, Error> {
        let length = headers
            .get("content-length")
            .map(|v| super::parse_content_length(v))
            .transpose()
            .map_err(|error| {
                recv.stop_sending(Code::H3_MESSAGE_ERROR);
                Error::malformed(&error)
            })?
            .flatten();
        if !no_content && length.is_some_and(|length| length > limit) {
            recv.stop_sending(Code::H3_EXCESSIVE_LOAD);
            return Err(Error::limit("HTTP/3 response exceeds body size limit"));
        }
        Ok(Self {
            recv,
            length,
            received: 0,
            limit,
            no_content,
        })
    }
    async fn next_inner(&mut self) -> Result<Option<Bytes>, Error> {
        if let Some(mut data) = self.recv.recv_data().await.map_err(Error::from)? {
            let count = data.remaining();
            self.received = self.received.saturating_add(count);
            if self.received > self.limit {
                return Err(Error::limit("HTTP/3 response exceeds body size limit"));
            }
            if self.no_content && count != 0 {
                return Err(Error::malformed(
                    "HTTP/3 body is forbidden for this response",
                ));
            }
            if !self.no_content && self.length.is_some_and(|length| self.received > length) {
                return Err(Error::malformed(
                    "HTTP/3 Content-Length does not match response body",
                ));
            }
            return Ok(Some(data.copy_to_bytes(count)));
        }
        if let Some(trailers) = self.recv.recv_trailers().await.map_err(Error::from)? {
            wire::response_headers(&trailers)?;
        }
        if !self.no_content && self.length.is_some_and(|length| self.received != length) {
            return Err(Error::malformed(
                "HTTP/3 Content-Length does not match response body",
            ));
        }
        Ok(None)
    }
    async fn next(&mut self) -> Result<Option<Bytes>, Error> {
        let result = self.next_inner().await;
        if let Err(error) = &result {
            self.recv.stop_sending(error.code);
        }
        result
    }
}

async fn receive(
    mut recv: Receive,
    request: &Request,
    timing: &mut FetchTiming,
    clock: &io::ResponseClock,
) -> Result<ResponseParts, Error> {
    let (status, headers, cookies) = response_head(&mut recv, timing, clock)
        .await
        .inspect_err(|error| recv.stop_sending(error.code))?;
    let no_content = request.method == "HEAD" || matches!(status, 204 | 205 | 304);
    let navigation_get = request.method == "GET"
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
        return Ok((status, headers, Vec::new(), cookies));
    }
    let mut incoming = Incoming::new(recv, &headers, no_content, super::MAX_BODY)?;
    let mut body = Vec::new();
    while let Some(chunk) = incoming.next().await? {
        body.extend_from_slice(&chunk);
    }
    timing.response_end = crate::performance::now_ms();
    timing.encoded_body_size = body.len();
    let body = super::decode_content_encoding(&headers, body);
    timing.decoded_body_size = body.len();
    Ok((status, headers, body, cookies))
}

pub(super) async fn exchange(
    lease: &Lease,
    request: &Request,
    origin: Option<&str>,
    timing: &mut FetchTiming,
) -> Result<ResponseParts, Error> {
    let (mut send, recv) = start(lease, request, origin, false, timing).await?;
    let payload = request
        .body
        .as_ref()
        .map_or(&[][..], |(_, bytes)| bytes.as_slice());
    let result = {
        let receive = receive(recv, request, timing, &lease.session.clock);
        let upload = upload(&mut send, payload);
        tokio::pin!(receive, upload);
        tokio::select! {
            biased;
            result = &mut receive => {
                // Give an already-complete early response precedence over an
                // upload reset, but attempt to finish sending normally (§4.1).
                if result.is_ok() { let _ = tokio::time::timeout(FIRST_WAIT, &mut upload).await; }
                result
            }
            result = &mut upload => {
                match result {
                    Ok(()) => receive.await,
                    // STOP_SENDING closes only the request direction. §4.1
                    // requires preserving a complete response regardless of
                    // which stop code the peer chose (NO_ERROR is a SHOULD).
                    Err(StreamError::RemoteTerminate { .. }) => receive.await,
                    Err(error) => Err(error.into()),
                }
            }
        }
    };
    // Finished streams ignore reset. A still-open upload must not hold a
    // completed early response hostage when the peer stops consuming it.
    send.stop_stream(
        result
            .as_ref()
            .err()
            .map_or(Code::H3_NO_ERROR, |error| error.code),
    );
    result
}

pub(crate) struct DownloadResponse {
    pub(crate) status: u16,
    pub(crate) headers: Headers,
    incoming: Incoming,
    _lease: Lease,
}
impl DownloadResponse {
    pub(crate) async fn write_to(&mut self, file: &mut tokio::fs::File) -> Result<u64, String> {
        use tokio::io::AsyncWriteExt;
        while let Some(chunk) = self.incoming.next().await.map_err(|e| e.to_string())? {
            file.write_all(&chunk).await.map_err(|e| e.to_string())?;
        }
        Ok(self.incoming.received as u64)
    }
}

pub(super) async fn download(
    lease: &Lease,
    request: &Request,
    limit: usize,
    timing: &mut FetchTiming,
) -> Result<DownloadResponse, Error> {
    let (mut send, mut recv) = start(lease, request, None, true, timing).await?;
    send.finish().await.map_err(Error::from)?;
    let (status, headers, cookies) = response_head(&mut recv, timing, &lease.session.clock)
        .await
        .inspect_err(|error| recv.stop_sending(error.code))?;
    for cookie in cookies {
        super::response_cookie(request, &cookie);
    }
    remember(request, status, &headers, timing);
    let incoming = Incoming::new(recv, &headers, matches!(status, 204 | 205 | 304), limit)?;
    Ok(DownloadResponse {
        status,
        headers,
        incoming,
        _lease: lease.clone(),
    })
}

#[cfg(test)]
mod tests;
