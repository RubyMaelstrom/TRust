//! Gopher protocol (RFC 1436): one-shot fetches and menu/text parsing.
//!
//! Gopher is request/response over raw TCP: connect, send the selector,
//! read until EOF. There is no persistent session, so unlike telnet this
//! module has no long-lived task — just `fetch` plus parsers that build a
//! line-oriented document for the gopherus-style browser view.

use std::fmt;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::doc::{Doc, DocLine, Kind, Link, push_wrapped};
use crate::gemini;

/// Cap on a single response; gopherspace text rarely exceeds kilobytes.
const MAX_RESPONSE: usize = 2 * 1024 * 1024;
const FETCH_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GopherUrl {
    pub host: String,
    pub port: u16,
    pub item_type: char,
    pub selector: String,
}

impl GopherUrl {
    /// Parse `gopher://host[:port][/Xselector]`. The first path character
    /// is the item type per the gopher URL convention (RFC 4266).
    pub fn parse(s: &str) -> Option<Self> {
        let rest = s.strip_prefix("gopher://")?;
        let (authority, path) = match rest.split_once('/') {
            Some((authority, path)) => (authority, path),
            None => (rest, ""),
        };
        if authority.is_empty() {
            return None;
        }
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) if !port.is_empty() => (host, port.parse().ok()?),
            _ => (authority, 70),
        };
        let (item_type, selector) = match path.chars().next() {
            None => ('1', String::new()),
            Some(t) => (t, path[t.len_utf8()..].to_string()),
        };
        Some(Self {
            host: host.to_string(),
            port,
            item_type,
            selector,
        })
    }
}

impl fmt::Display for GopherUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "gopher://{}", self.host)?;
        if self.port != 70 {
            write!(f, ":{}", self.port)?;
        }
        // Tabs appear in search selectors; show them readably.
        write!(f, "/{}{}", self.item_type, self.selector.replace('\t', " "))
    }
}

/// Map a gopher item type to a styling class and link policy.
fn kind_of(item_type: char) -> Kind {
    match item_type {
        'i' => Kind::Info,
        '3' => Kind::Error,
        '1' => Kind::Dir,
        '0' => Kind::Document,
        '7' => Kind::Search,
        _ => Kind::OtherLink,
    }
}

/// Resolve a `=>` target found in a gopher-hosted .gmi file. Absolute
/// URLs of any scheme work as usual; relative references become gopher
/// selectors on the same host (menus when the path ends in `/`, text
/// items otherwise — .gmi targets render as gemtext again on arrival).
fn resolve_gmi(base: &GopherUrl, target: &str) -> Link {
    if let Some(link) = gemini::absolute_link(target) {
        return link;
    }
    if let Some(rest) = target.strip_prefix("//") {
        // Network-path reference: same scheme (gopher), new authority.
        return match GopherUrl::parse(&format!("gopher://{rest}")) {
            Some(url) => Link::Gopher(url),
            None => Link::External(target.to_string()),
        };
    }
    let selector = if let Some(abs) = target.strip_prefix('/') {
        gemini::normalize(&format!("/{abs}"))
    } else {
        let sel = &base.selector;
        let dir = sel.rfind('/').map(|i| &sel[..=i]).unwrap_or("/");
        gemini::normalize(&format!("{dir}{target}"))
    };
    let item_type = if selector.ends_with('/') { '1' } else { '0' };
    Link::Gopher(GopherUrl {
        host: base.host.clone(),
        port: base.port,
        item_type,
        selector,
    })
}

/// Fetch one item: send the selector, read to EOF.
pub async fn fetch(url: &GopherUrl) -> Result<Vec<u8>, String> {
    let io = async {
        let mut stream = TcpStream::connect((url.host.as_str(), url.port))
            .await
            .map_err(|e| e.to_string())?;
        stream
            .write_all(format!("{}\r\n", url.selector).as_bytes())
            .await
            .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            let n = stream.read(&mut buf).await.map_err(|e| e.to_string())?;
            if n == 0 {
                return Ok(out);
            }
            out.extend_from_slice(&buf[..n]);
            if out.len() > MAX_RESPONSE {
                return Err(String::from("response exceeds 2 MB cap"));
            }
        }
    };
    tokio::time::timeout(FETCH_TIMEOUT, io)
        .await
        .map_err(|_| String::from("timed out"))?
}

