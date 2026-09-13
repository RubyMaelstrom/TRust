//! Browsing subset of UMN Gopher+, July 30 1993, §§2.2–2.9 / Appendix II.
//! Original protocol memo (also referenced by RFC 4266):
//! https://github.com/jgoerzen/pygopherd/blob/master/doc/standards/Gopher%2B.txt
//! RFC Editor snapshot 2026-09-06: RFC 4266 §2; RFC 2045 §5.1.
//! Replies are unframed here exactly once, including binary downloads. ASK
//! items expose their information without ever submitting answers.

use super::*;
use std::{collections::VecDeque, io};
use tokio::io::BufReader;

pub(super) fn menu_mime(mime: &str) -> bool {
    matches!(mime, "application/gopher-menu" | "application/gopher+-menu")
}

fn mime_token(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_graphic() && !b"()<>@,;:\\\"/[]?=".contains(&b))
}

/// A view is a MIME name and optional language, not a filename. Preserve its
/// spelling in requests; compare MIME type/subtype without ASCII case.
pub(super) fn view_mime(command: &[u8]) -> Option<String> {
    let view = std::str::from_utf8(command.strip_prefix(b"+")?).ok()?;
    let mime = view.split_ascii_whitespace().next()?;
    let (kind, subtype) = mime.split_once('/')?;
    (mime_token(kind) && mime_token(subtype)).then(|| mime.to_ascii_lowercase())
}

pub(super) fn request_command(url: &GopherUrl, command: &[u8]) -> Result<Vec<u8>, String> {
    match command {
        b"?" => Ok(b"!".to_vec()),
        b"+" if url.is_metadata() => Ok(b"!".to_vec()),
        b"+" | b"!" | b"$" => Ok(command.to_vec()),
        _ if command.starts_with(b"+") && view_mime(command).is_some() => {
            let view = std::str::from_utf8(&command[1..]).unwrap();
            if view.split_ascii_whitespace().count() > 2
                || !view.bytes().all(|b| b.is_ascii_graphic() || b == b' ')
            {
                return Err("Invalid Gopher+ view or language".into());
            }
            Ok(command.to_vec())
        }
        _ if matches!(command.first(), Some(b'!' | b'$')) => {
            // RFC 4266 §2.7 separates names with spaces in URLs; the UMN
            // wire grammar concatenates +BLOCK names. Names are case sensitive.
            let names: Vec<u8> = command[1..]
                .iter()
                .copied()
                .filter(|b| *b != b' ')
                .collect();
            if !names.starts_with(b"+")
                || names[1..].split(|b| *b == b'+').any(|name| {
                    name.is_empty() || !name.iter().all(|b| b.is_ascii_graphic() && *b != b':')
                })
            {
                return Err("Invalid Gopher+ attribute request".into());
            }
            Ok([&command[..1], &names].concat())
        }
        _ => Err("Unsupported Gopher+ command; use +, !, $, or a named view".into()),
    }
}

