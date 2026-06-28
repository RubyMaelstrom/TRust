//! One-shot query protocols: finger (RFC 1288), WHOIS (RFC 3912), and
//! DICT (RFC 2229). All three are: connect, send a line, read the
//! reply, hang up — so they share one exchange helper and render into
//! the browser panel like any other document.

use std::fmt;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::doc::{Doc, DocLine, Kind, Link, push_wrapped};

const MAX_RESPONSE: usize = 1024 * 1024;
const FETCH_TIMEOUT: Duration = Duration::from_secs(15);

/// The canonical WHOIS referral root.
pub const WHOIS_DEFAULT: &str = "whois.iana.org";
/// The classic public DICT server.
pub const DICT_DEFAULT: &str = "dict.org";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scheme {
    Finger,
    Whois,
    Dict,
}

impl Scheme {
    pub fn default_port(self) -> u16 {
        match self {
            Scheme::Finger => 79,
            Scheme::Whois => 43,
            Scheme::Dict => 2628,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Scheme::Finger => "finger",
            Scheme::Whois => "whois",
            Scheme::Dict => "dict",
        }
    }
}

/// A one-shot query target: `finger://host[:port][/user]`,
/// `whois://server[:port]/query`, `dict://host[:port]/word`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OneShotUrl {
    pub scheme: Scheme,
    pub host: String,
    pub port: u16,
    /// The user (finger), domain (whois), or word (dict) queried.
    /// Finger allows an empty query: "who is logged in".
    pub query: String,
}

impl OneShotUrl {
    pub fn parse(s: &str) -> Option<Self> {
        let (scheme, rest) = if let Some(r) = s.strip_prefix("finger://") {
            (Scheme::Finger, r)
        } else if let Some(r) = s.strip_prefix("whois://") {
            (Scheme::Whois, r)
        } else if let Some(r) = s.strip_prefix("dict://") {
            (Scheme::Dict, r)
        } else {
            return None;
        };
        let (authority, query) = match rest.split_once('/') {
            Some((a, q)) => (a, q),
            None => (rest, ""),
        };
        if authority.is_empty() {
            return None;
        }
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) if !host.is_empty() && port.chars().all(|c| c.is_ascii_digit()) => {
                (host, port.parse().ok()?)
            }
            _ => (authority, scheme.default_port()),
        };
        // RFC 2229 dict URLs spell definitions `/d:word[:database...]`.
        let query = match scheme {
            Scheme::Dict => {
                let q = query.strip_prefix("d:").unwrap_or(query);
                q.split(':').next().unwrap_or("").to_string()
            }
            _ => query.to_string(),
        };
        Some(Self {
            scheme,
            host: host.to_string(),
            port,
            query,
        })
    }
}

impl fmt::Display for OneShotUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}://{}", self.scheme.name(), self.host)?;
        if self.port != self.scheme.default_port() {
            write!(f, ":{}", self.port)?;
        }
        if !self.query.is_empty() {
            write!(f, "/{}", self.query)?;
        }
        Ok(())
    }
}

/// Connect, send `payload`, read to EOF.
async fn exchange(host: &str, port: u16, payload: String) -> Result<Vec<u8>, String> {
    let io = async {
        let mut stream = TcpStream::connect((host, port))
            .await
            .map_err(|e| e.to_string())?;
        stream
            .write_all(payload.as_bytes())
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
                return Err(String::from("response exceeds 1 MB cap"));
            }
        }
    };
    tokio::time::timeout(FETCH_TIMEOUT, io)
        .await
        .map_err(|_| String::from("timed out"))?
}

pub async fn fetch(url: &OneShotUrl) -> Result<Vec<u8>, String> {
    match url.scheme {
        Scheme::Finger => exchange(&url.host, url.port, format!("{}\r\n", url.query)).await,
        // DEFINE and QUIT pipeline fine, which keeps DICT a one-shot:
        // the server answers everything and closes.
        Scheme::Dict => {
            exchange(
                &url.host,
                url.port,
                format!("DEFINE * {}\r\nQUIT\r\n", url.query),
            )
            .await
        }
        Scheme::Whois => {
            let body = exchange(&url.host, url.port, format!("{}\r\n", url.query)).await?;
            // Registries answer with a referral (IANA always does);
            // follow it one hop, the way whois(1) would.
            let Some(server) = referral(&String::from_utf8_lossy(&body), &url.host) else {
                return Ok(body);
            };
            let (host, port) = match server.rsplit_once(':') {
                Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) => {
                    (h.to_string(), p.parse().unwrap_or(43))
                }
                _ => (server, 43),
            };
            match exchange(&host, port, format!("{}\r\n", url.query)).await {
                Ok(referred) => {
                    let mut out =
                        format!("% Referred by {} to {host}\r\n\r\n", url.host).into_bytes();
                    out.extend_from_slice(&referred);
                    Ok(out)
                }
                // A dead referral target still leaves the first answer.
                Err(_) => Ok(body),
            }
        }
    }
}

