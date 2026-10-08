//! Response bodies read incrementally after the response is returned.
//!
//! Fetch (local whatwg/fetch@394d20d) #concept-http-network-fetch returns the
//! response once all of its headers have arrived. In parallel it reads the
//! message body, decodes it with #handle-content-codings, and appends the
//! bytes to a network buffer that the response body's ReadableStream pulls
//! from. When the buffer exceeds an upper limit the user agent suspends the
//! fetch; a pull with the buffer below a lower limit resumes it. Aborting the
//! fetch transmits RST_STREAM on HTTP/2 and otherwise closes the connection.
//!
//! [`BodyStream`] is that in-parallel reader for one response, over HTTP/1.1
//! (RFC 9112 §§6–7), HTTP/2 or HTTP/3. Dropping it cancels the transfer: an
//! HTTP/2 stream is reset with CANCEL (RFC 9113 §8.7), an HTTP/3 stream is
//! stopped with H3_REQUEST_CANCELLED (RFC 9114 §4.1.1), and an HTTP/1.1
//! connection is closed. A body read to its end returns an HTTP/1.1
//! connection to the pool when it is at a reusable message boundary.
//! [`BodyFlow`] is the network buffer's suspend/resume and cancellation
//! signal shared with the consumer.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use tokio::io::{AsyncReadExt as _, BufReader};

use super::content_coding::ContentDecoder;
use super::{Conn, Headers, MAX_BODY, PoolKey, http2, http3};

/// How an HTTP/1.x message body is delimited (RFC 9112 §6.3).
pub(crate) enum Http1Framing {
    /// No message body follows the header section, or it is not read.
    None,
    Chunked,
    Length(usize),
    /// Delimited by the server closing the connection.
    Eof,
}

impl Http1Framing {
    /// Whether a message body remains to be read.
    pub(crate) fn streams(&self) -> bool {
        match self {
            Self::None => false,
            Self::Length(length) => *length > 0,
            Self::Chunked | Self::Eof => true,
        }
    }
}

/// Responses whose bodies are read incrementally. Redirects are consumed by
/// the redirect loop, and 421 is retried on another connection, so their
/// bodies are always read whole (Fetch #http-redirect-fetch,
/// #http-network-or-cache-fetch).
pub(crate) fn streamable_status(status: u16) -> bool {
    status >= 200 && !matches!(status, 301 | 302 | 303 | 307 | 308 | 421)
}

/// Bytes read from an HTTP/1.1 connection per step.
const READ_PIECE: usize = 64 * 1024;

#[derive(Clone, Copy)]
enum Chunk {
    /// Expecting a chunk-size line.
    Size,
    /// Inside chunk data, with this many bytes left.
    Data(usize),
    /// Expecting the CRLF after chunk data.
    DataEnd,
}

pub(crate) struct Http1Body {
    io: Option<BufReader<Conn>>,
    key: Option<PoolKey>,
    reusable: bool,
    framing: Http1Framing,
    remaining: usize,
    chunk: Chunk,
}

impl Http1Body {
    pub(super) fn new(io: BufReader<Conn>, key: PoolKey, head: &super::Http1Head) -> Self {
        let (framing, remaining) = match head.framing {
            Http1Framing::None => (Http1Framing::None, 0),
            Http1Framing::Chunked => (Http1Framing::Chunked, 0),
            Http1Framing::Length(length) => (Http1Framing::Length(length), length),
            Http1Framing::Eof => (Http1Framing::Eof, 0),
        };
        Self {
            io: Some(io),
            key: Some(key),
            reusable: head.reusable,
            framing,
            remaining,
            chunk: Chunk::Size,
        }
    }

    /// The message body ended at a message boundary.
    fn complete(&mut self) {
        let io = self.io.take();
        if self.reusable
            && let (Some(io), Some(key)) = (io, self.key.take())
        {
            super::pool_put(key, io);
        }
    }

