//! Gemini 0.24.1 Requests, Responses and Closing connections. Limits below
//! are client resource budgets, not a historical 1,024-byte META restriction.

use super::{GeminiUrl, MediaType, Response, resolve_reference};
use crate::{doc::Link, tls};
use std::{future::Future, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpStream,
    time::{Instant, sleep_until, timeout_at},
};

const MAX_HEADER: usize = 8192;
const MAX_BODY: usize = 2 * 1024 * 1024;
type Stream = BufReader<tokio_rustls::client::TlsStream<TcpStream>>;

/// The unread body travels with the offer: saving must never replay a Gemini
/// transaction, whose query may represent a state-changing input submission.
pub(crate) struct Transfer {
    stream: Stream,
    prefix: Vec<u8>,
}
impl std::fmt::Debug for Transfer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Gemini response body")
    }
}
impl Transfer {
    pub(crate) async fn save(mut self, path: &std::path::Path) -> Result<u64, String> {
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .await
            .map_err(|e| e.to_string())?;
        let mut count = self.prefix.len() as u64;
        file.write_all(&self.prefix)
            .await
            .map_err(|e| e.to_string())?;
        let deadline = Instant::now() + Duration::from_secs(1800);
        let mut buf = [0; 65536];
        loop {
            let idle = Instant::now() + Duration::from_secs(30);
            let n = timeout_at(deadline.min(idle), self.stream.read(&mut buf))
                .await
                .map_err(|_| "Gemini download timed out")?
                .map_err(|e| format!("Incomplete Gemini download: {e}"))?;
            if n == 0 {
                break;
            }
            count += n as u64;
            if count > 2 * 1024 * 1024 * 1024 {
                return Err("Download exceeds 2 GiB limit".into());
            }
            file.write_all(&buf[..n]).await.map_err(|e| e.to_string())?;
        }
        file.flush().await.map_err(|e| e.to_string())?;
        file.sync_all().await.map_err(|e| e.to_string())?;
        Ok(count)
    }
}

fn header(bytes: &[u8]) -> Result<(u8, String), String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "Gemini header is not valid UTF-8")?;
    let code = bytes.get(..2).ok_or("Malformed Gemini status")?;
    if !(b'1'..=b'6').contains(&code[0]) || !code[1].is_ascii_digit() {
        return Err("Gemini status must be two digits between 10 and 69".into());
    }
    let status = (code[0] - b'0') * 10 + code[1] - b'0';
    let meta = if bytes.len() == 2 {
        ""
    } else {
        text[2..]
            .strip_prefix(' ')
            .ok_or("Gemini status requires a space separator")?
    };
    if meta
        .chars()
        .any(|c| c.is_control() && !(status / 10 == 2 && c == '\t'))
        || (bytes.len() > 2 && meta.is_empty())
    {
        return Err("Malformed Gemini META field".into());
    }
    match status / 10 {
        1 if meta.is_empty() => return Err("Gemini input response has no prompt".into()),
        2 => {
            MediaType::parse(meta)?;
        }
        3 if meta.is_empty() || !super::url::valid_reference(meta) => {
            return Err("Invalid Gemini redirect URI".into());
        }
        _ => {}
    }
    Ok((status, meta.to_string()))
}

/// A connection that answered, or a handshake TOFU stopped before sending.
enum Opened {
    Answered(Box<Stream>, Box<Response>),
    Stopped(Box<Response>),
}