/// The `refer:`/`whois:` server out of a WHOIS reply, if any.
fn referral(text: &str, asked: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let line = line.trim();
        let server = line
            .strip_prefix("refer:")
            .or_else(|| line.strip_prefix("whois:"))?
            .trim();
        (!server.is_empty() && !server.contains(' ') && server.contains('.') && server != asked)
            .then(|| server.to_string())
    })
}

/// Render a reply into a document. Finger and WHOIS are plain text;
/// DICT gets its protocol lines stripped and definitions styled.
pub fn parse(url: &OneShotUrl, raw: Vec<u8>, width: usize) -> Doc {
    let width = width.max(10);
    let lines = match url.scheme {
        Scheme::Finger | Scheme::Whois => {
            crate::doc::wrap_plain(&String::from_utf8_lossy(&raw), width)
        }
        Scheme::Dict => dict_lines(&String::from_utf8_lossy(&raw), width),
    };
    Doc {
        url: Link::OneShot(url.clone()),
        lines,
        raw,
        wrapped_to: width,
        cp437: false,
        meta: None,
        forms: Vec::new(),
        rows: Vec::new(),
        image_urls: Vec::new(),
        carousels: Vec::new(),
        regions: Vec::new(),
        scroll_clips: Vec::new(),
        boundaries: Vec::new(),
    }
}

/// Convert a DICT session transcript into document lines: each `151`
/// definition header becomes a heading, its body (up to the `.`
/// terminator, dot-unstuffed) plain text; `552` means no match; other
/// 4xx/5xx errors show as-is. Status chatter (220/150/250/QUIT) drops.
fn dict_lines(text: &str, width: usize) -> Vec<DocLine> {
    let mut lines = Vec::new();
    let mut in_definition = false;
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if in_definition {
            if line == "." {
                in_definition = false;
                lines.push(DocLine {
                    kind: Kind::Text,
                    text: String::new(),
                    link: None,
                });
                continue;
            }
            // Text lines starting with '.' arrive dot-stuffed.
            let line = line
                .strip_prefix('.')
                .filter(|r| r.starts_with('.'))
                .unwrap_or(line);
            push_wrapped(&mut lines, Kind::Text, line.to_string(), None, width);
        } else if let Some(rest) = line.strip_prefix("151 ") {
            push_wrapped(
                &mut lines,
                Kind::Heading(2),
                definition_title(rest),
                None,
                width,
            );
            in_definition = true;
        } else if line.starts_with("552") {
            lines.push(DocLine {
                kind: Kind::Error,
                text: String::from("No definitions found."),
                link: None,
            });
        } else if line.starts_with('4') || line.starts_with('5') {
            push_wrapped(&mut lines, Kind::Error, line.to_string(), None, width);
        }
    }
    while lines.last().is_some_and(|l| l.text.is_empty()) {
        lines.pop();
    }
    lines
}

/// `151 "word" db "description"` → `word — description`.
fn definition_title(rest: &str) -> String {
    let (word, rest) = quoted_or_token(rest);
    let (_db, rest) = quoted_or_token(rest);
    let (description, _) = quoted_or_token(rest);
    if description.is_empty() {
        word.to_string()
    } else {
        format!("{word} — {description}")
    }
}

