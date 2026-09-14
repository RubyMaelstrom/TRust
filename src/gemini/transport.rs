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

async fn open(url: &GeminiUrl) -> Result<(Stream, Response), String> {
    let request = url.request()?; // validate before DNS, TLS, or identity access
    let sock = TcpStream::connect((url.host.as_str(), url.port))
        .await
        .map_err(|e| e.to_string())?;
    let _ = sock.set_nodelay(true);
    let configured =
        tls::identity_authorized(url) && tls::identity_path(&url.host).is_some_and(|p| p.exists());
    let (connector, presented) = tls::gemini_connector(url)?;
    let mut stream = connector
        .connect(tls::server_name(&url.host)?, sock)
        .await
        .map_err(|e| format!("TLS: {e}"))?;
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
    let mut response = Response::new(url.clone(), status, meta);
    response.identity = presented.load(std::sync::atomic::Ordering::Relaxed);
    response.identity_configured = configured;
    if configured && !response.identity && status / 10 == 6 {
        response.notice = Some("The authorized identity was not requested during TLS authentication; the capsule cannot authenticate this connection.".into());
    }
    Ok((stream, response))
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
        let (mut stream, mut response) = timeout_at(initial, open(&url))
            .await
            .map_err(|_| "Gemini connection or header timed out")??;
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
    async fn configured_identity_is_distinct_from_certificate_actually_requested_by_tls() {
        unsafe {
            std::env::set_var(
                "TRUST_IDENTITIES",
                std::env::temp_dir().join(format!("trust-test-ids-{}", std::process::id())),
            );
        }
        let (url, server) = serve(b"60 Certificate required\r\n".to_vec(), vec![], false).await;
        tls::authorize_identity(&url, "scoped").unwrap();
        let response = fetch(&url).await.unwrap();
        assert!(response.identity_configured);
        assert!(!response.identity);
        assert!(
            !response.certificate_prompt(),
            "do not repeatedly offer an identity which TLS never requests"
        );
        assert!(response.notice.unwrap().contains("TLS"));
        server.await.unwrap();
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
        assert!(response.notice.as_deref().unwrap().contains("Incomplete"));
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
