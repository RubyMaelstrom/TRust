//! Fetching media resources: HTML #concept-media-load-resource's resource
//! fetch algorithm, with byte-range requests (Fetch #concept-request-add-range-header)
//! so that a resource can be read out of order (an MP4 movie box at its end,
//! seeking) without downloading everything first.
//!
//! [`HttpSource`] is a [`ByteSource`]: the player's control thread reads
//! from it and blocks until the bytes arrive. Downloads run on the network
//! runtime into a bounded cache of byte runs; a download pauses once it is
//! far enough ahead of the reader (HTML lets the user agent suspend a load
//! and fire `suspend`), and a read outside what is cached or coming starts a
//! new range request.

use std::collections::BTreeMap;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use super::demux::ByteSource;

/// How far a download may run ahead of the reader before it pauses.
const READ_AHEAD: u64 = 16 << 20;
/// Cached bytes kept behind the reader (short backward seeks, re-reads).
const KEEP_BEHIND: u64 = 4 << 20;
/// A read this close ahead of an active download waits for it instead of
/// starting another request.
const WAIT_AHEAD: u64 = 512 << 10;
/// HTML #media-element-stall-timeout (3 seconds is the suggested value).
const STALL_TIMEOUT: Duration = Duration::from_secs(3);

/// Network activity the media element reports as events (HTML
/// #concept-media-load-resource): `progress` while bytes arrive (about every
/// 350 ms), `suspend` when the download pauses, `stalled` when bytes stop
/// arriving.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetworkEvent {
    Progress,
    Suspend,
    Stalled,
    /// The whole resource has been fetched.
    Complete,
}

pub type NetworkEvents = Arc<dyn Fn(NetworkEvent) + Send + Sync>;

struct Download {
    id: u64,
    /// The next byte this download will deliver.
    next: u64,
    cancel: Arc<crate::http::BodyFlow>,
}

#[derive(Default)]
struct State {
    /// Cached runs of bytes keyed by their start offset; never overlapping.
    runs: BTreeMap<u64, Vec<u8>>,
    cached: u64,
    length: Option<u64>,
    /// The server honoured a range request (HTTP 206).
    ranges: Option<bool>,
    download: Option<Download>,
    error: Option<String>,
    /// The reader's latest position, for read-ahead and eviction.
    position: u64,
    last_data: Option<Instant>,
    last_progress: Option<Instant>,
    suspended: bool,
    complete: bool,
}

struct Shared {
    state: Mutex<State>,
    arrived: Condvar,
    resume: tokio::sync::Notify,
    aborted: AtomicBool,
    next_id: AtomicU64,
    /// The origins of the URLs that answered (after redirects), for
    /// whether the resource is CORS-cross-origin.
    origins: Mutex<Vec<url::Origin>>,
}

/// The origins that answered a media resource's requests.
#[derive(Clone)]
pub struct ResponseOrigins(Arc<Shared>);

