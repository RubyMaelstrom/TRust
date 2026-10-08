//! Hand-rolled WebSocket client (RFC 6455) — the transport beneath socket.io.
//!
//! Open WebUI (and any websocket-enabled SvelteKit/socket.io app) streams its
//! real-time data — chat completion tokens, notifications — back over a
//! WebSocket, NOT the fetch. We provide only the TRANSPORT here; the page's own
//! bundled `socket.io-client` runs the Engine.IO/socket.io protocol on top of
//! it (handshake, heartbeat, packet framing), exactly as in a real browser.
//!
//! Consistent with the project's binding "hand-rolled HTTP/1.1, no reqwest/
//! hyper" ethos (and the hand-rolled telnet parser): the `Upgrade` handshake
//! reuses `http`'s dial (TCP + WebPKI TLS for `wss`), then this module does the
//! RFC 6455 framing itself — client masking, fragmentation reassembly, and the
//! ping/pong/close control frames. The opening response is validated against
//! RFC 6455 §4.1, including `Connection`, `Sec-WebSocket-Accept`, and negotiated
//! subprotocol. The handshake nonce and every masking key come from the
//! operating system's CSPRNG: RFC 6455 §5.3 requires unpredictable masks, the
//! defence against proxy-cache poisoning (§10.3). Messages may be compressed
//! with the RFC 7692 `permessage-deflate` extension, and a connection failed
//! for a protocol violation first sends the matching Close status (§7.1.7).
//!
//! The connection runs as one tokio task. It forwards inbound events to the
//! page actor over an mpsc channel (mapped to `PageCmd::Ws`, dispatched like a
//! click — no idle CPU, no busy poll) and accepts outbound messages on another.

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

/// An outbound message from page JS (`WebSocket.send`/`.close`).
#[derive(Debug)]
pub enum WsOut {
    Text(String),
    Binary(Vec<u8>),
    Close(u16, String),
}

/// An inbound event delivered to the page actor (becomes `PageCmd::Ws`).
#[derive(Debug)]
pub enum WsIn {
    Open {
        protocol: String,
        /// RFC 6455 §4.1 _Extensions In Use_: the server's
        /// `Sec-WebSocket-Extensions` value (WebSockets #dom-websocket-extensions).
        extensions: String,
    },
    Text(String),
    Binary(Vec<u8>),
    /// Application bytes from a successful send which reached the transport.
    Sent(usize),
    /// The connection ended (clean close with code/reason, or a transport drop
    /// reported as 1006). Always the final event for a socket.
    Closed {
        code: u16,
        reason: String,
        was_clean: bool,
        failed: bool,
    },
}

/// Append a diagnostic line to the file named by `TRUST_WS_DIAG` (reliable
/// across the WS task's lifetime, unlike stderr which races process shutdown).
fn wsdiag(msg: &str) {
    if let Some(path) = std::env::var_os("TRUST_WS_DIAG") {
        use std::io::Write as _;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            let _ = writeln!(f, "{msg}");
        }
    }
}

/// How many payload bytes to echo into the frame log (`TRUST_WS_DIAG_CAP`,
/// default 300). Bump it to inspect full socket.io packets (a chat-completion
/// frame is several KB) without recompiling.
fn diag_cap() -> usize {
    std::env::var("TRUST_WS_DIAG_CAP")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(300)
}

const OP_CONT: u8 = 0x0;
const OP_TEXT: u8 = 0x1;
const OP_BINARY: u8 = 0x2;
const OP_CLOSE: u8 = 0x8;
const OP_PING: u8 = 0x9;
const OP_PONG: u8 = 0xA;

/// Parse the host boundary's comma-separated subprotocol list. WebSockets §3.1 requires
/// every value to be a distinct RFC 6455 `token`; commas cannot occur inside a token.
pub(crate) fn parse_protocols(value: &str) -> Option<Vec<String>> {
    if value.is_empty() {
        return Some(Vec::new());
    }
    let mut protocols = Vec::new();
    for protocol in value.split(',') {
        if protocol.is_empty()
            || !protocol.bytes().all(|byte| {
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
            })
            || protocols.iter().any(|existing| existing == protocol)
        {
            return None;
        }
        protocols.push(protocol.to_string());
    }
    Some(protocols)
}

/// A control/data frame's payload is capped so a hostile server can't make us
/// buffer unboundedly (mirrors http's `MAX_BODY` spirit; a single chat token
/// frame is tiny).
const MAX_FRAME: usize = 16 * 1024 * 1024;

/// Open a WebSocket to `url` (`ws`/`wss`). The connection task forwards `WsIn`
/// events (tagged with `id`) to `events`; the returned sender takes `WsOut`
/// messages to write. Dropping the returned sender closes the socket.
pub fn connect(
    url: url::Url,
    protocols: Vec<String>,
    origin: String,
    cookie: Option<crate::http::Request>,
    handle: &tokio::runtime::Handle,
    id: usize,
    events: mpsc::Sender<(usize, WsIn)>,
) -> (mpsc::Sender<WsOut>, tokio::task::JoinHandle<()>) {
    let (out_tx, out_rx) = mpsc::channel::<WsOut>(64);
    let task = handle.spawn(async move {
        run_session(url, protocols, origin, cookie, id, events, out_rx).await;
    });
    (out_tx, task)
}