enum Framing {
    Eof,
    Length(u64),
    Dot,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DotState {
    LineStart,
    Dot,
    DotCr,
    Data,
}

/// `read` is cancellation safe: partially recognized dot prefixes are kept
/// here, and after producing bytes it never awaits an empty socket buffer.
pub(crate) struct Transfer {
    stream: BufReader<TcpStream>,
    framing: Framing,
    dot_state: DotState,
    pending: VecDeque<u8>,
    ended: bool,
    error: Option<io::Error>,
}

impl Transfer {
    pub(crate) async fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        use DotState::*;
        if out.is_empty() {
            return Ok(0);
        }
        match &mut self.framing {
            Framing::Eof => return self.stream.read(out).await,
            Framing::Length(remaining) => {
                if *remaining == 0 {
                    return Ok(0);
                }
                let room = (*remaining).min(out.len() as u64) as usize;
                let n = self.stream.read(&mut out[..room]).await?;
                if n == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "Gopher+ body ended before its declared length",
                    ));
                }
                *remaining -= n as u64;
                return Ok(n);
            }
            Framing::Dot => {}
        }
        let mut written = 0;
        loop {
            while written < out.len() {
                let Some(byte) = self.pending.pop_front() else {
                    break;
                };
                out[written] = byte;
                written += 1;
            }
            if written == out.len()
                || (written > 0 && (self.ended || self.stream.buffer().is_empty()))
            {
                return Ok(written);
            }
            if let Some(error) = self.error.take() {
                return Err(error);
            }
            if self.ended {
                return Ok(written);
            }
            let byte = match self.stream.read_u8().await {
                Ok(byte) => byte,
                Err(error) => {
                    self.pending.extend(match self.dot_state {
                        Dot => &b"."[..],
                        DotCr => &b".\r"[..],
                        _ => &[],
                    });
                    self.ended = true;
                    self.error = Some(if error.kind() == io::ErrorKind::UnexpectedEof {
                        io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "Gopher+ body ended before its dot terminator",
                        )
                    } else {
                        error
                    });
                    continue;
                }
            };
            match (self.dot_state, byte) {
                (LineStart, b'.') => self.dot_state = Dot,
                (Dot | DotCr, b'\n') => self.ended = true,
                (Dot, b'\r') => self.dot_state = DotCr,
                (Dot, b'.') => {
                    self.pending.push_back(b'.');
                    self.dot_state = Data;
                }
                (state, byte) => {
                    if state == Dot {
                        self.pending.push_back(b'.');
                    }
                    if state == DotCr {
                        self.pending.extend(b".\r");
                    }
                    self.pending.push_back(byte);
                    self.dot_state = if byte == b'\n' { LineStart } else { Data };
                }
            }
        }
    }
}

pub(crate) async fn open(url: &GopherUrl) -> Result<Transfer, String> {
    let mut transfer = Transfer {
        stream: BufReader::new(connect(url).await?),
        framing: Framing::Eof,
        dot_state: DotState::LineStart,
        pending: VecDeque::new(),
        ended: false,
        error: None,
    };
    if url.gopher_plus.is_none() {
        return Ok(transfer);
    }
    // UMN §2.3: the status sign and signed length precede every plus reply.
    // Never trust a remote length as an allocation size.
    timeout(IDLE_TIMEOUT, async {
        let mut header = Vec::new();
        loop {
            let byte = transfer
                .stream
                .read_u8()
                .await
                .map_err(|e| format!("Incomplete Gopher+ header: {e}"))?;
            header.push(byte);
            if byte == b'\n' {
                break;
            }
            if header.len() >= 64 {
                return Err("Gopher+ response header is too long".into());
            }
        }
        let line = header
            .strip_suffix(b"\r\n")
            .ok_or("Invalid Gopher+ response header")?;
        let (&sign, length) = line.split_first().ok_or("Empty Gopher+ response header")?;
        if !matches!(sign, b'+' | b'-') {
            return Err("Server did not return a Gopher+ response".into());
        }
        transfer.framing = match length {
            b"-1" => Framing::Dot,
            b"-2" => Framing::Eof,
            _ if !length.is_empty() && length.iter().all(u8::is_ascii_digit) => Framing::Length(
                std::str::from_utf8(length)
                    .unwrap()
                    .parse()
                    .map_err(|_| "Invalid Gopher+ response length")?,
            ),
            _ => return Err("Invalid Gopher+ response length".into()),
        };
        if sign == b'-' {
            let mut body = Vec::new();
            let mut bytes = [0; 1024];
            while body.len() < MAX_LINE {
                match transfer.read(&mut bytes).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => body.extend_from_slice(&bytes[..n]),
                }
            }
            let (message, _) = text_reply::display_text(&body, false);
            let (status, detail) = message.split_once('\n').unwrap_or((&message, ""));
            let detail = detail.split_whitespace().collect::<Vec<_>>().join(" ");
            // Keep the explanation visible in a one-line browser status bar;
            // the first protocol line usually contains only a code and admin.
            return Err(if detail.is_empty() {
                format!("Gopher+ server error: {}", status.trim())
            } else {
                format!("Gopher+ server error: {detail} ({})", status.trim())
            });
        }
        Ok(transfer)
    })
    .await
    .map_err(|_| "Gopher+ response header timed out".to_string())?
}

