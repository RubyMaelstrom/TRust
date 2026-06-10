//! A deliberately small HTTP/1.1 client for the text web.
//!
//! One request per connection (`Connection: close`), no compression
//! (`Accept-Encoding: identity`), redirects followed here, chunked
//! transfer decoded after the fact since we read to EOF anyway. HTTPS
//! uses standard WebPKI validation (`tls::webpki_connector`), not TOFU.
//! HTML renders through html2text's rich mode into the shared document
//! model; JavaScript is a design non-goal.

use std::collections::HashMap;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use url::Url;

use crate::doc::{Doc, DocLine, Kind, Link};
use crate::tls;

const MAX_BODY: usize = 5 * 1024 * 1024;
const FETCH_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_REDIRECTS: usize = 10;
const USER_AGENT: &str = "TRust/0.1";

/// An HTTP request as the app sees it: method plus optional body.
#[derive(Clone, Debug)]
pub struct Request {
    pub method: String,
    pub url: Url,
    /// (content-type, payload) for POST and friends.
    pub body: Option<(String, Vec<u8>)>,
}

impl Request {
    pub fn get(url: Url) -> Self {
        Self {
            method: String::from("GET"),
            url,
            body: None,
        }
    }
}

#[derive(Debug)]
pub struct Response {
    /// The URL that finally answered, after redirects.
    pub url: Url,
    pub status: u16,
    pub content_type: String,
    pub body: Vec<u8>,
}

/// Parse an absolute http(s) URL.
pub fn parse_url(s: &str) -> Option<Url> {
    if !(s.starts_with("http://") || s.starts_with("https://")) {
        return None;
    }
    Url::parse(s).ok()
}

/// Fetch a request, following up to `MAX_REDIRECTS` redirects.
/// 301/302/303 turn into GET (dropping the body); 307/308 keep both.
pub async fn fetch(request: &Request) -> Result<Response, String> {
    let mut request = request.clone();
    for _ in 0..=MAX_REDIRECTS {
        let response = tokio::time::timeout(FETCH_TIMEOUT, fetch_once(&request))
            .await
            .map_err(|_| String::from("timed out"))??;
        match response.status {
            301 | 302 | 303 | 307 | 308 => {}
            _ => return Ok(response),
        }
        // Redirect: fetch_once stashes the Location header in
        // content_type for 3xx (their bodies are never rendered).
        let target = request
            .url
            .join(response.content_type.trim())
            .map_err(|e| format!("bad redirect location: {e}"))?;
        match target.scheme() {
            "http" | "https" => {}
            other => return Err(format!("redirect leaves the web: {other}://")),
        }
        if matches!(response.status, 301..=303) {
            request.method = String::from("GET");
            request.body = None;
        }
        request.url = target;
    }
    Err(format!("too many redirects (>{MAX_REDIRECTS})"))
}

async fn fetch_once(request: &Request) -> Result<Response, String> {
    let url = &request.url;
    let host = url.host_str().ok_or("URL has no host")?.to_string();
    let port = url.port_or_known_default().unwrap_or(80);
    let stream = TcpStream::connect((host.as_str(), port))
        .await
        .map_err(|e| e.to_string())?;
    let _ = stream.set_nodelay(true);

    let raw = if url.scheme() == "https" {
        let name = tls::server_name(&host)?;
        let stream = tls::webpki_connector()
            .connect(name, stream)
            .await
            .map_err(|e| format!("TLS: {e}"))?;
        exchange(stream, request, &host, port).await?
    } else {
        exchange(stream, request, &host, port).await?
    };

    let (status, headers, body) = parse_response(&raw)?;
    if matches!(status, 301 | 302 | 303 | 307 | 308) {
        let location = headers
            .get("location")
            .cloned()
            .ok_or_else(|| format!("{status} redirect without a Location header"))?;
        // Smuggle the location to `fetch` via content_type; 3xx bodies
        // are not rendered.
        return Ok(Response {
            url: url.clone(),
            status,
            content_type: location,
            body: Vec::new(),
        });
    }
    let content_type = headers
        .get("content-type")
        .cloned()
        .unwrap_or_else(|| String::from("text/html"));
    Ok(Response {
        url: url.clone(),
        status,
        content_type,
        body,
    })
}

