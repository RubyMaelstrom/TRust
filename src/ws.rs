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
//! ping/pong/close control frames. `Sec-WebSocket-Accept` is not strictly
//! verified (we require the `101` + `Upgrade: websocket`); the key/mask use a
//! cheap PRNG since their randomness guards proxy-cache poisoning, not secrecy.
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
    Open,
    Text(String),
    Binary(Vec<u8>),
    /// The connection ended (clean close with code/reason, or a transport drop
    /// reported as 1006). Always the final event for a socket.
    Closed {
        code: u16,
        reason: String,
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

/// A control/data frame's payload is capped so a hostile server can't make us
/// buffer unboundedly (mirrors http's `MAX_BODY` spirit; a single chat token
/// frame is tiny).
const MAX_FRAME: usize = 16 * 1024 * 1024;

/// Open a WebSocket to `url` (`ws`/`wss`). The connection task forwards `WsIn`
/// events (tagged with `id`) to `events`; the returned sender takes `WsOut`
/// messages to write. Dropping the returned sender closes the socket.
pub fn connect(
    url: url::Url,
    origin: String,
    cookie: Option<String>,
    handle: &tokio::runtime::Handle,
    id: usize,
    events: mpsc::Sender<(usize, WsIn)>,
) -> mpsc::Sender<WsOut> {
    let (out_tx, out_rx) = mpsc::channel::<WsOut>(64);
    handle.spawn(async move {
        run_session(url, origin, cookie, id, events, out_rx).await;
    });
    out_tx
}

async fn run_session(
    url: url::Url,
    origin: String,
    cookie: Option<String>,
    id: usize,
    events: mpsc::Sender<(usize, WsIn)>,
    mut out_rx: mpsc::Receiver<WsOut>,
) {
    let io = match handshake(&url, &origin, cookie.as_deref()).await {
        Ok(io) => io,
        Err(e) => {
            wsdiag(&format!("WS handshake FAILED for {url}: {e}"));
            // Transport failure: report an abnormal closure and stop.
            let _ = events
                .send((
                    id,
                    WsIn::Closed {
                        code: 1006,
                        reason: String::new(),
                    },
                ))
                .await;
            return;
        }
    };
    wsdiag(&format!("WS open ok for {url}"));
    if events.send((id, WsIn::Open)).await.is_err() {
        return; // actor gone
    }
    // Split so the read future borrows the read half while a control/outbound
    // write borrows the write half — no aliasing across the `select!`.
    let (mut rd, mut wr) = tokio::io::split(io);
    let mut frag: Vec<u8> = Vec::new();
    let mut frag_op: u8 = 0;
    loop {
        tokio::select! {
            frame = read_frame(&mut rd) => {
                let (fin, opcode, payload) = match frame {
                    Ok(f) => f,
                    Err(e) => {
                        wsdiag(&format!("WS read error: {e}"));
                        let _ = events.send((id, WsIn::Closed { code: 1006, reason: String::new() })).await;
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
                        let (code, reason) = parse_close(&payload);
                        // Echo a close, then report and stop.
                        let _ = write_frame(&mut wr, OP_CLOSE, &payload).await;
                        let _ = events.send((id, WsIn::Closed { code, reason })).await;
                        break;
                    }
                    OP_TEXT | OP_BINARY => {
                        if fin {
                            if deliver(&events, id, opcode, payload).await.is_err() { break; }
                        } else {
                            frag_op = opcode;
                            frag = payload;
                        }
                    }
                    OP_CONT => {
                        if frag.len() + payload.len() > MAX_FRAME { break; }
                        frag.extend_from_slice(&payload);
                        if fin {
                            let msg = std::mem::take(&mut frag);
                            let op = frag_op;
                            if deliver(&events, id, op, msg).await.is_err() { break; }
                        }
                    }
                    _ => {} // reserved opcode: ignore
                }
            }
            out = out_rx.recv() => {
                match out {
                    Some(WsOut::Text(s)) => {
                        if write_frame(&mut wr, OP_TEXT, s.as_bytes()).await.is_err() { break; }
                    }
                    Some(WsOut::Binary(b)) => {
                        if write_frame(&mut wr, OP_BINARY, &b).await.is_err() { break; }
                    }
                    Some(WsOut::Close(code, reason)) => {
                        let _ = write_frame(&mut wr, OP_CLOSE, &close_payload(code, &reason)).await;
                        let _ = events.send((id, WsIn::Closed { code, reason })).await;
                        break;
                    }
                    None => {
                        // JS dropped the WebSocket: send a normal close and stop.
                        let _ = write_frame(&mut wr, OP_CLOSE, &close_payload(1000, "")).await;
                        let _ = events.send((id, WsIn::Closed { code: 1000, reason: String::new() })).await;
                        break;
                    }
                }
            }
        }
    }
}

async fn deliver(
    events: &mpsc::Sender<(usize, WsIn)>,
    id: usize,
    opcode: u8,
    payload: Vec<u8>,
) -> Result<(), ()> {
    let ev = if opcode == OP_TEXT {
        WsIn::Text(String::from_utf8_lossy(&payload).into_owned())
    } else {
        WsIn::Binary(payload)
    };
    events.send((id, ev)).await.map_err(|_| ())
}

/// The RFC 6455 opening handshake over a fresh dial. Returns the live transport
/// positioned right after the `\r\n\r\n` of the `101` response.
async fn handshake(
    url: &url::Url,
    origin: &str,
    cookie: Option<&str>,
) -> Result<crate::http::WsTransport, String> {
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
         Origin: {origin}\r\n",
        ua = crate::http::USER_AGENT,
    );
    if let Some(c) = cookie.filter(|c| !c.is_empty()) {
        req.push_str(&format!("Cookie: {c}\r\n"));
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
    if !status_line.contains(" 101") {
        return Err(format!("not a websocket upgrade: {status_line}"));
    }
    let lower = head.to_ascii_lowercase();
    if !lower.contains("upgrade: websocket") {
        return Err(String::from("missing Upgrade: websocket"));
    }
    Ok(io)
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

/// Read one frame: `(fin, opcode, unmasked_payload)`. Server→client frames are
/// never masked (RFC 6455 §5.1); we tolerate a mask bit anyway.
async fn read_frame(
    io: &mut tokio::io::ReadHalf<crate::http::WsTransport>,
) -> Result<(bool, u8, Vec<u8>), String> {
    let mut hdr = [0u8; 2];
    io.read_exact(&mut hdr).await.map_err(|e| e.to_string())?;
    let fin = hdr[0] & 0x80 != 0;
    let opcode = hdr[0] & 0x0F;
    let masked = hdr[1] & 0x80 != 0;
    let mut len = (hdr[1] & 0x7F) as usize;
    if len == 126 {
        let mut ext = [0u8; 2];
        io.read_exact(&mut ext).await.map_err(|e| e.to_string())?;
        len = u16::from_be_bytes(ext) as usize;
    } else if len == 127 {
        let mut ext = [0u8; 8];
        io.read_exact(&mut ext).await.map_err(|e| e.to_string())?;
        len = u64::from_be_bytes(ext) as usize;
    }
    if len > MAX_FRAME {
        return Err(String::from("frame too large"));
    }
    let mask = if masked {
        let mut m = [0u8; 4];
        io.read_exact(&mut m).await.map_err(|e| e.to_string())?;
        Some(m)
    } else {
        None
    };
    let mut payload = vec![0u8; len];
    if len > 0 {
        io.read_exact(&mut payload)
            .await
            .map_err(|e| e.to_string())?;
    }
    if let Some(m) = mask {
        for (i, byte) in payload.iter_mut().enumerate() {
            *byte ^= m[i & 3];
        }
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

fn parse_close(payload: &[u8]) -> (u16, String) {
    if payload.len() >= 2 {
        let code = u16::from_be_bytes([payload[0], payload[1]]);
        let reason = String::from_utf8_lossy(&payload[2..]).into_owned();
        (code, reason)
    } else {
        (1005, String::new()) // "no status received"
    }
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
    fn close_payload_roundtrips() {
        let p = close_payload(1000, "bye");
        let (code, reason) = parse_close(&p);
        assert_eq!(code, 1000);
        assert_eq!(reason, "bye");
        assert_eq!(parse_close(&[]).0, 1005);
    }
}
