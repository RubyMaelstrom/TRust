//! Graphical presentation adapter for TRust's line-model protocols.
//!
//! Gopher, Gemini, Finger, WHOIS and DICT retain their protocol-neutral
//! [`Doc`](crate::doc::Doc) parsing. This adapter performs proportional Parley
//! wrapping directly in CSS pixels and emits the same display list as HTML; it
//! never round-trips through terminal cells. RFC 1436 requires clients to
//! distinguish Gopher item types for users, and Gemtext 0.24.1 deliberately
//! leaves presentation to the user agent while requiring preformatted lines
//! to retain monowidth spacing. The semantic palette below is therefore the
//! same one used by TRust's terminal frontend. Desktop Gopher additionally
//! honors color-only ANSI SGR annotations in menu labels and text.

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
    /// Original colors per painted paragraph piece, retained so selection
    /// changes do not reshape text or lose author colors/backgrounds.
    colors: Vec<RowColors>,
}

#[derive(Clone, Debug)]
struct RowColors {
    normal: PaintColor,
    foregrounds: Vec<Option<[u8; 3]>>,
    backgrounds: Vec<(CssRect, PaintColor)>,
}

pub fn document(page: &BrowserPage) -> Option<Doc> {
    document_for_viewport(page, f32::INFINITY)
}

