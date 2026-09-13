//! Gopher/Gophers transport; RFC 8446 §§2, 4.4.3 and 6.1.
//! Share the plain/TLS I/O adapter with HTTP and WebSockets, while keeping
//! Gopher's certificate policy, connection racing and request lifetime here.

use super::{CONNECT_TIMEOUT, GopherUrl, connect_addresses};
use crate::{http::Conn, tls};
use std::{
    io,
    pin::Pin,
    sync::{Arc, LazyLock},
    task::{Context, Poll, Waker},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadBuf},
    runtime::Handle,
    sync::Semaphore,
    time::{Instant, timeout, timeout_at},
};

static CLOSE_SLOTS: LazyLock<Arc<Semaphore>> = LazyLock::new(|| Arc::new(Semaphore::new(8)));

pub(crate) struct Connection {
    io: Option<Conn>,
    runtime: Handle,
}

impl Connection {
    fn io(&mut self) -> &mut Conn {
        self.io.as_mut().expect("live Gopher connection")
    }
}

impl AsyncRead for Connection {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(self.get_mut().io()).poll_read(cx, buf)
    }
}

impl AsyncWrite for Connection {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(self.get_mut().io()).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(self.get_mut().io()).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(self.get_mut().io()).poll_shutdown(cx)
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        let Some(mut io) = self.io.take() else { return };
        // RFC 8446 §6.1: send close_notify on completion or cancellation.
        // Usually one poll suffices. Bound any delayed flush without making
        // page display/navigation wait for the peer, as in HTTP's idle close.
        let mut cx = Context::from_waker(Waker::noop());
        if Pin::new(&mut io).poll_shutdown(&mut cx).is_pending()
            && let Ok(permit) = CLOSE_SLOTS.clone().try_acquire_owned()
        {
            self.runtime.spawn(async move {
                let _permit = permit;
                let _ = timeout(Duration::from_secs(1), io.shutdown()).await;
            });
        }
    }
}

pub(crate) async fn connect(url: &GopherUrl) -> Result<Connection, String> {
    connect_before(url, Instant::now() + CONNECT_TIMEOUT).await
}

async fn connect_before(url: &GopherUrl, deadline: Instant) -> Result<Connection, String> {
    let request = url.request()?;
    let stream = timeout_at(deadline, connect_addresses(&url.host, url.port))
        .await
        .map_err(|_| "Gopher connection timed out".to_string())?
        .map_err(|e| format!("Gopher connection failed: {e}"))?;
    let _ = stream.set_nodelay(true);
    let io = if url.tls {
        let name = tls::server_name(&url.host)?;
        let stream = timeout_at(deadline, tls::unverified_connector().connect(name, stream))
            .await
            .map_err(|_| "Gopher TLS handshake timed out".to_string())?
            .map_err(|e| format!("Gopher TLS handshake failed: {e}"))?;
        Conn::Tls(Box::new(stream))
    } else {
        Conn::Plain(stream)
    };
    let mut connection = Connection {
        io: Some(io),
        runtime: Handle::current(),
    };
    // Complete TLS before sending any selector/query. An explicit gophers
    // request never retries as plaintext after a failed handshake.
    timeout(CONNECT_TIMEOUT, async {
        connection.write_all(&request).await?;
        connection.flush().await
    })
    .await
    .map_err(|_| "Gopher request timed out".to_string())?
    .map_err(|e: io::Error| format!("Gopher request failed: {e}"))?;
    Ok(connection)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::{io::AsyncReadExt, net::TcpListener};

    #[tokio::test]
    async fn gophers_handshake_failure_never_retries_or_sends_plaintext_selector() {
        let listener = TcpListener::bind("127.0.0.3:0").await.unwrap();
        let url = GopherUrl::parse(&format!(
            "gophers://127.0.0.3:{}/0/PRIVATE-SELECTOR",
            listener.local_addr().unwrap().port()
        ))
        .unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut hello = [0; 8192];
            let n = stream.read(&mut hello).await.unwrap();
            assert_eq!(hello[0], 0x16, "first bytes must be TLS, not a selector");
            assert!(
                !hello[..n]
                    .windows(b"PRIVATE-SELECTOR".len())
                    .any(|w| w == b"PRIVATE-SELECTOR")
            );
            stream.write_all(b"not a TLS response\r\n").await.unwrap();
            drop(stream);
            assert!(
                timeout(Duration::from_millis(250), listener.accept())
                    .await
                    .is_err(),
                "unexpected plaintext retry"
            );
        });
        let error = connect(&url).await.err().unwrap();
        assert!(error.contains("TLS handshake failed"), "{error}");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn gophers_handshake_deadline_releases_the_socket() {
        let listener = TcpListener::bind("127.0.0.3:0").await.unwrap();
        let url = GopherUrl::parse(&format!(
            "gophers://127.0.0.3:{}/",
            listener.local_addr().unwrap().port()
        ))
        .unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            timeout(Duration::from_secs(2), stream.read_to_end(&mut bytes))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(bytes[0], 0x16);
        });
        let error = connect_before(&url, Instant::now() + Duration::from_millis(150))
            .await
            .err()
            .unwrap();
        assert!(error.contains("TLS handshake timed out"), "{error}");
        server.await.unwrap();
    }
}
