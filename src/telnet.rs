//! Telnet connection task: owns the socket and the protocol state machine.
//!
//! The app talks to a connection through a pair of mpsc channels. All telnet
//! protocol bytes (IAC sequences, negotiation, subnegotiation) are produced
//! and consumed here; the app only ever sees decoded application data.

use std::collections::HashSet;
use std::net::SocketAddr;

use bytes::Bytes;
use libmudtelnet::Parser;
use libmudtelnet::compatibility::CompatibilityTable;
use libmudtelnet::events::TelnetEvents;
use libmudtelnet::telnet::{op_command, op_option};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

use crate::tls;

/// Messages from the app to the connection task.
#[derive(Debug)]
pub enum Command {
    /// Raw user input; IAC bytes are escaped before hitting the wire.
    Send(Vec<u8>),
    /// Transmit `IAC <command>` unescaped (GNU telnet's `send brk` etc.).
    SendIac(u8),
    /// The rendering area changed; renegotiate NAWS if it is active.
    Resize { cols: u16, rows: u16 },
    /// Request a LINEMODE MODE change (RFC 1184): `edit` true asks the
    /// server for local line editing, false for character-at-a-time. Only
    /// sent while LINEMODE is active; the server confirms with an ACK.
    LineModeRequest { edit: bool },
    /// Close the connection.
    Close,
}

/// Events from the connection task to the app.
#[derive(Debug)]
pub enum Event {
    Connected {
        peer: SocketAddr,
        tls: bool,
    },
    /// Application data with all telnet protocol bytes stripped.
    Data(Vec<u8>),
    /// An accepted option state change (WILL/WONT = remote side,
    /// DO/DONT = our side). The app derives echo mode and `status`
    /// output from these.
    Negotiation {
        command: u8,
        option: u8,
    },
    /// LINEMODE (RFC 1184) state changed. `active` is whether the option is
    /// in effect at all; `edit` is the MODE EDIT bit (true = the client does
    /// local line editing, false = character-at-a-time). The app folds this
    /// into its line/character input decision alongside ECHO.
    LineMode {
        active: bool,
        edit: bool,
    },
    /// The session ended, with an error message if it ended abnormally.
    Closed(Option<String>),
}

pub struct Handle {
    pub commands: mpsc::Sender<Command>,
}

/// Spawn a connection task for `host:port`, optionally wrapped in TLS
/// (`telnets://`, RFC-less but common on modern BBSes). Returns
/// immediately; connection progress and data arrive on the event channel.
pub fn connect(
    host: String,
    port: u16,
    size: (u16, u16),
    use_tls: bool,
) -> (Handle, mpsc::Receiver<Event>) {
    let (cmd_tx, cmd_rx) = mpsc::channel(64);
    let (evt_tx, evt_rx) = mpsc::channel(256);
    tokio::spawn(run(host, port, size, use_tls, cmd_rx, evt_tx));
    (Handle { commands: cmd_tx }, evt_rx)
}

/// Options we are willing to negotiate, mirroring GNU telnet's defaults.
/// The parser automatically answers WILL/WONT/DO/DONT based on this table
/// and refuses everything else.
fn compat_table() -> CompatibilityTable {
    let mut table = CompatibilityTable::new();
    table.support_remote(op_option::ECHO);
    table.support_remote(op_option::SGA);
    table.support_local(op_option::SGA);
    table.support_local(op_option::NAWS);
    table.support_local(op_option::TTYPE);
    // 8-bit clean paths (RFC 856); accepted if the server asks.
    table.support_local(op_option::BINARY);
    table.support_remote(op_option::BINARY);
    // Answered in suboption_reply: TSPEED (RFC 1079), NEW-ENVIRON
    // (RFC 1572, deliberately empty), STATUS (RFC 859).
    table.support_local(op_option::TSPEED);
    table.support_local(op_option::NEWENVIRON);
    table.support_local(op_option::STATUS);
    // LFLOW (RFC 1372): accepted so servers get their WILL; its ON/OFF
    // subnegotiations expect no reply, and pausing output is meaningless
    // here — the remote feeds a vt100 emulator, not a real tty.
    table.support_local(op_option::LFLOW);
    // LINEMODE (RFC 1184): we are the line-editing party, so we WILL it when
    // a server sends DO. The MODE/SLC/FORWARDMASK subnegotiations are handled
    // in linemode_reply.
    table.support_local(op_option::LINEMODE);
    table
}

/// RFC 1184 LINEMODE subnegotiation vocabulary.
mod linemode {
    /// Subcommands (the first payload byte after the option).
    pub const MODE: u8 = 1;
    pub const FORWARDMASK: u8 = 2;
    pub const SLC: u8 = 3;
    /// MODE mask bits.
    pub const EDIT: u8 = 0x01;
    pub const MODE_ACK: u8 = 0x04;
    /// SLC flag bit: acknowledgement.
    pub const SLC_ACK: u8 = 0x80;
}

