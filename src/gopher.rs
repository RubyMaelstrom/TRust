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

/// One display line of a fetched document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocLine {
    /// Gopher item type for menus (`'i'` info, `'1'` menu, ...);
    /// `' '` for plain text documents.
    pub kind: char,
    pub text: String,
    pub link: Option<GopherUrl>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GopherDoc {
    pub url: GopherUrl,
    pub lines: Vec<DocLine>,
    /// The bytes as fetched, kept so the document can be re-wrapped when
    /// the terminal resizes or re-decoded when the encoding changes.
    pub raw: Vec<u8>,
    /// Width `lines` was wrapped to.
    pub wrapped_to: usize,
    /// Whether `lines` was decoded as CP437.
    pub cp437: bool,
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
pub fn parse(url: &GopherUrl, raw: Vec<u8>, cp437: bool, width: usize) -> GopherDoc {
    let width = width.max(10);
    let lines = match url.item_type {
        '1' | '7' => parse_menu(&raw, cp437, width),
        _ => parse_text(&raw, cp437, width),
    };
    GopherDoc {
        url: url.clone(),
        lines,
        raw,
        wrapped_to: width,
        cp437,
    }
}

/// Push a display line, word-wrapping it to `width`. Continuation rows
/// keep the kind (for styling) but never the link, so only an item's
/// first row is selectable.
fn push_wrapped(
    out: &mut Vec<DocLine>,
    kind: char,
    text: String,
    link: Option<GopherUrl>,
    width: usize,
) {
    if text.chars().count() <= width {
        out.push(DocLine { kind, text, link });
        return;
    }
    let mut link = link;
    for piece in textwrap::wrap(&text, width) {
        out.push(DocLine {
            kind,
            text: piece.into_owned(),
            link: link.take(),
        });
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
                kind: 'i',
                text: String::new(),
                link: None,
            });
            continue;
        }
        let fields: Vec<&[u8]> = line.split(|&b| b == b'\t').collect();
        if fields.len() < 2 {
            // Not a well-formed item; some servers emit bare text.
            push_wrapped(&mut lines, 'i', decode(line, cp437), None, width);
            continue;
        }
        let kind = fields[0][0] as char;
        let text = decode(&fields[0][1..], cp437);
        let host = fields
            .get(2)
            .map(|h| decode(h, cp437).trim().to_string())
            .unwrap_or_default();
        let link = (kind != 'i' && kind != '3' && !host.is_empty()).then(|| GopherUrl {
            host,
            port: fields
                .get(3)
                .and_then(|p| decode(p, cp437).trim().parse().ok())
                .unwrap_or(70),
            item_type: kind,
            selector: decode(fields[1], cp437),
        });
        push_wrapped(&mut lines, kind, text, link, width);
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
        push_wrapped(&mut lines, ' ', decode(line, cp437), None, width);
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
        assert_eq!(doc.lines[0].kind, 'i');
        assert!(doc.lines[0].link.is_none());
        let dir = doc.lines[1].link.as_ref().unwrap();
        assert_eq!((dir.item_type, dir.selector.as_str()), ('1', "/tunnels"));
        let txt = doc.lines[2].link.as_ref().unwrap();
        assert_eq!((txt.host.as_str(), txt.port), ("mirror.example", 7070));
        assert_eq!(doc.lines[3].link.as_ref().unwrap().item_type, '7');
        assert!(doc.lines[4].link.is_none(), "errors are not links");
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
        assert!(doc.lines[1..].iter().all(|l| l.kind == '1'));
    }

    #[test]
    fn parses_text_with_byte_stuffing() {
        let url = GopherUrl::parse("gopher://example.org/0/readme").unwrap();
        let doc = parse(&url, b"hello\r\n..dotted line\r\n.\r\n".to_vec(), false, 80);
        let texts: Vec<&str> = doc.lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts, ["hello", ".dotted line"]);
    }
}