/// Parse a fetched item into a document, menu or text depending on type,
/// word-wrapping long lines to `width` columns.
pub fn parse(url: &GopherUrl, raw: Vec<u8>, cp437: bool, width: usize) -> Doc {
    let width = width.max(10);
    let lines = match url.item_type {
        '1' | '7' => parse_menu(&raw, cp437, width),
        // Gemtext hosted in gopherspace: a common modern habit is to
        // serve .gmi files over gopher; render them properly, with
        // relative links resolving to gopher selectors on this host.
        _ if url
            .selector
            .split(['?', '\t'])
            .next()
            .is_some_and(|s| s.to_ascii_lowercase().ends_with(".gmi")) =>
        {
            gemini::parse_gemtext(&raw, width, &|target| resolve_gmi(url, target))
        }
        _ => parse_text(&raw, cp437, width),
    };
    Doc {
        url: Link::Gopher(url.clone()),
        lines,
        raw,
        wrapped_to: width,
        cp437,
        meta: None,
        forms: Vec::new(),
    }
}

fn decode(bytes: &[u8], cp437: bool) -> String {
    if cp437 {
        String::from_utf8_lossy(&crate::cp437::decode(bytes)).into_owned()
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    }
}

fn split_lines(raw: &[u8]) -> impl Iterator<Item = &[u8]> {
    raw.split(|&b| b == b'\n')
        .map(|line| line.strip_suffix(b"\r").unwrap_or(line))
}

fn parse_menu(raw: &[u8], cp437: bool, width: usize) -> Vec<DocLine> {
    let mut lines = Vec::new();
    for line in split_lines(raw) {
        if line == b"." {
            break; // RFC 1436 menu terminator
        }
        if line.is_empty() {
            lines.push(DocLine {
                kind: Kind::Info,
                text: String::new(),
                link: None,
            });
            continue;
        }
        let fields: Vec<&[u8]> = line.split(|&b| b == b'\t').collect();
        if fields.len() < 2 {
            // Not a well-formed item; some servers emit bare text.
            push_wrapped(&mut lines, Kind::Info, decode(line, cp437), None, width);
            continue;
        }
        let item_type = fields[0][0] as char;
        let text = decode(&fields[0][1..], cp437);
        let host = fields
            .get(2)
            .map(|h| decode(h, cp437).trim().to_string())
            .unwrap_or_default();
        let selector = decode(fields[1], cp437);
        let link = if item_type == 'h' && selector.starts_with("URL:") {
            // `h` items conventionally carry a web URL in the selector;
            // those are followable in our own browser now.
            let target = &selector["URL:".len()..];
            Some(
                crate::http::parse_url(target)
                    .map(Link::Http)
                    .unwrap_or_else(|| Link::External(target.to_string())),
            )
        } else if item_type != 'i' && item_type != '3' && !host.is_empty() {
            Some(Link::Gopher(GopherUrl {
                host,
                port: fields
                    .get(3)
                    .and_then(|p| decode(p, cp437).trim().parse().ok())
                    .unwrap_or(70),
                item_type,
                selector,
            }))
        } else {
            None
        };
        push_wrapped(&mut lines, kind_of(item_type), text, link, width);
    }
    lines
}