/// Terminal names offered through TTYPE (RFC 1091), most capable first.
/// The server cycles with repeated SENDs; repeating the final name tells
/// it the list is exhausted. "ANSI" first is what BBS ANSI detection wants.
const TERMINAL_TYPES: [&[u8]; 3] = [b"ANSI", b"XTERM", b"VT100"];

/// Build the `IS <name>` reply for a TTYPE SEND, advancing through
/// `TERMINAL_TYPES` and repeating the last entry once exhausted.
fn ttype_reply(parser: &mut Parser, sends_seen: &mut usize) -> Option<TelnetEvents> {
    let name = TERMINAL_TYPES[(*sends_seen).min(TERMINAL_TYPES.len() - 1)];
    *sends_seen += 1;
    let mut payload = vec![op_command::IS];
    payload.extend_from_slice(name);
    parser.subnegotiation(op_option::TTYPE, Bytes::from(payload))
}

/// Speed reported through TSPEED (RFC 1079), "transmit,receive". There is
/// no serial line here; 38400 is what modern terminal emulators claim.
const TERMINAL_SPEED: &[u8] = b"38400,38400";

/// Answer a server subnegotiation that demands a reply. All four queries
/// share the `SEND` opcode in their first payload byte: TTYPE and TSPEED
/// get `IS <value>`, NEW-ENVIRON gets an empty `IS` (no local environment
/// ever goes on the wire), STATUS gets the live option states. Anything
/// else (e.g. LFLOW toggles) needs no answer.
fn suboption_reply(
    parser: &mut Parser,
    opts: &OptionStates,
    option: u8,
    payload: &[u8],
    ttype_sends: &mut usize,
) -> Option<TelnetEvents> {
    if payload.first() != Some(&op_command::SEND) {
        return None;
    }
    match option {
        op_option::TTYPE => ttype_reply(parser, ttype_sends),
        op_option::TSPEED => {
            let mut reply = vec![op_command::IS];
            reply.extend_from_slice(TERMINAL_SPEED);
            parser.subnegotiation(op_option::TSPEED, Bytes::from(reply))
        }
        op_option::NEWENVIRON => {
            parser.subnegotiation(op_option::NEWENVIRON, Bytes::from(vec![op_command::IS]))
        }
        op_option::STATUS => {
            parser.subnegotiation(op_option::STATUS, Bytes::from(status_reply(opts)))
        }
        _ => None,
    }
}

/// Handle a LINEMODE (RFC 1184) subnegotiation. `payload` is the bytes
/// between `IAC SB LINEMODE` and `IAC SE`, subcommand byte first. Returns the
/// reply to send (if any) and the new EDIT state (if a MODE settled it).
/// Loop-safe: only un-acknowledged requests get an answer.
fn linemode_reply(parser: &mut Parser, payload: &[u8]) -> (Option<TelnetEvents>, Option<bool>) {
    match payload.split_first() {
        // MODE <mask>: adopt the EDIT bit. A request (ACK clear) is adopted
        // and acknowledged; a MODE already carrying ACK is the server
        // confirming our own request, so we only record its EDIT state.
        Some((&linemode::MODE, rest)) => {
            let mask = rest.first().copied().unwrap_or(0);
            let edit = mask & linemode::EDIT != 0;
            if mask & linemode::MODE_ACK != 0 {
                return (None, Some(edit));
            }
            let ack = mask | linemode::MODE_ACK;
            let reply =
                parser.subnegotiation(op_option::LINEMODE, Bytes::from(vec![linemode::MODE, ack]));
            (reply, Some(edit))
        }
        // SLC: accept the server's special-character assignments by echoing
        // every un-acknowledged triple back with SLC_ACK set. We forward
        // control characters to the remote verbatim, so the server's values
        // stand. ACK'd triples confirm our own replies — ignored, which
        // terminates the handshake.
        Some((&linemode::SLC, triples)) => (slc_ack(parser, triples), None),
        // FORWARDMASK: declined. We forward whole lines on Enter (and each
        // key in character mode), not via a per-character mask.
        Some((&op_command::DO, rest)) if rest.first() == Some(&linemode::FORWARDMASK) => {
            let reply = parser.subnegotiation(
                op_option::LINEMODE,
                Bytes::from(vec![op_command::WONT, linemode::FORWARDMASK]),
            );
            (reply, None)
        }
        _ => (None, None),
    }
}

