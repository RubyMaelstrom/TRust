//! Gemini protocol: one TLS request/response, gemtext documents.
//!
//! A transaction is: TLS-connect (SNI required, TOFU certs — see
//! `tls.rs`), send `gemini://host/path\r\n`, read a `<status> <meta>`
//! header line, then for 2x responses the body until close. Redirects
//! (3x) are followed here in the fetch task, capped to avoid loops.

use std::fmt;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::doc::{Doc, DocLine, Kind, Link, push_wrapped};
use crate::tls;

const MAX_BODY: usize = 2 * 1024 * 1024;
const FETCH_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_REDIRECTS: usize = 5;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GeminiUrl {
    pub host: String,
    pub port: u16,
    /// Absolute path, always starting with `/`; may carry a `?query`.
    pub path: String,
}

impl fmt::Display for GeminiUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "gemini://{}", self.host)?;
        if self.port != 1965 {
            write!(f, ":{}", self.port)?;
        }
        f.write_str(&self.path)
    }
}

impl GeminiUrl {
    /// Parse an absolute `gemini://host[:port][/path][?query]` URL.
    pub fn parse(s: &str) -> Option<Self> {
        let rest = s.strip_prefix("gemini://")?;
        let (authority, path) = match rest.find(['/', '?']) {
            Some(i) if rest.as_bytes()[i] == b'/' => (&rest[..i], rest[i..].to_string()),
            Some(i) => (&rest[..i], format!("/{}", &rest[i..])),
            None => (rest, String::from("/")),
        };
        if authority.is_empty() {
            return None;
        }
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) if !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) => {
                (host, port.parse().ok()?)
            }
            _ => (authority, 1965),
        };
        Some(Self {
            host: host.to_string(),
            port,
            path,
        })
    }

    /// The directory part of the path (through the final `/`), with any
    /// query stripped — the base for relative references.
    fn directory(&self) -> &str {
        let path = self.path.split('?').next().unwrap_or("/");
        match path.rfind('/') {
            Some(i) => &path[..=i],
            None => "/",
        }
    }
}

/// Interpret a target as an absolute URL of any scheme, if it is one:
/// gemini/gopher links are followable, everything else (`http:`,
/// `mailto:`, ...) is External. Relative references return None.
pub fn absolute_link(target: &str) -> Option<Link> {
    if let Some(url) = GeminiUrl::parse(target) {
        return Some(Link::Gemini(url));
    }
    if let Some(url) = crate::gopher::GopherUrl::parse(target) {
        return Some(Link::Gopher(url));
    }
    if let Some(url) = crate::http::parse_url(target) {
        return Some(Link::Http(url));
    }
    let colon = target.find(':')?;
    let scheme = &target[..colon];
    let valid_scheme = !scheme.is_empty()
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c))
        && !scheme.contains('/');
    // `scheme://...` is always absolute; `scheme:...` (mailto:) counts
    // when the colon comes before any slash.
    if valid_scheme && (target[colon..].starts_with("://") || !target[..colon].contains('.')) {
        return Some(Link::External(target.to_string()));
    }
    None
}

/// Resolve a gemtext link or redirect target against the current page,
/// per the RFC 3986 relative-reference rules gemini borrows.
pub fn resolve(base: &GeminiUrl, target: &str) -> Link {
    if let Some(link) = absolute_link(target) {
        return link;
    }

    let mut url = base.clone();
    if let Some(rest) = target.strip_prefix("//") {
        // Network-path reference: new authority, same scheme.
        return match GeminiUrl::parse(&format!("gemini://{rest}")) {
            Some(url) => Link::Gemini(url),
            None => Link::External(target.to_string()),
        };
    }
    url.path = if let Some(target) = target.strip_prefix('/') {
        normalize(&format!("/{target}"))
    } else {
        normalize(&format!("{}{}", base.directory(), target))
    };
    Link::Gemini(url)
}

/// Remove `.` and `..` segments (RFC 3986 §5.2.4), preserving a query.
pub(crate) fn normalize(path: &str) -> String {
    let (path, query) = match path.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (path, None),
    };
    let mut out: Vec<&str> = Vec::new();
    let trailing_slash = path.ends_with('/') || path.ends_with("/.") || path.ends_with("/..");
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            seg => out.push(seg),
        }
    }
    let mut result = format!("/{}", out.join("/"));
    if trailing_slash && result.len() > 1 {
        result.push('/');
    }
    if let Some(query) = query {
        result.push('?');
        result.push_str(query);
    }
    result
}

