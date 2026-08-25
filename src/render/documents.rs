//! Graphical presentation adapter for TRust's line-model protocols.
//!
//! Gopher, Gemini, Finger, WHOIS and DICT retain their protocol-neutral
//! [`Doc`](crate::doc::Doc) parsing. This adapter performs proportional Parley
//! wrapping directly in CSS pixels and emits the same display list as HTML; it
//! never round-trips through terminal cells. RFC 1436 requires clients to
//! distinguish Gopher item types for users, and Gemtext 0.24.1 deliberately
//! leaves presentation to the user agent while requiring preformatted lines
//! to retain monowidth spacing. The semantic palette below is therefore the
//! same one used by TRust's terminal frontend, not author-controlled styling.

use crate::core::{BrowserPage, CssPoint, CssSize, FetchedDocument};
use crate::doc::{Doc, DocLine, Kind, Link};
use crate::text::{self, TextBreakStyle, TextStyle};

use super::{
    CssRect, DecorationStyle, DisplayCommand, HitRegion, PagePaint, PaintColor, PaintLine,
    TextDecorationPaint,
};

#[derive(Clone, Debug, PartialEq)]
pub struct ProtocolLine {
    pub rect: CssRect,
    pub link: Option<Link>,
}

#[derive(Clone, Debug)]
pub struct ProtocolPaint {
    pub paint: PagePaint,
    /// One entry per parsed line, preserving Gopherus document order while
    /// carrying graphical CSS-pixel bounds for desktop navigation.
    pub lines: Vec<ProtocolLine>,
}

pub fn document(page: &BrowserPage) -> Option<Doc> {
    Some(match (&page.document, page.target()) {
        (FetchedDocument::Gopher(raw), Link::Gopher(url)) => {
            crate::gopher::parse(url, raw.clone(), false, usize::MAX / 4)
        }
        (FetchedDocument::Gemini(response), Link::Gemini(url)) => {
            crate::gemini::parse(url, &response.meta, &response.body, usize::MAX / 4)
        }
        (FetchedDocument::OneShot(raw), Link::OneShot(url)) => {
            crate::oneshot::parse(url, raw.clone(), usize::MAX / 4)
        }
        (FetchedDocument::Internal(raw), Link::External(url)) => {
            let lines = crate::gemini::parse_gemtext(raw, usize::MAX / 4, &|target| {
                crate::gemini::absolute_link(target)
                    .unwrap_or_else(|| Link::External(target.to_string()))
            });
            Doc::from_lines(
                Link::External(url.clone()),
                lines,
                raw.clone(),
                usize::MAX / 4,
                false,
                Some(String::from("text/gemini")),
            )
        }
        (FetchedDocument::Http(response), Link::Http(url)) => {
            let text = String::from_utf8_lossy(&response.body);
            Doc::from_lines(
                Link::Http(url.clone()),
                text.lines()
                    .map(|text| DocLine {
                        kind: Kind::Text,
                        text: text.to_string(),
                        link: None,
                    })
                    .collect(),
                response.body.clone(),
                usize::MAX / 4,
                false,
                Some(response.content_type.clone()),
            )
        }
        _ => return None,
    })
}

pub fn page(page: &BrowserPage, viewport: CssSize) -> Option<PagePaint> {
    let doc = document(page)?;
    Some(paint_doc(&doc, viewport.width))
}

pub fn paint_doc(doc: &Doc, viewport_width: f32) -> PagePaint {
    paint_doc_selected(doc, viewport_width, None).paint
}

