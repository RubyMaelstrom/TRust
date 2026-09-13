//! Bounded text presentation and CRLF/EOF exchange for human-readable query protocols.
//! RFC Editor snapshot 2026-09-06: RFC 1288 §§2.1, 3.3; RFC 3912 §§2, 4;
//! RFC 3986 §§2.4, 3.1, 3.2.2, 3.5. Protocol-specific interpretation stays
//! with the caller; raw bytes remain independent of the filtered display.

use crate::doc::{DocLine, Kind, Link};
use crate::oneshot::{OneShotUrl, Scheme};
use std::future::Future;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::{Instant, sleep_until, timeout_at};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

pub const MAX_RESPONSE: usize = 1024 * 1024;
const MAX_QUERY: usize = 8192;
pub(crate) const MAX_ROWS: usize = 4096;
pub(crate) const MAX_COLUMNS: usize = 4096;
pub(crate) const MAX_LINKS: usize = 256;
const UPDATE_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reply {
    pub body: Vec<u8>,
    pub finished: bool,
    pub notice: Option<String>,
}

/// View controls shared by Finger and WHOIS. Each protocol owns its history.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct View {
    pub wrap: bool,
    pub changes: bool,
    pub loading: bool,
    pub notice: Option<String>,
    pub horizontal: usize,
}

pub fn view_action(
    view: &mut View,
    action: &str,
    enabled: Option<bool>,
    has_previous: bool,
) -> Result<&'static str, &'static str> {
    match action {
        "wrap" => {
            view.wrap = enabled.unwrap_or(!view.wrap);
            view.horizontal = 0;
            Ok(if view.wrap {
                "Wrapping on."
            } else {
                "Wrapping off — Shift+Left/Right pans the reply."
            })
        }
        "changes" => {
            let enabled = enabled.unwrap_or(!view.changes);
            if enabled && !has_previous {
                return Err("Refresh this reply before comparing changes.");
            }
            view.changes = enabled;
            Ok(if enabled {
                "Showing changes since the previous refresh."
            } else {
                "Showing the original reply."
            })
        }
        _ => Err("Unknown reply view action."),
    }
}

/// RFC 3986 §§2.4, 3.2.2, 3.5: separate components before decoding once;
/// fragments never enter the request. These protocols use a path query, not the URI query component.
pub(crate) fn parse_url(input: &str, scheme: Scheme) -> Option<OneShotUrl> {
    if input.len() > MAX_QUERY * 3 + 4096 || input.chars().any(char::is_control) {
        return None;
    }
    let parsed = url::Url::parse(input).ok()?;
    if parsed.scheme() != scheme.name()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
    {
        return None;
    }
    let host = match parsed.host()? {
        url::Host::Ipv6(ip) => ip.to_string(),
        url::Host::Ipv4(ip) => ip.to_string(),
        url::Host::Domain(name) => {
            // A network hostname ultimately needs the URL domain parser's
            // IDNA/percent-decoding rules, even for this non-special scheme.
            url::Host::parse(name).ok()?.to_string()
        }
    };
    if host.is_empty() {
        return None;
    }
    // The path names a protocol query, not a filesystem hierarchy. Preserve
    // literal dot segments as query data rather than normalizing query data.
    let path = input
        .split('#')
        .next()?
        .split_once("://")?
        .1
        .split_once('/')
        .map_or("", |(_, path)| path);
    let query = decode(path)?;
    validate_query(&query, scheme.name()).ok()?;
    Some(OneShotUrl {
        scheme,
        host,
        port: parsed.port().unwrap_or(scheme.default_port()),
        query,
    })
}

fn decode(value: &str) -> Option<String> {
    let mut bytes = Vec::with_capacity(value.len());
    let mut iter = value.bytes();
    while let Some(byte) = iter.next() {
        bytes.push(if byte == b'%' {
            let hi = (iter.next()? as char).to_digit(16)?;
            let lo = (iter.next()? as char).to_digit(16)?;
            (hi * 16 + lo) as u8
        } else {
            byte
        });
    }
    String::from_utf8(bytes).ok()
}

pub fn encode_query(query: &str, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    for byte in query.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~@/".contains(&byte) {
            write!(f, "{}", byte as char)?;
        } else {
            write!(f, "%{byte:02X}")?;
        }
    }
    Ok(())
}

pub(crate) fn validate_query(query: &str, protocol: &str) -> Result<(), String> {
    // RFC 1288 §2.1 / RFC 3912 §2: exactly one query line. Validate at the send boundary
    // too: OneShotUrl is public and callers can construct it directly.
    if query.chars().any(char::is_control) {
        return Err(format!(
            "{protocol} queries cannot contain control characters"
        ));
    }
    if query.len() > MAX_QUERY {
        return Err(format!("{protocol} query exceeds 8 KiB"));
    }
    Ok(())
}