/// Percent-encode a user query for a 1x input prompt.
pub fn encode_query(query: &str) -> String {
    let mut out = String::new();
    for byte in query.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// A gemini response: header (always) plus body (2x only).
#[derive(Debug)]
pub struct Response {
    /// The URL that finally answered, after redirects.
    pub url: GeminiUrl,
    pub status: u8,
    pub meta: String,
    pub body: Vec<u8>,
}

/// Fetch a URL, following up to `MAX_REDIRECTS` 3x redirects.
pub async fn fetch(url: &GeminiUrl) -> Result<Response, String> {
    let mut url = url.clone();
    for _ in 0..=MAX_REDIRECTS {
        let response = tokio::time::timeout(FETCH_TIMEOUT, fetch_once(&url))
            .await
            .map_err(|_| String::from("timed out"))??;
        if (30..40).contains(&response.status) {
            match resolve(&url, response.meta.trim()) {
                Link::Gemini(next) => {
                    url = next;
                    continue;
                }
                other => return Err(format!("redirect leaves geminispace: {other}")),
            }
        }
        return Ok(response);
    }
    Err(format!("too many redirects (>{MAX_REDIRECTS})"))
}

async fn fetch_once(url: &GeminiUrl) -> Result<Response, String> {
    let stream = TcpStream::connect((url.host.as_str(), url.port))
        .await
        .map_err(|e| e.to_string())?;
    let _ = stream.set_nodelay(true);
    let name = tls::server_name(&url.host)?;
    let mut stream = tls::connector(&url.host, url.port)
        .connect(name, stream)
        .await
        .map_err(|e| format!("TLS: {e}"))?;

    stream
        .write_all(format!("{url}\r\n").as_bytes())
        .await
        .map_err(|e| e.to_string())?;

    // Header: `<status><SP><meta>\r\n`, at most 1024 bytes of meta.
    let mut header = Vec::new();
    let mut byte = [0u8; 1];
    while !header.ends_with(b"\r\n") {
        if header.len() > 1100 {
            return Err(String::from("malformed response header (too long)"));
        }
        match stream.read(&mut byte).await.map_err(|e| e.to_string())? {
            0 => return Err(String::from("connection closed before header")),
            _ => header.push(byte[0]),
        }
    }
    let header = String::from_utf8_lossy(&header[..header.len() - 2]).into_owned();
    let (status, meta) = match header.split_once(' ') {
        Some((s, meta)) => (s, meta.trim().to_string()),
        None => (header.as_str(), String::new()),
    };
    let status: u8 = status
        .parse()
        .map_err(|_| format!("malformed status line: {header:?}"))?;

    let mut body = Vec::new();
    if (20..30).contains(&status) {
        let mut buf = [0u8; 8192];
        loop {
            // Plenty of real servers close without a TLS close_notify;
            // treat that as EOF rather than an error.
            let n = match stream.read(&mut buf).await {
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => 0,
                Err(e) => return Err(e.to_string()),
            };
            if n == 0 {
                break;
            }
            body.extend_from_slice(&buf[..n]);
            if body.len() > MAX_BODY {
                return Err(String::from("response exceeds 2 MB cap"));
            }
        }
    }
    Ok(Response {
        url: url.clone(),
        status,
        meta,
        body,
    })
}

/// Parse a successful response body into a document. Gemtext gets full
/// treatment; other text/* render as plain text; anything else gets a
/// placeholder line.
pub fn parse(url: &GeminiUrl, meta: &str, body: &[u8], width: usize) -> Doc {
    let width = width.max(10);
    let media = meta.split(';').next().unwrap_or("").trim();
    let lines = if media.is_empty() || media == "text/gemini" {
        parse_gemtext(body, width, &|target| resolve(url, target))
    } else if media.starts_with("text/") {
        String::from_utf8_lossy(body)
            .lines()
            .flat_map(|l| {
                let mut out = Vec::new();
                push_wrapped(&mut out, Kind::Text, l.trim_end().to_string(), None, width);
                out
            })
            .collect()
    } else {
        vec![DocLine {
            kind: Kind::Error,
            text: format!("unsupported media type: {meta}"),
            link: None,
        }]
    };
    Doc {
        url: Link::Gemini(url.clone()),
        lines,
        raw: body.to_vec(),
        wrapped_to: width,
        cp437: false,
        meta: Some(meta.to_string()),
    }
}

/// Parse gemtext into document lines. The resolver maps `=>` targets to
/// links — gemini pages resolve against their own URL, while `.gmi`
/// files served over gopher resolve relative targets to gopher
/// selectors (see `gopher::parse`).
pub fn parse_gemtext(
    body: &[u8],
    width: usize,
    resolve_link: &dyn Fn(&str) -> Link,
) -> Vec<DocLine> {
    let text = String::from_utf8_lossy(body);
    let mut lines = Vec::new();
    let mut pre = false;
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if line.starts_with("```") {
            // Toggle lines themselves are not rendered (alt text ignored).
            pre = !pre;
            continue;
        }
        if pre {
            // Preformatted content: never wrapped, never linked.
            lines.push(DocLine {
                kind: Kind::Pre,
                text: line.to_string(),
                link: None,
            });
        } else if let Some(rest) = line.strip_prefix("=>") {
            let rest = rest.trim_start();
            let (target, label) = match rest.split_once(char::is_whitespace) {
                Some((t, l)) => (t, l.trim()),
                None => (rest, ""),
            };
            if target.is_empty() {
                continue;
            }
            let link = resolve_link(target);
            let text = if label.is_empty() {
                target.to_string()
            } else {
                label.to_string()
            };
            push_wrapped(&mut lines, Kind::GemLink, text, Some(link), width);
        } else if let Some(rest) = line.strip_prefix("###") {
            push_wrapped(
                &mut lines,
                Kind::Heading(3),
                rest.trim().to_string(),
                None,
                width,
            );
        } else if let Some(rest) = line.strip_prefix("##") {
            push_wrapped(
                &mut lines,
                Kind::Heading(2),
                rest.trim().to_string(),
                None,
                width,
            );
        } else if let Some(rest) = line.strip_prefix('#') {
            push_wrapped(
                &mut lines,
                Kind::Heading(1),
                rest.trim().to_string(),
                None,
                width,
            );
        } else if let Some(rest) = line.strip_prefix("* ") {
            push_wrapped(
                &mut lines,
                Kind::List,
                format!("• {}", rest.trim()),
                None,
                width,
            );
        } else if let Some(rest) = line.strip_prefix('>') {
            push_wrapped(
                &mut lines,
                Kind::Quote,
                format!("▌ {}", rest.trim()),
                None,
                width,
            );
        } else {
            push_wrapped(&mut lines, Kind::Text, line.to_string(), None, width);
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fetches_over_tls_with_redirects_and_input() {
        use std::sync::Arc;
        use tokio::io::AsyncWriteExt as _;
        use tokio_rustls::TlsAcceptor;
        use tokio_rustls::rustls::ServerConfig;
        use tokio_rustls::rustls::pki_types::PrivateKeyDer;

        unsafe {
            std::env::set_var(
                "TRUST_KNOWN_HOSTS",
                std::env::temp_dir().join(format!("trust-test-kh-{}", std::process::id())),
            );
        }
        tls::ensure_provider();

        // One certificate for the whole test so the TOFU pin stays happy.
        // Host is 127.0.0.1 to avoid colliding with the telnet TLS test's
        // "localhost" pin (pins are per server name, process-wide).
        let signed = rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()]).unwrap();
        let key = PrivateKeyDer::try_from(signed.signing_key.serialize_der()).unwrap();
        let config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![signed.cert.der().clone()], key)
            .unwrap();
        let acceptor = TlsAcceptor::from(Arc::new(config));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        // Each gemini request is its own connection; serve until dropped.
        let server = tokio::spawn(async move {
            loop {
                let Ok((sock, _)) = listener.accept().await else {
                    return;
                };
                let Ok(mut stream) = acceptor.accept(sock).await else {
                    continue;
                };
                let mut req = Vec::new();
                let mut byte = [0u8; 1];
                while !req.ends_with(b"\r\n") {
                    match stream.read(&mut byte).await {
                        Ok(1..) => req.push(byte[0]),
                        _ => break,
                    }
                }
                let req = String::from_utf8_lossy(&req);
                let path = req.trim().rsplit_once(":").map(|(_, hp)| {
                    hp.split_once('/')
                        .map(|(_, p)| format!("/{p}"))
                        .unwrap_or_default()
                });
                let reply: &[u8] = match path.as_deref() {
                    Some("/") => b"20 text/gemini\r\n# Welcome\n=> /next Next page\n",
                    Some("/redir") => b"31 /target\r\n",
                    Some("/target") => b"20 text/plain\r\nplain body",
                    Some("/ask") => b"10 What is your handle?\r\n",
                    _ => b"51 Not found\r\n",
                };
                let _ = stream.write_all(reply).await;
                let _ = stream.shutdown().await;
            }
        });

        let url = |path: &str| GeminiUrl {
            host: String::from("127.0.0.1"),
            port,
            path: path.to_string(),
        };

        // Success: header parsed, gemtext body delivered.
        let response = fetch(&url("/")).await.unwrap();
        assert_eq!(
            (response.status, response.meta.as_str()),
            (20, "text/gemini")
        );
        let doc = parse(&response.url, &response.meta, &response.body, 80);
        assert_eq!(doc.lines[0].kind, Kind::Heading(1));
        assert_eq!(doc.lines[1].text, "Next page");
        assert!(matches!(doc.lines[1].link, Some(Link::Gemini(_))));

        // Redirect: followed to /target, final URL reported.
        let response = fetch(&url("/redir")).await.unwrap();
        assert_eq!(response.status, 20);
        assert_eq!(response.url.path, "/target");
        assert_eq!(response.meta, "text/plain");
        assert_eq!(response.body, b"plain body");

        // 1x input status comes back without a body.
        let response = fetch(&url("/ask")).await.unwrap();
        assert_eq!(
            (response.status, response.meta.as_str()),
            (10, "What is your handle?")
        );
        assert!(response.body.is_empty());

        server.abort();
    }

    #[test]
    fn parses_urls() {
        let url = GeminiUrl::parse("gemini://example.org").unwrap();
        assert_eq!((url.port, url.path.as_str()), (1965, "/"));
        let url = GeminiUrl::parse("gemini://example.org:1966/foo/bar?q").unwrap();
        assert_eq!((url.port, url.path.as_str()), (1966, "/foo/bar?q"));
        assert!(GeminiUrl::parse("gopher://example.org").is_none());
        assert!(GeminiUrl::parse("gemini://").is_none());
    }

    #[test]
    fn resolves_relative_references() {
        let base = GeminiUrl::parse("gemini://e.org/dir/page.gmi").unwrap();
        let gem = |s: &str| match resolve(&base, s) {
            Link::Gemini(u) => u.to_string(),
            other => panic!("expected gemini link, got {other:?}"),
        };
        assert_eq!(gem("other.gmi"), "gemini://e.org/dir/other.gmi");
        assert_eq!(gem("/top.gmi"), "gemini://e.org/top.gmi");
        assert_eq!(gem("../up.gmi"), "gemini://e.org/up.gmi");
        assert_eq!(gem("./same.gmi"), "gemini://e.org/dir/same.gmi");
        assert_eq!(gem("sub/"), "gemini://e.org/dir/sub/");
        assert_eq!(gem("//other.host/x"), "gemini://other.host/x");
        assert_eq!(gem("gemini://abs.host:1966/y"), "gemini://abs.host:1966/y");
        assert!(matches!(
            resolve(&base, "https://example.com/"),
            Link::Http(_)
        ));
        assert!(matches!(
            resolve(&base, "mailto:sister@night.city"),
            Link::External(_)
        ));
        assert!(matches!(
            resolve(&base, "gopher://floodgap.com/"),
            Link::Gopher(_)
        ));
    }

    #[test]
    fn parses_gemtext() {
        let url = GeminiUrl::parse("gemini://e.org/dir/").unwrap();
        let body = b"# Title\n\
                     plain paragraph\n\
                     => /abs Label here\n\
                     => rel.gmi\n\
                     * item\n\
                     > wisdom\n\
                     ```alt text\n\
                     ascii  art   with   spacing\n\
                     => not/a/link inside pre\n\
                     ```\n\
                     after";
        let doc = parse(&url, "text/gemini", body, 80);
        let kinds: Vec<Kind> = doc.lines.iter().map(|l| l.kind).collect();
        assert_eq!(
            kinds,
            [
                Kind::Heading(1),
                Kind::Text,
                Kind::GemLink,
                Kind::GemLink,
                Kind::List,
                Kind::Quote,
                Kind::Pre,
                Kind::Pre,
                Kind::Text,
            ]
        );
        assert_eq!(doc.lines[0].text, "Title");
        assert_eq!(doc.lines[2].text, "Label here");
        assert_eq!(
            doc.lines[2].link,
            Some(Link::Gemini(
                GeminiUrl::parse("gemini://e.org/abs").unwrap()
            ))
        );
        // Bare-target link uses the target as its label.
        assert_eq!(doc.lines[3].text, "rel.gmi");
        // Pre block content is untouched (no link parsing, no wrap).
        assert_eq!(doc.lines[7].text, "=> not/a/link inside pre");
        assert!(doc.lines[7].link.is_none());
    }

    #[test]
    fn pre_blocks_are_never_wrapped() {
        let url = GeminiUrl::parse("gemini://e.org/").unwrap();
        let long = "x".repeat(200);
        let body = format!("```\n{long}\n```\n{long}");
        let doc = parse(&url, "text/gemini", body.as_bytes(), 40);
        assert_eq!(doc.lines[0].text.len(), 200, "pre line untouched");
        assert!(
            doc.lines[1..].iter().all(|l| l.text.chars().count() <= 40),
            "regular text wraps"
        );
    }

    #[test]
    fn encodes_queries() {
        assert_eq!(encode_query("hello world&more"), "hello%20world%26more");
        assert_eq!(encode_query("safe-chars_1.2~"), "safe-chars_1.2~");
    }
}