async fn open(url: &GeminiUrl) -> Result<Opened, String> {
    use super::{security, tofu};
    let request = url.request()?; // validate before DNS, TLS, or identity access
    let sock = TcpStream::connect((url.host.as_str(), url.port))
        .await
        .map_err(|e| e.to_string())?;
    let _ = sock.set_nodelay(true);
    let tls = tls::gemini_connector(url)?;
    let handshake = tls
        .connector
        .connect(tls::server_name(&url.host)?, sock)
        .await;
    let observed = tls.observation();
    let stream = match handshake {
        Ok(stream) => stream,
        Err(error) => {
            // Gemini §TLS Server certificate validation: a different
            // certificate before the pinned one expires is potential MITM
            // evidence. Stop, send nothing, and let the user decide.
            if let Some((presented, tofu::Verdict::Changed(pinned))) = observed {
                let mut response = Response::new(url.clone(), 0, String::new());
                response.view.security = Some(security::Security::Changed(Box::new(
                    security::TrustWarning { presented, pinned },
                )));
                return Ok(Opened::Stopped(Box::new(response)));
            }
            return Err(format!("TLS: {error}"));
        }
    };
    let mut notices = Vec::new();
    // The handshake proved possession of the certificate's key (RFC 8446
    // §4.4.3); only now may a first use or renewal become the pin.
    let now = crate::tls::x509::now();
    match observed {
        Some((presented, tofu::Verdict::First)) => {
            if let Err(error) = tofu::trust(&presented, now) {
                notices.push(error);
            }
        }
        Some((presented, tofu::Verdict::Renewed(old))) => {
            notices.push(format!(
                "The capsule's certificate changed after the pinned one expired{}; the new certificate is now pinned.",
                old.not_after
                    .map_or_else(String::new, |t| format!(" on {}", crate::tls::x509::date(t)))
            ));
            if let Err(error) = tofu::trust(&presented, now) {
                notices.push(error);
            }
        }
        Some((_, tofu::Verdict::Known)) => {}
        _ => return Err("TLS: the capsule's certificate was not checked.".into()),
    }
    let tls12 = stream.get_ref().1.protocol_version()
        == Some(tokio_rustls::rustls::ProtocolVersion::TLSv1_2);
    let mut stream = stream;
    stream
        .write_all(&request)
        .await
        .map_err(|e| e.to_string())?;
    let mut stream = BufReader::new(stream);
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n") {
        if bytes.len() >= MAX_HEADER {
            return Err("Gemini header exceeds the 8 KiB client limit".into());
        }
        let byte = stream
            .read_u8()
            .await
            .map_err(|e| format!("Incomplete Gemini header: {e}"))?;
        if byte == b'\n' && bytes.last() != Some(&b'\r') {
            return Err("Gemini header requires CRLF".into());
        }
        bytes.push(byte);
    }
    let (status, meta) = header(&bytes[..bytes.len() - 2])?;
    let presented = tls.presented.load(std::sync::atomic::Ordering::Relaxed);
    let configured = tls.identity.is_some();
    let mut response = Response::new(url.clone(), status, meta);
    response.identity = presented;
    response.identity_configured = configured;
    response.identity_name = tls
        .identity
        .as_ref()
        .filter(|_| presented)
        .map(|identity| identity.name.clone());
    response.tls12 = tls12;
    if presented && tls12 {
        notices.push(
            "Your client certificate was sent over TLS 1.2, which does not encrypt it.".into(),
        );
    }
    if status / 10 == 6 {
        response.view.security = Some(security::Security::Certificate(Box::new(
            security::CertificateRequest::new(
                &response.url,
                status,
                &response.meta,
                tls.identity.as_ref().filter(|_| presented),
                configured && !presented,
                tls12,
            ),
        )));
        if configured && !presented {
            notices.push("The chosen identity was not requested during TLS authentication; the capsule cannot authenticate this connection.".into());
        }
    }
    if !notices.is_empty() {
        response.notice = Some(notices.join(" "));
    }
    Ok(Opened::Answered(Box::new(stream), Box::new(response)))
}

pub async fn fetch(url: &GeminiUrl) -> Result<Response, String> {
    fetch_updates(url, |_| async { true }).await
}