async fn run_session(
    url: url::Url,
    protocols: Vec<String>,
    origin: String,
    cookie: Option<crate::http::Request>,
    id: usize,
    events: mpsc::Sender<(usize, WsIn)>,
    mut out_rx: mpsc::Receiver<WsOut>,
) {
    let (io, negotiated) = match handshake(&url, &protocols, &origin, cookie.as_ref()).await {
        Ok(result) => result,
        Err(e) => {
            wsdiag(&format!("WS handshake FAILED for {url}: {e}"));
            // Transport failure: report an abnormal closure and stop.
            let _ = events
                .send((
                    id,
                    WsIn::Closed {
                        code: 1006,
                        reason: String::new(),
                        was_clean: false,
                        failed: true,
                    },
                ))
                .await;
            return;
        }
    };
    wsdiag(&format!("WS open ok for {url}"));
    let Negotiated {
        protocol,
        extensions,
        deflate,
    } = negotiated;
    if events
        .send((
            id,
            WsIn::Open {
                protocol,
                extensions,
            },
        ))
        .await
        .is_err()
    {
        return; // actor gone
    }
    // RFC 7692 §7.2: one decompressor for the server's messages and one
    // compressor for ours, each taking over its LZ77 window between
    // messages unless the agreed parameters forbid it.
    let mut inflater = deflate.map(Inflater::new);
    let mut deflater = deflate.and_then(Deflater::new);
    // Split so the read future borrows the read half while a control/outbound
    // write borrows the write half — no aliasing across the `select!`.
    let (mut rd, mut wr) = tokio::io::split(io);
    // Decode frames on a dedicated task. Recreating a `read_exact` future in
    // this writer/read `select!` can discard bytes already consumed when a
    // send wins the race (Tokio documents `read_exact` as cancellation-
    // unsafe). The reader owns its decoder state until a complete frame is
    // available, then forwards bounded messages to this task.
    let compressed_frames = deflate.is_some();
    let (frame_tx, mut frame_rx) = mpsc::channel::<Result<Frame, FrameError>>(16);
    let reader = tokio::spawn(async move {
        loop {
            let result = read_frame(&mut rd, compressed_frames).await;
            let failed = result.is_err();
            if frame_tx.send(result).await.is_err() || failed {
                break;
            }
        }
    });
    let mut frag: Vec<u8> = Vec::new();
    let mut frag_op: u8 = 0;
    let mut frag_compressed = false;
    let mut closing = false;
    let mut closed_reported = false;
    // RFC 6455 §7.1.7 _Fail the WebSocket Connection_: an endpoint with an
    // established connection SHOULD send a Close frame with the §7.4.1 status
    // for the problem, then closes; the page sees an unclean 1006 closure
    // (WebSockets #feedback-from-the-protocol).
    let mut failure: Option<FrameError> = None;
    loop {
        tokio::select! {
            frame = frame_rx.recv() => {
                let Some(frame) = frame else { break; };
                let Frame { fin, compressed, opcode, payload } = match frame {
                    Ok(f) => f,
                    Err(e) => {
                        failure = Some(e);
                        break;
                    }
                };
                wsdiag(&format!("WS frame op={opcode:#x} fin={fin} len={} head={:?}",
                    payload.len(), String::from_utf8_lossy(&payload[..payload.len().min(diag_cap())])));
                match opcode {
                    OP_PING => {
                        // Reply to a server ping with a pong (RFC 6455 §5.5.3).
                        let pong = write_frame(&mut wr, OP_PONG, false, &payload).await;
                        if pong.is_err() {
                            break;
                        }
                    }
                    OP_PONG => {}
                    OP_CLOSE => {
                        let (code, reason) = match parse_close(&payload) {
                            Ok(close) => close,
                            Err(e) => {
                                failure = Some(e);
                                break;
                            }
                        };
                        // RFC 6455 §7.1.2: answer a peer-initiated Close, then report a
                        // clean closure only after both directions of the handshake exist.
                        if !closing
                            && write_frame(&mut wr, OP_CLOSE, false, &payload).await.is_err()
                        {
                            break;
                        }
                        let _ = events.send((id, WsIn::Closed {
                            code,
                            reason,
                            was_clean: true,
                            failed: false,
                        })).await;
                        closed_reported = true;
                        break;
                    }
                    OP_TEXT | OP_BINARY => {
                        if frag_op != 0 {
                            failure = Some(FrameError::protocol("new WebSocket message inside a fragmented one"));
                            break;
                        }
                        if fin {
                            match message(&mut inflater, compressed, payload) {
                                Ok(payload) => {
                                    if let Err(e) = deliver(&events, id, opcode, payload).await {
                                        failure = e;
                                        break;
                                    }
                                }
                                Err(e) => {
                                    failure = Some(e);
                                    break;
                                }
                            }
                        } else {
                            frag_op = opcode;
                            frag_compressed = compressed;
                            frag = payload;
                        }
                    }
                    OP_CONT => {
                        if frag_op == 0 {
                            failure = Some(FrameError::protocol("WebSocket continuation without a message"));
                            break;
                        }
                        if frag.len() + payload.len() > MAX_FRAME {
                            failure = Some(FrameError::too_big());
                            break;
                        }
                        frag.extend_from_slice(&payload);
                        if fin {
                            let msg = std::mem::take(&mut frag);
                            let op = frag_op;
                            frag_op = 0;
                            match message(&mut inflater, frag_compressed, msg) {
                                Ok(payload) => {
                                    if let Err(e) = deliver(&events, id, op, payload).await {
                                        failure = e;
                                        break;
                                    }
                                }
                                Err(e) => {
                                    failure = Some(e);
                                    break;
                                }
                            }
                        }
                    }
                    _ => unreachable!("read_frame rejects reserved opcodes"),
                }
            }
            out = out_rx.recv(), if !closing => {
                match out {
                    Some(WsOut::Text(s)) => {
                        let len = s.len();
                        if send_message(&mut wr, &mut deflater, OP_TEXT, s.as_bytes()).await.is_err() { break; }
                        if events.send((id, WsIn::Sent(len))).await.is_err() { break; }
                    }
                    Some(WsOut::Binary(b)) => {
                        let len = b.len();
                        if send_message(&mut wr, &mut deflater, OP_BINARY, &b).await.is_err() { break; }
                        if events.send((id, WsIn::Sent(len))).await.is_err() { break; }
                    }
                    Some(WsOut::Close(code, reason)) => {
                        if write_frame(&mut wr, OP_CLOSE, false, &close_payload(code, &reason)).await.is_err() {
                            let _ = events.send((id, WsIn::Closed {
                                code: 1006,
                                reason: String::new(),
                                was_clean: false,
                                failed: true,
                            })).await;
                            closed_reported = true;
                            break;
                        }
                        closing = true;
                    }
                    None => {
                        // WebSockets §7: a collected live socket starts a 1001 closing
                        // handshake. Navigation cancellation owns the task if the peer stalls.
                        if write_frame(&mut wr, OP_CLOSE, false, &close_payload(1001, "")).await.is_err() {
                            break;
                        }
                        closing = true;
                    }
                }
            }
        }
    }
    reader.abort();
    if let Some(failure) = &failure {
        wsdiag(&format!("WS failed: {}", failure.message));
        if let Some(code) = failure.close_code.filter(|_| !closing) {
            let _ = write_frame(&mut wr, OP_CLOSE, false, &close_payload(code, "")).await;
        }
    }
    if !closed_reported {
        let _ = events
            .send((
                id,
                WsIn::Closed {
                    code: 1006,
                    reason: String::new(),
                    was_clean: false,
                    failed: true,
                },
            ))
            .await;
    }
}

/// A complete received message's application data: RFC 7692 §6.2 decompresses
/// a message whose first frame had the Per-Message Compressed bit.
fn message(
    inflater: &mut Option<Inflater>,
    compressed: bool,
    payload: Vec<u8>,
) -> Result<Vec<u8>, FrameError> {
    if !compressed {
        return Ok(payload);
    }
    match inflater {
        Some(inflater) => inflater.inflate(&payload, MAX_FRAME),
        None => Err(FrameError::protocol(
            "compressed message without permessage-deflate",
        )),
    }
}

