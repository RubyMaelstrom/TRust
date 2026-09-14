//! Gemtext 0.24.1: line-oriented parsing, mandatory monospaced preformatted
//! blocks, optional heading/list/quote styles, and preformatted alt text.

use super::{MediaType, Response};
use crate::doc::{Doc, DocLine, Kind, Link, push_wrapped};
use crate::text_reply::{MAX_COLUMNS, MAX_LINKS, MAX_ROWS};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Heading {
    pub row: usize,
    pub level: u8,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct View {
    pub controls: crate::text_reply::View,
    pub owners: Vec<usize>,
    pub sources: Vec<usize>,
    pub headings: Vec<Heading>,
    pub show_alt: bool,
    pub outline: bool,
    pub reading_columns: usize,
}
impl Default for View {
    fn default() -> Self {
        Self {
            controls: crate::text_reply::View {
                wrap: true,
                ..Default::default()
            },
            owners: Vec::new(),
            sources: Vec::new(),
            headings: Vec::new(),
            show_alt: false,
            outline: false,
            reading_columns: 96,
        }
    }
}

impl Response {
    pub fn document(&self, width: usize) -> Doc {
        let mut view = self.view.clone();
        view.controls.loading = !self.finished;
        // Gemini 0.24.1, Closing connections: report an abrupt TLS close to
        // the user through status_text (including COMMAND), without adding
        // transport diagnostics to the received document. Rendering below
        // supplies its own display-limit notices.
        view.controls.notice = None;
        if (20..30).contains(&self.status) {
            return render(
                Link::Gemini(self.url.public_url()),
                &self.meta,
                &self.body,
                width,
                view,
            );
        }
        let mut lines = vec![DocLine {
            kind: Kind::Heading(1),
            text: super::status_name(self.status).into(),
            link: None,
        }];
        if !self.meta.is_empty() {
            lines.push(DocLine {
                kind: Kind::Info,
                text: self.meta.clone(),
                link: None,
            });
        }
        if (30..40).contains(&self.status) {
            lines.push(DocLine {
                kind: Kind::GemLink,
                text: format!("Continue to {}", self.meta),
                link: Some(super::resolve_reference(&self.url, &self.meta, true)),
            });
        }
        if let Some(notice) = &self.notice {
            lines.push(DocLine {
                kind: Kind::Error,
                text: notice.clone(),
                link: None,
            });
        }
        Doc::from_lines(
            Link::Gemini(self.url.public_url()),
            lines,
            Vec::new(),
            width,
            false,
            None,
        )
    }
}

pub fn render(url: Link, meta: &str, body: &[u8], width: usize, mut view: View) -> Doc {
    let result = MediaType::parse(meta).and_then(|media| {
        if !media.is_text() {
            return Err(format!("Save or open this {meta} response."));
        }
        let text = media.decode(body, view.controls.loading)?;
        // Bare CR is not a Gemini line ending. Filter it as a control while
        // retaining LF and CRLF; display_text additionally bounds geometry.
        let text = text.replace("\r\n", "\n").replace('\r', "");
        let (text, clipped) =
            crate::text_reply::display_text(text.as_bytes(), view.controls.loading);
        if clipped {
            view.controls.notice =
                Some("Display limit reached; save the received source for the full text.".into());
        }
        let columns = if width > MAX_COLUMNS {
            width
        } else {
            width.max(10).min(view.reading_columns)
        };
        let columns = if view.controls.wrap {
            columns
        } else {
            usize::MAX / 4
        };
        Ok(parse_lines(
            &text,
            columns,
            media.essence == "text/gemini",
            &mut view,
            &|target| match &url {
                Link::Gemini(base) => super::resolve(base, target),
                Link::Http(base) => super::absolute_link(target)
                    .or_else(|| base.join(target).ok().map(Link::Http))
                    .unwrap_or_else(|| Link::External(target.into())),
                _ => super::absolute_link(target).unwrap_or_else(|| Link::External(target.into())),
            },
        ))
    });
    let mut lines = result.unwrap_or_else(|error| {
        vec![DocLine {
            kind: Kind::Error,
            text: error,
            link: None,
        }]
    });
    if view.outline {
        lines = view
            .headings
            .iter()
            .enumerate()
            .map(|(index, heading)| DocLine {
                kind: Kind::Heading(heading.level),
                text: format!("{}. {}", index + 1, heading.text),
                link: None,
            })
            .collect();
        if lines.is_empty() {
            lines.push(DocLine {
                kind: Kind::Info,
                text: "This page has no headings.".into(),
                link: None,
            });
        }
        lines.push(DocLine {
            kind: Kind::Info,
            text: "Use heading N to jump, or outline to return to the page.".into(),
            link: None,
        });
        view.owners = (0..lines.len()).collect();
    }
    if let Some(notice) = &view.controls.notice {
        lines.push(DocLine {
            kind: Kind::Info,
            text: notice.clone(),
            link: None,
        });
    }
    let mut doc = Doc::from_lines(url, lines, body.to_vec(), width, false, Some(meta.into()));
    doc.gemini = Some(view);
    doc
}

pub fn parse_gemtext(body: &[u8], width: usize, resolve: &dyn Fn(&str) -> Link) -> Vec<DocLine> {
    let text = String::from_utf8_lossy(body);
    let text = text
        .strip_prefix('\u{feff}')
        .unwrap_or(&text)
        .replace("\r\n", "\n")
        .replace('\r', "");
    let (text, _) = crate::text_reply::display_text(text.as_bytes(), false);
    parse_lines(&text, width.max(1), true, &mut View::default(), resolve)
}

fn parse_lines(
    text: &str,
    width: usize,
    gemtext: bool,
    view: &mut View,
    resolve: &dyn Fn(&str) -> Link,
) -> Vec<DocLine> {
    view.owners.clear();
    view.sources.clear();
    view.headings.clear();
    let mut lines = Vec::new();
    let mut pre = false;
    let mut links = 0;
    for (source, line) in text.lines().enumerate() {
        if lines.len() >= MAX_ROWS {
            view.controls.notice =
                Some("Display limited to 4,096 rows; save the source to read more.".into());
            break;
        }
        let (kind, text, link) = if gemtext && line.starts_with("```") {
            pre = !pre;
            let alt = line[3..].trim();
            if !pre || !view.show_alt || alt.is_empty() {
                continue;
            }
            (Kind::Info, format!("[Preformatted: {alt}]"), None)
        } else if pre {
            (Kind::Pre, line.to_string(), None)
        } else if gemtext && line.starts_with("=>") {
            let rest = line[2..].trim_start_matches([' ', '\t']);
            let (target, label) = rest
                .split_once([' ', '\t'])
                .map_or((rest, ""), |(t, l)| (t, l.trim()));
            if target.is_empty() {
                continue;
            }
            let link = if links < MAX_LINKS {
                links += 1;
                Some(resolve(target))
            } else {
                None
            };
            (
                Kind::GemLink,
                if label.is_empty() { target } else { label }.to_string(),
                link,
            )
        } else if gemtext && line.starts_with('#') {
            let level = line.bytes().take_while(|&b| b == b'#').count().min(3) as u8;
            let text = line[level as usize..].trim_start().to_string();
            view.headings.push(Heading {
                row: lines.len(),
                level,
                text: text.clone(),
            });
            (Kind::Heading(level), text, None)
        } else if gemtext && line.starts_with("* ") {
            (Kind::List, line[2..].to_string(), None)
        } else if gemtext && line.starts_with('>') {
            (Kind::Quote, line[1..].trim_start().to_string(), None)
        } else {
            (Kind::Text, line.to_string(), None)
        };
        let owner = lines.len();
        if kind == Kind::Pre {
            lines.push(DocLine { kind, text, link });
        } else {
            push_wrapped(&mut lines, kind, text, link, width);
        }
        if lines.len() > MAX_ROWS {
            lines.truncate(MAX_ROWS);
            view.controls.notice =
                Some("Display limited to 4,096 rows; save the source to read more.".into());
        }
        view.owners.resize(lines.len(), owner);
        view.sources.resize(lines.len(), source);
    }
    if links == MAX_LINKS {
        view.controls.notice.get_or_insert(
            "Only the first 256 links are active; save the source to read more.".into(),
        );
    }
    lines
}

pub fn source_offer(doc: &Doc) -> Result<crate::download::DownloadOffer, String> {
    let url = url::Url::parse(&doc.url.to_string()).map_err(|e| e.to_string())?;
    let gemtext = doc
        .meta
        .as_deref()
        .and_then(|m| MediaType::parse(m).ok())
        .is_some_and(|m| m.essence == "text/gemini");
    Ok(crate::download::DownloadOffer::from_bytes(
        url,
        if gemtext {
            "gemini-source.gmi"
        } else {
            "gemini-source.txt"
        }
        .into(),
        doc.raw.clone(),
    ))
}

/// Pick a heading in the full document, independently of frontend geometry.
pub fn heading_row(view: &View, current: usize, which: &str) -> Result<usize, &'static str> {
    let headings = &view.headings;
    if headings.is_empty() {
        return Err("This page has no headings.");
    }
    let heading = match which {
        "next" => headings
            .iter()
            .find(|h| h.row > current)
            .or(headings.first()),
        "previous" | "prev" => headings
            .iter()
            .rev()
            .find(|h| h.row < current)
            .or(headings.last()),
        number => number
            .parse::<usize>()
            .ok()
            .and_then(|n| n.checked_sub(1))
            .and_then(|n| headings.get(n)),
    };
    heading
        .map(|h| h.row)
        .ok_or("Use heading next|previous|N (a number from the outline).")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounded_rendering_retains_source_alt_headings_and_link_ownership() {
        let url = super::super::GeminiUrl::parse("gemini://example.org/dir/page").unwrap();
        let body = b"\xef\xbb\xbf# Heading\r\n=> ?q A long label wraps onto several rows\r\n```diagram\r\nX\tY\r\n```\r\n";
        let mut doc = super::super::parse(&url, "TEXT/GEMINI; charset=\"UTF-8\"", body, 12);
        assert_eq!(doc.raw, body);
        assert_eq!(doc.gemini.as_ref().unwrap().headings[0].text, "Heading");
        assert_eq!(doc.line_link(2), doc.line_link(1));
        assert!(doc.line_link(2).is_some());
        assert!(!doc.lines.iter().any(|l| l.text.contains("diagram")));
        doc.gemini.as_mut().unwrap().show_alt = true;
        doc.rerender_reply(80);
        assert!(doc.lines.iter().any(|l| l.text.contains("diagram")));
        assert!(
            doc.lines
                .iter()
                .any(|l| l.kind == Kind::Pre && l.text == "X       Y")
        );
        let raw = "x\n".repeat(10000);
        let doc = super::super::parse(&url, "text/gemini", raw.as_bytes(), 80);
        assert!(doc.lines.len() <= MAX_ROWS + 1);
        assert_eq!(doc.raw.len(), raw.len());
        assert!(doc.gemini.unwrap().controls.notice.is_some());
    }
}