/// Choose the field presentation in CSS pixels, before graphical shaping.
/// Compact fields retain whole nameserver values in narrow windows.
pub fn document_for_viewport(page: &BrowserPage, viewport_width: f32) -> Option<Doc> {
    Some(match (&page.document, page.target()) {
        (FetchedDocument::Gopher(raw), Link::Gopher(url)) => {
            crate::gopher::render_desktop(url, raw.clone())
        }
        (FetchedDocument::Gemini(response), Link::Gemini(_)) => response.document(usize::MAX / 4),
        (FetchedDocument::OneShot(raw), Link::OneShot(url)) => {
            crate::oneshot::parse(url, raw.clone(), usize::MAX / 4)
        }
        (FetchedDocument::Dict(page), Link::Dict(_)) => {
            crate::dict::render(page.as_ref().clone(), usize::MAX / 4)
        }
        (FetchedDocument::Whois(page), Link::OneShot(url)) => crate::whois::render_with_columns(
            url,
            page.clone(),
            usize::MAX / 4,
            viewport_width >= 520.0,
        ),
        (FetchedDocument::Rdap(page), _) => {
            crate::rdap::render_with_columns(page.clone(), usize::MAX / 4, viewport_width >= 520.0)
        }
        (FetchedDocument::Finger(page), Link::OneShot(url)) => {
            crate::finger::render(url, page.reply.clone(), page.view.clone(), usize::MAX / 4)
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
            if !crate::download::mime_is_renderable(&response.content_type, false) {
                return None;
            }
            if crate::gemini::MediaType::parse(&response.content_type)
                .is_ok_and(|m| m.essence == "text/gemini")
            {
                return Some(crate::gemini::render(
                    Link::Http(url.clone()),
                    &response.content_type,
                    &response.body,
                    usize::MAX / 4,
                    Default::default(),
                ));
            }
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
    let doc = document_for_viewport(page, viewport.width)?;
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
    let left = doc.gemini.as_ref().map_or(22.0, |view| {
        ((viewport_width - view.reading_columns as f32 * 9.0) / 2.0).max(22.0)
    });
    let gopher = doc.gopher.is_some();
    let width = (viewport_width - left * 2.0).max(if gopher { 1.0 } else { 40.0 });
    let mut paint = PagePaint {
        background: Some(theme_color(crate::theme::BG)),
        ..PagePaint::default()
    };
    let mut lines = Vec::with_capacity(doc.lines.len());
    let mut row_colors = Vec::new();
    let mut y = 22.0;
    let mut far_right = viewport_width.max(0.0);
    let mut widest_line = 0.0f32;
    let mut soft_wrapped = false;
    for (line_index, line) in doc.lines.iter().enumerate() {
        if doc.text_view().is_some() && paint.lines.len() >= crate::text_reply::MAX_ROWS {
            break;
        }
        let (mut style, mut normal_color) = line_style(line.kind);
        if doc.gemini.is_some()
            && let Kind::Heading(level) = line.kind
        {
            // Gemtext 0.24.1, Heading lines: convey the three semantic
            // levels through font size, using the ordinary text color.
            style.size *= match level {
                1 => 1.8,
                2 => 1.5,
                _ => 1.25,
            };
            normal_color = theme_color(crate::theme::TEXT);
        }
        if doc.text_view().is_some() && line.kind == Kind::Pre {
            normal_color = theme_color(crate::theme::TEXT);
        }
        let color = normal_color;
        let spans = doc
            .gopher
            .as_ref()
            .and_then(|view| view.colors.get(line_index))
            .map_or(&[][..], Vec::as_slice);
        let line_top = y;
        let mut line_width = 1.0f32;
        // Shape each physical paragraph once for every line-model protocol.
        // CSS Text 3 #word-break-shaping (snapshot 81c27f686901): retain
        // joining forms across soft wraps. Re-shaping each shrinking suffix
        // also makes a single long reply line quadratic.
        let wrap = doc
            .text_view()
            .map_or(!matches!(line.kind, Kind::Pre), |view| {
                (view.wrap && !(doc.gemini.is_some() && line.kind == Kind::Pre))
                    || doc
                        .whois
                        .as_ref()
                        .is_some_and(|page| page.section != crate::registration::Section::Raw)
                    || doc
                        .rdap
                        .as_ref()
                        .is_some_and(|page| page.section != crate::registration::Section::Raw)
                    || (doc.dict.is_some() && line.kind != Kind::Pre)
                    || (doc.whois.is_some()
                        && matches!(
                            line.kind,
                            Kind::Heading(_) | Kind::Info | Kind::Error | Kind::OtherLink
                        ))
            });
        let hanging = if line.kind == Kind::List && wrap {
            // Gemtext 0.24.1, List items: wrapped text aligns after the
            // bullet. Measure it with this frontend's actual font metrics.
            text::shape(crate::gemini::LIST_MARKER, &style).advance
        } else if doc.dict.is_some() && line.kind == Kind::Pre && wrap {
            let columns = crate::dict::hanging_indent(&line.text);
            text::shape(&" ".repeat(columns), &style)
                .advance
                .min((width - 40.0).max(0.0))
        } else {
            0.0
        };
        let mut pieces = if !line.text.is_empty()
            && spans
                .iter()
                .any(|s| s.colors.foreground.is_some() || s.colors.inverse)
        {
            text::colored_lines(
                &line.text,
                &style,
                wrap.then_some(width),
                spans.iter().filter_map(|span| {
                    span_colors(span.colors, normal_color)
                        .0
                        .map(|color| (span.range.clone(), color))
                }),
            )
        } else if wrap && !line.text.is_empty() {
            text::wrapped_lines(
                &line.text,
                &style,
                width,
                width - hanging,
                TextBreakStyle {
                    wrap: true,
                    overflow_wrap: if doc.text_view().is_some() {
                        crate::text::TextOverflowWrap::Anywhere
                    } else {
                        crate::text::TextOverflowWrap::Normal
                    },
                    ..TextBreakStyle::default()
                },
            )
        } else {
            vec![text::shape(&line.text, &style)]
        }
        .into_iter()
        .peekable();
        let mut continuation = false;
        let mut paragraph_offset = 0;
        while let Some(mut shaped) = pieces.next() {
            let more = pieces.peek().is_some();
            soft_wrapped |= more;
            let truncated = doc.text_view().is_some()
                && paint.lines.len() >= crate::text_reply::MAX_ROWS - 1
                && (more || line_index + 1 < doc.lines.len());
            if truncated {
                shaped = text::shape("Display truncated to keep this reply responsive.", &style);
            }
            let color = if truncated {
                theme_color(crate::theme::NEON_PINK)
            } else {
                color
            };
            let link = if truncated { None } else { line.link.clone() };
            let origin = CssPoint::new(left + if continuation { hanging } else { 0.0 }, y);
            continuation = true;
            let rect = CssRect::new(
                origin.x,
                origin.y,
                shaped.advance.max(1.0),
                shaped.line_height.max(style.size),
            );
            let backgrounds = if truncated {
                Vec::new()
            } else {
                color_backgrounds(&shaped, origin, paragraph_offset, spans, normal_color)
            };
            row_colors.push(RowColors {
                normal: color,
                foregrounds: shaped.runs.iter().map(|run| run.color).collect(),
                backgrounds,
            });
            paragraph_offset += shaped.text.len();
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
                shadows: Vec::new(),
                clip: None,
                node: line_index + 1,
                link: link.clone(),
            });
            paint.primitives.push(DisplayCommand::HitRegion(HitRegion {
                rect,
                node: line_index + 1,
                actor: None,
                link,
                cursor: None,
            }));
            line_width = line_width.max(rect.x - left + rect.width);
            far_right = far_right.max(rect.x + rect.width + left);
            y += shaped.line_height.max(style.size * 1.2);
            if truncated || !more {
                break;
            }
        }
        lines.push(ProtocolLine {
            rect: CssRect::new(left, line_top, line_width, (y - line_top).max(style.size)),
            link: line.link.clone(),
        });
        widest_line = widest_line.max(line_width);
    }
    if gopher {
        // CSS Sizing 3 #auto-box-sizes: fit the column to its content within
        // the available width, without a minimum that leaves short phlogs
        // off-center. This is a presentation choice for Gopher, not a wire rule.
        // CSS 2 §10.3.3 #blockwidth: distribute spare width equally outside
        // the reading column, keeping the existing 22px minimum side padding.
        // Wrapping at that available width first gives the same line breaks,
        // without shaping the document twice just to measure its longest line.
        let column = if soft_wrapped {
            width
        } else {
            widest_line.min(width)
        };
        let offset = (width - column) / 2.0;
        if offset > 0.0 {
            for primitive in &mut paint.primitives {
                match primitive {
                    DisplayCommand::GlyphRun { origin, .. } => origin.x += offset,
                    DisplayCommand::FillRect { rect, .. } => rect.x += offset,
                    DisplayCommand::HitRegion(hit) => hit.rect.x += offset,
                    _ => {}
                }
            }
            for line in &mut paint.lines {
                line.rect.x += offset;
            }
            for line in &mut lines {
                line.rect.x += offset;
            }
            for row in &mut row_colors {
                for (rect, _) in &mut row.backgrounds {
                    rect.x += offset;
                }
            }
        }
        // Only fitting content is shifted, so centering cannot add overflow.
    }
    // Gemtext requires preformatted lines to remain unwrapped and recommends
    // horizontal scrolling in graphical clients. Preserve their actual width
    // in the page extent instead of clipping it to the viewport.
    paint.width = far_right;
    paint.height = y + 22.0;
    let mut layout = ProtocolPaint {
        paint,
        lines,
        colors: row_colors,
    };
    select(&mut layout, doc, selected);
    layout
}

/// Change selection colors using retained glyph geometry. Navigation must
/// not reshape the entire menu or move lines when the selection changes.
pub fn select(layout: &mut ProtocolPaint, _doc: &Doc, selected: Option<usize>) {
    layout
        .paint
        .primitives
        .retain(|p| !matches!(p, DisplayCommand::FillRect { .. }));
    let mut backgrounds = Vec::new();
    let mut colors = layout.colors.iter();
    for primitive in &mut layout.paint.primitives {
        if let DisplayCommand::GlyphRun {
            node,
            color,
            origin,
            shaped,
            link,
            decoration,
            ..
        } = primitive
            && let Some(row) = colors.next()
        {
            let is_selected = selected == node.checked_sub(1) && link.is_some();
            for (run, original) in shaped.runs.iter_mut().zip(&row.foregrounds) {
                run.color = if is_selected { None } else { *original };
            }
            if is_selected {
                *color = theme_color(crate::theme::BG);
                backgrounds.push(DisplayCommand::FillRect {
                    rect: CssRect::new(
                        origin.x,
                        origin.y,
                        shaped.advance.max(1.0),
                        shaped
                            .line_height
                            .max(crate::theme::TERMINAL_FONT_SIZE_CSS_PX),
                    ),
                    color: row.normal,
                });
            } else {
                *color = row.normal;
                backgrounds.extend(
                    row.backgrounds
                        .iter()
                        .map(|&(rect, color)| DisplayCommand::FillRect { rect, color }),
                );
            }
            decoration.color = *color;
        }
    }
    layout.paint.primitives.splice(0..0, backgrounds);
}

fn span_colors(
    colors: crate::gopher::ansi::Colors,
    normal: PaintColor,
) -> (Option<[u8; 3]>, Option<[u8; 3]>) {
    if colors.inverse {
        let PaintColor::Rgba(r, g, b, _) = normal else {
            unreachable!("protocol palette is RGB")
        };
        (
            Some(colors.background.unwrap_or(crate::theme::BG)),
            Some(colors.foreground.unwrap_or([r, g, b])),
        )
    } else {
        (colors.foreground, colors.background)
    }
}

fn color_backgrounds(
    shaped: &text::ShapedText,
    origin: CssPoint,
    paragraph_offset: usize,
    spans: &[crate::gopher::ansi::Span],
    normal: PaintColor,
) -> Vec<(CssRect, PaintColor)> {
    let mut backgrounds: Vec<(CssRect, PaintColor)> = Vec::new();
    if !spans
        .iter()
        .any(|s| s.colors.background.is_some() || s.colors.inverse)
    {
        return backgrounds;
    }
    for cluster in &shaped.clusters {
        let source = paragraph_offset + cluster.text_range.start;
        let index = spans.partition_point(|span| span.range.end <= source);
        let Some(span) = spans.get(index).filter(|span| span.range.start <= source) else {
            continue;
        };
        let Some(color) = span_colors(span.colors, normal).1 else {
            continue;
        };
        let color = theme_color(color);
        let rect = CssRect::new(
            origin.x + cluster.x,
            origin.y,
            cluster.advance,
            shaped.line_height,
        );
        if let Some((previous, previous_color)) = backgrounds.last_mut()
            && *previous_color == color
            && (previous.x + previous.width - rect.x).abs() < 0.01
        {
            previous.width += rect.width;
        } else {
            backgrounds.push((rect, color));
        }
    }
    backgrounds
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
        Kind::Heading(_) | Kind::Field => theme_color(crate::theme::NEON_CYAN),
        Kind::GemLink | Kind::Dir => {
            style.weight = 700.0;
            theme_color(crate::theme::NEON_CYAN)
        }
        Kind::Document => theme_color(crate::theme::NEON_GREEN),
        Kind::Search => theme_color(crate::theme::PASTEL_GREEN),
        Kind::OtherLink => theme_color(crate::theme::NEON_PINK),
        Kind::Error => theme_color(crate::theme::NEON_PINK),
        Kind::Quote => theme_color(crate::theme::DIM),
        Kind::Pre | Kind::Added => theme_color(crate::theme::NEON_GREEN),
        Kind::Removed => theme_color(crate::theme::NEON_PINK),
        _ => theme_color(crate::theme::TEXT),
    };
    (style, color)
}