pub fn paint_doc_selected(
    doc: &Doc,
    viewport_width: f32,
    selected: Option<usize>,
) -> ProtocolPaint {
    let left = 22.0;
    let width = (viewport_width - left * 2.0).max(40.0);
    let mut paint = PagePaint {
        background: Some(theme_color(crate::theme::BG)),
        ..PagePaint::default()
    };
    let mut lines = Vec::with_capacity(doc.lines.len());
    let mut y = 22.0;
    let mut far_right = viewport_width.max(0.0);
    for (line_index, line) in doc.lines.iter().enumerate() {
        let (mut style, normal_color) = line_style(line.kind);
        let is_selected = selected == Some(line_index) && line.link.is_some();
        if is_selected {
            style.weight = 700.0;
        }
        let color = if is_selected {
            theme_color(crate::theme::BG)
        } else {
            normal_color
        };
        let line_top = y;
        let mut line_width = 1.0f32;
        let mut remaining = line.text.as_str();
        loop {
            let end = if remaining.is_empty() {
                0
            } else {
                text::first_line_end(
                    remaining,
                    &style,
                    width,
                    TextBreakStyle {
                        wrap: !matches!(line.kind, Kind::Pre),
                        ..TextBreakStyle::default()
                    },
                )
            };
            let end = if end == 0 && !remaining.is_empty() {
                remaining.chars().next().map_or(0, char::len_utf8)
            } else {
                end
            };
            let piece = remaining.get(..end).unwrap_or(remaining);
            let shaped = text::shape(piece, &style);
            let origin = CssPoint::new(left, y);
            let rect = CssRect::new(
                origin.x,
                origin.y,
                shaped.advance.max(1.0),
                shaped.line_height.max(style.size),
            );
            if is_selected {
                paint.primitives.push(DisplayCommand::FillRect {
                    rect,
                    color: normal_color,
                });
            }
            paint.lines.push(PaintLine {
                rect,
                baseline: y + shaped.baseline,
                ascent: shaped.ascent,
                descent: shaped.descent,
            });
            paint.primitives.push(DisplayCommand::GlyphRun {
                origin,
                shaped: shaped.clone(),
                color,
                decoration: TextDecorationPaint {
                    color,
                    style: DecorationStyle::Solid,
                },
                clip: None,
                node: line_index + 1,
                link: line.link.clone(),
            });
            paint.primitives.push(DisplayCommand::HitRegion(HitRegion {
                rect,
                node: line_index + 1,
                actor: None,
                link: line.link.clone(),
                cursor: None,
            }));
            line_width = line_width.max(rect.width);
            far_right = far_right.max(rect.x + rect.width + left);
            y += shaped.line_height.max(style.size * 1.2);
            if remaining.is_empty() || end >= remaining.len() {
                break;
            }
            remaining = remaining[end..].trim_start_matches(' ');
        }
        lines.push(ProtocolLine {
            rect: CssRect::new(left, line_top, line_width, (y - line_top).max(style.size)),
            link: line.link.clone(),
        });
    }
    // Gemtext requires preformatted lines to remain unwrapped and recommends
    // horizontal scrolling in graphical clients. Preserve their actual width
    // in the page extent instead of clipping it to the viewport.
    paint.width = far_right;
    paint.height = y + 22.0;
    ProtocolPaint { paint, lines }
}

fn line_style(kind: Kind) -> (TextStyle, PaintColor) {
    let mut style = TextStyle {
        family: String::from(crate::theme::TERMINAL_FONT_FAMILY),
        size: crate::theme::TERMINAL_FONT_SIZE_CSS_PX,
        weight: crate::theme::TERMINAL_FONT_WEIGHT,
        ..TextStyle::default()
    };
    let color = match kind {
        Kind::Heading(1) => {
            style.weight = 700.0;
            theme_color(crate::theme::NEON_PINK)
        }
        Kind::Heading(2) => {
            style.weight = 700.0;
            theme_color(crate::theme::NEON_CYAN)
        }
        Kind::Heading(_) => theme_color(crate::theme::NEON_CYAN),
        Kind::GemLink | Kind::Dir => {
            style.weight = 700.0;
            theme_color(crate::theme::NEON_CYAN)
        }
        Kind::Document => theme_color(crate::theme::NEON_GREEN),
        Kind::Search => theme_color(crate::theme::PASTEL_GREEN),
        Kind::OtherLink => theme_color(crate::theme::NEON_PINK),
        Kind::Error => theme_color(crate::theme::NEON_PINK),
        Kind::Quote => theme_color(crate::theme::DIM),
        Kind::Pre => theme_color(crate::theme::NEON_GREEN),
        _ => theme_color(crate::theme::TEXT),
    };
    (style, color)
}