/// Send one data message: RFC 7692 §6.1 compresses it when the agreed
/// parameters allow this client's compressor.
async fn send_message(
    io: &mut tokio::io::WriteHalf<crate::http::WsTransport>,
    deflater: &mut Option<Deflater>,
    opcode: u8,
    payload: &[u8],
) -> Result<(), std::io::Error> {
    match deflater {
        Some(deflater) => {
            let compressed = deflater.deflate(payload)?;
            write_frame(io, opcode, true, &compressed).await
        }
        None => write_frame(io, opcode, false, payload).await,
    }
}

/// Deliver one message. `Err(Some(_))` fails the connection: RFC 6455 §8.1
/// requires a text message to be valid UTF-8 (status 1007, §7.4.1).
/// `Err(None)` means the page actor is gone.
async fn deliver(
    events: &mpsc::Sender<(usize, WsIn)>,
    id: usize,
    opcode: u8,
    payload: Vec<u8>,
) -> Result<(), Option<FrameError>> {
    let ev = if opcode == OP_TEXT {
        WsIn::Text(String::from_utf8(payload).map_err(|_| {
            Some(FrameError::invalid_data(
                "invalid UTF-8 in a WebSocket text message",
            ))
        })?)
    } else {
        WsIn::Binary(payload)
    };
    events.send((id, ev)).await.map_err(|_| None)
}

/// What the opening handshake established (RFC 6455 §4.1).
struct Negotiated {
    protocol: String,
    extensions: String,
    deflate: Option<DeflateParams>,
}

/// The RFC 6455 opening handshake over a fresh dial. Returns the live transport
/// positioned right after the `\r\n\r\n` of the `101` response.
async fn handshake(
    url: &url::Url,
    protocols: &[String],
    origin: &str,
    cookie: Option<&crate::http::Request>,
) -> Result<(crate::http::WsTransport, Negotiated), String> {
    let host = url.host_str().ok_or("no host")?.to_string();
    let secure = url.scheme() == "wss";
    let port = url.port().unwrap_or(if secure { 443 } else { 80 });
    let mut io = crate::http::ws_dial(secure, &host, port).await?;

    let path = {
        let p = url.path();
        match url.query() {
            Some(q) => format!("{p}?{q}"),
            None => {
                if p.is_empty() {
                    String::from("/")
                } else {
                    p.to_string()
                }
            }
        }
    };
    let host_hdr = if url.port().is_some() {
        format!("{host}:{port}")
    } else {
        host.clone()
    };
    let key = ws_key().map_err(|e| e.to_string())?;
    let mut req = format!(
        "GET {path} HTTP/1.1\r\n\
         Host: {host_hdr}\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Key: {key}\r\n\
         Sec-WebSocket-Version: 13\r\n\
         Sec-WebSocket-Extensions: {DEFLATE_OFFER}\r\n\
         User-Agent: {ua}\r\n\
         Accept-Language: {accept_language}\r\n\
         Origin: {origin}\r\n",
        ua = crate::http::user_agent(),
        accept_language = crate::locale::ACCEPT_LANGUAGE,
    );
    if crate::http::GLOBAL_PRIVACY_CONTROL {
        req.push_str("Sec-GPC: 1\r\n");
    }
    // The opening handshake is a fetch (WebSockets #concept-websocket-
    // establish), so it carries the same low-entropy client hints.
    for (name, value) in crate::http::client_hints(url, None) {
        req.push_str(&format!("{name}: {value}\r\n"));
    }
    if let Some(request) = cookie {
        let cookies = crate::http::request_cookies(request);
        if !cookies.is_empty() {
            req.push_str(&format!("Cookie: {cookies}\r\n"));
        }
    }
    if !protocols.is_empty() {
        req.push_str(&format!(
            "Sec-WebSocket-Protocol: {}\r\n",
            protocols.join(", ")
        ));
    }
    req.push_str("\r\n");
    io.write_all(req.as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    io.flush().await.map_err(|e| e.to_string())?;

    // Read the response headers up to the blank line. Anything after is the
    // first frame's bytes — the BufReader keeps them for `read_frame`.
    let head = read_until_headers_end(&mut io).await?;
    wsdiag("WS handshake response received");
    if let Some(request) = cookie {
        for line in head.lines().skip(1) {
            if let Some((name, value)) = line.split_once(':')
                && name.eq_ignore_ascii_case("set-cookie")
            {
                crate::http::response_cookie(request, value.trim());
            }
        }
    }
    let status_line = head.lines().next().unwrap_or("");
    if status_line.split_ascii_whitespace().nth(1) != Some("101") {
        return Err(format!("not a websocket upgrade: {status_line}"));
    }
    let headers = parse_response_headers(&head);
    if !header_has_token(&headers, "upgrade", "websocket") {
        return Err(String::from("missing Upgrade: websocket"));
    }
    if !header_has_token(&headers, "connection", "upgrade") {
        return Err(String::from("missing Connection: Upgrade"));
    }
    let expected_accept = websocket_accept(&key);
    if header_value(&headers, "sec-websocket-accept") != Some(expected_accept.as_str()) {
        return Err(String::from("invalid Sec-WebSocket-Accept"));
    }
    // RFC 6455 §4.1 (client step 5) and RFC 7692 §5/§7: every extension the
    // server selected must be one this client offered, with agreed
    // parameters it supports; anything else fails the connection.
    let extension_values: Vec<&str> = headers
        .iter()
        .filter(|(name, _)| name == "sec-websocket-extensions")
        .map(|(_, value)| value.as_str())
        .collect();
    let deflate = parse_extensions_response(&extension_values)?;
    let extensions = extension_values.join(", ");
    let protocol_headers = headers
        .iter()
        .filter(|(name, _)| name == "sec-websocket-protocol")
        .collect::<Vec<_>>();
    let protocol = match (protocols.is_empty(), protocol_headers.as_slice()) {
        (true, []) => String::new(),
        (false, [(_, selected)])
            if !selected.is_empty() && protocols.iter().any(|offered| offered == selected) =>
        {
            selected.clone()
        }
        _ => return Err(String::from("invalid Sec-WebSocket-Protocol")),
    };
    Ok((
        io,
        Negotiated {
            protocol,
            extensions,
            deflate,
        },
    ))
}

/// Read the response head (through `\r\n\r\n`) one byte at a time so we never
/// consume into the first frame. (Handshake responses are tiny.)
async fn read_until_headers_end(io: &mut crate::http::WsTransport) -> Result<String, String> {
    let mut buf = Vec::with_capacity(256);
    let mut b = [0u8; 1];
    loop {
        let n = io.read(&mut b).await.map_err(|e| e.to_string())?;
        if n == 0 {
            return Err(String::from("connection closed during handshake"));
        }
        buf.push(b[0]);
        if buf.ends_with(b"\r\n\r\n") {
            break;
        }
        if buf.len() > 16 * 1024 {
            return Err(String::from("handshake response too large"));
        }
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

fn parse_response_headers(head: &str) -> Vec<(String, String)> {
    head.lines()
        .skip(1)
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            Some((name.trim().to_ascii_lowercase(), value.trim().to_string()))
        })
        .collect()
}

fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    let mut values = headers
        .iter()
        .filter(|(candidate, _)| candidate.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str());
    let value = values.next()?;
    values.next().is_none().then_some(value)
}