/// Build the SLC reply: every triple the server has not acknowledged is
/// echoed back with SLC_ACK set. Returns None when nothing needs answering
/// (all triples already acknowledged), which ends the handshake.
fn slc_ack(parser: &mut Parser, triples: &[u8]) -> Option<TelnetEvents> {
    let mut out = vec![linemode::SLC];
    for triple in triples.chunks_exact(3) {
        let (func, flags, ch) = (triple[0], triple[1], triple[2]);
        if flags & linemode::SLC_ACK == 0 {
            out.extend_from_slice(&[func, flags | linemode::SLC_ACK, ch]);
        }
    }
    if out.len() == 1 {
        return None;
    }
    parser.subnegotiation(op_option::LINEMODE, Bytes::from(out))
}

/// Build a LINEMODE MODE request (RFC 1184) asking the server for local line
/// editing (`edit` true) or character-at-a-time (`edit` false). The ACK bit
/// is clear: this is a request the server confirms.
fn linemode_mode_request(parser: &mut Parser, edit: bool) -> Option<TelnetEvents> {
    let mask = if edit { linemode::EDIT } else { 0 };
    parser.subnegotiation(op_option::LINEMODE, Bytes::from(vec![linemode::MODE, mask]))
}

/// Option states as actually negotiated on the wire, tracked from
/// Negotiation events. libmudtelnet's own table marks an option
/// "remotely enabled" whenever we merely accept a DO, so it cannot be
/// trusted for STATUS reporting.
#[derive(Default)]
struct OptionStates {
    local: HashSet<u8>,
    remote: HashSet<u8>,
}

impl OptionStates {
    fn update(&mut self, command: u8, option: u8) {
        match command {
            op_command::WILL => {
                self.remote.insert(option);
            }
            op_command::WONT => {
                self.remote.remove(&option);
            }
            op_command::DO => {
                self.local.insert(option);
            }
            op_command::DONT => {
                self.local.remove(&option);
            }
            _ => {}
        }
    }
}

/// The `IS` payload for a STATUS SEND (RFC 859): `WILL <opt>` for every
/// option enabled on our side, `DO <opt>` for every option enabled on the
/// remote, in ascending option order.
fn status_reply(opts: &OptionStates) -> Vec<u8> {
    let mut reply = vec![op_command::IS];
    for option in 0..=u8::MAX {
        if opts.local.contains(&option) {
            reply.extend_from_slice(&[op_command::WILL, option]);
        }
        if opts.remote.contains(&option) {
            reply.extend_from_slice(&[op_command::DO, option]);
        }
    }
    reply
}

async fn run(
    host: String,
    port: u16,
    size: (u16, u16),
    use_tls: bool,
    commands: mpsc::Receiver<Command>,
    events: mpsc::Sender<Event>,
) {
    let stream = match TcpStream::connect((host.as_str(), port)).await {
        Ok(stream) => stream,
        Err(err) => {
            let _ = events.send(Event::Closed(Some(err.to_string()))).await;
            return;
        }
    };
    let _ = stream.set_nodelay(true);
    let peer = stream.peer_addr().ok();

    if use_tls {
        let name = match tls::server_name(&host) {
            Ok(name) => name,
            Err(err) => {
                let _ = events.send(Event::Closed(Some(err))).await;
                return;
            }
        };
        let stream = match tls::connector(&host, port).connect(name, stream).await {
            Ok(stream) => stream,
            Err(err) => {
                let _ = events
                    .send(Event::Closed(Some(format!("TLS: {err}"))))
                    .await;
                return;
            }
        };
        if let Some(peer) = peer {
            let _ = events.send(Event::Connected { peer, tls: true }).await;
        }
        run_session(stream, size, commands, events).await;
    } else {
        if let Some(peer) = peer {
            let _ = events.send(Event::Connected { peer, tls: false }).await;
        }
        run_session(stream, size, commands, events).await;
    }
}