const fn theme_color(rgb: crate::theme::Rgb) -> PaintColor {
    PaintColor::Rgba(rgb[0], rgb[1], rgb[2], 255)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgba(rgb: crate::theme::Rgb) -> PaintColor {
        PaintColor::Rgba(rgb[0], rgb[1], rgb[2], 255)
    }

    #[test]
    fn protocol_adapter_wraps_using_shaped_advances_and_keeps_links() {
        let link = Link::External(String::from("mailto:test@example.com"));
        let doc = Doc::from_lines(
            Link::External(String::from("test:")),
            vec![DocLine {
                kind: Kind::GemLink,
                text: String::from("WWWW iiiiiiii WWWW iiiiiiii"),
                link: Some(link.clone()),
            }],
            Vec::new(),
            0,
            false,
            None,
        );
        let paint = paint_doc(&doc, 140.0);
        assert!(paint.lines.len() > 1);
        assert!(paint.primitives.iter().any(|command| matches!(
            command,
            DisplayCommand::HitRegion(HitRegion { link: Some(found), .. }) if found == &link
        )));
    }

    #[test]
    fn protocol_styles_share_the_terminal_palette_and_eleven_point_face() {
        let cases = [
            (
                Kind::Text,
                crate::theme::TEXT,
                crate::theme::TERMINAL_FONT_WEIGHT,
            ),
            (Kind::Dir, crate::theme::NEON_CYAN, 700.0),
            (Kind::Document, crate::theme::NEON_GREEN, 700.0),
            (Kind::Search, crate::theme::PASTEL_GREEN, 700.0),
            (Kind::OtherLink, crate::theme::NEON_PINK, 700.0),
            (Kind::Heading(1), crate::theme::NEON_PINK, 700.0),
            (Kind::Heading(2), crate::theme::NEON_CYAN, 700.0),
            (Kind::Quote, crate::theme::DIM, 700.0),
            (Kind::Pre, crate::theme::NEON_GREEN, 700.0),
        ];
        for (kind, expected_color, expected_weight) in cases {
            let (style, color) = line_style(kind);
            assert_eq!(style.family, crate::theme::TERMINAL_FONT_FAMILY);
            assert_eq!(style.size, crate::theme::TERMINAL_FONT_SIZE_CSS_PX);
            assert_eq!(style.weight, expected_weight);
            assert_eq!(color, rgba(expected_color));
            assert!(!style.underline, "the TUI does not underline {kind:?}");
        }
    }

    #[test]
    fn selected_protocol_link_uses_the_tui_reverse_palette() {
        let link = Link::External(String::from("test:target"));
        let doc = Doc::from_lines(
            Link::External(String::from("test:")),
            vec![DocLine {
                kind: Kind::Dir,
                text: String::from("directory"),
                link: Some(link),
            }],
            Vec::new(),
            0,
            false,
            None,
        );
        let layout = paint_doc_selected(&doc, 400.0, Some(0));
        assert_eq!(layout.paint.background, Some(rgba(crate::theme::BG)));
        assert!(layout.paint.primitives.iter().any(|command| matches!(
            command,
            DisplayCommand::FillRect { color, .. } if *color == rgba(crate::theme::NEON_CYAN)
        )));
        assert!(layout.paint.primitives.iter().any(|command| matches!(
            command,
            DisplayCommand::GlyphRun { color, shaped, .. }
                if *color == rgba(crate::theme::BG) && shaped.text == "directory"
        )));
    }

    #[test]
    fn preformatted_protocol_text_retains_a_horizontal_extent() {
        let doc = Doc::from_lines(
            Link::External(String::from("test:")),
            vec![DocLine {
                kind: Kind::Pre,
                text: "x".repeat(200),
                link: None,
            }],
            Vec::new(),
            0,
            false,
            None,
        );
        let paint = paint_doc(&doc, 160.0);
        assert!(paint.width > 160.0);
        assert_eq!(paint.lines.len(), 1, "Gemtext pre lines do not wrap");
    }
}
