//! Finger's human-readable replies (RFC 1288 §§2.1–2.5, 3.3).
//! RFC Editor snapshot 2026-09-06; URL snapshot 55d6699373ba.
//! Wire bytes remain available independently of the bounded, filtered view.

use std::collections::HashSet;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::{Instant, sleep_until, timeout_at};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::doc::{Doc, DocLine, Kind, Link};
use crate::oneshot::{OneShotUrl, Scheme};

pub const MAX_RESPONSE: usize = 1024 * 1024;
const MAX_QUERY: usize = 8192;
pub(crate) const MAX_ROWS: usize = 4096;
const MAX_COLUMNS: usize = 4096;
const MAX_LINKS: usize = 256;
const UPDATE_INTERVAL: Duration = Duration::from_millis(100);
const FETCH_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reply {
    pub body: Vec<u8>,
    pub finished: bool,
    pub notice: Option<String>,
}

/// Presentation preferences travel with a reply and its history entry. Only
/// the immediately preceding successful manual refresh is retained.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct View {
    pub wrap: bool,
    pub changes: bool,
    pub previous: Option<Arc<[u8]>>,
    pub loading: bool,
    pub notice: Option<String>,
    pub horizontal: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Page {
    pub reply: Reply,
    pub view: View,
}

impl Page {
    pub fn new(reply: Reply) -> Self {
        Self {
            reply,
            view: View::default(),
        }
    }
}

pub fn view_action(
    view: &mut View,
    action: &str,
    enabled: Option<bool>,
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
            if enabled && view.previous.is_none() {
                return Err("Refresh this reply before comparing changes.");
            }
            view.changes = enabled;
            Ok(if enabled {
                "Showing changes since the previous refresh."
            } else {
                "Showing the original reply."
            })
        }
        _ => Err("Unknown Finger view action."),
    }
}