    async fn next(&mut self) -> Result<Option<Vec<u8>>, String> {
        let Some(io) = self.io.as_mut() else {
            return Ok(None);
        };
        match self.framing {
            Http1Framing::None => {
                self.complete();
                Ok(None)
            }
            Http1Framing::Length(_) => {
                if self.remaining == 0 {
                    self.complete();
                    return Ok(None);
                }
                let mut piece = vec![0u8; self.remaining.min(READ_PIECE)];
                let read = io.read(&mut piece).await.map_err(|e| e.to_string())?;
                if read == 0 {
                    self.io = None;
                    return Err(String::from("incomplete response body"));
                }
                piece.truncate(read);
                self.remaining -= read;
                Ok(Some(piece))
            }
            Http1Framing::Eof => {
                let mut piece = vec![0u8; READ_PIECE];
                // Tolerate a missing TLS close_notify, as the whole-body
                // reader does.
                let read = match io.read(&mut piece).await {
                    Ok(read) => read,
                    Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => 0,
                    Err(e) => return Err(e.to_string()),
                };
                if read == 0 {
                    self.io = None;
                    return Ok(None);
                }
                piece.truncate(read);
                Ok(Some(piece))
            }
            Http1Framing::Chunked => loop {
                let Some(io) = self.io.as_mut() else {
                    return Ok(None);
                };
                match self.chunk {
                    Chunk::Size => {
                        // RFC 9112 §7.1: chunk-size [ chunk-ext ] CRLF.
                        let line = super::read_line(io)
                            .await
                            .map_err(|_| String::from("incomplete chunked response"))?;
                        let size_text = line.split(';').next().unwrap_or("").trim();
                        if size_text.is_empty()
                            || !size_text.bytes().all(|byte| byte.is_ascii_hexdigit())
                        {
                            return Err(String::from("incomplete chunked response"));
                        }
                        let size = usize::from_str_radix(size_text, 16)
                            .map_err(|_| String::from("incomplete chunked response"))?;
                        if size == 0 {
                            // The trailer section ends at an empty line.
                            let mut trailer_bytes = 0usize;
                            let mut trailer_fields = 0usize;
                            loop {
                                let line = super::read_line(io)
                                    .await
                                    .map_err(|_| String::from("incomplete chunked response"))?;
                                if line.is_empty() {
                                    break;
                                }
                                trailer_bytes =
                                    trailer_bytes.saturating_add(line.len().saturating_add(2));
                                trailer_fields += 1;
                                if !super::valid_chunk_trailer(&line)
                                    || trailer_bytes > 256 * 1024
                                    || trailer_fields > 256
                                {
                                    return Err(String::from("incomplete chunked response"));
                                }
                            }
                            self.complete();
                            return Ok(None);
                        }
                        self.chunk = Chunk::Data(size);
                    }
                    Chunk::Data(left) => {
                        let mut piece = vec![0u8; left.min(READ_PIECE)];
                        let read = io.read(&mut piece).await.map_err(|e| e.to_string())?;
                        if read == 0 {
                            self.io = None;
                            return Err(String::from("incomplete chunked response"));
                        }
                        piece.truncate(read);
                        self.chunk = if read == left {
                            Chunk::DataEnd
                        } else {
                            Chunk::Data(left - read)
                        };
                        return Ok(Some(piece));
                    }
                    Chunk::DataEnd => {
                        let mut crlf = [0u8; 2];
                        if io.read_exact(&mut crlf).await.is_err() || crlf != *b"\r\n" {
                            self.io = None;
                            return Err(String::from("incomplete chunked response"));
                        }
                        self.chunk = Chunk::Size;
                    }
                }
            },
        }
    }
}

enum BodySource {
    Http1(Box<Http1Body>),
    Http2(Box<http2::StreamingBody>),
    Http3(Box<http3::StreamingBody>),
}

/// One response body read incrementally, decoded as it arrives.
pub(crate) struct BodyStream {
    source: BodySource,
    decoder: ContentDecoder,
    encoded: usize,
    decoded: usize,
    done: bool,
}

impl BodyStream {
    fn new(source: BodySource, headers: &Headers) -> Self {
        Self {
            source,
            decoder: ContentDecoder::new(headers),
            encoded: 0,
            decoded: 0,
            done: false,
        }
    }

    pub(super) fn http1(body: Http1Body, headers: &Headers) -> Self {
        Self::new(BodySource::Http1(Box::new(body)), headers)
    }

    pub(super) fn http2(body: http2::StreamingBody, headers: &Headers) -> Self {
        Self::new(BodySource::Http2(Box::new(body)), headers)
    }

    pub(super) fn http3(body: Box<http3::StreamingBody>, headers: &Headers) -> Self {
        Self::new(BodySource::Http3(body), headers)
    }

    async fn next_transmitted(&mut self) -> Result<Option<Vec<u8>>, String> {
        match &mut self.source {
            BodySource::Http1(body) => body.next().await,
            BodySource::Http2(body) => body.next().await.map(|bytes| bytes.map(Vec::from)),
            BodySource::Http3(body) => body.next().await.map(|bytes| bytes.map(Vec::from)),
        }
    }