/// The telnet session loop, generic over the transport (plain TCP or TLS).
async fn run_session<S: AsyncRead + AsyncWrite + Unpin>(
    stream: S,
    mut size: (u16, u16),
    mut commands: mpsc::Receiver<Command>,
    events: mpsc::Sender<Event>,
) {
    let (mut reader, mut writer) = tokio::io::split(stream);
    let mut parser = Parser::with_support(compat_table());
    let mut buf = vec![0u8; 8192];
    let mut ttype_sends = 0usize;
    let mut opts = OptionStates::default();
    // LINEMODE (RFC 1184) state, mirrored to the app via Event::LineMode.
    let mut linemode_active = false;
    let mut linemode_edit = false;

    loop {
        tokio::select! {
            read = reader.read(&mut buf) => match read {
                Ok(0) => {
                    let _ = events.send(Event::Closed(None)).await;
                    return;
                }
                Ok(n) => {
                    for event in parser.receive(&buf[..n]) {
                        match event {
                            TelnetEvents::DataReceive(data) => {
                                if events.send(Event::Data(data.to_vec())).await.is_err() {
                                    return;
                                }
                            }
                            // Negotiation answers the parser generated from the
                            // compatibility table.
                            TelnetEvents::DataSend(data) => {
                                if write_or_close(&mut writer, &data, &events).await.is_err() {
                                    return;
                                }
                            }
                            TelnetEvents::Negotiation(neg) => {
                                opts.update(neg.command, neg.option);
                                // The WILL NAWS answer is already queued as DataSend;
                                // RFC 1073 requires us to follow it with our size.
                                if (neg.command, neg.option) == (op_command::DO, op_option::NAWS)
                                    && let Some(sub) = naws_subnegotiation(&mut parser, size)
                                    && write_or_close(&mut writer, &sub.to_bytes(), &events)
                                        .await
                                        .is_err()
                                {
                                    return;
                                }
                                // Agreeing to LINEMODE (server DO → our WILL)
                                // starts it in the cooked default (EDIT on)
                                // until a MODE says otherwise; DONT ends it.
                                if neg.option == op_option::LINEMODE {
                                    let active = neg.command == op_command::DO;
                                    if active != linemode_active {
                                        linemode_active = active;
                                        if active {
                                            linemode_edit = true;
                                        }
                                        let evt = Event::LineMode {
                                            active,
                                            edit: linemode_edit,
                                        };
                                        if events.send(evt).await.is_err() {
                                            return;
                                        }
                                    }
                                }
                                let event = Event::Negotiation {
                                    command: neg.command,
                                    option: neg.option,
                                };
                                if events.send(event).await.is_err() {
                                    return;
                                }
                            }
                            TelnetEvents::Subnegotiation(sub) => {
                                if sub.option == op_option::LINEMODE {
                                    let (reply, new_edit) =
                                        linemode_reply(&mut parser, &sub.buffer);
                                    if let Some(reply) = reply
                                        && write_or_close(&mut writer, &reply.to_bytes(), &events)
                                            .await
                                            .is_err()
                                    {
                                        return;
                                    }
                                    if let Some(edit) = new_edit
                                        && edit != linemode_edit
                                    {
                                        linemode_edit = edit;
                                        let evt = Event::LineMode {
                                            active: linemode_active,
                                            edit,
                                        };
                                        if events.send(evt).await.is_err() {
                                            return;
                                        }
                                    }
                                } else if let Some(reply) = suboption_reply(
                                    &mut parser,
                                    &opts,
                                    sub.option,
                                    &sub.buffer,
                                    &mut ttype_sends,
                                ) && write_or_close(&mut writer, &reply.to_bytes(), &events)
                                    .await
                                    .is_err()
                                {
                                    return;
                                }
                            }
                            TelnetEvents::IAC(_) => {}
                            TelnetEvents::DecompressImmediate(_) => {
                                // MCCP is a MUD extension; not part of GNU telnet.
                            }
                        }
                    }
                }
                Err(err) => {
                    let _ = events.send(Event::Closed(Some(err.to_string()))).await;
                    return;
                }
            },
            cmd = commands.recv() => match cmd {
                Some(Command::Send(data)) => {
                    let escaped = Parser::escape_iac(data);
                    if write_or_close(&mut writer, &escaped, &events).await.is_err() {
                        return;
                    }
                }
                Some(Command::SendIac(command)) => {
                    let raw = [op_command::IAC, command];
                    if write_or_close(&mut writer, &raw, &events).await.is_err() {
                        return;
                    }
                }
                Some(Command::Resize { cols, rows }) => {
                    size = (cols, rows);
                    // Returns None until the server has accepted NAWS.
                    if let Some(sub) = naws_subnegotiation(&mut parser, size)
                        && write_or_close(&mut writer, &sub.to_bytes(), &events).await.is_err() {
                            return;
                        }
                }
                Some(Command::LineModeRequest { edit }) => {
                    // Only meaningful while LINEMODE is active; the server
                    // confirms (or amends) with an ACK we adopt above.
                    if linemode_active
                        && let Some(sub) = linemode_mode_request(&mut parser, edit)
                        && write_or_close(&mut writer, &sub.to_bytes(), &events).await.is_err()
                    {
                        return;
                    }
                }
                Some(Command::Close) | None => {
                    let _ = events.send(Event::Closed(None)).await;
                    return;
                }
            },
        }
    }
}

async fn write_or_close(
    writer: &mut (impl AsyncWrite + Unpin),
    data: &[u8],
    events: &mpsc::Sender<Event>,
) -> Result<(), ()> {
    if let Err(err) = writer.write_all(data).await {
        let _ = events.send(Event::Closed(Some(err.to_string()))).await;
        return Err(());
    }
    Ok(())
}