impl ResponseOrigins {
    pub fn get(&self) -> Vec<url::Origin> {
        self.0
            .origins
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

/// An HTTP(S) media resource.
pub struct HttpSource {
    shared: Arc<Shared>,
    request: crate::http::Request,
    runtime: tokio::runtime::Handle,
    events: NetworkEvents,
}

/// Interrupts a source's blocked reads from another thread (seeking,
/// teardown).
#[derive(Clone)]
pub struct AbortHandle(Arc<Shared>);

impl AbortHandle {
    pub fn abort(&self) {
        self.0.aborted.store(true, Ordering::Release);
        let state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(download) = &state.download {
            download.cancel.cancel();
        }
        drop(state);
        self.0.arrived.notify_all();
        self.0.resume.notify_waiters();
    }
}

impl HttpSource {
    /// `request` is the potential-CORS request the element prepared
    /// (destination "audio"/"video", its referrer and credentials); each
    /// range request is a copy with a `Range` header.
    pub(crate) fn new(
        request: crate::http::Request,
        runtime: tokio::runtime::Handle,
        events: NetworkEvents,
    ) -> Self {
        Self {
            shared: Arc::new(Shared {
                state: Mutex::default(),
                arrived: Condvar::new(),
                resume: tokio::sync::Notify::new(),
                aborted: AtomicBool::new(false),
                next_id: AtomicU64::new(1),
                origins: Mutex::new(Vec::new()),
            }),
            request,
            runtime,
            events,
        }
    }

    pub fn abort_handle(&self) -> AbortHandle {
        AbortHandle(self.shared.clone())
    }

    pub fn response_origins(&self) -> ResponseOrigins {
        ResponseOrigins(self.shared.clone())
    }

    /// Whether the server honoured a range request; `None` before the
    /// first response.
    pub fn ranges_supported(&self) -> Option<bool> {
        self.lock().ranges
    }

    /// Whether every byte of the resource has been fetched.
    pub fn complete(&self) -> bool {
        self.lock().complete
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.shared.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Start a range request at `offset`, replacing any running download.
    fn start(&self, state: &mut State, offset: u64) {
        if let Some(old) = state.download.take() {
            old.cancel.cancel();
        }
        let id = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
        let cancel = crate::http::BodyFlow::new();
        state.download = Some(Download {
            id,
            next: offset,
            cancel: cancel.clone(),
        });
        state.error = None;
        state.suspended = false;
        let mut request = self.request.clone();
        request
            .headers
            .retain(|(name, _)| !name.eq_ignore_ascii_case("range"));
        // HTML #concept-media-load-resource: "add a range header" for the
        // byte range needed; `bytes=0-` from the start asks whether the
        // server supports ranges at all, as Chrome and Firefox do.
        request
            .headers
            .push((String::from("Range"), format!("bytes={offset}-")));
        let shared = self.shared.clone();
        let events = self.events.clone();
        self.runtime.spawn(async move {
            download(shared, request, id, offset, cancel, events).await;
        });
    }

    /// Copy cached bytes at `offset` into `buf`; how many.
    fn copy_cached(state: &State, offset: u64, buf: &mut [u8]) -> usize {
        let Some((&start, run)) = state.runs.range(..=offset).next_back() else {
            return 0;
        };
        let end = start + run.len() as u64;
        if offset >= end {
            return 0;
        }
        let from = (offset - start) as usize;
        let count = buf.len().min(run.len() - from);
        buf[..count].copy_from_slice(&run[from..from + count]);
        count
    }
}

/// One range request's transfer into the cache.
async fn download(
    shared: Arc<Shared>,
    request: crate::http::Request,
    id: u64,
    offset: u64,
    cancel: Arc<crate::http::BodyFlow>,
    events: NetworkEvents,
) {
    let fail = |message: String| {
        let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.download.as_ref().is_some_and(|d| d.id == id) {
            state.error = Some(message);
            state.download = None;
        }
        drop(state);
        shared.arrived.notify_all();
    };
    let (response, body) = match crate::http::fetch_script_streaming(&request).await {
        Ok(result) => result,
        Err(error) => return fail(error.message),
    };
    {
        let origin = response.url.origin();
        let mut origins = shared.origins.lock().unwrap_or_else(|e| e.into_inner());
        if !origins.contains(&origin) {
            origins.push(origin);
        }
    }
    // HTML #verify-a-media-response: an ok status, and for a ranged
    // request a 206 whose Content-Range starts where asked (a 200 to
    // `bytes=0-` is the whole resource from a server without ranges).
    let mut skip = 0u64;
    let length;
    let ranged;
    match response.status {
        206 => {
            let content_range = crate::http::header_list_get(&response.headers, "content-range")
                .unwrap_or_default();
            let Some((start, total)) = parse_content_range(&content_range) else {
                return fail(String::from("invalid Content-Range"));
            };
            if start != offset {
                return fail(String::from("Content-Range does not match the request"));
            }
            length = total;
            ranged = true;
        }
        200 => {
            // The server ignored the range: the body starts at 0.
            skip = offset;
            length = crate::http::header_list_get(&response.headers, "content-length")
                .and_then(|value| value.trim().parse::<u64>().ok());
            ranged = false;
        }
        status => return fail(format!("HTTP status {status}")),
    }
    {
        let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if !state.download.as_ref().is_some_and(|d| d.id == id) {
            return;
        }
        if length.is_some() {
            state.length = length;
        }
        if state.ranges.is_none() || ranged {
            state.ranges = Some(ranged);
        }
    }
    let mut next = offset;
    let mut body = match body {
        Some(body) => body,
        None => {
            // The whole body arrived with the header section.
            let bytes = response.body;
            let bytes = if (skip as usize) < bytes.len() {
                &bytes[skip as usize..]
            } else {
                &[][..]
            };
            store(&shared, id, next, bytes, &events);
            finish(&shared, id, next + bytes.len() as u64, &events);
            return;
        }
    };
    loop {
        // Pause while far enough ahead of the reader.
        loop {
            let (ahead, current) = {
                let state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
                (
                    next.saturating_sub(state.position) > READ_AHEAD,
                    state.download.as_ref().is_some_and(|d| d.id == id),
                )
            };
            if !current || cancel.is_cancelled() || shared.aborted.load(Ordering::Acquire) {
                return;
            }
            if !ahead {
                break;
            }
            let newly = {
                let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
                !std::mem::replace(&mut state.suspended, true)
            };
            if newly {
                events(NetworkEvent::Suspend);
            }
            let _ = tokio::time::timeout(Duration::from_secs(1), shared.resume.notified()).await;
        }
        shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .suspended = false;
        let piece = tokio::select! {
            biased;
            () = cancelled(&cancel) => return,
            piece = body.next() => piece,
        };
        match piece {
            Ok(Some(mut bytes)) => {
                if skip > 0 {
                    let drop = (skip as usize).min(bytes.len());
                    bytes.drain(..drop);
                    skip -= drop as u64;
                }
                if !bytes.is_empty() {
                    store(&shared, id, next, &bytes, &events);
                    next += bytes.len() as u64;
                }
            }
            Ok(None) => {
                finish(&shared, id, next, &events);
                return;
            }
            Err(error) => {
                // The per-response size cap or a dropped connection: the
                // reader restarts from where it is if it needs more.
                let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
                if state.download.as_ref().is_some_and(|d| d.id == id) {
                    if state.length.is_some_and(|len| next >= len) {
                        drop(state);
                        finish(&shared, id, next, &events);
                        return;
                    }
                    state.download = None;
                    if next == offset {
                        state.error = Some(error);
                    }
                }
                drop(state);
                shared.arrived.notify_all();
                return;
            }
        }
    }
}

async fn cancelled(flow: &crate::http::BodyFlow) {
    while !flow.is_cancelled() {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Append bytes delivered by download `id` at `offset`.
fn store(shared: &Shared, id: u64, offset: u64, bytes: &[u8], events: &NetworkEvents) {
    let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
    if !state.download.as_ref().is_some_and(|d| d.id == id) {
        return;
    }
    insert(&mut state, offset, bytes);
    if let Some(download) = state.download.as_mut() {
        download.next = offset + bytes.len() as u64;
    }
    let now = Instant::now();
    state.last_data = Some(now);
    let progress = state
        .last_progress
        .is_none_or(|last| now.duration_since(last) >= Duration::from_millis(350));
    if progress {
        state.last_progress = Some(now);
    }
    evict(&mut state);
    drop(state);
    shared.arrived.notify_all();
    if progress {
        events(NetworkEvent::Progress);
    }
}

fn finish(shared: &Shared, id: u64, end: u64, events: &NetworkEvents) {
    let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
    if !state.download.as_ref().is_some_and(|d| d.id == id) {
        return;
    }
    state.download = None;
    if state.length.is_none() {
        state.length = Some(end);
    }
    let complete = state.length.is_some_and(|len| covered(&state, 0, len));
    state.complete = complete;
    drop(state);
    shared.arrived.notify_all();
    events(NetworkEvent::Progress);
    if complete {
        events(NetworkEvent::Complete);
    }
}

/// Whether the cache holds every byte of `start..end`.
fn covered(state: &State, start: u64, end: u64) -> bool {
    let mut at = start;
    for (&run_start, run) in state.runs.range(..end) {
        let run_end = run_start + run.len() as u64;
        if run_start > at {
            return false;
        }
        at = at.max(run_end);
        if at >= end {
            return true;
        }
    }
    at >= end
}

/// Add bytes to the run cache, merging with an adjacent run.
fn insert(state: &mut State, offset: u64, bytes: &[u8]) {
    // Skip bytes some run already holds.
    let mut offset = offset;
    let mut bytes = bytes;
    if let Some((&start, run)) = state.runs.range(..=offset).next_back() {
        let end = start + run.len() as u64;
        if end > offset {
            let overlap = ((end - offset) as usize).min(bytes.len());
            offset += overlap as u64;
            bytes = &bytes[overlap..];
        }
    }
    if bytes.is_empty() {
        return;
    }
    // Trim where the next run begins.
    if let Some((&next_start, _)) = state.runs.range(offset + 1..).next() {
        let room = (next_start - offset) as usize;
        bytes = &bytes[..bytes.len().min(room)];
    }
    state.cached += bytes.len() as u64;
    let append_to = state
        .runs
        .range(..=offset)
        .next_back()
        .filter(|(start, run)| **start + run.len() as u64 == offset)
        .map(|(start, _)| *start);
    match append_to {
        Some(start) => state
            .runs
            .get_mut(&start)
            .expect("present")
            .extend_from_slice(bytes),
        None => {
            state.runs.insert(offset, bytes.to_vec());
        }
    }
}

/// Bound the cache: drop what lies far behind the reader, then runs that
/// are not near it.
fn evict(state: &mut State) {
    let limit = READ_AHEAD + KEEP_BEHIND + (8 << 20);
    if state.cached <= limit {
        return;
    }
    let keep_from = state.position.saturating_sub(KEEP_BEHIND);
    let starts: Vec<u64> = state.runs.keys().copied().collect();
    for start in starts {
        if state.cached <= limit {
            break;
        }
        let Some(run) = state.runs.get(&start) else {
            continue;
        };
        let end = start + run.len() as u64;
        if end <= keep_from {
            state.cached -= run.len() as u64;
            state.runs.remove(&start);
        } else if start < keep_from {
            let drop = (keep_from - start) as usize;
            let mut run = state.runs.remove(&start).expect("present");
            run.drain(..drop);
            state.cached -= drop as u64;
            state.runs.insert(keep_from, run);
        }
    }
    // Runs far ahead of the reader (other than the active download) go
    // next; they can be fetched again.
    while state.cached > limit {
        let Some((&start, _)) = state
            .runs
            .iter()
            .rev()
            .find(|(start, _)| **start > state.position + READ_AHEAD)
        else {
            break;
        };
        let run = state.runs.remove(&start).expect("present");
        state.cached -= run.len() as u64;
    }
}

/// `Content-Range: bytes first-last/complete` (RFC 9110 §14.4): the first
/// byte and the complete length, when stated.
fn parse_content_range(value: &str) -> Option<(u64, Option<u64>)> {
    let value = value.trim();
    value
        .get(..6)
        .filter(|p| p.eq_ignore_ascii_case("bytes "))?;
    let rest = value[6..].trim();
    let (range, complete) = rest.split_once('/')?;
    let (first, last) = range.split_once('-')?;
    let first: u64 = first.trim().parse().ok()?;
    let last: u64 = last.trim().parse().ok()?;
    if last < first {
        return None;
    }
    let complete = match complete.trim() {
        "*" => None,
        value => Some(value.parse::<u64>().ok()?),
    };
    Some((first, complete))
}

impl ByteSource for HttpSource {
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let mut state = self.lock();
        state.position = offset;
        let mut started = false;
        // Requests made for this read; a server that keeps answering
        // without the bytes asked for must not be asked forever.
        let mut attempts = 0;
        loop {
            if self.shared.aborted.load(Ordering::Acquire) {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "media load aborted",
                ));
            }
            let copied = Self::copy_cached(&state, offset, buf);
            if copied > 0 {
                // The reader advanced: let a paused download continue.
                self.shared.resume.notify_waiters();
                return Ok(copied);
            }
            if state.length.is_some_and(|len| offset >= len) {
                return Ok(0);
            }
            if let Some(error) = state.error.take()
                && started
            {
                return Err(io::Error::other(error));
            }
            let coming = state
                .download
                .as_ref()
                .is_some_and(|d| d.next <= offset && offset - d.next <= WAIT_AHEAD);
            if !coming {
                if started && state.download.is_none() && state.error.is_none() {
                    // The download ended without reaching `offset`: the
                    // resource is shorter than its length claimed.
                    if state.length.is_none() {
                        return Ok(0);
                    }
                }
                if started && state.ranges == Some(false) && offset > 0 && state.download.is_none()
                {
                    return Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        "server does not support range requests",
                    ));
                }
                attempts += 1;
                if attempts > 3 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "media resource did not deliver the requested bytes",
                    ));
                }
                self.start(&mut state, offset);
                started = true;
            }
            let (next, timeout) = self
                .shared
                .arrived
                .wait_timeout(state, STALL_TIMEOUT)
                .unwrap_or_else(|e| e.into_inner());
            state = next;
            if timeout.timed_out() && state.last_data.is_none_or(|t| t.elapsed() >= STALL_TIMEOUT) {
                drop(state);
                (self.events)(NetworkEvent::Stalled);
                state = self.lock();
            }
        }
    }

    fn len(&self) -> Option<u64> {
        self.lock().length
    }

    fn seekable(&self) -> bool {
        let state = self.lock();
        state.ranges == Some(true) || state.complete
    }
}

impl Drop for HttpSource {
    fn drop(&mut self) {
        self.abort_handle().abort();
    }
}

/// A local file (a `file:` media resource of a local document).
pub struct FileSource {
    file: std::fs::File,
    len: u64,
}

impl FileSource {
    pub fn open(path: &std::path::Path) -> io::Result<Self> {
        let file = std::fs::File::open(path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "not a regular file",
            ));
        }
        Ok(Self {
            len: metadata.len(),
            file,
        })
    }
}

impl ByteSource for FileSource {
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileExt;
            self.file.read_at(buf, offset)
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::FileExt;
            self.file.seek_read(buf, offset)
        }
    }

    fn len(&self) -> Option<u64> {
        Some(self.len)
    }

    fn seekable(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests;