/// Split one field off a 151 line: a quoted string or a bare token.
fn quoted_or_token(s: &str) -> (&str, &str) {
    let s = s.trim_start();
    if let Some(rest) = s.strip_prefix('"')
        && let Some(end) = rest.find('"')
    {
        return (&rest[..end], &rest[end + 1..]);
    }
    s.split_once(' ').unwrap_or((s, ""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_urls() {
        let url = OneShotUrl::parse("finger://sdf.org/ruby").unwrap();
        assert_eq!(
            (url.scheme, url.host.as_str(), url.port, url.query.as_str()),
            (Scheme::Finger, "sdf.org", 79, "ruby")
        );
        // Empty finger query lists logged-in users.
        let url = OneShotUrl::parse("finger://sdf.org").unwrap();
        assert_eq!(url.query, "");
        assert_eq!(url.to_string(), "finger://sdf.org");

        let url = OneShotUrl::parse("whois://whois.iana.org/example.com").unwrap();
        assert_eq!((url.port, url.query.as_str()), (43, "example.com"));

        // RFC 2229 forms: d: prefix and :database suffix drop away.
        let url = OneShotUrl::parse("dict://dict.org/d:neon:wn").unwrap();
        assert_eq!((url.port, url.query.as_str()), (2628, "neon"));
        assert_eq!(url.to_string(), "dict://dict.org/neon");

        let url = OneShotUrl::parse("finger://bbs.example:7979/sysop").unwrap();
        assert_eq!(url.port, 7979);
        assert_eq!(url.to_string(), "finger://bbs.example:7979/sysop");

        assert!(OneShotUrl::parse("gopher://x").is_none());
        assert!(OneShotUrl::parse("finger://").is_none());
    }

    #[test]
    fn finds_whois_referrals() {
        let iana = "refer:        whois.verisign-grs.com\n\ndomain: EXAMPLE.COM\n";
        assert_eq!(
            referral(iana, "whois.iana.org"),
            Some(String::from("whois.verisign-grs.com"))
        );
        // No self-referrals, no garbage.
        assert_eq!(referral("refer: whois.iana.org\n", "whois.iana.org"), None);
        assert_eq!(referral("refer: not a host\n", "x"), None);
        assert_eq!(referral("domain: EXAMPLE.COM\n", "x"), None);
    }

    #[test]
    fn renders_dict_transcripts() {
        let transcript = "220 dict.org banner <auth.mime>\r\n\
                          150 2 definitions retrieved\r\n\
                          151 \"neon\" wn \"WordNet (r) 3.0 (2006)\"\r\n\
                          neon\r\n    n 1: a colorless element\r\n\
                          ..literally starts with a dot\r\n\
                          .\r\n\
                          151 \"neon\" gcide \"The Collaborative International\"\r\n\
                          Neon \\Ne\"on\\, n.\r\n\
                          .\r\n\
                          250 ok\r\n\
                          221 bye\r\n";
        let url = OneShotUrl::parse("dict://dict.org/neon").unwrap();
        let doc = parse(&url, transcript.as_bytes().to_vec(), 80);

        assert_eq!(doc.lines[0].kind, Kind::Heading(2));
        assert_eq!(doc.lines[0].text, "neon — WordNet (r) 3.0 (2006)");
        assert_eq!(doc.lines[1].text, "neon");
        assert!(
            doc.lines
                .iter()
                .any(|l| l.text == ".literally starts with a dot")
        );
        assert!(
            doc.lines
                .iter()
                .filter(|l| l.kind == Kind::Heading(2))
                .count()
                == 2,
            "both definitions render"
        );
        assert!(!doc.lines.iter().any(|l| l.text.contains("250")));

        let miss = "220 banner\r\n552 no match\r\n221 bye\r\n";
        let doc = parse(&url, miss.as_bytes().to_vec(), 80);
        assert_eq!(doc.lines[0].kind, Kind::Error);
        assert_eq!(doc.lines[0].text, "No definitions found.");
    }

    #[tokio::test]
    async fn fetches_finger_and_follows_whois_referrals() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        use tokio::net::TcpListener;

        // A finger daemon: echoes the query it was sent.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 64];
            let n = sock.read(&mut buf).await.unwrap();
            assert_eq!(&buf[..n], b"ruby\r\n");
            sock.write_all(b"Login: ruby\nPlan: ship TRust\n")
                .await
                .unwrap();
        });
        let url = OneShotUrl {
            scheme: Scheme::Finger,
            host: String::from("127.0.0.1"),
            port,
            query: String::from("ruby"),
        };
        let body = fetch(&url).await.unwrap();
        assert!(String::from_utf8_lossy(&body).contains("ship TRust"));

        // A WHOIS root that refers to a second server (host:port form
        // so the test can pick its own port).
        let registry = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let registry_port = registry.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut sock, _) = registry.accept().await.unwrap();
            let mut buf = [0u8; 64];
            let _ = sock.read(&mut buf).await.unwrap();
            sock.write_all(b"domain: EXAMPLE.COM\nstatus: ACTIVE\n")
                .await
                .unwrap();
        });
        let root = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let root_port = root.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut sock, _) = root.accept().await.unwrap();
            let mut buf = [0u8; 64];
            let n = sock.read(&mut buf).await.unwrap();
            assert_eq!(&buf[..n], b"example.com\r\n");
            sock.write_all(format!("refer: 127.0.0.1:{registry_port}\n").as_bytes())
                .await
                .unwrap();
        });
        let url = OneShotUrl {
            scheme: Scheme::Whois,
            host: String::from("127.0.0.1"),
            port: root_port,
            query: String::from("example.com"),
        };
        let body = String::from_utf8_lossy(&fetch(&url).await.unwrap()).into_owned();
        assert!(body.contains("Referred by 127.0.0.1"), "got: {body}");
        assert!(body.contains("status: ACTIVE"), "followed the referral");
    }
}