    /// The next decoded body bytes, or `None` once the body has ended.
    /// After an error the transfer is over.
    pub(crate) async fn next(&mut self) -> Result<Option<Vec<u8>>, String> {
        loop {
            if self.done {
                return Ok(None);
            }
            let Some(bytes) = self.next_transmitted().await.inspect_err(|_| {
                self.done = true;
            })?
            else {
                self.done = true;
                let rest = self.decoder.finish()?;
                self.decoded = self.decoded.saturating_add(rest.len());
                return Ok((!rest.is_empty()).then_some(rest));
            };
            self.encoded = self.encoded.saturating_add(bytes.len());
            let decoded = self.decoder.decode(&bytes).inspect_err(|_| {
                self.done = true;
            })?;
            self.decoded = self.decoded.saturating_add(decoded.len());
            if self.encoded > MAX_BODY || self.decoded > MAX_BODY {
                self.done = true;
                return Err(format!(
                    "response exceeds {} MB cap",
                    MAX_BODY / (1024 * 1024)
                ));
            }
            if !decoded.is_empty() {
                return Ok(Some(decoded));
            }
        }
    }

    /// Bytes received in the message body so far (Fetch "encoded size").
    pub(crate) fn encoded_size(&self) -> usize {
        self.encoded
    }

    /// Bytes after content-coding decoding so far (Fetch "decoded size").
    pub(crate) fn decoded_size(&self) -> usize {
        self.decoded
    }
}

/// The network buffer's flow control between the transfer and the consumer
/// of its bytes, and the fetch controller's cancellation.
#[derive(Default)]
pub(crate) struct BodyFlow {
    /// Bytes handed to the consumer that it has not yet taken.
    queued: AtomicUsize,
    cancelled: AtomicBool,
    wake: tokio::sync::Notify,
}

/// Suspend the transfer while more than this many bytes wait to be taken.
pub(crate) const BODY_FLOW_UPPER: usize = 1024 * 1024;
/// Resume a suspended transfer once fewer bytes than this wait.
pub(crate) const BODY_FLOW_LOWER: usize = 256 * 1024;

impl BodyFlow {
    pub(crate) fn new() -> Arc<Self> {
        Arc::default()
    }

    /// Bytes waiting for the consumer.
    pub(crate) fn queued(&self) -> usize {
        self.queued.load(Ordering::Acquire)
    }

    /// The consumer took `count` queued bytes.
    pub(crate) fn taken(&self, count: usize) {
        // The transfer counts bytes before delivering them, so the consumer
        // never takes more than are queued.
        let before = self.queued.fetch_sub(count, Ordering::AcqRel);
        if before.saturating_sub(count) < BODY_FLOW_LOWER && before >= BODY_FLOW_LOWER {
            self.wake.notify_one();
        }
    }

    /// Terminate the transfer (Fetch #fetch-controller-abort and
    /// #fetch-controller-terminate).
    pub(crate) fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.wake.notify_one();
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    /// Whether the transfer is suspended waiting for the consumer.
    pub(crate) fn suspended(&self) -> bool {
        self.queued() > BODY_FLOW_UPPER
    }

    async fn cancelled(&self) {
        while !self.is_cancelled() {
            self.wake.notified().await;
        }
    }
}