pub fn information_target(
    selected: Option<&Link>,
    current: Option<&Link>,
    page: bool,
) -> Result<Link, String> {
    let as_gopher = |link: &Link| match link {
        Link::Gopher(url) => Some(url.clone()),
        Link::Http(url) if url.scheme() == "gopher" => GopherUrl::parse(url.as_str()),
        _ => None,
    };
    let target = if page {
        current.and_then(as_gopher)
    } else {
        selected
            .and_then(as_gopher)
            .or_else(|| current.and_then(as_gopher))
    };
    match target {
        Some(url) => Ok(Link::Gopher(url.with_plus(b"!"))),
        _ => Err("Item information applies to Gopher pages and links.".into()),
    }
}

pub(super) fn information_lines(url: &GopherUrl, body: &[u8], encoding: Encoding) -> Vec<DocLine> {
    let mut lines = Vec::new();
    let mut target = None;
    let mut block = &b""[..];
    let mut has_info = false;
    let mut view_links = 0;
    let mut views_omitted = false;
    let line = |kind, text, link| DocLine { kind, text, link };
    let mut source = split_lines(body).take(MAX_ROWS).peekable();
    while let Some(raw) = source.next() {
        if let Some(info) = raw.strip_prefix(b"+INFO:") {
            let (_, label, mut link) =
                menu_item(info.strip_prefix(b" ").unwrap_or(info), &mut None);
            target = if let Some(Link::Gopher(item)) = &link {
                let mut item = item.clone();
                if item.host == url.host && item.port == url.port && item.selector == url.selector {
                    item.query = url.query.clone();
                }
                Some(item)
            } else {
                None
            };
            if let Some(target) = &target {
                link = Some(Link::Gopher(target.clone()));
            }
            has_info = true;
            block = b"INFO";
            let title = if label.is_empty() && link.is_some() {
                "Open item".into()
            } else {
                decode(label, encoding)
            };
            lines.push(line(Kind::Heading(2), title, link));
        } else if raw.starts_with(b"+") {
            let Some(colon) = raw.iter().position(|b| *b == b':') else {
                continue;
            };
            block = &raw[1..colon];
            let title = match block {
                b"ADMIN" => "Details".into(),
                b"ABSTRACT" => "Description".into(),
                b"VIEWS" => "Available formats".into(),
                b"ASK" => {
                    "This item requires a Gopher+ form. Form submission is not supported yet."
                        .into()
                }
                _ => decode(block, encoding),
            };
            lines.push(line(
                if block == b"ASK" {
                    Kind::Info
                } else {
                    Kind::Heading(2)
                },
                title,
                None,
            ));
            // Referenced attributes are followed only on an explicit click.
            let rest = raw[colon + 1..]
                .strip_prefix(b" ")
                .unwrap_or(&raw[colon + 1..]);
            if !rest.is_empty() {
                let (_, label, link) = menu_item(rest, &mut None);
                // UMN §2.5: an explicit attribute value takes precedence
                // over a reference to a separate attribute document.
                if link.is_none() || !source.peek().is_some_and(|next| next.starts_with(b" ")) {
                    lines.push(line(Kind::Text, decode(label, encoding), link));
                }
            }
        } else if let Some(value) = raw.strip_prefix(b" ") {
            let mut link = None;
            if block == b"VIEWS"
                && let Some(target) = &target
                && let Some(colon) = value.iter().position(|b| *b == b':')
            {
                let view = value[..colon].trim_ascii();
                let command = [b"+".as_slice(), view].concat();
                if request_command(target, &command).is_ok() {
                    // One long selector can be repeated by many tiny view
                    // records. Bound that expansion as well as input bytes.
                    if view_links < text_reply::MAX_LINKS {
                        link = Some(Link::Gopher(target.with_plus(&command)));
                        view_links += 1;
                    } else {
                        views_omitted = true;
                    }
                }
            }
            lines.push(line(
                if link.is_some() {
                    Kind::Document
                } else {
                    Kind::Text
                },
                decode(value, encoding),
                link,
            ));
        }
    }
    if !has_info {
        lines.insert(
            0,
            line(
                Kind::Info,
                "No Gopher+ item information was returned.".into(),
                None,
            ),
        );
    }
    if views_omitted {
        lines.push(line(
            Kind::Info,
            "Additional formats are shown as text at the format-link limit.".into(),
            None,
        ));
    }
    if url.gopher_plus.as_deref() == Some(b"?") && !body.windows(5).any(|s| s == b"+ASK:") {
        lines.insert(
            0,
            line(
                Kind::Info,
                "This item requires a Gopher+ form. Form submission is not supported yet.".into(),
                None,
            ),
        );
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    #[test]
    fn gopher_plus_url_and_wire_fields_preserve_queries_views_and_opaque_selectors() {
        for (address, expected) in [
            (
                "gopher://e/0/a/../%FF%09%09+",
                b"/a/../\xff\t+\r\n".as_slice(),
            ),
            ("gopher://e/7/s%09rust%20lang%09+", b"/s\trust lang\t+\r\n"),
            ("gopher://e/7/s%09%09!", b"/s\t\t!\r\n"),
            (
                "gopher://e/0/a%09%09+Text/plain%20De_DE",
                b"/a\t+Text/plain De_DE\r\n",
            ),
            (
                "gopher://e/1/%09%09!+VIEWS%20+ABSTRACT",
                b"/\t!+VIEWS+ABSTRACT\r\n",
            ),
            ("gopher://e/1/%09%09$+ADMIN+VIEWS", b"/\t$+ADMIN+VIEWS\r\n"),
        ] {
            let url = GopherUrl::parse(address).unwrap();
            assert_eq!(url.request().unwrap(), expected);
            assert_eq!(GopherUrl::parse(&url.to_string()), Some(url));
        }
        let endpoint = GopherUrl::parse("gopher://e/7/s").unwrap().with_plus(b"+");
        assert_eq!(endpoint.to_string(), "gopher://e/7/s%09%09%2B");
        assert!(
            GopherUrl::parse(&endpoint.to_string())
                .unwrap()
                .needs_query()
        );
        assert!(!endpoint.with_plus(b"!").needs_query());
        for command in [
            b"BOGUS".as_slice(),
            b"!+",
            b"+text/plain\r\n/second",
            b"+\t1\r\n+-1\r\nsecret\r\n.\r\n",
        ] {
            assert!(endpoint.with_plus(command).request().is_err());
        }
    }

    #[test]
    fn gopher_plus_menu_capabilities_and_mirrors_are_independent() {
        let url = GopherUrl::parse("gopher://e").unwrap();
        let doc = parse(&url, b"0Plus\t/a\te\t70\t+\r\n+Mirror\t/a\tm\t70\r\n7Ask\t/form\te\t70\t?\r\n0Old\t/b\to\t70\r\n.\r\n".to_vec(), false, 80);
        let urls: Vec<_> = doc
            .lines
            .iter()
            .filter_map(|line| match &line.link {
                Some(Link::Gopher(url)) => Some(url),
                _ => None,
            })
            .collect();
        assert_eq!(urls.len(), 4);
        assert_eq!(urls[0].gopher_plus.as_deref(), Some(b"+".as_slice()));
        assert_eq!(urls[1].item_type, '0');
        assert_eq!(urls[1].gopher_plus, None);
        assert_eq!(urls[2].request().unwrap(), b"/form\t\t!\r\n");
        assert!(!urls[2].needs_query());
        assert_eq!(urls[3].gopher_plus, None);
    }

    #[test]
    fn gopher_plus_metadata_links_use_item_destinations_and_survive_rewrap() {
        let url = GopherUrl::parse("gopher://directory.test/1/%09%09$").unwrap();
        let raw = b"+INFO: 0A document\t/opaque/../\xff\tone.test\t70\t+\r\n+ADMIN:\r\n Mod-Date: today <20260913010203>\r\n+VIEWS:\r\n Text/HTML En_US: <2k>\r\n image/webp: <7k>\r\n application/pdf: <10k>\r\n+CUSTOM:\r\n \x1b[31mPlain details\r\n+INFO: 1Elsewhere\t/two\ttwo.test\t7070\t+\r\n+VIEWS:\r\n application/gopher+-menu: <1k>\r\n";
        let mut doc = parse(&url, raw.to_vec(), false, 30);
        let links: Vec<_> = doc
            .lines
            .iter()
            .filter_map(|line| line.link.clone())
            .collect();
        let Link::Gopher(html) = &links[1] else {
            panic!()
        };
        assert_eq!(html.host, "one.test");
        assert_eq!(html.selector, b"/opaque/../\xff");
        assert!(html.is_html());
        let Link::Gopher(image) = &links[2] else {
            panic!()
        };
        assert!(image.is_image());
        assert!(!image.is_text());
        let Link::Gopher(pdf) = &links[3] else {
            panic!()
        };
        assert!(pdf.is_download());
        let Link::Gopher(menu) = links.last().unwrap() else {
            panic!()
        };
        assert!(menu.is_menu());
        assert_eq!(menu.host, "two.test");
        assert_eq!(menu.port, 7070);
        assert!(doc.lines.iter().all(|line| !line.text.contains('\x1b')));
        doc.rerender_reply(100);
        assert_eq!(
            doc.lines
                .iter()
                .filter_map(|line| line.link.clone())
                .collect::<Vec<_>>(),
            links
        );
        assert_eq!(doc.raw, raw);
    }

    #[test]
    fn gopher_plus_ask_and_multimedia_open_information_without_submission() {
        for kind in ['0', '7', '9', ':', ';', '<'] {
            let url = GopherUrl::new("e".into(), 70, kind, b"/item".to_vec()).with_plus(b"?");
            assert!(url.is_metadata());
            assert!(!url.is_download());
            assert!(!url.needs_query());
            assert!(url.request().unwrap().ends_with(b"\t!\r\n"));
            let doc = parse(
                &url,
                b"+INFO: 0Item\t/item\te\t70\t?\r\n+ASK:\r\n AskP: Password\r\n".to_vec(),
                false,
                100,
            );
            assert!(
                doc.lines
                    .iter()
                    .any(|line| line.text.contains("Form submission is not supported"))
            );
        }
        let url = GopherUrl::parse("gopher://e/:/image%09%09+").unwrap();
        assert!(url.is_metadata());
        assert_eq!(url.request().unwrap(), b"/image\t!\r\n");
        let view = url.with_plus(b"+image/webp");
        assert!(view.is_image());
        assert_eq!(view.request().unwrap(), b"/image\t+image/webp\r\n");
    }

    #[test]
    fn gopher_plus_metadata_retains_search_and_does_not_reuse_malformed_items() {
        let url = GopherUrl::parse("gopher://e/7/s%09query%09!").unwrap();
        let doc = parse(&url, b"+INFO: 7Search\t/s\te\t70\t+\r\n+VIEWS:\r\n text/plain: <1k>\r\n+ABSTRACT: 0Remote abstract\t/abstract\te\t70\t+\r\n Inline description\r\n+INFO: malformed\r\n+VIEWS:\r\n image/webp: <1k>\r\n".to_vec(), false, 80);
        let links: Vec<_> = doc
            .lines
            .iter()
            .filter_map(|line| match &line.link {
                Some(Link::Gopher(url)) => Some(url),
                _ => None,
            })
            .collect();
        assert_eq!(
            links.len(),
            2,
            "inline abstracts override references; malformed items have no format links"
        );
        assert_eq!(links[0].query.as_deref(), Some(b"query".as_slice()));
        assert_eq!(links[1].request().unwrap(), b"/s\tquery\t+text/plain\r\n");
    }

    async fn fixture(
        header: &[u8],
        data: &[u8],
        hold: bool,
    ) -> (GopherUrl, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = GopherUrl::new(
            "127.0.0.1".into(),
            listener.local_addr().unwrap().port(),
            '0',
            b"/item".to_vec(),
        )
        .with_plus(b"+");
        let reply = [header, data].concat();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n") {
                request.push(stream.read_u8().await.unwrap());
            }
            assert_eq!(request, b"/item\t+\r\n");
            for chunk in reply.chunks(if reply.len() > 4096 { 8192 } else { 3 }) {
                if stream.write_all(chunk).await.is_err() {
                    return;
                }
                tokio::task::yield_now().await;
            }
            if hold {
                std::future::pending::<()>().await;
            }
        });
        (url, server)
    }

    #[tokio::test]
    async fn gopher_plus_frames_finish_without_eof_and_unframe_only_once() {
        for (header, wire, expected, hold) in [
            (
                b"+18\r\n".as_slice(),
                b"first\r\n.\r\n..last\r\nTRAILER".as_slice(),
                b"first\r\n.\r\n..last\r\n".as_slice(),
                true,
            ),
            (
                b"+-1\r\n",
                b"first\r\n..\r\n...last\r\n.\r\nTRAILER",
                b"first\r\n.\r\n..last\r\n",
                true,
            ),
            (
                b"+-2\r\n",
                b"first\r\n.\r\n..last\r\n",
                b"first\r\n.\r\n..last\r\n",
                false,
            ),
            (b"+0\r\n", b"", b"", true),
        ] {
            let (url, server) = fixture(header, wire, hold).await;
            let page = timeout(Duration::from_secs(2), fetch(&url))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(page.reply.body, expected);
            assert!(page.reply.notice.is_none());
            let mut doc = render(&url, page, 80);
            if !expected.is_empty() {
                assert_eq!(
                    doc.lines
                        .iter()
                        .map(|l| l.text.as_str())
                        .collect::<Vec<_>>(),
                    ["first", ".", "..last"]
                );
                doc.rerender_reply(120);
                assert_eq!(doc.lines[2].text, "..last");
            }
            server.abort();
        }
    }

    #[tokio::test]
    async fn gopher_plus_truncated_bodies_keep_the_readable_prefix() {
        for header in [b"+100\r\n".as_slice(), b"+-1\r\n"] {
            let (url, server) = fixture(header, b"Readable\r\n.", false).await;
            let page = fetch(&url).await.unwrap();
            assert_eq!(page.reply.body, b"Readable\r\n.");
            assert!(page.reply.notice.unwrap().contains("Incomplete"));
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn gopher_plus_errors_headers_and_limits_are_bounded() {
        for header in [
            b"+wat\r\n".as_slice(),
            b"+-3\r\n",
            b"+18446744073709551616\r\n",
            b"legacy\r\n",
            b"+1\n",
        ] {
            let (url, server) = fixture(header, b"", false).await;
            assert!(fetch(&url).await.is_err());
            server.abort();
        }
        let (url, server) = fixture(
            b"--2\r\n",
            b"2 Admin <a@e>\r\nTry again later\x1b[31m\r\n",
            false,
        )
        .await;
        let error = fetch(&url).await.unwrap_err();
        assert!(error.contains("Try again later"));
        assert!(!error.contains('\x1b'));
        server.await.unwrap();
        let (url, server) =
            fixture(b"+999999999999\r\n", &vec![b'x'; MAX_RESPONSE + 1], false).await;
        let page = fetch(&url).await.unwrap();
        assert_eq!(page.reply.body.len(), MAX_RESPONSE);
        assert!(page.reply.notice.unwrap().contains("2 MiB"));
        server.abort();
    }

    #[tokio::test]
    async fn gopher_plus_streaming_can_cancel_at_a_split_dot_prefix() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = GopherUrl::new(
            "127.0.0.1".into(),
            listener.local_addr().unwrap().port(),
            '0',
            b"/item".to_vec(),
        )
        .with_plus(b"+");
        let (resume, wait) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            while stream.read_u8().await.unwrap() != b'\n' {}
            stream.write_all(b"+-1\r\nFirst\r\n.").await.unwrap();
            wait.await.unwrap();
            stream.write_all(b".Second\r\n.\r\n").await.unwrap();
        });
        let mut resume = Some(resume);
        let mut updates = Vec::new();
        let reply = timeout(
            Duration::from_secs(2),
            fetch_updates(&url, |reply| {
                updates.push(reply.body);
                if let Some(resume) = resume.take() {
                    resume.send(()).unwrap();
                }
                async { true }
            }),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(updates, [b"First\r\n".to_vec()]);
        assert_eq!(reply.body, b"First\r\n.Second\r\n");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn gopher_plus_generic_images_are_detected_after_the_header() {
        let body = file_tests::webp();
        let response = [format!("+{}\r\n", body.len()).as_bytes(), &body].concat();
        let (url, server) = file_tests::serve(response, b"/image").await;
        let url = url.with_plus(b"+");
        let FileResponse::Document(response) = fetch_file(&url).await.unwrap() else {
            panic!()
        };
        assert_eq!(response.content_type, "image/webp");
        assert_eq!(response.body, body);
        assert_eq!(server.await.unwrap(), b"/image\t+\r\n");
    }
}