pub async fn fetch_updates<F, Fut>(url: &GeminiUrl, mut publish: F) -> Result<Response, String>
where
    F: FnMut(Response) -> Fut,
    Fut: Future<Output = bool>,
{
    let mut url = url.clone();
    let deadline = Instant::now() + Duration::from_secs(180);
    for redirects in 0..=5 {
        let initial = deadline.min(Instant::now() + Duration::from_secs(15));
        let (mut stream, mut response) = match timeout_at(initial, open(&url))
            .await
            .map_err(|_| "Gemini connection or header timed out")??
        {
            Opened::Answered(stream, response) => (*stream, *response),
            Opened::Stopped(response) => return Ok(*response),
        };
        if (30..40).contains(&response.status) {
            if let Link::Gemini(next) = resolve_reference(&url, &response.meta, true) {
                next.request()?;
                if redirects == 5 {
                    return Err("Gemini exceeded five redirects".into());
                }
                url = next;
                continue;
            }
            return Ok(response); // explicit, followable cross-protocol choice
        }
        if !(20..30).contains(&response.status) {
            return Ok(response);
        }
        let media = response.media_type()?;
        if !media.is_text() && !media.is_image() {
            response.download = Some(crate::download::DownloadOffer::from_gemini(
                &response.url,
                &response.meta,
                Transfer {
                    stream,
                    prefix: Vec::new(),
                },
            )?);
            return Ok(response);
        }
        let mut buf = [0; 8192];
        let mut dirty = false;
        let mut next_update = Instant::now();
        let mut idle = Instant::now() + Duration::from_secs(30);
        response.finished = false;
        let notice = loop {
            tokio::select! {
                result = timeout_at(deadline.min(idle), stream.read(&mut buf)) => {
                    match result {
                        Ok(Ok(0)) => break None,
                        Ok(Ok(n)) => {
                            let room = MAX_BODY.saturating_sub(response.body.len());
                            if n > room && media.is_image() {
                                response.body.extend_from_slice(&buf[..n]);
                                response.download = Some(crate::download::DownloadOffer::from_gemini(
                                    &response.url, &response.meta, Transfer { stream, prefix: std::mem::take(&mut response.body) })?);
                                response.finished = true;
                                return Ok(response);
                            }
                            response.body.extend_from_slice(&buf[..n.min(room)]);
                            if n > room { break Some("Partial response: reached the 2 MiB display limit; source contains the received prefix".into()); }
                            idle = Instant::now() + Duration::from_secs(30);
                            if !dirty { next_update = Instant::now() + Duration::from_millis(100); }
                            dirty = true;
                        }
                        Ok(Err(error)) => break Some(format!("Incomplete response: {error}")),
                        Err(_) => break Some("Incomplete response: server timed out".into()),
                    }
                }
                _ = sleep_until(next_update), if dirty && media.is_text() => {
                    if !timeout_at(deadline, publish(response.clone())).await
                        .map_err(|_| "Gemini update timed out")? { return Err("Gemini request cancelled".into()); }
                    dirty = false;
                }
            }
        };
        response.finished = true;
        response.notice = notice;
        if media.is_image() && crate::img::sniff(&response.body).is_none() {
            let mut offer = crate::download::DownloadOffer::from_bytes(
                ::url::Url::parse(&response.url.to_string()).map_err(|e| e.to_string())?,
                "gemini-image.bin".into(),
                std::mem::take(&mut response.body),
            );
            offer.content_type = response.meta.clone();
            response.download = Some(offer);
        }
        return Ok(response);
    }
    unreachable!()
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn serve(
        prefix: Vec<u8>,
        suffix: Vec<u8>,
        abrupt: bool,
    ) -> (GeminiUrl, tokio::task::JoinHandle<Vec<u8>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.3:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let acceptor = tls::tests::unverified_acceptor(tokio_rustls::rustls::ALL_VERSIONS);
            let mut stream = acceptor.accept(socket).await.unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n") {
                request.push(stream.read_u8().await.unwrap());
            }
            stream.write_all(&prefix).await.unwrap();
            if !suffix.is_empty() {
                tokio::time::sleep(Duration::from_millis(240)).await;
                stream.write_all(&suffix).await.unwrap();
            }
            if !abrupt {
                stream.shutdown().await.unwrap();
            }
            request
        });
        (GeminiUrl::new("127.0.0.3", port, "/response"), server)
    }

    #[tokio::test]
    // The lock serializes tests that share the identity directory; each
    // test owns its runtime, so holding it across awaits cannot deadlock.
    #[allow(clippy::await_holding_lock)]
    async fn configured_identity_is_distinct_from_certificate_actually_requested_by_tls() {
        let _lock = crate::gemini::identity::test_lock();
        let (url, server) = serve(b"60 Certificate required\r\n".to_vec(), vec![], false).await;
        let scoped = crate::gemini::identity::create(&url, "scoped").unwrap();
        let response = fetch(&url).await.unwrap();
        assert!(response.identity_configured);
        assert!(!response.identity);
        let Some(crate::gemini::security::Security::Certificate(request)) = &response.view.security
        else {
            panic!()
        };
        assert!(request.unrequested, "explain why no certificate was sent");
        assert!(response.notice.unwrap().contains("TLS"));
        server.await.unwrap();
        crate::gemini::identity::delete(&scoped.fingerprint).unwrap();
    }

    fn certificate(not_after: i32) -> (Vec<u8>, Vec<u8>) {
        let mut params = rcgen::CertificateParams::new(vec!["capsule.test".into()]).unwrap();
        params.not_before = rcgen::date_time_ymd(2020, 1, 1);
        params.not_after = rcgen::date_time_ymd(not_after, 1, 1);
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = params.self_signed(&key).unwrap();
        (cert.der().to_vec(), key.serialize_der())
    }

    fn acceptor(
        (cert, key): &(Vec<u8>, Vec<u8>),
        versions: &[&'static tokio_rustls::rustls::SupportedProtocolVersion],
    ) -> tokio_rustls::TlsAcceptor {
        tls::ensure_provider();
        let config = tokio_rustls::rustls::ServerConfig::builder_with_protocol_versions(versions)
            .with_no_client_auth()
            .with_single_cert(vec![cert.clone().into()], key.clone().try_into().unwrap())
            .unwrap();
        tokio_rustls::TlsAcceptor::from(std::sync::Arc::new(config))
    }

    #[tokio::test]
    async fn tofu_pins_first_use_stops_on_change_and_continues_after_explicit_trust() {
        use crate::gemini::{security, tofu};
        let first = certificate(2100);
        let second = certificate(2101);
        let listener = tokio::net::TcpListener::bind("127.0.0.4:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        // Connection n presents certificate `order[n]` and reports whether a
        // request arrived.
        let order = [first.clone(), first.clone(), second.clone(), second.clone()];
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            for cert in order {
                let (socket, _) = listener.accept().await.unwrap();
                let Ok(mut stream) = acceptor(&cert, tokio_rustls::rustls::ALL_VERSIONS)
                    .accept(socket)
                    .await
                else {
                    requests.push(false);
                    continue;
                };
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n") {
                    match stream.read_u8().await {
                        Ok(byte) => request.push(byte),
                        Err(_) => break,
                    }
                }
                requests.push(!request.is_empty());
                let _ = stream.write_all(b"20 text/plain\r\nok").await;
                let _ = stream.shutdown().await;
            }
            requests
        });
        let url = GeminiUrl::new("127.0.0.4", port, "/");
        // First use pins the certificate; using it again is quiet.
        for _ in 0..2 {
            let response = fetch(&url).await.unwrap();
            assert_eq!(
                (response.status, response.body.as_slice()),
                (20, &b"ok"[..])
            );
            assert!(response.notice.is_none());
        }
        // A different, unexpired certificate: stop before sending anything.
        let response = fetch(&url).await.unwrap();
        let Some(security::Security::Changed(warning)) = &response.view.security else {
            panic!(
                "expected a certificate-change decision, got {}",
                response.status_text()
            )
        };
        assert_eq!(
            warning.pinned.fingerprint,
            tofu::Presented::new("127.0.0.4", port, &first.0).fingerprint
        );
        assert!(response.status_text().contains("certificate changed"));
        let page = response.document(100);
        assert!(
            page.lines
                .iter()
                .any(|l| l.link == Some(security::Action::TrustChanged.link()))
        );
        // The user's explicit choice trusts exactly the certificate shown.
        let outcome = security::apply(
            &security::Action::TrustChanged,
            &security::Context {
                address: &crate::doc::Link::Gemini(url.clone()),
                security: response.view.security.as_ref(),
            },
        )
        .unwrap();
        assert!(matches!(outcome, security::Outcome::Navigate(_)));
        let response = fetch(&url).await.unwrap();
        assert_eq!(response.body, b"ok");
        assert_eq!(server.await.unwrap(), [true, true, false, true]);
    }

    #[tokio::test]
    async fn progressive_text_preserves_partial_content_and_reports_abrupt_tls_eof() {
        let (url, server) = serve(
            b"20 TEXT/GEMINI; charset=\"utf-8\"\r\n# First\n".to_vec(),
            b"Last\n".to_vec(),
            false,
        )
        .await;
        let mut updates = Vec::new();
        let response = fetch_updates(&url, |r| {
            updates.push(r);
            async { true }
        })
        .await
        .unwrap();
        assert!(
            updates
                .iter()
                .any(|r| !r.finished && r.body == b"# First\n")
        );
        assert_eq!(response.body, b"# First\nLast\n");
        assert!(response.finished && response.notice.is_none());
        server.await.unwrap();

        let (url, server) = serve(b"20 text/plain\r\npartial".to_vec(), vec![], true).await;
        let response = fetch(&url).await.unwrap();
        assert_eq!(response.body, b"partial");
        let notice = response.notice.as_deref().unwrap();
        assert!(notice.contains("Incomplete"));
        assert!(response.status_text().contains(notice));
        let mut doc = response.document(80);
        for width in [80, 40] {
            doc.rerender_reply(width);
            assert_eq!(doc.raw, b"partial");
            assert_eq!(doc.lines.len(), 1);
            assert_eq!(doc.lines[0].text, "partial");
        }
        server.await.unwrap();
    }

    #[tokio::test]
    async fn binary_save_consumes_original_response_and_never_replays_sensitive_input() {
        let body = vec![0xab; MAX_BODY + 8192];
        let (base, server) = serve(
            b"20 application/octet-stream\r\n".to_vec(),
            body.clone(),
            false,
        )
        .await;
        let url = base.with_input("private value", true).unwrap();
        let response = fetch(&url).await.unwrap();
        assert!(!format!("{response:?}").contains("private"));
        let offer = response.download.unwrap();
        assert!(offer.url.query().is_none());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("saved.bin");
        assert_eq!(
            crate::download::save(&offer, &path).await.unwrap(),
            body.len() as u64
        );
        assert_eq!(std::fs::read(&path).unwrap(), body);
        assert_eq!(server.await.unwrap(), url.request().unwrap());
        assert!(
            crate::download::save(&offer, &dir.path().join("second.bin"))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn text_limit_preserves_source_prefix_and_redirect_offers_actionable_link() {
        let (url, server) = serve(
            b"20 text/plain\r\n".to_vec(),
            vec![b'x'; MAX_BODY + 1],
            false,
        )
        .await;
        let response = fetch(&url).await.unwrap();
        assert_eq!(response.body.len(), MAX_BODY);
        assert!(response.notice.is_some());
        let _ = server.await;
        let (url, server) = serve(
            b"31 https://example.org/destination\r\n".to_vec(),
            vec![],
            false,
        )
        .await;
        let response = fetch(&url).await.unwrap();
        let doc = response.document(80);
        assert!(
            doc.lines
                .iter()
                .any(|l| matches!(&l.link, Some(Link::Http(u)) if u.path() == "/destination"))
        );
        server.await.unwrap();
    }
    #[test]
    fn status_grammar_and_client_header_budget() {
        for valid in [
            "50",
            "60",
            "10 Prompt",
            "11 Password",
            "22 TEXT/GEMINI; Charset=\"utf-8\"",
            "30 ../new",
            "69 Unknown",
            "20 text/gemini",
        ] {
            assert!(header(valid.as_bytes()).is_ok(), "{valid}");
        }
        for invalid in [
            "020 text/plain",
            "+20 text/plain",
            "99 Failure",
            "09 Failure",
            "20",
            "20 ",
            "10",
            "30 ",
            "30 a b",
            "30 /bad%xx",
            "30 /bad\\path",
            "50 ",
            "51\tOops",
            "51 a\rb",
            "51 a\u{85}b",
            "\u{feff}20 text/plain",
        ] {
            assert!(header(invalid.as_bytes()).is_err(), "{invalid:?}");
        }
        assert!(header(b"51 \xff").is_err());
        assert!(header(format!("51 {}", "x".repeat(2048)).as_bytes()).is_ok());
    }
}
