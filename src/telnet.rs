//! Telnet connection task: owns the socket and the protocol state machine.
//!
//! The app talks to a connection through a pair of mpsc channels. All telnet
//! protocol bytes (IAC sequences, negotiation, subnegotiation) are produced
//! and consumed here; the app only ever sees decoded application data.

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
    // TODO for GNU telnet parity: TSPEED (RFC 1079), LFLOW (RFC 1372),
    // LINEMODE (RFC 1184), NEW-ENVIRON (RFC 1572), STATUS (RFC 859).
    table
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
        let stream = match tls::connector().connect(name, stream).await {
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
                                let event = Event::Negotiation {
                                    command: neg.command,
                                    option: neg.option,
                                };
                                if events.send(event).await.is_err() {
                                    return;
                                }
                            }
                            TelnetEvents::Subnegotiation(sub) => {
                                // TTYPE SEND (RFC 1091) — answer IS <name>.
                                if sub.option == op_option::TTYPE
                                    && sub.buffer.first() == Some(&op_command::SEND)
                                    && let Some(reply) = ttype_reply(&mut parser, &mut ttype_sends)
                                    && write_or_close(&mut writer, &reply.to_bytes(), &events)
                                        .await
                                        .is_err()
                                {
                                    return;
                                }
                                // TODO: STATUS and LINEMODE subnegotiations for
                                // GNU telnet parity.
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

        // Make sure a crypto provider is installed before building any
        // rustls config in this test.
        let _ = crate::tls::connector();

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

        // Phase 2: the same host presenting a *different* certificate is
        // refused by the fingerprint pin.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
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
}