/// Write the request, read the whole response (we always close).
async fn exchange<S: AsyncRead + AsyncWrite + Unpin>(
    mut stream: S,
    request: &Request,
    host: &str,
    port: u16,
) -> Result<Vec<u8>, String> {
    let url = &request.url;
    let mut path = url.path().to_string();
    if let Some(query) = url.query() {
        path.push('?');
        path.push_str(query);
    }
    let host_header = match (url.scheme(), port) {
        ("http", 80) | ("https", 443) => host.to_string(),
        _ => format!("{host}:{port}"),
    };
    let mut head = format!(
        "{} {} HTTP/1.1\r\n\
         Host: {}\r\n\
         User-Agent: {}\r\n\
         Accept: text/html, text/*;q=0.8, */*;q=0.1\r\n\
         Accept-Encoding: identity\r\n\
         Connection: close\r\n",
        request.method, path, host_header, USER_AGENT,
    );
    if let Some((content_type, payload)) = &request.body {
        head.push_str(&format!(
            "Content-Type: {}\r\nContent-Length: {}\r\n",
            content_type,
            payload.len()
        ));
    }
    head.push_str("\r\n");

    stream
        .write_all(head.as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    if let Some((_, payload)) = &request.body {
        stream.write_all(payload).await.map_err(|e| e.to_string())?;
    }

    let mut raw = Vec::new();
    let mut buf = [0u8; 16384];
    loop {
        // Tolerate a missing TLS close_notify, as on the small net.
        let n = match stream.read(&mut buf).await {
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => 0,
            Err(e) => return Err(e.to_string()),
        };
        if n == 0 {
            return Ok(raw);
        }
        raw.extend_from_slice(&buf[..n]);
        if raw.len() > MAX_BODY {
            return Err(String::from("response exceeds 5 MB cap"));
        }
    }
}

type Headers = HashMap<String, String>;

/// Split a raw response into status, lowercased headers, and the body
/// (chunked decoding and Content-Length truncation applied).
fn parse_response(raw: &[u8]) -> Result<(u16, Headers, Vec<u8>), String> {
    let head_end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or("malformed response: no header terminator")?;
    let head = String::from_utf8_lossy(&raw[..head_end]);
    let mut lines = head.lines();
    let status_line = lines.next().ok_or("empty response")?;
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| format!("malformed status line: {status_line:?}"))?;
    let mut headers = HashMap::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }

    let body = &raw[head_end + 4..];
    let body = if headers
        .get("transfer-encoding")
        .is_some_and(|t| t.to_ascii_lowercase().contains("chunked"))
    {
        dechunk(body)?
    } else if let Some(len) = headers.get("content-length").and_then(|l| l.parse().ok()) {
        body[..body.len().min(len)].to_vec()
    } else {
        body.to_vec()
    };
    Ok((status, headers, body))
}

/// Decode a chunked transfer body (RFC 9112 §7.1).
fn dechunk(mut data: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    loop {
        let line_end = data
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or("truncated chunk header")?;
        let size_str = String::from_utf8_lossy(&data[..line_end]);
        let size = usize::from_str_radix(size_str.split(';').next().unwrap_or("").trim(), 16)
            .map_err(|_| format!("bad chunk size: {size_str:?}"))?;
        data = &data[line_end + 2..];
        if size == 0 {
            return Ok(out); // trailers, if any, are ignored
        }
        if data.len() < size {
            // Truncated by the server; keep what arrived.
            out.extend_from_slice(data);
            return Ok(out);
        }
        out.extend_from_slice(&data[..size]);
        data = &data[size..];
        data = data.strip_prefix(b"\r\n").unwrap_or(data);
    }
}