fn naws_subnegotiation(parser: &mut Parser, (cols, rows): (u16, u16)) -> Option<TelnetEvents> {
    let payload = [(cols >> 8) as u8, cols as u8, (rows >> 8) as u8, rows as u8];
    parser.subnegotiation(op_option::NAWS, Bytes::copy_from_slice(&payload))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn tls_telnet_with_tofu_pinning() {
        use std::sync::Arc;
        use tokio_rustls::TlsAcceptor;
        use tokio_rustls::rustls::ServerConfig;
        use tokio_rustls::rustls::pki_types::PrivateKeyDer;

        // Pin store goes to a temp file; provider must exist before any
        // rustls config is built in this test.
        unsafe {
            std::env::set_var(
                "TRUST_KNOWN_HOSTS",
                std::env::temp_dir().join(format!("trust-test-kh-{}", std::process::id())),
            );
        }
        crate::tls::ensure_provider();

        let make_acceptor = || {
            let signed = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
            let key = PrivateKeyDer::try_from(signed.signing_key.serialize_der()).unwrap();
            let config = ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(vec![signed.cert.der().clone()], key)
                .unwrap();
            TlsAcceptor::from(Arc::new(config))
        };

        // Phase 1: a self-signed cert is accepted and pinned (TOFU), and
        // telnet flows through the TLS stream.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let acceptor = make_acceptor();
        let server = tokio::spawn(async move {
            let (sock, _) = listener.accept().await.unwrap();
            let mut tls = acceptor.accept(sock).await.unwrap();
            tls.write_all(b"\xff\xfb\x01secure login: ").await.unwrap(); // IAC WILL ECHO + text
            let mut buf = [0u8; 3];
            tls.read_exact(&mut buf).await.unwrap();
            assert_eq!(buf, [255, 253, 1]); // IAC DO ECHO came back through TLS
        });

        let (handle, mut events) = connect("localhost".into(), port, (80, 24), true);
        let event = events.recv().await.unwrap();
        assert!(
            matches!(event, Event::Connected { tls: true, .. }),
            "got {event:?}"
        );
        let event = events.recv().await.unwrap();
        assert!(matches!(event, Event::Negotiation { .. }), "got {event:?}");
        let event = events.recv().await.unwrap();
        match event {
            Event::Data(data) => assert_eq!(data, b"secure login: "),
            other => panic!("expected data, got {other:?}"),
        }
        server.await.unwrap();
        drop(handle);
        let _ = events.recv().await; // drain the close

        // Phase 2: the same host:port (pins are keyed by both) presenting
        // a *different* certificate is refused by the fingerprint pin.
        // The phase-1 listener is gone, so the port is free to rebind.
        let listener = TcpListener::bind(("127.0.0.1", port)).await.unwrap();
        let acceptor = make_acceptor(); // brand-new cert
        let server = tokio::spawn(async move {
            let (sock, _) = listener.accept().await.unwrap();
            let _ = acceptor.accept(sock).await; // handshake will fail
        });

        let (_handle, mut events) = connect("localhost".into(), port, (80, 24), true);
        loop {
            match events.recv().await.unwrap() {
                Event::Closed(Some(err)) => {
                    assert!(
                        err.contains("changed since first use"),
                        "unexpected error: {err}"
                    );
                    break;
                }
                Event::Closed(None) => panic!("connection closed without the pin error"),
                _ => {}
            }
        }
        let _ = server.await;
    }

    #[tokio::test]
    async fn negotiates_naws_and_delivers_data() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            sock.write_all(&[255, 253, 31]).await.unwrap(); // IAC DO NAWS
            sock.write_all(b"login: ").await.unwrap();
            // Expect IAC WILL NAWS plus the size subnegotiation (12 bytes).
            let mut wire = Vec::new();
            let mut buf = [0u8; 64];
            while wire.len() < 12 {
                let n = sock.read(&mut buf).await.unwrap();
                assert_ne!(n, 0, "client closed before finishing negotiation");
                wire.extend_from_slice(&buf[..n]);
            }
            wire
        });

        let (handle, mut events) = connect("127.0.0.1".into(), port, (80, 24), false);

        let event = events.recv().await.unwrap();
        assert!(matches!(event, Event::Connected { .. }), "got {event:?}");
        let event = events.recv().await.unwrap();
        assert!(
            matches!(
                event,
                Event::Negotiation {
                    command: op_command::DO,
                    option: op_option::NAWS,
                }
            ),
            "got {event:?}"
        );
        let event = events.recv().await.unwrap();
        match event {
            Event::Data(data) => assert_eq!(data, b"login: "),
            other => panic!("expected data, got {other:?}"),
        }

        let wire = server.await.unwrap();
        let expected = [
            255, 251, 31, // IAC WILL NAWS
            255, 250, 31, 0, 80, 0, 24, 255, 240, // IAC SB NAWS 0 80 0 24 IAC SE
        ];
        assert_eq!(wire, expected);

        handle.commands.send(Command::Close).await.unwrap();
        let event = events.recv().await.unwrap();
        assert!(matches!(event, Event::Closed(None)), "got {event:?}");
    }

    #[tokio::test]
    async fn answers_ttype_with_ansi_first() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 3];
            sock.write_all(&[255, 253, 24]).await.unwrap(); // IAC DO TTYPE
            sock.read_exact(&mut buf).await.unwrap();
            assert_eq!(buf, [255, 251, 24]); // IAC WILL TTYPE

            // IAC SB TTYPE SEND IAC SE, twice: expect ANSI then XTERM.
            let mut reply = [0u8; 10]; // IAC SB TTYPE IS "ANSI" IAC SE
            sock.write_all(&[255, 250, 24, 1, 255, 240]).await.unwrap();
            sock.read_exact(&mut reply).await.unwrap();
            assert_eq!(reply, *b"\xff\xfa\x18\x00ANSI\xff\xf0");
            let mut reply = [0u8; 11];
            sock.write_all(&[255, 250, 24, 1, 255, 240]).await.unwrap();
            sock.read_exact(&mut reply).await.unwrap();
            assert_eq!(reply, *b"\xff\xfa\x18\x00XTERM\xff\xf0");
        });

        let (handle, mut events) = connect("127.0.0.1".into(), port, (80, 24), false);
        let event = events.recv().await.unwrap();
        assert!(matches!(event, Event::Connected { .. }), "got {event:?}");

        server.await.unwrap();
        drop(handle);
    }

    #[tokio::test]
    async fn reports_remote_echo_transitions() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 3];
            sock.write_all(&[255, 251, 1]).await.unwrap(); // IAC WILL ECHO
            sock.read_exact(&mut buf).await.unwrap();
            assert_eq!(buf, [255, 253, 1]); // IAC DO ECHO
            sock.write_all(&[255, 252, 1]).await.unwrap(); // IAC WONT ECHO
            sock.read_exact(&mut buf).await.unwrap();
            assert_eq!(buf, [255, 254, 1]); // IAC DONT ECHO
        });

        let (handle, mut events) = connect("127.0.0.1".into(), port, (80, 24), false);

        let event = events.recv().await.unwrap();
        assert!(matches!(event, Event::Connected { .. }), "got {event:?}");
        let event = events.recv().await.unwrap();
        assert!(
            matches!(
                event,
                Event::Negotiation {
                    command: op_command::WILL,
                    option: op_option::ECHO,
                }
            ),
            "got {event:?}"
        );
        let event = events.recv().await.unwrap();
        assert!(
            matches!(
                event,
                Event::Negotiation {
                    command: op_command::WONT,
                    option: op_option::ECHO,
                }
            ),
            "got {event:?}"
        );

        server.await.unwrap();
        drop(handle);
    }

    #[tokio::test]
    async fn answers_tspeed_with_fixed_speed() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 3];
            sock.write_all(&[255, 253, 32]).await.unwrap(); // IAC DO TSPEED
            sock.read_exact(&mut buf).await.unwrap();
            assert_eq!(buf, [255, 251, 32]); // IAC WILL TSPEED

            // IAC SB TSPEED SEND IAC SE → IS "38400,38400"
            sock.write_all(&[255, 250, 32, 1, 255, 240]).await.unwrap();
            let mut reply = [0u8; 17];
            sock.read_exact(&mut reply).await.unwrap();
            assert_eq!(reply, *b"\xff\xfa\x20\x0038400,38400\xff\xf0");
        });

        let (handle, mut events) = connect("127.0.0.1".into(), port, (80, 24), false);
        let event = events.recv().await.unwrap();
        assert!(matches!(event, Event::Connected { .. }), "got {event:?}");

        server.await.unwrap();
        drop(handle);
    }

    #[tokio::test]
    async fn answers_new_environ_with_empty_is() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 3];
            sock.write_all(&[255, 253, 39]).await.unwrap(); // IAC DO NEW-ENVIRON
            sock.read_exact(&mut buf).await.unwrap();
            assert_eq!(buf, [255, 251, 39]); // IAC WILL NEW-ENVIRON

            // IAC SB NEW-ENVIRON SEND IAC SE → IS with no variables:
            // nothing from the local environment leaks to the server.
            sock.write_all(&[255, 250, 39, 1, 255, 240]).await.unwrap();
            let mut reply = [0u8; 6];
            sock.read_exact(&mut reply).await.unwrap();
            assert_eq!(reply, [255, 250, 39, 0, 255, 240]);
        });

        let (handle, mut events) = connect("127.0.0.1".into(), port, (80, 24), false);
        let event = events.recv().await.unwrap();
        assert!(matches!(event, Event::Connected { .. }), "got {event:?}");

        server.await.unwrap();
        drop(handle);
    }

    #[tokio::test]
    async fn answers_status_with_live_option_states() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 3];
            sock.write_all(&[255, 253, 5]).await.unwrap(); // IAC DO STATUS
            sock.read_exact(&mut buf).await.unwrap();
            assert_eq!(buf, [255, 251, 5]); // IAC WILL STATUS
            sock.write_all(&[255, 251, 1]).await.unwrap(); // IAC WILL ECHO
            sock.read_exact(&mut buf).await.unwrap();
            assert_eq!(buf, [255, 253, 1]); // IAC DO ECHO

            // IAC SB STATUS SEND IAC SE → IS DO ECHO, WILL STATUS
            // (ascending option order: ECHO=1 then STATUS=5).
            sock.write_all(&[255, 250, 5, 1, 255, 240]).await.unwrap();
            let mut reply = [0u8; 10];
            sock.read_exact(&mut reply).await.unwrap();
            assert_eq!(reply, [255, 250, 5, 0, 253, 1, 251, 5, 255, 240]);
        });

        let (handle, mut events) = connect("127.0.0.1".into(), port, (80, 24), false);
        let event = events.recv().await.unwrap();
        assert!(matches!(event, Event::Connected { .. }), "got {event:?}");

        server.await.unwrap();
        drop(handle);
    }

    #[tokio::test]
    async fn accepts_lflow_and_ignores_its_toggles() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 3];
            sock.write_all(&[255, 253, 33]).await.unwrap(); // IAC DO LFLOW
            sock.read_exact(&mut buf).await.unwrap();
            assert_eq!(buf, [255, 251, 33]); // IAC WILL LFLOW

            // IAC SB LFLOW OFF IAC SE expects no reply (RFC 1372) and
            // must not pause anything — data after it still flows.
            sock.write_all(&[255, 250, 33, 0, 255, 240]).await.unwrap();
            sock.write_all(b"still here").await.unwrap();
        });

        let (handle, mut events) = connect("127.0.0.1".into(), port, (80, 24), false);
        let event = events.recv().await.unwrap();
        assert!(matches!(event, Event::Connected { .. }), "got {event:?}");
        let event = events.recv().await.unwrap();
        assert!(
            matches!(
                event,
                Event::Negotiation {
                    command: op_command::DO,
                    option: op_option::LFLOW,
                }
            ),
            "got {event:?}"
        );
        let event = events.recv().await.unwrap();
        match event {
            Event::Data(data) => assert_eq!(data, b"still here"),
            other => panic!("expected data, got {other:?}"),
        }

        server.await.unwrap();
        drop(handle);
    }

    #[tokio::test]
    async fn send_iac_goes_out_unescaped() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 2];
            sock.read_exact(&mut buf).await.unwrap();
            assert_eq!(buf, [255, 243]); // IAC BRK
        });

        let (handle, mut events) = connect("127.0.0.1".into(), port, (80, 24), false);
        let event = events.recv().await.unwrap();
        assert!(matches!(event, Event::Connected { .. }), "got {event:?}");

        handle.commands.send(Command::SendIac(243)).await.unwrap();
        server.await.unwrap();
        drop(handle);
    }

    #[tokio::test]
    async fn linemode_acks_mode_and_reports_edit() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 3];
            sock.write_all(&[255, 253, 34]).await.unwrap(); // IAC DO LINEMODE
            sock.read_exact(&mut buf).await.unwrap();
            assert_eq!(buf, [255, 251, 34]); // IAC WILL LINEMODE

            // MODE EDIT|TRAPSIG (0x03) → echo the mask back with MODE_ACK set.
            sock.write_all(&[255, 250, 34, 1, 0x03, 255, 240])
                .await
                .unwrap();
            let mut reply = [0u8; 7];
            sock.read_exact(&mut reply).await.unwrap();
            assert_eq!(reply, [255, 250, 34, 1, 0x07, 255, 240]); // MODE 0x03|ACK

            // MODE with EDIT clear (character-at-a-time) → MODE_ACK only.
            sock.write_all(&[255, 250, 34, 1, 0x00, 255, 240])
                .await
                .unwrap();
            let mut reply = [0u8; 7];
            sock.read_exact(&mut reply).await.unwrap();
            assert_eq!(reply, [255, 250, 34, 1, 0x04, 255, 240]); // MODE ACK, EDIT clear
        });

        let (handle, mut events) = connect("127.0.0.1".into(), port, (80, 24), false);
        let event = events.recv().await.unwrap();
        assert!(matches!(event, Event::Connected { .. }), "got {event:?}");

        // Agreeing to LINEMODE reports it active in the cooked default (EDIT on).
        let event = events.recv().await.unwrap();
        assert!(
            matches!(
                event,
                Event::LineMode {
                    active: true,
                    edit: true
                }
            ),
            "got {event:?}"
        );
        let event = events.recv().await.unwrap();
        assert!(
            matches!(
                event,
                Event::Negotiation {
                    command: op_command::DO,
                    option: op_option::LINEMODE,
                }
            ),
            "got {event:?}"
        );
        // MODE EDIT|TRAPSIG keeps EDIT on (no change → no event); the MODE
        // clear flips us into character mode.
        let event = events.recv().await.unwrap();
        assert!(
            matches!(
                event,
                Event::LineMode {
                    active: true,
                    edit: false
                }
            ),
            "got {event:?}"
        );

        server.await.unwrap();
        drop(handle);
    }

    #[tokio::test]
    async fn linemode_acks_unacknowledged_slc_triples() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 3];
            sock.write_all(&[255, 253, 34]).await.unwrap(); // IAC DO LINEMODE
            sock.read_exact(&mut buf).await.unwrap();
            assert_eq!(buf, [255, 251, 34]); // IAC WILL LINEMODE

            // SLC list: two un-acked triples plus one already carrying SLC_ACK.
            // Only the un-acked pair is echoed back with SLC_ACK (0x80) set;
            // the already-acked triple is the server confirming our reply and
            // gets no answer (this terminates the handshake).
            sock.write_all(&[
                255, 250, 34, 3, // IAC SB LINEMODE SLC
                3, 3, 3, // (func 3, SLC_DEFAULT, char 3)
                4, 2, 4, // (func 4, SLC_VALUE, char 4)
                5, 0x82, 21, // (func 5, SLC_VALUE|ACK, char 21) — already acked
                255, 240, // IAC SE
            ])
            .await
            .unwrap();
            let mut reply = [0u8; 12];
            sock.read_exact(&mut reply).await.unwrap();
            assert_eq!(
                reply,
                [
                    255, 250, 34, 3, // IAC SB LINEMODE SLC
                    3, 0x83, 3, // func 3, SLC_DEFAULT|ACK
                    4, 0x82, 4, // func 4, SLC_VALUE|ACK
                    255, 240, // IAC SE
                ]
            );
        });

        let (handle, mut events) = connect("127.0.0.1".into(), port, (80, 24), false);
        let event = events.recv().await.unwrap();
        assert!(matches!(event, Event::Connected { .. }), "got {event:?}");

        server.await.unwrap();
        drop(handle);
    }

    #[tokio::test]
    async fn linemode_declines_forwardmask() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 3];
            sock.write_all(&[255, 253, 34]).await.unwrap(); // IAC DO LINEMODE
            sock.read_exact(&mut buf).await.unwrap();
            assert_eq!(buf, [255, 251, 34]); // IAC WILL LINEMODE

            // DO FORWARDMASK <mask> → we refuse with WONT FORWARDMASK.
            sock.write_all(&[255, 250, 34, 253, 2, 0, 0, 0, 0, 255, 240])
                .await
                .unwrap();
            let mut reply = [0u8; 7];
            sock.read_exact(&mut reply).await.unwrap();
            assert_eq!(reply, [255, 250, 34, 252, 2, 255, 240]); // WONT FORWARDMASK
        });

        let (handle, mut events) = connect("127.0.0.1".into(), port, (80, 24), false);
        let event = events.recv().await.unwrap();
        assert!(matches!(event, Event::Connected { .. }), "got {event:?}");

        server.await.unwrap();
        drop(handle);
    }

    #[tokio::test]
    async fn linemode_request_sends_a_mode_subnegotiation() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 3];
            sock.write_all(&[255, 253, 34]).await.unwrap(); // IAC DO LINEMODE
            sock.read_exact(&mut buf).await.unwrap();
            assert_eq!(buf, [255, 251, 34]); // IAC WILL LINEMODE

            // The client's `mode character` request: MODE EDIT clear, no ACK.
            let mut reply = [0u8; 7];
            sock.read_exact(&mut reply).await.unwrap();
            assert_eq!(reply, [255, 250, 34, 1, 0, 255, 240]);
        });

        let (handle, mut events) = connect("127.0.0.1".into(), port, (80, 24), false);
        // The request is dropped unless LINEMODE is active, so wait for it.
        loop {
            match events.recv().await.unwrap() {
                Event::LineMode { active: true, .. } => break,
                Event::Closed(_) => panic!("closed before LINEMODE activated"),
                _ => {}
            }
        }
        handle
            .commands
            .send(Command::LineModeRequest { edit: false })
            .await
            .unwrap();

        server.await.unwrap();
        drop(handle);
    }
}