const fn theme_color(rgb: crate::theme::Rgb) -> PaintColor {
    PaintColor::Rgba(rgb[0], rgb[1], rgb[2], 255)
}

#[cfg(test)]
mod tests {
    fn scene(paint: &PagePaint) -> crate::render::Scene {
        let viewport = crate::core::ViewportMetrics::from_physical(
            crate::core::PhysicalSize::new(1200, 800),
            crate::core::ScaleFactor::default(),
        );
        let mut scene = crate::render::Scene {
            viewport,
            primitives: Vec::new(),
            controls: Vec::new(),
            content_viewport: CssRect::new(0., 0., 1200., 800.),
            image_store: Default::default(),
            canvas_images: Default::default(),
            page_scroll_containers: Vec::new(),
            page_size: CssSize::default(),
        };
        scene.append_page(paint, CssPoint::default());
        scene
    }

    fn colored_gopher(raw: &str, item_type: char) -> Doc {
        let url = crate::gopher::GopherUrl::new("example.test".into(), 70, item_type, Vec::new());
        crate::gopher::render_desktop(&url, raw.as_bytes().to_vec().into())
    }

    #[test]
    fn gopher_ansi_paint_preserves_wrapping_shaping_search_and_copy() {
        let doc = colored_gopher(
            "co\x1b[38;2;27;75;105mlor\x1b[0m and سا\x1b[31mلام\x1b[0m  e\u{301}  end",
            '0',
        );
        let mut plain = doc.clone();
        plain.gopher.as_mut().unwrap().colors.clear();
        for width in [1200.0, 110.0] {
            let colored = paint_doc_selected(&doc, width, None);
            let plain = paint_doc_selected(&plain, width, None);
            assert_eq!(colored.lines, plain.lines);
            assert_eq!(colored.paint.lines, plain.paint.lines);
            let glyphs = |paint: &PagePaint| {
                paint
                    .primitives
                    .iter()
                    .filter_map(|p| match p {
                        DisplayCommand::GlyphRun { shaped, .. } => Some(
                            shaped
                                .runs
                                .iter()
                                .flat_map(|r| r.glyphs.iter().map(|g| (r.font.resource_id(), *g)))
                                .collect::<Vec<_>>(),
                        ),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
            };
            let colored_glyphs = glyphs(&colored.paint);
            let plain_glyphs = glyphs(&plain.paint);
            assert_eq!(colored_glyphs.len(), plain_glyphs.len());
            for (colored, plain) in colored_glyphs.iter().zip(&plain_glyphs) {
                assert_eq!(colored.len(), plain.len());
                for ((cf, c), (pf, p)) in colored.iter().zip(plain) {
                    assert_eq!((cf, c.id), (pf, p.id), "colors must not break joining");
                    assert!(
                        (c.x - p.x).abs() < 0.001
                            && (c.y - p.y).abs() < 0.001
                            && (c.advance - p.advance).abs() < 0.001,
                        "colors must not move glyphs"
                    );
                }
            }
            let scene = scene(&colored.paint);
            let matches = scene.find_text("color");
            assert_eq!(matches.len(), 1);
            assert_eq!(scene.selected_text(matches[0]), "color");
        }
    }

    #[test]
    fn gopher_ansi_selection_restores_colors_and_centered_backgrounds() {
        let doc = colored_gopher(
            "0co\x1b[31;44mlor\x1b[0m link\t/a\texample.test\t70\r\n.\r\n",
            '1',
        );
        let mut layout = paint_doc_selected(&doc, 1200.0, None);
        let original = layout.paint.clone();
        let lines = layout.lines.clone();
        assert!(original.primitives.iter().any(|p| matches!(p, DisplayCommand::FillRect { color: PaintColor::Rgba(0, 0, 170, 255), rect } if rect.x > lines[0].rect.x)));
        select(&mut layout, &doc, Some(0));
        for primitive in &layout.paint.primitives {
            if let DisplayCommand::GlyphRun { shaped, color, .. } = primitive {
                assert_eq!(*color, theme_color(crate::theme::BG));
                assert!(shaped.runs.iter().all(|run| run.color.is_none()));
            }
        }
        assert_eq!(layout.lines, lines);
        assert_eq!(
            scene(&layout.paint).interactive_hits(),
            scene(&original).interactive_hits()
        );
        select(&mut layout, &doc, None);
        assert_eq!(layout.paint.primitives, original.primitives);
        assert_eq!(layout.paint.lines, original.lines);
    }

    #[test]
    fn gopher_ansi_wrapped_backgrounds_follow_their_text() {
        let doc = colored_gopher("\x1b[44mabcdefghijklmno\x1b[49mXYZ", '0');
        let layout = paint_doc_selected(&doc, 100.0, None);
        assert!(layout.paint.lines.len() > 1);
        let backgrounds: Vec<_> = layout
            .paint
            .primitives
            .iter()
            .filter_map(|p| match p {
                DisplayCommand::FillRect {
                    rect,
                    color: PaintColor::Rgba(0, 0, 170, 255),
                } => Some(rect),
                _ => None,
            })
            .collect();
        assert!(backgrounds.len() > 1);
        for rect in backgrounds {
            assert!(layout.paint.lines.iter().any(|line| line.rect.y == rect.y
                && rect.x >= line.rect.x
                && rect.x + rect.width <= line.rect.x + line.rect.width + 0.01));
        }
    }

    #[test]
    fn gopher_ansi_truecolor_reaches_desktop_pixels() {
        let doc = colored_gopher(
            "\x1b[38;2;250;10;20m██\x1b[38;2;10;20;250m██\x1b[0m  \x1b[48;2;10;240;20m   \x1b[0m",
            '0',
        );
        let paint = paint_doc(&doc, 400.0);
        let frame =
            crate::render::headless::render_paint(&paint, CssSize::new(400.0, 100.0)).unwrap();
        for color in [[250, 10, 20, 255], [10, 20, 250, 255], [10, 240, 20, 255]] {
            assert!(
                frame
                    .pixels
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .filter(|pixel| **pixel == color)
                    .count()
                    > 5,
                "missing raster color {color:?}"
            );
        }
    }

    #[test]
    fn gopher_centered_column_moves_text_selection_and_hits_together() {
        let url = crate::gopher::GopherUrl::parse("gopher://example.test").unwrap();
        let label = "0".repeat(34);
        let raw =
            format!("0{label}\t/a\texample.test\t70\r\n1  Next\t/b\texample.test\t70\r\n.\r\n");
        let doc = crate::gopher::parse(&url, raw.into_bytes(), false, usize::MAX / 4);
        let viewport = 1200.0;
        let left = (viewport - text::shape(&label, &line_style(Kind::Text).0).advance) / 2.0;
        let mut layout = paint_doc_selected(&doc, viewport, Some(0));
        assert_eq!(layout.paint.lines.len(), 2);
        assert_eq!(
            layout.paint.width, viewport,
            "centering must not add scrolling"
        );
        for line in &layout.lines {
            assert!((line.rect.x - left).abs() < 0.01);
        }
        for primitive in &layout.paint.primitives {
            let x = match primitive {
                DisplayCommand::GlyphRun { origin, .. } => origin.x,
                DisplayCommand::FillRect { rect, .. } => rect.x,
                DisplayCommand::HitRegion(hit) => hit.rect.x,
                _ => continue,
            };
            assert!((x - left).abs() < 0.01);
        }
        let geometry = layout.lines.clone();
        select(&mut layout, &doc, Some(1));
        assert_eq!(layout.lines, geometry);
        assert!(layout.paint.primitives.iter().any(|p| matches!(p,
            DisplayCommand::FillRect { rect, .. }
                if (rect.x - left).abs() < 0.01 && rect.y == layout.lines[1].rect.y
        )));
        assert!(
            layout
                .paint
                .lines
                .iter()
                .all(|l| (l.rect.x - left).abs() < 0.01)
        );
    }

    #[test]
    fn gopher_centered_column_fits_short_text_without_reflow() {
        let url = crate::gopher::GopherUrl::parse("gopher://example.test/0/phlog").unwrap();
        let raw = b"  Quiet morning.\r\nRain at the window.\r\n\r\n  Time to go.  \r\n.\r\n";
        let mut doc = crate::gopher::parse(&url, raw.to_vec(), false, usize::MAX / 4);
        let longest = text::shape("Rain at the window.", &line_style(Kind::Text).0).advance;
        for wrap in [true, false] {
            doc.gopher.as_mut().unwrap().controls.wrap = wrap;
            for viewport in [700.0, 1200.0] {
                let layout = paint_doc_selected(&doc, viewport, None);
                let left = (viewport - longest) / 2.0;
                assert_eq!(layout.paint.width, viewport);
                assert_eq!(layout.paint.lines.len(), 4);
                assert!(
                    layout
                        .paint
                        .lines
                        .iter()
                        .all(|line| (line.rect.x - left).abs() < 0.01)
                );
                let text: Vec<_> = layout
                    .paint
                    .primitives
                    .iter()
                    .filter_map(|p| match p {
                        DisplayCommand::GlyphRun { shaped, .. } => Some(shaped.text.as_str()),
                        _ => None,
                    })
                    .collect();
                assert_eq!(
                    text,
                    [
                        "  Quiet morning.",
                        "Rain at the window.",
                        "",
                        "  Time to go.  "
                    ]
                );
            }
        }
    }

    #[test]
    fn gopher_centered_column_expands_for_authored_lines_and_wraps_on_resize() {
        let url = crate::gopher::GopherUrl::parse("gopher://example.test/0/phlog").unwrap();
        let original = "0".repeat(88);
        let doc = crate::gopher::parse(&url, original.as_bytes().to_vec(), false, usize::MAX / 4);
        let style = line_style(Kind::Text).0;
        let ch = text::shape("0", &style).advance;
        let viewport = ch * 120.0 + 44.0;
        let wide = paint_doc_selected(&doc, viewport, None);
        let actual_width = text::shape(&original, &style).advance;
        assert_eq!(
            wide.paint.lines.len(),
            1,
            "an 88-column diagram must stay intact"
        );
        assert!((wide.lines[0].rect.x - (viewport - actual_width) / 2.0).abs() < 0.1);
        assert_eq!(wide.paint.width, viewport);

        let narrow = paint_doc_selected(&doc, ch * 60.0 + 44.0, None);
        assert!(narrow.paint.lines.len() > 1);
        assert!(narrow.paint.lines.iter().all(|l| l.rect.x == 22.0));
        let reconstructed: String = narrow
            .paint
            .primitives
            .iter()
            .filter_map(|p| match p {
                DisplayCommand::GlyphRun { shaped, .. } => Some(shaped.text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(reconstructed, original);

        let mut unwrapped = doc.clone();
        unwrapped.gopher.as_mut().unwrap().controls.wrap = false;
        let narrow = paint_doc_selected(&unwrapped, ch * 60.0 + 44.0, None);
        assert_eq!(narrow.paint.lines.len(), 1);
        assert_eq!(narrow.lines[0].rect.x, 22.0);
        assert!(narrow.paint.width > ch * 60.0 + 44.0);
    }

    #[test]
    fn gopher_centered_column_keeps_full_width_after_a_soft_wrap() {
        let url = crate::gopher::GopherUrl::parse("gopher://example.test/0/phlog").unwrap();
        // Both resulting lines fit in 80 columns, but the physical source line
        // is wider than the viewport. It must retain the full wrapping width.
        let raw = format!("{} {}", "0".repeat(70), "0".repeat(70));
        let doc = crate::gopher::parse(&url, raw.into_bytes(), false, usize::MAX / 4);
        let ch = text::shape("0", &line_style(Kind::Text).0).advance;
        let viewport = ch * 100.0 + 44.0;
        let layout = paint_doc_selected(&doc, viewport, None);
        assert_eq!(layout.paint.lines.len(), 2);
        assert!(layout.paint.lines.iter().all(|l| l.rect.x == 22.0));
        assert_eq!(layout.paint.width, viewport);
    }

    #[test]
    fn gopher_selection_keeps_shaping_and_hit_geometry() {
        let url = crate::gopher::GopherUrl::parse("gopher://e").unwrap();
        let doc = crate::gopher::parse(&url, b"0A long text link that should wrap into more than one row\t/a\te\t70\r\n1Next menu\t/b\te\t70\r\n.\r\n".to_vec(), false, usize::MAX / 4);
        let mut layout = super::paint_doc_selected(&doc, 160.0, Some(0));
        let geometry = layout.lines.clone();
        let before = layout
            .paint
            .primitives
            .iter()
            .filter_map(|p| {
                if let super::DisplayCommand::GlyphRun { shaped, .. } = p {
                    Some(shaped.clone())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        super::select(&mut layout, &doc, Some(1));
        assert_eq!(layout.lines, geometry);
        let after = layout
            .paint
            .primitives
            .iter()
            .filter_map(|p| {
                if let super::DisplayCommand::GlyphRun { shaped, .. } = p {
                    Some(shaped.clone())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        assert_eq!(before, after);
    }

    use super::*;

    #[test]
    fn dict_native_wrap_retains_hanging_indent_and_bounds_the_scene() {
        let target = crate::dict::Target::parse("dict://example.test/d:word").unwrap();
        let mut reply = crate::dict::Reply {
            finished: true,
            complete: true,
            ..Default::default()
        };
        reply.definitions.push(crate::dict::Definition {
            word: "word".into(),
            database: "wn".into(),
            description: "Source".into(),
            body: std::sync::Arc::new(
                "    n 1: a colorless element in the atmosphere with examples\n".into(),
            ),
            complete: true,
        });
        let page = crate::dict::Page::new(target, reply);
        let doc = crate::dict::render(page.clone(), usize::MAX / 4);
        let source = doc
            .lines
            .iter()
            .position(|line| line.kind == Kind::Pre)
            .unwrap();
        let paint = paint_doc(&doc, 240.0);
        let runs: Vec<_> = paint
            .primitives
            .iter()
            .filter_map(|primitive| match primitive {
                DisplayCommand::GlyphRun {
                    node,
                    origin,
                    shaped,
                    ..
                } if *node == source + 1 => Some((origin, shaped)),
                _ => None,
            })
            .collect();
        assert!(runs.len() > 1);
        assert!(runs[1].0.x > runs[0].0.x);
        assert!(
            runs.iter()
                .all(|(origin, shaped)| origin.x + shaped.advance <= 240.1)
        );
        let mut page = page;
        page.reply.definitions[0].body = std::sync::Arc::new("x\n".repeat(20000));
        let doc = crate::dict::render(page, usize::MAX / 4);
        assert!(doc.lines.len() <= crate::text_reply::MAX_ROWS);
        assert!(doc.lines.iter().any(|l| l.text.contains("Display limited")));
        let paint = paint_doc(&doc, 240.0);
        assert!(paint.lines.len() <= crate::text_reply::MAX_ROWS);
    }

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

    #[test]
    fn gemtext_native_lists_paint_one_bullet_and_align_wrapped_text() {
        let url = crate::gemini::GeminiUrl::parse("gemini://example.org/").unwrap();
        let body = b"* An item long enough to wrap across several visible lines\n* Next item\n=> /server Server\n";
        let doc = crate::gemini::parse(&url, "text/gemini", body, usize::MAX / 4);
        for width in [180.0, 260.0] {
            let paint = paint_doc(&doc, width);
            let pieces: Vec<_> = paint
                .primitives
                .iter()
                .filter_map(|command| match command {
                    DisplayCommand::GlyphRun {
                        origin,
                        shaped,
                        node: 1,
                        ..
                    } => Some((origin, shaped)),
                    _ => None,
                })
                .collect();
            assert!(pieces.len() > 1);
            assert!(pieces[0].1.text.starts_with("• "));
            let indent = text::shape(crate::gemini::LIST_MARKER, &line_style(Kind::List).0).advance;
            for (origin, shaped) in &pieces[1..] {
                assert!((origin.x - pieces[0].0.x - indent).abs() < 0.01);
                assert!(!shaped.text.contains('•'));
            }
            assert_eq!(
                paint
                    .primitives
                    .iter()
                    .filter(|command| matches!(command,
                        DisplayCommand::GlyphRun { shaped, .. } if shaped.text.contains('•')
                    ))
                    .count(),
                2
            );
            assert!(paint.primitives.iter().any(|command| matches!(command,
                DisplayCommand::GlyphRun { link: Some(_), origin, shaped, .. }
                    if shaped.text == "Server" && (origin.x - pieces[0].0.x).abs() < 0.01
            )));
        }
        assert_eq!(doc.raw, body);
    }

    #[test]
    fn gemtext_native_headings_use_three_larger_sizes_and_the_text_color() {
        let url = crate::gemini::GeminiUrl::parse("gemini://example.org/").unwrap();
        let doc = crate::gemini::parse(
            &url,
            "text/gemini",
            b"# Heading\n## Heading\n### Heading\nHeading\n",
            usize::MAX / 4,
        );
        let paint = paint_doc(&doc, 400.0);
        let runs: Vec<_> = paint
            .primitives
            .iter()
            .filter_map(|command| match command {
                DisplayCommand::GlyphRun { shaped, color, .. } => {
                    assert_eq!(*color, theme_color(crate::theme::TEXT));
                    Some(shaped)
                }
                _ => None,
            })
            .collect();
        assert_eq!(runs.len(), 4);
        for pair in runs.windows(2) {
            assert!(pair[0].advance > pair[1].advance);
            assert!(pair[0].line_height > pair[1].line_height);
        }
        for pair in paint.lines.windows(2) {
            assert!(pair[1].rect.y >= pair[0].rect.y + pair[0].rect.height);
        }
    }

    #[test]
    fn finger_native_wrap_preserves_text_and_unwrapped_columns() {
        let url = crate::finger::parse_url("finger://example.test/alice").unwrap();
        let mut doc =
            crate::oneshot::parse(&url, b"  columns\tand    spaces    and more".to_vec(), 80);
        let original = doc.lines[0].text.clone();
        let paint = paint_doc(&doc, 100.0);
        assert_eq!(paint.lines.len(), 1);
        assert!(paint.width > 100.0);
        assert!(paint.primitives.iter().any(|p| matches!(p,
            DisplayCommand::GlyphRun { color, .. } if *color == rgba(crate::theme::TEXT)
        )));
        doc.finger.as_mut().unwrap().wrap = true;
        let paint = paint_doc(&doc, 100.0);
        assert!(paint.lines.len() > 1);
        let pieces: String = paint
            .primitives
            .iter()
            .filter_map(|p| match p {
                DisplayCommand::GlyphRun { shaped, .. } => Some(shaped.text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(pieces, original);
    }

    #[test]
    fn finger_native_wrapping_bounds_paint_rows() {
        let url = crate::finger::parse_url("finger://example.test/alice").unwrap();
        let mut doc = crate::oneshot::parse(&url, "a".repeat(4096).repeat(100).into_bytes(), 80);
        // Many long physical lines, each requiring hundreds of painted rows.
        let line = doc.lines[0].clone();
        doc.lines = vec![line; 100];
        doc.finger.as_mut().unwrap().wrap = true;
        let paint = paint_doc(&doc, 100.0);
        assert!(paint.lines.len() <= crate::text_reply::MAX_ROWS);
        assert!(paint.primitives.iter().any(|p| matches!(p,
            DisplayCommand::GlyphRun { shaped, .. } if shaped.text.contains("truncated")
        )));
    }
}
