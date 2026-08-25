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
//! subprotocol. The key/mask use a cheap PRNG since their randomness guards
//! proxy-cache poisoning, not secrecy.
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
    cookie: Option<String>,
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
    cookie: Option<String>,
    id: usize,
    events: mpsc::Sender<(usize, WsIn)>,
    mut out_rx: mpsc::Receiver<WsOut>,
) {
    let (io, protocol) = match handshake(&url, &protocols, &origin, cookie.as_deref()).await {
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
    if events.send((id, WsIn::Open { protocol })).await.is_err() {
        return; // actor gone
    }
    // Split so the read future borrows the read half while a control/outbound
    // write borrows the write half — no aliasing across the `select!`.
    let (mut rd, mut wr) = tokio::io::split(io);
    let mut frag: Vec<u8> = Vec::new();
    let mut frag_op: u8 = 0;
    let mut closing = false;
    let mut closed_reported = false;
    loop {
        tokio::select! {
            frame = read_frame(&mut rd) => {
                let (fin, opcode, payload) = match frame {
                    Ok(f) => f,
                    Err(e) => {
                        wsdiag(&format!("WS read error: {e}"));
                        let _ = events.send((id, WsIn::Closed {
                            code: 1006,
                            reason: String::new(),
                            was_clean: false,
                            failed: true,
                        })).await;
                        closed_reported = true;
                        break;
                    }
                };
                wsdiag(&format!("WS frame op={opcode:#x} fin={fin} len={} head={:?}",
                    payload.len(), String::from_utf8_lossy(&payload[..payload.len().min(diag_cap())])));
                match opcode {
                    OP_PING => {
                        // Reply to a server ping with a pong (RFC 6455 §5.5.3).
                        let pong = write_frame(&mut wr, OP_PONG, &payload).await;
                        if pong.is_err() {
                            break;
                        }
                    }
                    OP_PONG => {}
                    OP_CLOSE => {
                        let Ok((code, reason)) = parse_close(&payload) else {
                            break;
                        };
                        // RFC 6455 §7.1.2: answer a peer-initiated Close, then report a
                        // clean closure only after both directions of the handshake exist.
                        if !closing
                            && write_frame(&mut wr, OP_CLOSE, &payload).await.is_err()
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
                            break;
                        }
                        if fin {
                            if deliver(&events, id, opcode, payload).await.is_err() { break; }
                        } else {
                            frag_op = opcode;
                            frag = payload;
                        }
                    }
                    OP_CONT => {
                        if frag_op == 0 {
                            break;
                        }
                        if frag.len() + payload.len() > MAX_FRAME { break; }
                        frag.extend_from_slice(&payload);
                        if fin {
                            let msg = std::mem::take(&mut frag);
                            let op = frag_op;
                            frag_op = 0;
                            if deliver(&events, id, op, msg).await.is_err() { break; }
                        }
                    }
                    _ => {} // reserved opcode: ignore
                }
            }
            out = out_rx.recv(), if !closing => {
                match out {
                    Some(WsOut::Text(s)) => {
                        let len = s.len();
                        if write_frame(&mut wr, OP_TEXT, s.as_bytes()).await.is_err() { break; }
                        if events.send((id, WsIn::Sent(len))).await.is_err() { break; }
                    }
                    Some(WsOut::Binary(b)) => {
                        let len = b.len();
                        if write_frame(&mut wr, OP_BINARY, &b).await.is_err() { break; }
                        if events.send((id, WsIn::Sent(len))).await.is_err() { break; }
                    }
                    Some(WsOut::Close(code, reason)) => {
                        if write_frame(&mut wr, OP_CLOSE, &close_payload(code, &reason)).await.is_err() {
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
                        if write_frame(&mut wr, OP_CLOSE, &close_payload(1001, "")).await.is_err() {
                            break;
                        }
                        closing = true;
                    }
                }
            }
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

async fn deliver(
    events: &mpsc::Sender<(usize, WsIn)>,
    id: usize,
    opcode: u8,
    payload: Vec<u8>,
) -> Result<(), ()> {
    let ev = if opcode == OP_TEXT {
        WsIn::Text(String::from_utf8(payload).map_err(|_| ())?)
    } else {
        WsIn::Binary(payload)
    };
    events.send((id, ev)).await.map_err(|_| ())
}

/// The RFC 6455 opening handshake over a fresh dial. Returns the live transport
/// positioned right after the `\r\n\r\n` of the `101` response.
async fn handshake(
    url: &url::Url,
    protocols: &[String],
    origin: &str,
    cookie: Option<&str>,
) -> Result<(crate::http::WsTransport, String), String> {
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
    let key = ws_key();
    let mut req = format!(
        "GET {path} HTTP/1.1\r\n\
         Host: {host_hdr}\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Key: {key}\r\n\
         Sec-WebSocket-Version: 13\r\n\
         User-Agent: {ua}\r\n\
         Accept-Language: {accept_language}\r\n\
         Origin: {origin}\r\n",
        ua = crate::http::USER_AGENT,
        accept_language = crate::locale::ACCEPT_LANGUAGE,
    );
    if crate::http::GLOBAL_PRIVACY_CONTROL {
        req.push_str("Sec-GPC: 1\r\n");
    }
    if let Some(c) = cookie.filter(|c| !c.is_empty()) {
        req.push_str(&format!("Cookie: {c}\r\n"));
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
    wsdiag(&format!(
        "WS handshake -> {path}\n--- response head ---\n{head}---"
    ));
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
    if headers
        .iter()
        .any(|(name, _)| name == "sec-websocket-extensions")
    {
        return Err(String::from(
            "server selected an unrequested WebSocket extension",
        ));
    }
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
    Ok((io, protocol))
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
async fn read_frame(
    io: &mut tokio::io::ReadHalf<crate::http::WsTransport>,
) -> Result<(bool, u8, Vec<u8>), String> {
    let mut hdr = [0u8; 2];
    io.read_exact(&mut hdr).await.map_err(|e| e.to_string())?;
    if hdr[0] & 0x70 != 0 {
        return Err(String::from("unnegotiated WebSocket extension bits"));
    }
    let fin = hdr[0] & 0x80 != 0;
    let opcode = hdr[0] & 0x0F;
    if !matches!(
        opcode,
        OP_CONT | OP_TEXT | OP_BINARY | OP_CLOSE | OP_PING | OP_PONG
    ) {
        return Err(String::from("reserved WebSocket opcode"));
    }
    if hdr[1] & 0x80 != 0 {
        return Err(String::from("masked server WebSocket frame"));
    }
    let length_code = hdr[1] & 0x7F;
    if opcode >= OP_CLOSE && (!fin || length_code > 125) {
        return Err(String::from("invalid WebSocket control frame"));
    }
    let mut len = usize::from(length_code);
    if length_code == 126 {
        let mut ext = [0u8; 2];
        io.read_exact(&mut ext).await.map_err(|e| e.to_string())?;
        len = u16::from_be_bytes(ext) as usize;
        if len < 126 {
            return Err(String::from("non-minimal WebSocket frame length"));
        }
    } else if length_code == 127 {
        let mut ext = [0u8; 8];
        io.read_exact(&mut ext).await.map_err(|e| e.to_string())?;
        if ext[0] & 0x80 != 0 {
            return Err(String::from("invalid 63-bit WebSocket frame length"));
        }
        let extended = u64::from_be_bytes(ext);
        if extended <= u64::from(u16::MAX) {
            return Err(String::from("non-minimal WebSocket frame length"));
        }
        len = usize::try_from(extended).map_err(|_| String::from("frame too large"))?;
    }
    if len > MAX_FRAME {
        return Err(String::from("frame too large"));
    }
    let mut payload = vec![0u8; len];
    if len > 0 {
        io.read_exact(&mut payload)
            .await
            .map_err(|e| e.to_string())?;
    }
    Ok((fin, opcode, payload))
}

/// Write one client frame (always masked, always FIN — we don't fragment what
/// we send; messages are whole `send()` calls).
async fn write_frame(
    io: &mut tokio::io::WriteHalf<crate::http::WsTransport>,
    opcode: u8,
    payload: &[u8],
) -> Result<(), std::io::Error> {
    let mut frame = Vec::with_capacity(payload.len() + 14);
    frame.push(0x80 | opcode); // FIN + opcode
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
    let mask = mask_key();
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

fn parse_close(payload: &[u8]) -> Result<(u16, String), String> {
    if payload.is_empty() {
        return Ok((1005, String::new()));
    }
    if payload.len() == 1 {
        return Err(String::from("one-byte WebSocket Close payload"));
    }
    let code = u16::from_be_bytes([payload[0], payload[1]]);
    if !(1000..=4999).contains(&code) || matches!(code, 1004..=1006 | 1015) {
        return Err(String::from("invalid WebSocket Close status code"));
    }
    let reason = std::str::from_utf8(&payload[2..])
        .map_err(|_| String::from("invalid UTF-8 WebSocket Close reason"))?
        .to_string();
    Ok((code, reason))
}

/// A fresh 16-byte `Sec-WebSocket-Key`, base64'd. Uniqueness (not secrecy) is
/// what matters, so a time+counter-seeded xorshift is plenty.
fn ws_key() -> String {
    let mut bytes = [0u8; 16];
    let mut s = seed();
    for b in bytes.iter_mut() {
        s = xorshift(s);
        *b = (s >> 24) as u8;
    }
    base64(&bytes)
}

fn mask_key() -> [u8; 4] {
    let mut s = seed();
    let mut m = [0u8; 4];
    for b in m.iter_mut() {
        s = xorshift(s);
        *b = (s >> 24) as u8;
    }
    m
}

fn seed() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0x9E3779B97F4A7C15);
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    t ^ COUNTER.fetch_add(0x6D2B79F5, Ordering::Relaxed)
}

fn xorshift(mut x: u64) -> u64 {
    if x == 0 {
        x = 0x9E3779B97F4A7C15;
    }
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    x
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
        assert_eq!(ws_key().len(), 24);
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
        let (transport, protocol) = handshake(&url, &[], "http://example.test", None)
            .await
            .unwrap();
        drop(transport);
        assert!(protocol.is_empty());
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