pub(crate) async fn exchange<F, Fut>(
    url: &OneShotUrl,
    deadline: Instant,
    max_response: usize,
    mut publish: F,
) -> Result<Reply, String>
where
    F: FnMut(Reply) -> Fut,
    Fut: Future<Output = bool>,
{
    let protocol = url.scheme.name();
    validate_query(&url.query, protocol)?;
    let mut stream = timeout_at(deadline, TcpStream::connect((url.host.as_str(), url.port)))
        .await
        .map_err(|_| format!("{protocol} connection timed out"))?
        .map_err(|e| format!("{protocol} connection failed: {e}"))?;
    timeout_at(
        deadline,
        stream.write_all(format!("{}\r\n", url.query).as_bytes()),
    )
    .await
    .map_err(|_| format!("{protocol} request timed out"))?
    .map_err(|e| format!("{protocol} request failed: {e}"))?;
    let mut body = Vec::new();
    let mut buf = [0; 8192];
    let mut dirty = false;
    let mut next_update = Instant::now() + UPDATE_INTERVAL;
    let notice = loop {
        tokio::select! {
            result = timeout_at(deadline, stream.read(&mut buf)) => {
                match result {
                    Ok(Ok(0)) => break None, // RFC 1288 §2.1 / RFC 3912 §2: EOF completes the reply.
                    Ok(Ok(n)) => {
                        let room = max_response.saturating_sub(body.len());
                        body.extend_from_slice(&buf[..n.min(room)]);
                        if n > room { break Some(String::from("Reply truncated at 1 MiB")); }
                        if !dirty { next_update = Instant::now() + UPDATE_INTERVAL; }
                        dirty = true;
                    }
                    Ok(Err(error)) => break Some(format!("Incomplete reply: {error}")),
                    Err(_) => break Some(String::from("Incomplete reply: server timed out")),
                }
            }
            _ = sleep_until(next_update), if dirty => {
                match timeout_at(deadline, publish(Reply {
                    body: body.clone(), finished: false, notice: None,
                })).await {
                    Ok(true) => {}
                    Ok(false) => return Err(format!("{protocol} request cancelled")),
                    Err(_) => break Some(String::from("Incomplete reply: request timed out")),
                }
                dirty = false;
            }
        }
    };
    if body.is_empty()
        && let Some(error) = notice
    {
        return Err(error);
    }
    Ok(Reply {
        body,
        finished: true,
        notice,
    })
}

