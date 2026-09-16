//! Shared Telnet protocol and cancellation-safe transport for both frontends.
//! See `protocol` for RFC 854 framing/Q negotiation and `session` for I/O.

use std::net::SocketAddr;

pub mod protocol;
mod session;
pub use protocol::{op_command, op_option};
pub use session::{CommandSender, Handle, SendError, connect};

#[derive(Debug)]
pub enum Command {
    /// Atomic application write. The transport performs NVT and IAC encoding.
    Send(Vec<u8>),
    SendIac(u8),
    Resize {
        cols: u16,
        rows: u16,
    },
    LineModeRequest {
        edit: bool,
    },
    Close,
}

#[derive(Debug)]
pub enum Event {
    Connected {
        peer: SocketAddr,
        tls: bool,
    },
    Data(Vec<u8>),
    /// Effective state change; DO/DONT are local, WILL/WONT are remote.
    Negotiation {
        command: u8,
        option: u8,
    },
    /// RFC 1184 MODE bits. EDIT and ECHO are independent.
    LineMode {
        active: bool,
        mode: u8,
    },
    /// SLC functions disabled by the server, one bit per RFC 1184 function.
    Slc {
        disabled: u32,
    },
    Closed(Option<String>),
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
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

            // IAC SB TTYPE SEND IAC SE, twice: expect ANSI then VT100.
            let mut reply = [0u8; 10]; // IAC SB TTYPE IS "ANSI" IAC SE
            sock.write_all(&[255, 250, 24, 1, 255, 240]).await.unwrap();
            sock.read_exact(&mut reply).await.unwrap();
            assert_eq!(reply, *b"\xff\xfa\x18\x00ANSI\xff\xf0");
            let mut reply = [0u8; 11];
            sock.write_all(&[255, 250, 24, 1, 255, 240]).await.unwrap();
            sock.read_exact(&mut reply).await.unwrap();
            assert_eq!(reply, *b"\xff\xfa\x18\x00VT100\xff\xf0");
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