/// Decode the body per the content-type charset: UTF-8 by default,
/// Latin-1 (and its windows-1252 sibling, near enough) by byte map.
fn decode_body(content_type: &str, body: &[u8]) -> String {
    let charset = content_type
        .split(';')
        .find_map(|p| p.trim().strip_prefix("charset="))
        .map(|c| c.trim_matches('"').to_ascii_lowercase());
    match charset.as_deref() {
        Some("iso-8859-1" | "latin1" | "windows-1252") => body.iter().map(|&b| b as char).collect(),
        _ => String::from_utf8_lossy(body).into_owned(),
    }
}

/// Render a response body into a document.
pub fn parse(url: &Url, content_type: &str, body: &[u8], width: usize) -> Doc {
    let width = width.max(10);
    let media = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let lines = if media.is_empty() || media == "text/html" || media == "application/xhtml+xml" {
        html_to_lines(url, &decode_body(content_type, body), width)
    } else if media.starts_with("text/") {
        let text = decode_body(content_type, body);
        text.lines()
            .flat_map(|l| {
                let mut out = Vec::new();
                crate::doc::push_wrapped(
                    &mut out,
                    Kind::Text,
                    l.trim_end().to_string(),
                    None,
                    width,
                );
                out
            })
            .collect()
    } else {
        vec![DocLine {
            kind: Kind::Error,
            text: format!("unsupported media type: {content_type}"),
            link: None,
        }]
    };
    Doc {
        url: Link::Http(url.clone()),
        lines,
        raw: body.to_vec(),
        wrapped_to: width,
        cp437: false,
        meta: Some(content_type.to_string()),
    }
}

/// Resolve an href against the page, mapping schemes to our link types.
fn resolve(base: &Url, target: &str) -> Link {
    match base.join(target) {
        Ok(joined) => match joined.scheme() {
            "http" | "https" => Link::Http(joined),
            "gemini" => crate::gemini::GeminiUrl::parse(joined.as_str())
                .map(Link::Gemini)
                .unwrap_or_else(|| Link::External(joined.to_string())),
            "gopher" => crate::gopher::GopherUrl::parse(joined.as_str())
                .map(Link::Gopher)
                .unwrap_or_else(|| Link::External(joined.to_string())),
            _ => Link::External(joined.to_string()),
        },
        Err(_) => Link::External(target.to_string()),
    }
}