/// RFC 1288 §3.3: controls never become terminal instructions. TRust's
/// international-text default permits UTF-8; tabs expand at eight-cell stops.
/// Escape sequences are discarded as units so their parameters do not litter
/// the reply. Both bytes and display geometry are bounded before shaping.
pub(crate) fn display_text(raw: &[u8], loading: bool) -> (String, bool) {
    let raw = if loading {
        match std::str::from_utf8(raw) {
            Err(e) if e.error_len().is_none() => &raw[..e.valid_up_to()],
            _ => raw,
        }
    } else {
        raw
    };
    let decoded = String::from_utf8_lossy(raw);
    let mut chars = decoded.chars().peekable();
    let mut filtered = String::new();
    let mut line_bytes = 0;
    let mut rows = 1;
    let mut clipped = false;
    while let Some(ch) = chars.next() {
        if filtered.len() + ch.len_utf8() > MAX_RESPONSE || rows > MAX_ROWS {
            clipped = true;
            break;
        }
        if ch == '\x1b' {
            match chars.next() {
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']' | 'P' | '_' | '^' | 'X') => {
                    while let Some(c) = chars.next() {
                        if c == '\x07' {
                            break;
                        }
                        if c == '\x1b' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                _ => {}
            }
            continue;
        }
        match ch {
            '\r' if chars.peek() == Some(&'\n') => continue,
            '\r' | '\n' => {
                filtered.push('\n');
                line_bytes = 0;
                rows += 1;
            }
            c if c.is_control() && c != '\t' => {}
            c => {
                if line_bytes + c.len_utf8() <= 16384 {
                    filtered.push(c);
                    line_bytes += c.len_utf8();
                } else {
                    clipped = true;
                }
            }
        }
    }
    // Measure whole graphemes: emoji joined by ZWJ and combining text must
    // advance tabs by the same cell count as Ratatui's renderer.
    let mut text = String::new();
    let mut column = 0;
    let mut line_full = false;
    for grapheme in filtered.graphemes(true) {
        if grapheme == "\n" {
            if text.len() == MAX_RESPONSE {
                clipped = true;
                break;
            }
            text.push('\n');
            column = 0;
            line_full = false;
            continue;
        }
        if line_full {
            continue;
        }
        let width = if grapheme == "\t" {
            8 - column % 8
        } else {
            UnicodeWidthStr::width(grapheme)
        };
        let bytes = if grapheme == "\t" {
            width
        } else {
            grapheme.len()
        };
        if text.len() + bytes > MAX_RESPONSE {
            clipped = true;
            break;
        }
        if column + width > MAX_COLUMNS {
            clipped = true;
            line_full = true;
            continue;
        }
        if grapheme == "\t" {
            text.extend(std::iter::repeat_n(' ', width));
        } else {
            text.push_str(grapheme);
        }
        column += width;
    }
    (text, clipped)
}

/// Bound the diff matrix as well as its output; widely separated edits in a
/// very large reply fall back to replacing the middle between equal edges.
pub(crate) fn changes<'a>(old: &'a str, new: &'a str) -> Vec<(Kind, &'a str)> {
    let old: Vec<_> = old.lines().collect();
    let new: Vec<_> = new.lines().collect();
    let prefix = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
    let suffix = old[prefix..]
        .iter()
        .rev()
        .zip(new[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let a = &old[prefix..old.len() - suffix];
    let b = &new[prefix..new.len() - suffix];
    let mut out: Vec<_> = new[..prefix].iter().map(|s| (Kind::Pre, *s)).collect();
    if (a.len() + 1).saturating_mul(b.len() + 1) <= 1_000_000 {
        let stride = b.len() + 1;
        let mut lcs = vec![0u16; (a.len() + 1) * stride];
        for i in (0..a.len()).rev() {
            for j in (0..b.len()).rev() {
                lcs[i * stride + j] = if a[i] == b[j] {
                    lcs[(i + 1) * stride + j + 1] + 1
                } else {
                    lcs[(i + 1) * stride + j].max(lcs[i * stride + j + 1])
                };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < a.len() || j < b.len() {
            if i < a.len() && j < b.len() && a[i] == b[j] {
                out.push((Kind::Pre, b[j]));
                i += 1;
                j += 1;
            } else if i < a.len()
                && (j == b.len() || lcs[(i + 1) * stride + j] >= lcs[i * stride + j + 1])
            {
                out.push((Kind::Removed, a[i]));
                i += 1;
            } else {
                out.push((Kind::Added, b[j]));
                j += 1;
            }
        }
    } else {
        out.extend(a.iter().map(|s| (Kind::Removed, *s)));
        out.extend(b.iter().map(|s| (Kind::Added, *s)));
    }
    out.extend(new[new.len() - suffix..].iter().map(|s| (Kind::Pre, *s)));
    out
}

pub(crate) fn links(text: &str) -> Vec<Link> {
    text.split_whitespace()
        .filter_map(|word| {
            let word = word
                .trim_start_matches(['<', '(', '[', '\'', '"'])
                .trim_end_matches(['>', '.', ',', ';', '!', '\'', '"']);
            let mut word = word;
            for (open, close) in [('(', ')'), ('[', ']')] {
                while word.ends_with(close)
                    && word.matches(close).count() > word.matches(open).count()
                {
                    word = &word[..word.len() - 1];
                }
            }
            if !word.contains("://") {
                return None;
            }
            crate::gemini::absolute_link(word).filter(|link| !matches!(link, Link::External(_)))
        })
        .take(MAX_LINKS)
        .collect()
}

pub(crate) fn push_line(
    out: &mut Vec<DocLine>,
    kind: Kind,
    text: String,
    link: Option<Link>,
    width: usize,
) {
    if width >= MAX_COLUMNS + 2 {
        if out.len() < MAX_ROWS {
            out.push(DocLine { kind, text, link });
        }
        return;
    }
    let mut start = 0;
    let mut cells = 0;
    let mut link = link;
    for (byte, grapheme) in text.grapheme_indices(true) {
        let advance = UnicodeWidthStr::width(grapheme);
        if cells + advance > width && byte > start {
            out.push(DocLine {
                kind,
                text: text[start..byte].to_string(),
                link: link.take(),
            });
            if out.len() >= MAX_ROWS {
                return;
            }
            start = byte;
            cells = 0;
        }
        cells += advance;
    }
    if out.len() < MAX_ROWS {
        out.push(DocLine {
            kind,
            text: text[start..].to_string(),
            link,
        });
    }
}
