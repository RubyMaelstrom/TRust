//! Finger's human-readable replies (RFC 1288 §§2.1–2.5, 3.3).
//! RFC Editor snapshot 2026-09-06; URL snapshot 55d6699373ba.
//! Wire bytes remain available independently of the bounded, filtered view.

use std::collections::HashSet;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use unicode_width::UnicodeWidthStr;

use crate::doc::{Doc, DocLine, Kind, Link};
use crate::oneshot::{OneShotUrl, Scheme};
use crate::text_reply::MAX_LINKS;
pub(crate) use crate::text_reply::MAX_ROWS;
pub use crate::text_reply::{MAX_RESPONSE, Reply, encode_query};
use crate::text_reply::{changes, display_text, links, push_line};

const FETCH_TIMEOUT: Duration = Duration::from_secs(15);

/// Presentation preferences travel with a reply and its history entry. Only
/// the immediately preceding successful manual refresh is retained.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct View {
    pub controls: crate::text_reply::View,
    pub previous: Option<Arc<[u8]>>,
}

impl std::ops::Deref for View {
    type Target = crate::text_reply::View;
    fn deref(&self) -> &Self::Target {
        &self.controls
    }
}
impl std::ops::DerefMut for View {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.controls
    }
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
    let has_previous = view.previous.is_some();
    crate::text_reply::view_action(&mut view.controls, action, enabled, has_previous)
}

pub fn parse_url(input: &str) -> Option<OneShotUrl> {
    crate::text_reply::parse_url(input, Scheme::Finger)
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
    crate::text_reply::validate_query(query, "Finger")
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

async fn exchange<F, Fut>(url: &OneShotUrl, limit: Duration, publish: F) -> Result<Reply, String>
where
    F: FnMut(Reply) -> Fut,
    Fut: Future<Output = bool>,
{
    crate::text_reply::exchange(
        url,
        tokio::time::Instant::now() + limit,
        MAX_RESPONSE,
        publish,
    )
    .await
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
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

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
                controls: crate::text_reply::View {
                    wrap: true,
                    ..Default::default()
                },
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
            controls: crate::text_reply::View {
                changes: true,
                ..Default::default()
            },
            previous: Some(Arc::from(b"Heading\r\nold\r\nunchanged\r\n".as_slice())),
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
                    controls: crate::text_reply::View {
                        wrap: true,
                        ..Default::default()
                    },
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