/// Convert HTML to document lines via html2text's rich mode. Lines come
/// back pre-wrapped to `width`. A line with exactly one link target
/// carries it directly; lines with several emit one indented `→ label`
/// row per link below the text, keeping our one-link-per-line model.
fn html_to_lines(base: &Url, html: &str, width: usize) -> Vec<DocLine> {
    use html2text::render::RichAnnotation;

    let rendered = match html2text::config::rich().lines_from_read(html.as_bytes(), width) {
        Ok(lines) => lines,
        Err(err) => {
            return vec![DocLine {
                kind: Kind::Error,
                text: format!("could not render HTML: {err}"),
                link: None,
            }];
        }
    };

    let mut lines: Vec<DocLine> = Vec::new();
    let mut previous_carried: Option<Link> = None;
    for tagged in rendered {
        let mut text = String::new();
        let mut pre = false;
        // (target, label, is_image) per annotated span, deduplicated.
        let mut found: Vec<(Link, String, bool)> = Vec::new();
        for piece in tagged.tagged_strings() {
            text.push_str(&piece.s);
            for annotation in &piece.tag {
                match annotation {
                    RichAnnotation::Link(href) => {
                        let link = resolve(base, href);
                        match found.iter_mut().find(|(l, _, _)| *l == link) {
                            Some((_, label, _)) => label.push_str(&piece.s),
                            None => found.push((link, piece.s.clone(), false)),
                        }
                    }
                    RichAnnotation::Image(src) => {
                        let link = resolve(base, src);
                        if !found.iter().any(|(l, _, _)| *l == link) {
                            let alt = if piece.s.trim().is_empty() {
                                String::from("image")
                            } else {
                                piece.s.trim().to_string()
                            };
                            found.push((link, format!("[img: {alt}]"), true));
                        }
                    }
                    RichAnnotation::Preformat(_) => pre = true,
                    _ => {}
                }
            }
        }

        // Map the markdown-ish prefixes html2text's RichDecorator emits.
        let trimmed = text.trim_end();
        let (kind, display) = if pre {
            (Kind::Pre, trimmed.to_string())
        } else if let Some(rest) = heading_text(trimmed) {
            rest
        } else if let Some(rest) = trimmed.strip_prefix("> ") {
            (Kind::Quote, format!("▌ {rest}"))
        } else {
            (Kind::Text, trimmed.to_string())
        };

        match found.len() {
            0 => {
                lines.push(DocLine {
                    kind,
                    text: display,
                    link: None,
                });
                previous_carried = None;
            }
            1 => {
                let (link, label, is_image) = found.into_iter().next().unwrap();
                // A wrapped link paragraph repeats its target on every
                // row; only the first row of a run is selectable.
                let carried = (previous_carried.as_ref() != Some(&link)).then(|| link.clone());
                previous_carried = Some(link);
                let (kind, text) = if is_image {
                    (Kind::OtherLink, label)
                } else if kind == Kind::Text {
                    (Kind::GemLink, display)
                } else {
                    (kind, display)
                };
                lines.push(DocLine {
                    kind,
                    text,
                    link: carried,
                });
            }
            _ => {
                lines.push(DocLine {
                    kind,
                    text: display,
                    link: None,
                });
                for (link, label, is_image) in found {
                    lines.push(DocLine {
                        kind: if is_image {
                            Kind::OtherLink
                        } else {
                            Kind::GemLink
                        },
                        text: format!("  → {}", label.trim()),
                        link: Some(link),
                    });
                }
                previous_carried = None;
            }
        }
    }
    // Trim runs of blank lines html2text leaves between blocks.
    lines.dedup_by(|a, b| a.text.is_empty() && b.text.is_empty() && a.link.is_none());
    lines
}