fn parse_text(raw: &[u8], cp437: bool, width: usize) -> Vec<DocLine> {
    let mut lines = Vec::new();
    for line in split_lines(raw) {
        if line == b"." {
            break; // text terminator, when the server sends one
        }
        // RFC 1436 byte-stuffing: a content line starting with '.' is
        // sent as '..'.
        let line = line
            .strip_prefix(b".")
            .filter(|r| r.starts_with(b"."))
            .unwrap_or(line);
        push_wrapped(&mut lines, Kind::Text, decode(line, cp437), None, width);
    }
    // Drop a trailing blank produced by the final CRLF.
    if lines.last().is_some_and(|l| l.text.is_empty()) {
        lines.pop();
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::{Kind, Link};

    /// Unwrap a gopher link in tests.
    fn gopher_link(line: &crate::doc::DocLine) -> &GopherUrl {
        match line.link.as_ref().unwrap() {
            Link::Gopher(url) => url,
            other => panic!("expected gopher link, got {other:?}"),
        }
    }

    #[test]
    fn parses_urls() {
        let url = GopherUrl::parse("gopher://gopher.floodgap.com").unwrap();
        assert_eq!(url.host, "gopher.floodgap.com");
        assert_eq!((url.port, url.item_type), (70, '1'));
        assert_eq!(url.selector, "");

        let url = GopherUrl::parse("gopher://host:7070/0/docs/readme.txt").unwrap();
        assert_eq!((url.port, url.item_type), (7070, '0'));
        assert_eq!(url.selector, "/docs/readme.txt");

        assert!(GopherUrl::parse("https://example.com").is_none());
        assert!(GopherUrl::parse("gopher://").is_none());
    }

    #[test]
    fn parses_menus() {
        let url = GopherUrl::parse("gopher://example.org").unwrap();
        let raw = b"iWelcome to the hole\t\terror.host\t1\r\n\
                    1Deep Tunnels\t/tunnels\texample.org\t70\r\n\
                    0README\t/readme\tmirror.example\t7070\r\n\
                    7Search the void\t/search\texample.org\t70\r\n\
                    3Something broke\t\terror.host\t1\r\n\
                    stray text without tabs\r\n\
                    .\r\nignored after terminator";
        let doc = parse(&url, raw.to_vec(), false, 80);

        assert_eq!(doc.lines.len(), 6);
        assert_eq!(doc.lines[0].kind, Kind::Info);
        assert!(doc.lines[0].link.is_none());
        let dir = gopher_link(&doc.lines[1]);
        assert_eq!((dir.item_type, dir.selector.as_str()), ('1', "/tunnels"));
        assert_eq!(doc.lines[1].kind, Kind::Dir);
        let txt = gopher_link(&doc.lines[2]);
        assert_eq!((txt.host.as_str(), txt.port), ("mirror.example", 7070));
        assert_eq!(doc.lines[2].kind, Kind::Document);
        assert_eq!(gopher_link(&doc.lines[3]).item_type, '7');
        assert_eq!(doc.lines[3].kind, Kind::Search);
        assert!(doc.lines[4].link.is_none(), "errors are not links");
        assert_eq!(doc.lines[4].kind, Kind::Error);
        assert_eq!(doc.lines[5].text, "stray text without tabs");
    }

    #[test]
    fn wraps_long_lines_at_word_boundaries() {
        // Text document: a 100-char line wraps to the given width.
        let url = GopherUrl::parse("gopher://e.org/0/x").unwrap();
        let long = "word ".repeat(20);
        let doc = parse(&url, format!("{long}\r\n.\r\n").into_bytes(), false, 20);
        assert!(doc.lines.len() >= 5, "got {} lines", doc.lines.len());
        assert!(doc.lines.iter().all(|l| l.text.chars().count() <= 20));
        assert!(doc.lines.iter().all(|l| !l.text.contains("wor d")));

        // Menu link: only the first wrapped row keeps the link.
        let url = GopherUrl::parse("gopher://e.org").unwrap();
        let menu = format!("1{}\t/sel\te.org\t70\r\n.\r\n", "Linkword ".repeat(8));
        let doc = parse(&url, menu.into_bytes(), false, 20);
        assert!(doc.lines.len() >= 2);
        assert!(doc.lines[0].link.is_some());
        assert!(doc.lines[1..].iter().all(|l| l.link.is_none()));
        // Continuations keep the kind so they style like their item.
        assert!(doc.lines[1..].iter().all(|l| l.kind == Kind::Dir));
    }

    #[test]
    fn renders_gmi_files_as_gemtext_with_gopher_links() {
        let url = GopherUrl::parse("gopher://e.org/0/phlog/post.gmi").unwrap();
        let body = b"# Hello from gopherspace\n\
                     => other.gmi Next post\n\
                     => ../top.gmi Up top\n\
                     => sub/ A menu\n\
                     => gemini://capsule.example/x Crossover\n\
                     => https://example.com/ Web\n";
        let doc = parse(&url, body.to_vec(), false, 80);

        assert_eq!(doc.lines[0].kind, Kind::Heading(1));
        assert_eq!(doc.lines[1].kind, Kind::GemLink);
        // Relative links resolve to gopher selectors on the same host.
        let next = gopher_link(&doc.lines[1]);
        assert_eq!(
            (next.item_type, next.selector.as_str()),
            ('0', "/phlog/other.gmi")
        );
        let up = gopher_link(&doc.lines[2]);
        assert_eq!((up.item_type, up.selector.as_str()), ('0', "/top.gmi"));
        let menu = gopher_link(&doc.lines[3]);
        assert_eq!(
            (menu.item_type, menu.selector.as_str()),
            ('1', "/phlog/sub/")
        );
        // Absolute URLs keep their scheme.
        assert!(matches!(doc.lines[4].link, Some(Link::Gemini(_))));
        assert!(matches!(doc.lines[5].link, Some(Link::Http(_))));

        // Plain .txt files still render as plain gopher text.
        let url = GopherUrl::parse("gopher://e.org/0/notes.txt").unwrap();
        let doc = parse(&url, b"# not a heading\n".to_vec(), false, 80);
        assert_eq!(doc.lines[0].kind, Kind::Text);
        assert_eq!(doc.lines[0].text, "# not a heading");
    }

    #[test]
    fn parses_text_with_byte_stuffing() {
        let url = GopherUrl::parse("gopher://example.org/0/readme").unwrap();
        let doc = parse(&url, b"hello\r\n..dotted line\r\n.\r\n".to_vec(), false, 80);
        let texts: Vec<&str> = doc.lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts, ["hello", ".dotted line"]);
    }
}