/// RFC 3986 §§2.4, 3.2.2, 3.5: separate components before decoding once;
/// fragments never enter the request. Finger has no URI-query convention.
pub fn parse_url(input: &str) -> Option<OneShotUrl> {
    if input.len() > MAX_QUERY * 3 + 4096 || input.chars().any(char::is_control) {
        return None;
    }
    let parsed = url::Url::parse(input).ok()?;
    if parsed.scheme() != "finger"
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
    // The path names a Finger query, not a filesystem hierarchy. Preserve
    // literal dot segments as query data rather than normalizing usernames.
    let path = input
        .split('#')
        .next()?
        .split_once("://")?
        .1
        .split_once('/')
        .map_or("", |(_, path)| path);
    let query = decode(path)?;
    validate_query(&query).ok()?;
    Some(OneShotUrl {
        scheme: Scheme::Finger,
        host,
        port: parsed.port().unwrap_or(79),
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

/// Command input is literal text, not an already percent-encoded URL.
pub fn command_target(target: &str) -> Option<OneShotUrl> {
    let (query, authority) = target.rsplit_once('@').unwrap_or(("", target));
    let address = if authority.parse::<std::net::Ipv6Addr>().is_ok() {
        format!("finger://[{authority}]")
    } else {
        format!("finger://{authority}")
    };
    let mut url = parse_url(&address)?;
    if !url.query.is_empty() || authority.contains(['/', '?', '#']) {
        return None;
    }
    validate_query(query).ok()?;
    url.query = query.to_string();
    Some(url)
}

fn validate_query(query: &str) -> Result<(), String> {
    // RFC 1288 §2.1: exactly one query line. Validate at the send boundary
    // too: OneShotUrl is public and callers can construct it directly.
    if query.chars().any(char::is_control) {
        return Err(String::from(
            "Finger queries cannot contain control characters",
        ));
    }
    if query.len() > MAX_QUERY {
        return Err(String::from("Finger query exceeds 8 KiB"));
    }
    Ok(())
}

pub async fn fetch(url: &OneShotUrl) -> Result<Reply, String> {
    fetch_updates(url, |_| async { true }).await
}

pub async fn fetch_updates<F, Fut>(url: &OneShotUrl, publish: F) -> Result<Reply, String>
where
    F: FnMut(Reply) -> Fut,
    Fut: Future<Output = bool>,
{
    exchange(url, FETCH_TIMEOUT, publish).await
}

async fn exchange<F, Fut>(
    url: &OneShotUrl,
    limit: Duration,
    mut publish: F,
) -> Result<Reply, String>
where
    F: FnMut(Reply) -> Fut,
    Fut: Future<Output = bool>,
{
    validate_query(&url.query)?;
    let deadline = Instant::now() + limit;
    let mut stream = timeout_at(deadline, TcpStream::connect((url.host.as_str(), url.port)))
        .await
        .map_err(|_| "Finger connection timed out".to_string())?
        .map_err(|e| format!("Finger connection failed: {e}"))?;
    timeout_at(
        deadline,
        stream.write_all(format!("{}\r\n", url.query).as_bytes()),
    )
    .await
    .map_err(|_| "Finger request timed out".to_string())?
    .map_err(|e| format!("Finger request failed: {e}"))?;
    let mut body = Vec::new();
    let mut buf = [0; 8192];
    let mut dirty = false;
    let mut next_update = Instant::now() + UPDATE_INTERVAL;
    let notice = loop {
        tokio::select! {
            result = timeout_at(deadline, stream.read(&mut buf)) => {
                match result {
                    Ok(Ok(0)) => break None, // RFC 1288 §2.1: server closes first.
                    Ok(Ok(n)) => {
                        let room = MAX_RESPONSE - body.len();
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
                    Ok(false) => return Err(String::from("Finger request cancelled")),
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
fn display_text(raw: &[u8], loading: bool) -> (String, bool) {
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
fn changes<'a>(old: &'a str, new: &'a str) -> Vec<(Kind, &'a str)> {
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

fn links(text: &str) -> Vec<Link> {
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

fn push_line(out: &mut Vec<DocLine>, kind: Kind, text: String, link: Option<Link>, width: usize) {
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

pub fn render(url: &OneShotUrl, reply: Reply, mut view: View, width: usize) -> Doc {
    view.loading = !reply.finished;
    view.notice = reply.notice;
    if view.wrap {
        view.horizontal = 0;
    }
    let (text, mut clipped) = display_text(&reply.body, view.loading);
    let old = view
        .previous
        .as_ref()
        .filter(|_| view.changes && !view.loading)
        .map(|raw| display_text(raw, false).0);
    let source = if view.changes
        && !view.loading
        && let Some(old) = &old
    {
        changes(old, &text)
    } else {
        text.lines().map(|s| (Kind::Pre, s)).collect()
    };
    let mut lines = Vec::new();
    if view.changes && !view.loading && old.is_some() {
        lines.push(DocLine {
            kind: Kind::Info,
            text: if old.as_deref() == Some(&text) {
                String::from("No changes since the previous refresh.")
            } else {
                String::from("Changes since the previous refresh: + added, - removed")
            },
            link: None,
        });
    }
    let mut extra = Vec::new();
    let mut seen = HashSet::new();
    let incomplete_tail = view.loading && !text.ends_with('\n');
    let source_len = source.len();
    for (index, (kind, line)) in source.into_iter().enumerate() {
        if lines.len() >= MAX_ROWS - 2 {
            clipped = true;
            break;
        }
        let found = if kind == Kind::Removed || (incomplete_tail && index + 1 == source_len) {
            Vec::new()
        } else {
            links(line)
        };
        for link in found.iter().skip(1) {
            if extra.len() < MAX_LINKS && seen.insert(link.to_string()) {
                extra.push(link.clone());
            }
        }
        let line = match kind {
            Kind::Added => format!("+ {line}"),
            Kind::Removed => format!("- {line}"),
            _ => line.to_string(),
        };
        push_line(
            &mut lines,
            kind,
            line,
            found.into_iter().next(),
            if view.wrap { width.max(2) } else { usize::MAX },
        );
    }
    for link in extra {
        if lines.len() >= MAX_ROWS - 2 {
            clipped = true;
            break;
        }
        push_line(
            &mut lines,
            Kind::OtherLink,
            format!("↗ {link}"),
            Some(link),
            width.max(2),
        );
    }
    if lines.is_empty() && reply.finished {
        lines.push(DocLine {
            kind: Kind::Info,
            text: String::from("Empty reply."),
            link: None,
        });
    }
    if clipped || lines.len() >= MAX_ROWS {
        lines.truncate(MAX_ROWS - 1);
        lines.push(DocLine {
            kind: Kind::Error,
            text: String::from("Display truncated to keep this reply responsive."),
            link: None,
        });
    }
    if let Some(notice) = &view.notice {
        lines.truncate(MAX_ROWS - 1);
        lines.push(DocLine {
            kind: Kind::Error,
            text: notice.clone(),
            link: None,
        });
    }
    view.horizontal = view.horizontal.min(
        lines
            .iter()
            .map(|line| UnicodeWidthStr::width(line.text.as_str()))
            .max()
            .unwrap_or(0)
            .saturating_sub(width),
    );
    let mut doc = Doc::from_lines(
        Link::OneShot(url.clone()),
        lines,
        reply.body,
        width,
        false,
        None,
    );
    doc.finger = Some(view);
    doc
}

pub fn rerender(doc: &mut Doc, width: usize) {
    let Some(view) = doc.finger.take() else {
        return;
    };
    let Link::OneShot(url) = &doc.url else {
        return;
    };
    let reply = Reply {
        body: std::mem::take(&mut doc.raw),
        finished: !view.loading,
        notice: view.notice.clone(),
    };
    *doc = render(url, reply, view, width);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url() -> OneShotUrl {
        parse_url("finger://example.test/alice").unwrap()
    }
    fn reply(body: &[u8]) -> Reply {
        Reply {
            body: body.to_vec(),
            finished: true,
            notice: None,
        }
    }

    #[test]
    fn finger_urls_roundtrip_decoded_queries_and_ipv6() {
        let parsed =
            OneShotUrl::parse("FiNgEr://[::1]:7979/%61lice%25%3F%23@relay#ignored").unwrap();
        assert_eq!(parsed.host, "::1");
        assert_eq!(parsed.port, 7979);
        assert_eq!(parsed.query, "alice%?#@relay");
        assert_eq!(OneShotUrl::parse(&parsed.to_string()), Some(parsed));
        for query in [".", "..", "a/../b", "%25", "a b", "name@relay"] {
            let mut u = url();
            u.query = query.into();
            assert_eq!(OneShotUrl::parse(&u.to_string()), Some(u));
        }
        assert_eq!(
            parse_url("finger://example.test/%2561lice").unwrap().query,
            "%61lice"
        );
        let command = command_target("alice%20@relay@[::1]:7979").unwrap();
        assert_eq!(
            (command.host.as_str(), command.port, command.query.as_str()),
            ("::1", 7979, "alice%20@relay")
        );
        assert_eq!(OneShotUrl::parse(&command.to_string()), Some(command));
        assert_eq!(command_target("alice@::1").unwrap().host, "::1");
        assert_eq!(
            parse_url("finger://host#fragment/not/a/query")
                .unwrap()
                .query,
            ""
        );
        assert_eq!(
            parse_url("finger://host/alice#fragment/not/a/query")
                .unwrap()
                .query,
            "alice"
        );
    }

    #[test]
    fn finger_rejects_controls_bad_escapes_and_ambiguous_authorities() {
        for address in [
            "finger://host/a%0d%0ab",
            "finger://host/a\nb",
            "finger://host/%00",
            "finger://host/%7f",
            "finger://host/%",
            "finger://host/%gg",
            "finger://host/%ff",
            "finger://::1/a",
            "finger://[::1",
            "finger://user@host/a",
            "finger://host/a?b",
        ] {
            assert!(OneShotUrl::parse(address).is_none(), "{address:?}");
        }
        for target in ["alice@", "alice@host/path", "alice@[::1]:bad"] {
            assert!(command_target(target).is_none(), "{target}");
        }
    }

    #[test]
    fn finger_preserves_spacing_expands_tabs_and_filters_controls() {
        let doc = render(
            &url(),
            reply(
                b"alice\tAlice  Example  \r\n\x1b[31mPlan\x1b[0m\r\n\x1b]0;title\x07OK\x00\x07\r\n",
            ),
            View::default(),
            10,
        );
        assert_eq!(
            doc.lines
                .iter()
                .map(|l| l.text.as_str())
                .collect::<Vec<_>>(),
            ["alice   Alice  Example  ", "Plan", "OK"]
        );
        assert!(doc.lines.iter().all(|l| l.kind == Kind::Pre));
        let unicode = "👩‍💻\tname\r\n界\te\u{301}\r\n";
        let doc = render(&url(), reply(unicode.as_bytes()), View::default(), 80);
        assert_eq!(doc.lines[0].text, "👩‍💻      name");
        assert_eq!(doc.lines[1].text, "界      e\u{301}");
    }

    #[test]
    fn finger_wrap_counts_cells_and_keeps_graphemes_and_whitespace() {
        let text = "界界界界界界界界\r\n   abc   def\r\ne\u{301}e\u{301}e\u{301}\r\n";
        let doc = render(
            &url(),
            reply(text.as_bytes()),
            View {
                wrap: true,
                ..View::default()
            },
            10,
        );
        assert_eq!(doc.lines[0].text, "界界界界界");
        assert_eq!(doc.lines[1].text, "界界界");
        assert_eq!(doc.lines[2].text, "   abc   d");
        assert_eq!(doc.lines[3].text, "ef");
        assert_eq!(doc.lines[4].text, "e\u{301}e\u{301}e\u{301}");
        assert!(
            doc.lines
                .iter()
                .all(|line| UnicodeWidthStr::width(line.text.as_str()) <= 10)
        );
    }

    #[test]
    fn finger_links_keep_original_text_and_expose_multiple_targets() {
        let text = "See https://example.test/a_(b) and finger://[::1]/alice.\r\n";
        let doc = render(&url(), reply(text.as_bytes()), View::default(), 80);
        assert_eq!(doc.lines[0].text, text.trim_end_matches(['\r', '\n']));
        assert_eq!(
            doc.lines[0].link.as_ref().unwrap().to_string(),
            "https://example.test/a_(b)"
        );
        assert!(doc.lines.iter().any(|l| {
            l.link
                .as_ref()
                .is_some_and(|link| link.to_string() == "finger://[::1]/alice")
        }));
        let unfinished = Reply {
            body: b"https://example.test/part".to_vec(),
            finished: false,
            notice: None,
        };
        assert!(
            render(&url(), unfinished, View::default(), 80).lines[0]
                .link
                .is_none()
        );
    }

    #[test]
    fn finger_diff_preserves_unchanged_lines_and_is_reversible() {
        let raw = b"Heading\r\nnew\r\nunchanged\r\nadded\r\n";
        let view = View {
            changes: true,
            previous: Some(Arc::from(b"Heading\r\nold\r\nunchanged\r\n".as_slice())),
            ..View::default()
        };
        let mut doc = render(&url(), reply(raw), view, 80);
        assert!(
            doc.lines
                .iter()
                .any(|l| l.kind == Kind::Removed && l.text == "- old")
        );
        assert!(
            doc.lines
                .iter()
                .any(|l| l.kind == Kind::Added && l.text == "+ new")
        );
        assert!(
            doc.lines
                .iter()
                .any(|l| l.kind == Kind::Pre && l.text == "unchanged")
        );
        view_action(doc.finger.as_mut().unwrap(), "changes", Some(false)).unwrap();
        rerender(&mut doc, 80);
        assert_eq!(doc.raw, raw);
        assert_eq!(doc.lines[1].text, "new");
        assert!(doc.finger.as_ref().unwrap().previous.is_some());
    }

    #[test]
    fn finger_display_bounds_rows_and_pathological_lines() {
        for raw in [
            vec![b'\n'; MAX_RESPONSE],
            vec![b'x'; MAX_RESPONSE],
            "\u{301}".repeat(MAX_RESPONSE / 2).into_bytes(),
            vec![b'\t'; MAX_RESPONSE],
        ] {
            let doc = render(
                &url(),
                reply(&raw),
                View {
                    wrap: true,
                    ..View::default()
                },
                10,
            );
            assert!(doc.lines.len() <= MAX_ROWS);
            assert!(doc.lines.iter().map(|l| l.text.len()).sum::<usize>() <= MAX_RESPONSE + 1024);
            assert!(doc.lines.last().unwrap().text.contains("truncated"));
        }
    }

    #[tokio::test]
    async fn finger_send_validates_before_connecting() {
        let mut u = url();
        u.query = "alice\r\nsecond".into();
        assert!(fetch(&u).await.unwrap_err().contains("control"));
    }

    #[tokio::test]
    async fn finger_stalled_presenter_cannot_hold_the_socket_past_the_deadline() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut u = url();
        u.host = "127.0.0.1".into();
        u.port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut query = [0; 7];
            socket.read_exact(&mut query).await.unwrap();
            socket.write_all(b"Received\r\n").await.unwrap();
            let mut byte = [0];
            assert_eq!(socket.read(&mut byte).await.unwrap(), 0);
        });
        let reply = tokio::time::timeout(
            Duration::from_secs(2),
            exchange(&u, Duration::from_millis(180), |_| std::future::pending()),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(reply.body, b"Received\r\n");
        assert!(reply.notice.unwrap().contains("timed out"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn finger_publishes_before_eof_and_sends_one_query() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut u = url();
        u.host = "127.0.0.1".into();
        u.port = listener.local_addr().unwrap().port();
        let release = Arc::new(tokio::sync::Notify::new());
        let gate = release.clone();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 7];
            socket.read_exact(&mut request).await.unwrap();
            assert_eq!(&request, b"alice\r\n");
            socket.write_all(b"Plan\r\nfirst").await.unwrap();
            gate.notified().await;
            socket.write_all(b" second\r\n").await.unwrap();
        });
        let mut updates = 0;
        let result = exchange(&u, Duration::from_secs(2), |part| {
            updates += 1;
            assert!(!part.finished);
            assert_eq!(part.body, b"Plan\r\nfirst");
            release.notify_one();
            std::future::ready(true)
        })
        .await
        .unwrap();
        server.await.unwrap();
        assert_eq!(updates, 1);
        assert!(result.finished && result.notice.is_none());
        assert_eq!(result.body, b"Plan\r\nfirst second\r\n");
    }

    #[tokio::test]
    async fn finger_timeout_and_cap_preserve_partial_replies() {
        for oversized in [false, true] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let mut u = url();
            u.host = "127.0.0.1".into();
            u.port = listener.local_addr().unwrap().port();
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0; 7];
                socket.read_exact(&mut request).await.unwrap();
                let body = if oversized {
                    vec![b'x'; MAX_RESPONSE + 1]
                } else {
                    b"Partial\r\n".to_vec()
                };
                let _ = socket.write_all(&body).await;
                if !oversized {
                    std::future::pending::<()>().await;
                }
            });
            let limit = if oversized {
                Duration::from_secs(2)
            } else {
                Duration::from_millis(80)
            };
            let result = exchange(&u, limit, |_| std::future::ready(true))
                .await
                .unwrap();
            assert!(result.finished);
            assert_eq!(result.body.len(), if oversized { MAX_RESPONSE } else { 9 });
            assert!(result.notice.as_ref().unwrap().contains(if oversized {
                "truncated"
            } else {
                "timed out"
            }));
            server.abort();
        }
    }
}