/// `# Title` → (Heading(1), "Title"), etc.; html2text emits these
/// prefixes for h1-h6.
fn heading_text(line: &str) -> Option<(Kind, String)> {
    let hashes = line.bytes().take_while(|&b| b == b'#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = line[hashes..].strip_prefix(' ')?;
    Some((Kind::Heading(hashes.min(3) as u8), rest.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn speaks_http_with_redirects_chunking_and_post() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let mut req = Vec::new();
                let mut buf = [0u8; 2048];
                // Read until end of headers (plus any body by length).
                while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                    match sock.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => req.extend_from_slice(&buf[..n]),
                    }
                }
                let text = String::from_utf8_lossy(&req).into_owned();
                let reply: Vec<u8> = if text.starts_with("GET /old ") {
                    b"HTTP/1.1 302 Found\r\nLocation: /new\r\n\r\n".to_vec()
                } else if text.starts_with("GET /new ") {
                    assert!(text.contains("User-Agent: TRust/0.1"));
                    assert!(text.contains("Connection: close"));
                    // Chunked HTML with a link.
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\
                      Transfer-Encoding: chunked\r\n\r\n\
                      1c\r\n<h1>Arrived</h1><p><a href=\"\r\n\
                      15\r\n/next\">onward</a></p>\r\n\
                      0\r\n\r\n"
                        .to_vec()
                } else if text.starts_with("POST /submit ") {
                    assert!(text.contains("Content-Type: application/x-www-form-urlencoded"));
                    assert!(text.ends_with("k=v&x=y"), "body arrived: {text:?}");
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\r\nposted ok".to_vec()
                } else {
                    b"HTTP/1.1 404 Nope\r\nContent-Length: 0\r\n\r\n".to_vec()
                };
                let _ = sock.write_all(&reply).await;
            }
        });

        // GET with a redirect into chunked HTML.
        let url = Url::parse(&format!("http://127.0.0.1:{port}/old")).unwrap();
        let response = fetch(&Request::get(url)).await.unwrap();
        assert_eq!(response.status, 200);
        assert!(response.url.path().ends_with("/new"), "followed redirect");
        let doc = parse(&response.url, &response.content_type, &response.body, 60);
        assert_eq!(doc.lines[0].kind, Kind::Heading(1));
        assert_eq!(doc.lines[0].text, "Arrived");
        let link = doc
            .lines
            .iter()
            .find_map(|l| l.link.as_ref())
            .expect("a link");
        assert_eq!(
            *link,
            Link::Http(Url::parse(&format!("http://127.0.0.1:{port}/next")).unwrap())
        );

        // POST carries content-type and body.
        let url = Url::parse(&format!("http://127.0.0.1:{port}/submit")).unwrap();
        let request = Request {
            method: String::from("POST"),
            url,
            body: Some((
                String::from("application/x-www-form-urlencoded"),
                b"k=v&x=y".to_vec(),
            )),
        };
        let response = fetch(&request).await.unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"posted ok");

        server.abort();
    }

    #[test]
    fn dechunks_bodies() {
        let chunked = b"4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n";
        assert_eq!(dechunk(chunked).unwrap(), b"Wikipedia");
        // Extensions after the size and truncated tails are tolerated.
        let ext = b"4;name=v\r\nWiki\r\nA\r\ntrunc";
        assert_eq!(dechunk(ext).unwrap(), b"Wikitrunc");
    }

    #[test]
    fn parses_responses() {
        let raw =
            b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 5\r\n\r\nhellojunk";
        let (status, headers, body) = parse_response(raw).unwrap();
        assert_eq!(status, 200);
        assert_eq!(headers["content-type"], "text/html");
        assert_eq!(body, b"hello");

        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n0\r\n\r\n";
        let (_, _, body) = parse_response(raw).unwrap();
        assert_eq!(body, b"abc");
    }

    #[test]
    fn decodes_latin1() {
        let body = [b'c', b'a', b'f', 0xe9];
        assert_eq!(decode_body("text/html; charset=ISO-8859-1", &body), "café");
        assert_eq!(decode_body("text/html", "café".as_bytes()), "café");
    }

    #[test]
    fn renders_html_into_doc_lines() {
        let base = Url::parse("https://example.com/dir/page.html").unwrap();
        let html = r#"
            <html><body>
            <h1>Big Title</h1>
            <p>Plain paragraph text.</p>
            <p><a href="other.html">A relative link</a></p>
            <p>Multi: <a href="/one">first</a> and <a href="https://other.example/">second</a>.</p>
            <pre>preformatted   text</pre>
            <p><img src="/cat.png" alt="a cat"></p>
            </body></html>"#;
        let doc = parse(&base, "text/html", html.as_bytes(), 60);
        let find = |needle: &str| {
            doc.lines
                .iter()
                .find(|l| l.text.contains(needle))
                .unwrap_or_else(|| panic!("no line containing {needle:?}"))
        };

        assert_eq!(find("Big Title").kind, Kind::Heading(1));
        assert!(find("Plain paragraph").link.is_none());

        let rel = find("A relative link");
        assert_eq!(
            rel.link,
            Some(Link::Http(
                Url::parse("https://example.com/dir/other.html").unwrap()
            ))
        );

        // Multi-link line: text row plus one selectable row per target.
        assert!(find("Multi:").link.is_none());
        assert_eq!(
            find("→ first").link,
            Some(Link::Http(Url::parse("https://example.com/one").unwrap()))
        );
        assert!(find("→ second").link.is_some());

        assert_eq!(find("preformatted   text").kind, Kind::Pre);
        let img = find("[img: a cat]");
        assert_eq!(
            img.link,
            Some(Link::Http(
                Url::parse("https://example.com/cat.png").unwrap()
            ))
        );
    }
}