/// Run `body`'s transfer to its end, handing each decoded piece to
/// `deliver`. Between pieces the transfer is suspended while the consumer has
/// more than [`BODY_FLOW_UPPER`] bytes queued, until it takes enough to fall
/// below [`BODY_FLOW_LOWER`]. Returns `Ok(true)` at the end of the body,
/// `Ok(false)` when cancelled, and the error that ended the transfer
/// otherwise. `deliver` returns false when its consumer is gone.
pub(crate) async fn pump(
    body: &mut BodyStream,
    flow: &BodyFlow,
    mut deliver: impl FnMut(Vec<u8>) -> bool,
) -> Result<bool, String> {
    loop {
        while flow.suspended() && !flow.is_cancelled() {
            // A spurious wake only repeats the check.
            flow.wake.notified().await;
        }
        if flow.is_cancelled() {
            return Ok(false);
        }
        let piece = tokio::select! {
            biased;
            () = flow.cancelled() => return Ok(false),
            piece = body.next() => piece?,
        };
        let Some(piece) = piece else {
            return Ok(true);
        };
        flow.queued.fetch_add(piece.len(), Ordering::AcqRel);
        if !deliver(piece) {
            return Ok(false);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Request, fetch, fetch_script_streaming};
    use std::time::Duration;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::{TcpListener, TcpStream};

    async fn read_head(socket: &mut TcpStream) -> String {
        let mut request = Vec::new();
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let mut byte = [0u8; 1];
            if socket.read(&mut byte).await.unwrap() == 0 {
                break;
            }
            request.push(byte[0]);
        }
        String::from_utf8(request).unwrap()
    }

    fn request(address: std::net::SocketAddr, path: &str) -> Request {
        Request::get(url::Url::parse(&format!("http://{address}{path}")).unwrap())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn http1_chunked_body_streams_then_returns_its_connection_to_the_pool() {
        // RFC 9112 §7.1 chunks are handed over as they arrive; a body read to
        // its end leaves the connection at a message boundary for reuse.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let first = read_head(&mut socket).await;
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nTransfer-Encoding: chunked\r\n\r\n3;ext=1\r\none\r\n")
                .await
                .unwrap();
            release_rx.await.unwrap();
            socket
                .write_all(b"3\r\ntwo\r\n0\r\nTrailer-Field: x\r\n\r\n")
                .await
                .unwrap();
            let second = read_head(&mut socket).await;
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                .await
                .unwrap();
            (first, second)
        });
        let (response, body) = fetch_script_streaming(&request(address, "/a"))
            .await
            .unwrap();
        assert_eq!(response.status, 200);
        assert!(response.body.is_empty());
        let mut body = body.expect("a chunked body streams");
        assert_eq!(body.next().await.unwrap().unwrap(), b"one");
        release_tx.send(()).unwrap();
        assert_eq!(body.next().await.unwrap().unwrap(), b"two");
        assert!(body.next().await.unwrap().is_none());
        assert_eq!((body.encoded_size(), body.decoded_size()), (6, 6));
        let reused = fetch(&request(address, "/b")).await.unwrap();
        assert_eq!(reused.body, b"ok");
        assert!(reused.timing.unwrap().connection_reused);
        let (first, second) = server.await.unwrap();
        assert!(first.starts_with("GET /a HTTP/1.1\r\n"));
        assert!(second.starts_with("GET /b HTTP/1.1\r\n"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn http1_length_and_close_delimited_bodies_decode_as_they_arrive() {
        use std::io::Write as _;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(b"first,").unwrap();
        encoder.flush().unwrap();
        let first = std::mem::take(encoder.get_mut());
        encoder.write_all(b"second").unwrap();
        let rest = encoder.finish().unwrap();
        let length = first.len() + rest.len();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            read_head(&mut socket).await;
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n"
            );
            socket.write_all(head.as_bytes()).await.unwrap();
            socket.write_all(&first).await.unwrap();
            release_rx.await.unwrap();
            socket.write_all(&rest).await.unwrap();
            let (mut socket, _) = listener.accept().await.unwrap();
            read_head(&mut socket).await;
            socket
                .write_all(b"HTTP/1.0 200 OK\r\nContent-Type: text/plain\r\n\r\nuntil close")
                .await
                .unwrap();
        });
        let (_, body) = fetch_script_streaming(&request(address, "/gzip"))
            .await
            .unwrap();
        let mut body = body.expect("a length-delimited body streams");
        assert_eq!(body.next().await.unwrap().unwrap(), b"first,");
        release_tx.send(()).unwrap();
        let mut rest = Vec::new();
        while let Some(piece) = body.next().await.unwrap() {
            rest.extend(piece);
        }
        assert_eq!(rest, b"second");
        assert_eq!(body.encoded_size(), length);
        let (_, body) = fetch_script_streaming(&request(address, "/eof"))
            .await
            .unwrap();
        let mut body = body.expect("a close-delimited body streams");
        let mut text = Vec::new();
        while let Some(piece) = body.next().await.unwrap() {
            text.extend(piece);
        }
        assert_eq!(text, b"until close");
        server.await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn http1_dropping_a_streamed_body_closes_the_connection() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            read_head(&mut socket).await;
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\nmore\r\n")
                .await
                .unwrap();
            let mut byte = [0u8; 1];
            tokio::time::timeout(Duration::from_secs(5), socket.read(&mut byte))
                .await
                .expect("the client closes the connection")
                .unwrap()
        });
        let (_, body) = fetch_script_streaming(&request(address, "/abandoned"))
            .await
            .unwrap();
        let mut body = body.expect("the body streams");
        assert_eq!(body.next().await.unwrap().unwrap(), b"more");
        drop(body);
        assert_eq!(server.await.unwrap(), 0, "EOF after the body was dropped");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn redirects_are_read_whole_and_only_the_final_body_streams() {
        // Fetch #http-redirect-fetch consumes the redirect; its body never
        // reaches the consumer, and the final response's body streams.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            read_head(&mut socket).await;
            socket
                .write_all(
                    b"HTTP/1.1 302 Found\r\nLocation: /final\r\nContent-Length: 8\r\n\r\nredirect",
                )
                .await
                .unwrap();
            let head = read_head(&mut socket).await;
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nfinal")
                .await
                .unwrap();
            head
        });
        let (response, body) = fetch_script_streaming(&request(address, "/start"))
            .await
            .unwrap();
        assert_eq!(response.url.path(), "/final");
        let mut body = body.expect("the final body streams");
        assert_eq!(body.next().await.unwrap().unwrap(), b"final");
        assert!(body.next().await.unwrap().is_none());
        assert!(server.await.unwrap().starts_with("GET /final HTTP/1.1"));
    }
}