fn header_has_token(headers: &[(String, String)], name: &str, token: &str) -> bool {
    headers
        .iter()
        .filter(|(candidate, _)| candidate.eq_ignore_ascii_case(name))
        .flat_map(|(_, value)| value.split(','))
        .any(|candidate| candidate.trim().eq_ignore_ascii_case(token))
}

/// RFC 6455 §4.1's server proof: base64(SHA-1(key || WebSocket GUID)). SHA-1 is used
/// here solely as a fixed protocol transform; no collision-resistance property is relied on.
pub(crate) fn websocket_accept(key: &str) -> String {
    let mut challenge = Vec::with_capacity(key.len() + 36);
    challenge.extend_from_slice(key.as_bytes());
    challenge.extend_from_slice(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
    base64(&sha1(&challenge))
}

fn sha1(input: &[u8]) -> [u8; 20] {
    let bit_len = (input.len() as u64).wrapping_mul(8);
    let mut padded = Vec::with_capacity((input.len() + 72) & !63);
    padded.extend_from_slice(input);
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());

    let mut state = [
        0x6745_2301u32,
        0xefcd_ab89,
        0x98ba_dcfe,
        0x1032_5476,
        0xc3d2_e1f0,
    ];
    for block in padded.as_chunks::<64>().0 {
        let mut words = [0u32; 80];
        for (word, bytes) in words[..16].iter_mut().zip(block.as_chunks::<4>().0) {
            *word = u32::from_be_bytes(*bytes);
        }
        for index in 16..80 {
            words[index] =
                (words[index - 3] ^ words[index - 8] ^ words[index - 14] ^ words[index - 16])
                    .rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = state;
        for (index, word) in words.into_iter().enumerate() {
            let (function, constant) = match index {
                0..=19 => ((b & c) | ((!b) & d), 0x5a82_7999),
                20..=39 => (b ^ c ^ d, 0x6ed9_eba1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8f1b_bcdc),
                _ => (b ^ c ^ d, 0xca62_c1d6),
            };
            let next = a
                .rotate_left(5)
                .wrapping_add(function)
                .wrapping_add(e)
                .wrapping_add(constant)
                .wrapping_add(word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = next;
        }
        for (slot, value) in state.iter_mut().zip([a, b, c, d, e]) {
            *slot = slot.wrapping_add(value);
        }
    }
    let mut digest = [0u8; 20];
    for (bytes, value) in digest.as_chunks_mut::<4>().0.iter_mut().zip(state) {
        bytes.copy_from_slice(&value.to_be_bytes());
    }
    digest
}

/// Read one server frame as `(fin, opcode, payload)`, rejecting protocol-invalid RSV bits,
/// masking, reserved opcodes, fragmented controls, and oversized/non-minimal length forms.
/// One received frame (RFC 6455 §5.2).
struct Frame {
    fin: bool,
    /// RSV1, the RFC 7692 §6 Per-Message Compressed bit.
    compressed: bool,
    opcode: u8,
    payload: Vec<u8>,
}

/// Why a connection fails, with the RFC 6455 §7.4.1 status code a Close frame
/// reports to the peer. Transport errors have none: no Close can be sent.
#[derive(Debug)]
struct FrameError {
    message: String,
    close_code: Option<u16>,
}

impl FrameError {
    /// 1002: a protocol error.
    fn protocol(message: &str) -> Self {
        Self {
            message: message.into(),
            close_code: Some(1002),
        }
    }

    /// 1007: data inconsistent with the message type.
    fn invalid_data(message: &str) -> Self {
        Self {
            message: message.into(),
            close_code: Some(1007),
        }
    }

    /// 1009: a message too big to process.
    fn too_big() -> Self {
        Self {
            message: String::from("WebSocket message too large"),
            close_code: Some(1009),
        }
    }

    fn transport(error: std::io::Error) -> Self {
        Self {
            message: error.to_string(),
            close_code: None,
        }
    }
}

/// Read one frame. `compression` is whether `permessage-deflate` was
/// negotiated, which gives RSV1 its RFC 7692 §6 meaning.
async fn read_frame(
    io: &mut tokio::io::ReadHalf<crate::http::WsTransport>,
    compression: bool,
) -> Result<Frame, FrameError> {
    let mut hdr = [0u8; 2];
    io.read_exact(&mut hdr)
        .await
        .map_err(FrameError::transport)?;
    let fin = hdr[0] & 0x80 != 0;
    let compressed = hdr[0] & 0x40 != 0;
    let opcode = hdr[0] & 0x0F;
    // RFC 6455 §5.2: a nonzero RSV bit without an extension defining it.
    if hdr[0] & 0x30 != 0 || (compressed && !compression) {
        return Err(FrameError::protocol(
            "unnegotiated WebSocket extension bits",
        ));
    }
    if !matches!(
        opcode,
        OP_CONT | OP_TEXT | OP_BINARY | OP_CLOSE | OP_PING | OP_PONG
    ) {
        return Err(FrameError::protocol("reserved WebSocket opcode"));
    }
    // RFC 7692 §6.1: only the first frame of a data message is marked.
    if compressed && !matches!(opcode, OP_TEXT | OP_BINARY) {
        return Err(FrameError::protocol(
            "Per-Message Compressed bit on a control or continuation frame",
        ));
    }
    if hdr[1] & 0x80 != 0 {
        return Err(FrameError::protocol("masked server WebSocket frame"));
    }
    let length_code = hdr[1] & 0x7F;
    if opcode >= OP_CLOSE && (!fin || length_code > 125) {
        return Err(FrameError::protocol("invalid WebSocket control frame"));
    }
    let mut len = usize::from(length_code);
    if length_code == 126 {
        let mut ext = [0u8; 2];
        io.read_exact(&mut ext)
            .await
            .map_err(FrameError::transport)?;
        len = u16::from_be_bytes(ext) as usize;
        if len < 126 {
            return Err(FrameError::protocol("non-minimal WebSocket frame length"));
        }
    } else if length_code == 127 {
        let mut ext = [0u8; 8];
        io.read_exact(&mut ext)
            .await
            .map_err(FrameError::transport)?;
        if ext[0] & 0x80 != 0 {
            return Err(FrameError::protocol(
                "invalid 63-bit WebSocket frame length",
            ));
        }
        let extended = u64::from_be_bytes(ext);
        if extended <= u64::from(u16::MAX) {
            return Err(FrameError::protocol("non-minimal WebSocket frame length"));
        }
        len = usize::try_from(extended).map_err(|_| FrameError::too_big())?;
    }
    if len > MAX_FRAME {
        return Err(FrameError::too_big());
    }
    let mut payload = vec![0u8; len];
    if len > 0 {
        io.read_exact(&mut payload)
            .await
            .map_err(FrameError::transport)?;
    }
    Ok(Frame {
        fin,
        compressed,
        opcode,
        payload,
    })
}

/// Write one client frame (always masked, always FIN — we don't fragment what
/// we send; messages are whole `send()` calls).
async fn write_frame(
    io: &mut tokio::io::WriteHalf<crate::http::WsTransport>,
    opcode: u8,
    compressed: bool,
    payload: &[u8],
) -> Result<(), std::io::Error> {
    let mut frame = Vec::with_capacity(payload.len() + 14);
    // FIN + RSV1 (RFC 7692 Per-Message Compressed) + opcode.
    frame.push(0x80 | if compressed { 0x40 } else { 0 } | opcode);
    let mask_bit = 0x80u8;
    let len = payload.len();
    if len < 126 {
        frame.push(mask_bit | len as u8);
    } else if len <= 0xFFFF {
        frame.push(mask_bit | 126);
        frame.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        frame.push(mask_bit | 127);
        frame.extend_from_slice(&(len as u64).to_be_bytes());
    }
    wsdiag(&format!(
        "WS send op={opcode:#x} len={len} head={:?}",
        String::from_utf8_lossy(&payload[..payload.len().min(diag_cap())])
    ));
    let mask = mask_key()?;
    frame.extend_from_slice(&mask);
    let base = frame.len();
    frame.extend_from_slice(payload);
    for (i, byte) in frame[base..].iter_mut().enumerate() {
        *byte ^= mask[i & 3];
    }
    io.write_all(&frame).await?;
    io.flush().await
}

fn close_payload(code: u16, reason: &str) -> Vec<u8> {
    if code == 0 {
        return Vec::new();
    }
    let mut p = Vec::with_capacity(2 + reason.len());
    p.extend_from_slice(&code.to_be_bytes());
    p.extend_from_slice(reason.as_bytes());
    p
}

fn parse_close(payload: &[u8]) -> Result<(u16, String), FrameError> {
    if payload.is_empty() {
        return Ok((1005, String::new()));
    }
    if payload.len() == 1 {
        return Err(FrameError::protocol("one-byte WebSocket Close payload"));
    }
    let code = u16::from_be_bytes([payload[0], payload[1]]);
    if !(1000..=4999).contains(&code) || matches!(code, 1004..=1006 | 1015) {
        return Err(FrameError::protocol("invalid WebSocket Close status code"));
    }
    let reason = std::str::from_utf8(&payload[2..])
        .map_err(|_| FrameError::invalid_data("invalid UTF-8 WebSocket Close reason"))?
        .to_string();
    Ok((code, reason))
}

/// `N` bytes from the operating system's CSPRNG.
fn random_bytes<const N: usize>() -> Result<[u8; N], std::io::Error> {
    let mut bytes = [0u8; N];
    getrandom::fill(&mut bytes).map_err(|error| std::io::Error::other(error.to_string()))?;
    Ok(bytes)
}

/// RFC 6455 §4.1 (client step 7): `Sec-WebSocket-Key` is "a nonce consisting
/// of a randomly selected 16-byte value", base64-encoded.
fn ws_key() -> Result<String, std::io::Error> {
    Ok(base64(&random_bytes::<16>()?))
}

/// RFC 6455 §5.3: a fresh masking key per frame, "derived from a strong
/// source of entropy" so that no peer can predict the next one.
fn mask_key() -> Result<[u8; 4], std::io::Error> {
    random_bytes::<4>()
}

/// RFC 7692 §5/§7.1.2.2: this client's `permessage-deflate` offer. It
/// declares support for `client_max_window_bits` in the response, as browsers
/// do, and accepts any server window (absent `server_max_window_bits`).
const DEFLATE_OFFER: &str = "permessage-deflate; client_max_window_bits";

/// RFC 7692 §7.1: the "agreed parameters" of an accepted `permessage-deflate`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct DeflateParams {
    server_no_context_takeover: bool,
    client_no_context_takeover: bool,
    server_max_window_bits: Option<u8>,
    client_max_window_bits: Option<u8>,
}

/// Validate the server's `Sec-WebSocket-Extensions` values (RFC 6455 §9.1
/// grammar) against this client's offer. `Ok(None)`: no extension in use.
fn parse_extensions_response(values: &[&str]) -> Result<Option<DeflateParams>, String> {
    let mut deflate: Option<DeflateParams> = None;
    for value in values {
        for element in split_unquoted(value, ',') {
            let mut parts = split_unquoted(element, ';').into_iter();
            let name = parts.next().unwrap_or_default().trim();
            if !is_token(name) {
                return Err(format!("malformed Sec-WebSocket-Extensions: {value}"));
            }
            if !name.eq_ignore_ascii_case("permessage-deflate") {
                return Err(format!(
                    "server selected an unrequested WebSocket extension: {name}"
                ));
            }
            if deflate.is_some() {
                return Err(String::from("server selected permessage-deflate twice"));
            }
            let mut params = DeflateParams::default();
            let mut seen: Vec<String> = Vec::new();
            for param in parts {
                let (key, value) = match param.split_once('=') {
                    Some((key, value)) => (key.trim(), Some(param_value(value.trim())?)),
                    None => (param.trim(), None),
                };
                let key = key.to_ascii_lowercase();
                if !is_token(&key) {
                    return Err(format!("malformed permessage-deflate parameter: {param}"));
                }
                // RFC 7692 §7: a repeated parameter fails the connection.
                if seen.contains(&key) {
                    return Err(format!("repeated permessage-deflate parameter: {key}"));
                }
                seen.push(key.clone());
                match (key.as_str(), value) {
                    ("server_no_context_takeover", None) => {
                        params.server_no_context_takeover = true
                    }
                    ("client_no_context_takeover", None) => {
                        params.client_no_context_takeover = true
                    }
                    ("server_max_window_bits", Some(bits)) => {
                        params.server_max_window_bits = Some(window_bits(&bits)?);
                    }
                    // §7.1.2.2: allowed in the response because the offer
                    // included `client_max_window_bits`; it needs a value.
                    ("client_max_window_bits", Some(bits)) => {
                        params.client_max_window_bits = Some(window_bits(&bits)?);
                    }
                    _ => return Err(format!("unsupported permessage-deflate parameter: {param}")),
                }
            }
            deflate = Some(params);
        }
    }
    Ok(deflate)
}

/// §7.1.2: "a decimal integer value without leading zeroes between 8 to 15".
fn window_bits(value: &str) -> Result<u8, String> {
    (value.len() <= 2 && !value.starts_with('0') && value.bytes().all(|b| b.is_ascii_digit()))
        .then(|| value.parse::<u8>().ok())
        .flatten()
        .filter(|bits| (8..=15).contains(bits))
        .ok_or_else(|| format!("invalid permessage-deflate window bits: {value}"))
}

/// RFC 6455 §9.1: a parameter value is a token, or a quoted-string whose
/// unescaped value is a token.
fn param_value(value: &str) -> Result<String, String> {
    let unquoted = match value.strip_prefix('"').and_then(|v| v.strip_suffix('"')) {
        Some(inner) => {
            let mut out = String::new();
            let mut chars = inner.chars();
            while let Some(c) = chars.next() {
                out.push(if c == '\\' {
                    chars.next().ok_or("dangling quoted-pair")?
                } else {
                    c
                });
            }
            out
        }
        None => value.to_string(),
    };
    if is_token(&unquoted) {
        Ok(unquoted)
    } else {
        Err(format!("invalid extension parameter value: {value}"))
    }
}

/// RFC 7230 `token`.
fn is_token(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
}

/// Split on `separator` outside quoted strings.
fn split_unquoted(value: &str, separator: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let (mut start, mut quoted, mut escaped) = (0, false, false);
    for (index, c) in value.char_indices() {
        if escaped {
            escaped = false;
        } else if quoted && c == '\\' {
            escaped = true;
        } else if c == '"' {
            quoted = !quoted;
        } else if c == separator && !quoted {
            parts.push(&value[start..index]);
            start = index + c.len_utf8();
        }
    }
    parts.push(&value[start..]);
    parts
}

/// RFC 7692 §7.2.2 decompression of the server's messages.
struct Inflater {
    stream: flate2::Decompress,
    /// `server_no_context_takeover`: each message starts with an empty window.
    reset_each: bool,
}

impl Inflater {
    fn new(params: DeflateParams) -> Self {
        Self {
            // A raw DEFLATE stream with the full 32,768-byte window, which
            // any agreed `server_max_window_bits` only narrows.
            stream: flate2::Decompress::new(false),
            reset_each: params.server_no_context_takeover,
        }
    }

    /// Append 0x00 0x00 0xff 0xff to the payload and inflate it, bounding the
    /// output by `limit` (1009 beyond it; a corrupt stream is 1002).
    fn inflate(&mut self, payload: &[u8], limit: usize) -> Result<Vec<u8>, FrameError> {
        let mut input = Vec::with_capacity(payload.len() + 4);
        input.extend_from_slice(payload);
        input.extend_from_slice(&[0x00, 0x00, 0xff, 0xff]);
        let mut output = Vec::new();
        let mut consumed = 0;
        loop {
            if output.len() > limit {
                return Err(FrameError::too_big());
            }
            if output.capacity() - output.len() < 4096 {
                output.reserve((payload.len() * 4).clamp(4096, 1 << 20));
            }
            let before_in = self.stream.total_in();
            let (start, before_out) = (consumed, output.len());
            let status = self
                .stream
                .decompress_vec(
                    &input[consumed..],
                    &mut output,
                    flate2::FlushDecompress::Sync,
                )
                .map_err(|_| FrameError::protocol("corrupt permessage-deflate data"))?;
            consumed += (self.stream.total_in() - before_in) as usize;
            if status == flate2::Status::StreamEnd {
                // A BFINAL block ends one DEFLATE stream; any following
                // blocks (§7.2.3.4/§7.2.3.5) start the next.
                self.stream.reset(false);
                if consumed == input.len() {
                    break;
                }
                continue;
            }
            // All input consumed with output room to spare: complete.
            if consumed == input.len() && output.len() < output.capacity() {
                break;
            }
            if consumed == start && output.len() == before_out {
                return Err(FrameError::protocol("stalled permessage-deflate data"));
            }
        }
        if output.len() > limit {
            return Err(FrameError::too_big());
        }
        if self.reset_each {
            self.stream.reset(false);
        }
        Ok(output)
    }
}

/// RFC 7692 §7.2.1 compression of this client's messages.
struct Deflater {
    stream: flate2::Compress,
    /// `client_no_context_takeover`: each message starts with an empty window.
    reset_each: bool,
}

impl Deflater {
    /// The DEFLATE encoder always uses a 32,768-byte window, so an agreed
    /// `client_max_window_bits` below 15 forbids compressing: this client
    /// then sends uncompressed messages, which §6.1 always permits.
    fn new(params: DeflateParams) -> Option<Self> {
        params
            .client_max_window_bits
            .is_none_or(|bits| bits == 15)
            .then(|| Self {
                stream: flate2::Compress::new(flate2::Compression::default(), false),
                reset_each: params.client_no_context_takeover,
            })
    }

    /// Compress with a sync flush, whose empty stored block's trailing
    /// 0x00 0x00 0xff 0xff is then removed (§7.2.1 steps 1-3).
    fn deflate(&mut self, payload: &[u8]) -> Result<Vec<u8>, std::io::Error> {
        let mut output = Vec::with_capacity(payload.len() / 2 + 64);
        let mut consumed = 0;
        loop {
            if output.capacity() - output.len() < 64 {
                output.reserve(output.capacity().max(1024));
            }
            let before_in = self.stream.total_in();
            self.stream
                .compress_vec(
                    &payload[consumed..],
                    &mut output,
                    flate2::FlushCompress::Sync,
                )
                .map_err(std::io::Error::other)?;
            consumed += (self.stream.total_in() - before_in) as usize;
            if consumed == payload.len() && output.len() < output.capacity() {
                break;
            }
        }
        if output.ends_with(&[0x00, 0x00, 0xff, 0xff]) {
            output.truncate(output.len() - 4);
        }
        if self.reset_each {
            self.stream.reset();
        }
        Ok(output)
    }
}

/// Minimal standard-alphabet base64 (no line wrapping) — for the handshake key.
fn base64(input: &[u8]) -> String {
    const ALPHA: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHA[(n >> 18) as usize & 63] as char);
        out.push(ALPHA[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHA[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHA[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        // A 16-byte key is always 24 base64 chars ending in "==".
        let key = ws_key().unwrap();
        assert_eq!(key.len(), 24);
        assert!(key.ends_with("=="));
    }

    #[test]
    fn websocket_accept_matches_rfc_6455_example() {
        assert_eq!(
            websocket_accept("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn subprotocols_are_distinct_rfc_tokens() {
        assert_eq!(
            parse_protocols("chat,graphql-ws"),
            Some(vec![String::from("chat"), String::from("graphql-ws")])
        );
        assert_eq!(parse_protocols(""), Some(Vec::new()));
        assert_eq!(parse_protocols("chat,chat"), None);
        assert_eq!(parse_protocols("chat, bad"), None);
        assert_eq!(parse_protocols("chat\r\ninjected"), None);
    }

    #[test]
    fn close_payload_roundtrips() {
        let p = close_payload(1000, "bye");
        let (code, reason) = parse_close(&p).unwrap();
        assert_eq!(code, 1000);
        assert_eq!(reason, "bye");
        assert_eq!(parse_close(&[]).unwrap().0, 1005);
        assert!(parse_close(&[0]).is_err());
        assert!(parse_close(&1006u16.to_be_bytes()).is_err());
        assert!(parse_close(&[0x03, 0xe8, 0xff]).is_err());
    }

    /// RFC 7692 §7 response validation against this client's offer.
    #[test]
    fn permessage_deflate_responses_are_validated() {
        assert_eq!(parse_extensions_response(&[]), Ok(None));
        assert_eq!(
            parse_extensions_response(&["permessage-deflate"]),
            Ok(Some(DeflateParams::default()))
        );
        assert_eq!(
            parse_extensions_response(&["permessage-deflate; server_no_context_takeover; \
                 client_no_context_takeover; server_max_window_bits=10; \
                 client_max_window_bits=\"12\""]),
            Ok(Some(DeflateParams {
                server_no_context_takeover: true,
                client_no_context_takeover: true,
                server_max_window_bits: Some(10),
                client_max_window_bits: Some(12),
            }))
        );
        for invalid in [
            "x-webkit-deflate-frame",
            "permessage-deflate, permessage-deflate",
            "permessage-deflate; server_no_context_takeover; server_no_context_takeover",
            "permessage-deflate; server_max_window_bits=7",
            "permessage-deflate; server_max_window_bits=16",
            "permessage-deflate; server_max_window_bits=010",
            "permessage-deflate; server_max_window_bits",
            "permessage-deflate; client_max_window_bits",
            "permessage-deflate; server_no_context_takeover=1",
            "permessage-deflate; unknown_param",
            "permessage-deflate; client_max_window_bits=\"1 2\"",
        ] {
            assert!(parse_extensions_response(&[invalid]).is_err(), "{invalid}");
        }
        // A second header line accepting the extension again is a repeat.
        assert!(parse_extensions_response(&["permessage-deflate", "permessage-deflate"]).is_err());
    }

    /// RFC 7692 §7.2.3: the specification's example payloads.
    #[test]
    fn permessage_deflate_decompresses_the_rfc_examples() {
        let mut inflater = Inflater::new(DeflateParams::default());
        let hello = [0xf2, 0x48, 0xcd, 0xc9, 0xc9, 0x07, 0x00];
        assert_eq!(inflater.inflate(&hello, 1024).unwrap(), b"Hello");
        // §7.2.3.2: the second message refers to the shared LZ77 window.
        assert_eq!(
            inflater
                .inflate(&[0xf2, 0x00, 0x11, 0x00, 0x00], 1024)
                .unwrap(),
            b"Hello"
        );
        let fresh = || Inflater::new(DeflateParams::default());
        // §7.2.3.3 stored block, §7.2.3.4 BFINAL, §7.2.3.5 two blocks,
        // §7.2.3.6 empty.
        for (payload, expected) in [
            (
                &[
                    0x00, 0x05, 0x00, 0xfa, 0xff, 0x48, 0x65, 0x6c, 0x6c, 0x6f, 0x00,
                ][..],
                &b"Hello"[..],
            ),
            (&[0xf3, 0x48, 0xcd, 0xc9, 0xc9, 0x07, 0x00, 0x00], b"Hello"),
            (
                &[
                    0xf2, 0x48, 0x05, 0x00, 0x00, 0x00, 0xff, 0xff, 0xca, 0xc9, 0xc9, 0x07, 0x00,
                ],
                b"Hello",
            ),
            (&[0x00], b""),
        ] {
            assert_eq!(fresh().inflate(payload, 1024).unwrap(), expected);
        }
        // Output beyond the message limit is 1009; garbage is 1002.
        assert_eq!(
            fresh().inflate(&hello, 3).unwrap_err().close_code,
            Some(1009)
        );
        assert_eq!(
            fresh()
                .inflate(&[0xff, 0xff, 0xff], 1024)
                .unwrap_err()
                .close_code,
            Some(1002)
        );
    }

    #[test]
    fn permessage_deflate_compression_round_trips_with_context_takeover() {
        let text = b"The quick brown fox jumps over the lazy dog. ".repeat(20);
        for takeover in [true, false] {
            let params = DeflateParams {
                client_no_context_takeover: !takeover,
                server_no_context_takeover: !takeover,
                ..DeflateParams::default()
            };
            let mut deflater = Deflater::new(params).unwrap();
            let mut inflater = Inflater::new(params);
            let first = deflater.deflate(&text).unwrap();
            assert!(first.len() < text.len() / 4);
            assert!(!first.ends_with(&[0x00, 0x00, 0xff, 0xff]));
            let second = deflater.deflate(&text).unwrap();
            // Taking over the window makes the repeat nearly free.
            assert_eq!(second.len() < first.len() / 2, takeover);
            assert_eq!(inflater.inflate(&first, 1 << 20).unwrap(), text);
            assert_eq!(inflater.inflate(&second, 1 << 20).unwrap(), text);
            assert_eq!(
                inflater
                    .inflate(&deflater.deflate(b"").unwrap(), 16)
                    .unwrap(),
                b""
            );
        }
        // A window the encoder cannot honor disables compression.
        for bits in [8, 14] {
            assert!(
                Deflater::new(DeflateParams {
                    client_max_window_bits: Some(bits),
                    ..DeflateParams::default()
                })
                .is_none()
            );
        }
        assert!(
            Deflater::new(DeflateParams {
                client_max_window_bits: Some(15),
                ..DeflateParams::default()
            })
            .is_some()
        );
    }

    /// Serve one WebSocket connection: answer the handshake with `extensions`
    /// and send `frames`; return what the client sent afterwards.
    async fn serve(
        extensions: &'static str,
        frames: Vec<Vec<u8>>,
    ) -> (u16, tokio::task::JoinHandle<(String, Vec<u8>)>) {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buf = [0u8; 4096];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = socket.read(&mut buf).await.unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buf[..read]);
            }
            let head = String::from_utf8_lossy(&request).into_owned();
            let key = head
                .lines()
                .find_map(|line| line.strip_prefix("Sec-WebSocket-Key:").map(str::trim))
                .unwrap();
            let accept = websocket_accept(key);
            let extension_header = if extensions.is_empty() {
                String::new()
            } else {
                format!("Sec-WebSocket-Extensions: {extensions}\r\n")
            };
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n\
                         Connection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\
                         {extension_header}\r\n"
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            for frame in frames {
                socket.write_all(&frame).await.unwrap();
            }
            // Read until the client's Close frame (or end of stream).
            let mut received = Vec::new();
            let read_until_close = async {
                loop {
                    let read = socket.read(&mut buf).await.unwrap_or(0);
                    if read == 0 {
                        break;
                    }
                    received.extend_from_slice(&buf[..read]);
                    if client_frames(&received)
                        .iter()
                        .any(|(first, _)| first & 0x0f == OP_CLOSE)
                    {
                        break;
                    }
                }
            };
            let _ = tokio::time::timeout(std::time::Duration::from_secs(5), read_until_close).await;
            (head, received)
        });
        (port, server)
    }

    /// Unmask the client frames in `bytes` as (first byte, payload).
    fn client_frames(mut bytes: &[u8]) -> Vec<(u8, Vec<u8>)> {
        let mut frames = Vec::new();
        while bytes.len() >= 6 {
            assert_eq!(bytes[1] & 0x80, 0x80, "client frames are masked");
            let (len, at) = match bytes[1] & 0x7f {
                126 => (usize::from(u16::from_be_bytes([bytes[2], bytes[3]])), 4),
                127 => unreachable!(),
                len => (usize::from(len), 2),
            };
            if bytes.len() < at + 4 + len {
                break;
            }
            let mask = [bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]];
            let payload = bytes[at + 4..at + 4 + len]
                .iter()
                .enumerate()
                .map(|(i, byte)| byte ^ mask[i & 3])
                .collect();
            frames.push((bytes[0], payload));
            bytes = &bytes[at + 4 + len..];
        }
        frames
    }

    #[tokio::test]
    async fn permessage_deflate_session_exchanges_compressed_messages() {
        // §7.2.3.1's "Hello", then §7.2.3.2's window-sharing repeat split
        // across two frames (RSV1 only on the first), then a server Close.
        let frames = vec![
            vec![0xc1, 0x07, 0xf2, 0x48, 0xcd, 0xc9, 0xc9, 0x07, 0x00],
            vec![0x41, 0x02, 0xf2, 0x00],
            vec![0x80, 0x03, 0x11, 0x00, 0x00],
        ];
        let (port, server) = serve("permessage-deflate; client_max_window_bits=15", frames).await;
        let url = url::Url::parse(&format!("ws://127.0.0.1:{port}/")).unwrap();
        let (events_tx, mut events) = mpsc::channel(16);
        let (out, task) = connect(
            url,
            Vec::new(),
            String::from("http://127.0.0.1"),
            None,
            &tokio::runtime::Handle::current(),
            7,
            events_tx,
        );
        match events.recv().await.unwrap().1 {
            WsIn::Open { extensions, .. } => {
                assert_eq!(extensions, "permessage-deflate; client_max_window_bits=15");
            }
            other => panic!("{other:?}"),
        }
        for _ in 0..2 {
            assert!(matches!(events.recv().await.unwrap().1, WsIn::Text(text) if text == "Hello"));
        }
        out.send(WsOut::Text(String::from("Hello Hello Hello")))
            .await
            .unwrap();
        assert!(matches!(events.recv().await.unwrap().1, WsIn::Sent(17)));
        out.send(WsOut::Close(1000, String::new())).await.unwrap();
        drop(out);
        let (head, received) = server.await.unwrap();
        task.abort();
        assert!(
            head.contains(
                "Sec-WebSocket-Extensions: permessage-deflate; client_max_window_bits\r\n"
            )
        );
        let frames = client_frames(&received);
        // FIN + RSV1 + text, then an uncompressed Close.
        assert_eq!(frames[0].0, 0xc1);
        let mut inflater = Inflater::new(DeflateParams::default());
        assert_eq!(
            inflater.inflate(&frames[0].1, 1024).unwrap(),
            b"Hello Hello Hello"
        );
        assert_eq!(frames[1], (0x88, 1000u16.to_be_bytes().to_vec()));
    }

    #[tokio::test]
    async fn protocol_violations_send_their_close_status() {
        for (frame, code) in [
            // Invalid UTF-8 in a text message: 1007.
            (vec![0x81, 0x02, 0xc3, 0x28], 1007u16),
            // RSV1 without a negotiated extension: 1002.
            (vec![0xc1, 0x01, b'x'], 1002),
            // A continuation frame with no message: 1002.
            (vec![0x80, 0x01, b'x'], 1002),
        ] {
            let (port, server) = serve("", vec![frame]).await;
            let url = url::Url::parse(&format!("ws://127.0.0.1:{port}/")).unwrap();
            let (events_tx, mut events) = mpsc::channel(16);
            let (_out, task) = connect(
                url,
                Vec::new(),
                String::from("http://127.0.0.1"),
                None,
                &tokio::runtime::Handle::current(),
                1,
                events_tx,
            );
            assert!(matches!(events.recv().await.unwrap().1, WsIn::Open { .. }));
            // The page sees an unclean closure (WebSockets: fail the connection).
            assert!(matches!(
                events.recv().await.unwrap().1,
                WsIn::Closed {
                    code: 1006,
                    was_clean: false,
                    failed: true,
                    ..
                }
            ));
            task.await.unwrap();
            let (_, received) = server.await.unwrap();
            assert_eq!(
                client_frames(&received),
                vec![(0x88, code.to_be_bytes().to_vec())],
                "{code}"
            );
        }
    }

    #[tokio::test]
    async fn opening_handshake_sends_the_browser_language_preferences() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (head_tx, head_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buf = [0u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = socket.read(&mut buf).await.unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buf[..read]);
            }
            let head = String::from_utf8_lossy(&request).into_owned();
            let key = head
                .lines()
                .find_map(|line| line.strip_prefix("Sec-WebSocket-Key:").map(str::trim))
                .unwrap();
            let accept = websocket_accept(key);
            let _ = head_tx.send(head);
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 101 Switching Protocols\r\n\
                         Upgrade: websocket\r\n\
                         Connection: Upgrade\r\n\
                         Sec-WebSocket-Accept: {accept}\r\n\r\n"
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        });

        let url = url::Url::parse(&format!("ws://127.0.0.1:{port}/socket")).unwrap();
        let (transport, negotiated) = handshake(&url, &[], "http://example.test", None)
            .await
            .unwrap();
        drop(transport);
        assert!(negotiated.protocol.is_empty());
        assert!(negotiated.extensions.is_empty());
        assert_eq!(negotiated.deflate, None);
        let head = head_rx.await.unwrap();
        assert!(
            head.contains("Accept-Language: en-US,en;q=0.9\r\n"),
            "WebSocket Fetch handshake omitted the language preference: {head}"
        );
        assert_eq!(head.matches("Accept-Language:").count(), 1);
        assert!(
            head.contains("Sec-GPC: 1\r\n"),
            "WebSocket HTTP handshake omitted the GPC preference: {head}"
        );
        assert_eq!(head.matches("Sec-GPC:").count(), 1);
        server.await.unwrap();
    }
}
