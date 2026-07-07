//! Rendering: cyberpunk chrome around the emulated remote screen, or the
//! gopher browser panel when one is open.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
};
use tui_term::widget::PseudoTerminal;

use crate::app::{App, BrowserView, Encoding, FindLoc, FindState, Mode};
use crate::doc::{Kind, Link};

pub mod theme {
    use ratatui::style::Color;

    pub const NEON_PINK: Color = Color::Rgb(0xff, 0x2b, 0xd6);
    pub const NEON_CYAN: Color = Color::Rgb(0x00, 0xff, 0xf9);
    pub const NEON_GREEN: Color = Color::Rgb(0x39, 0xff, 0x14);
    /// Soft pastel green for interactive fields (form controls, input
    /// prompts/badges) — gentler than the bright NEON_GREEN.
    pub const PASTEL_GREEN: Color = Color::Rgb(0xa8, 0xe6, 0xa1);
    pub const AMBER: Color = Color::Rgb(0xff, 0xb0, 0x00);
    pub const DIM: Color = Color::Rgb(0x6e, 0x4e, 0x9e);
    pub const TEXT: Color = Color::Rgb(0xc8, 0xc8, 0xdc);
    pub const BG: Color = Color::Rgb(0x0b, 0x02, 0x21);
}

pub fn draw(frame: &mut Frame, app: &mut App) {
    // While browsing or viewing an image the input field can't be typed
    // into, so its 3-row box is dropped and those rows go to the content
    // panel; everything folds into the single status line. The box returns
    // for command/search/line entry (and the char-mode strip).
    let collapse_input =
        app.mode == Mode::Session && (app.browser.is_some() || app.viewer.is_some());
    let (session_area, input_area, status_area) = if collapse_input {
        let [session_area, status_area] =
            Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).areas(frame.area());
        (session_area, None, status_area)
    } else {
        let [session_area, input_area, status_area] = Layout::vertical([
            Constraint::Min(3),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .areas(frame.area());
        (session_area, Some(input_area), status_area)
    };

    let (title, border_color) = match (&app.viewer, &app.browser) {
        (Some(v), _) => (format!("░▒▓ TRUST :: {} ▓▒░", v.url), theme::NEON_PINK),
        (None, Some(g)) => (format!("░▒▓ TRUST :: {} ▓▒░", g.doc.url), theme::NEON_CYAN),
        (None, None) => (
            match &app.host {
                Some(host) => format!("░▒▓ TRUST :: {host}:{port} ▓▒░", port = app.port),
                None => String::from("░▒▓ TRUST :: TELNET/NVT ▓▒░"),
            },
            if app.connected {
                theme::NEON_CYAN
            } else {
                theme::DIM
            },
        ),
    };
    let block = Block::bordered()
        .border_type(BorderType::Thick)
        .border_style(Style::new().fg(border_color))
        .style(Style::new().bg(theme::BG))
        .title(Line::styled(
            title,
            Style::new()
                .fg(theme::NEON_PINK)
                .add_modifier(Modifier::BOLD),
        ));

    let inner = block.inner(session_area);
    app.last_inner = (inner.width, inner.height);
    app.last_content_area = inner;
    app.last_status_row = status_area.y;

    // Scrollbar tracks recorded this frame for the mouse hit-test (assigned to
    // `app` after the match — `g` borrows `app.browser` inside the arm).
    let mut vbar = None;
    let mut hbars = Vec::new();
    match (&app.viewer, &app.browser) {
        (Some(v), _) => {
            frame.render_widget(block, session_area);
            // Center the scaled image in the panel; the protocol was
            // encoded to fit it. `centered` clamps the box to the panel (a
            // resize may not have re-encoded yet).
            let size = v.protocol.size();
            let image_area = inner.centered(
                Constraint::Length(size.width),
                Constraint::Length(size.height),
            );
            frame.render_widget(ratatui_image::Image::new(&v.protocol), image_area);
        }
        (None, Some(g)) => {
            let doc = Paragraph::new(browser_lines(g, inner.height as usize, app.find.as_ref()))
                .block(block);
            frame.render_widget(doc, session_area);
            // Second pass: overlay decoded inline images on their reserved
            // boxes. Each box encodes once to a `SlicedProtocol`; the renderer
            // clips it to its on-screen slice (sixel bands stripped), so a
            // partly-scrolled image draws its visible portion and undecoded
            // boxes stay alt text.
            if g.doc.laid_out() {
                render_inline_images(frame, g, inner, &app.image_protocols);
            }
            // Third pass: the PINNED fixed layer (sidebar/header rails captured
            // from `position:fixed`) draws over the scrolling document at a fixed
            // screen position, so it stays put while the center scrolls.
            if !g.doc.fixed.is_empty() {
                render_fixed_layer(frame, g, inner, app.find.as_ref(), &app.image_protocols);
            }
            // Scroll-position indicator on the right border, when the document
            // overflows the panel — plus a horizontal bar under each overflowing
            // carousel. Both are clickable/draggable (tracks recorded for the
            // mouse hit-test).
            vbar = render_browser_scrollbar(frame, g, session_area, inner);
            hbars = render_carousel_scrollbars(frame, g, inner);
        }
        (None, None) => {
            let term = PseudoTerminal::new(app.vt.screen()).block(block);
            frame.render_widget(term, session_area);
        }
    }
    app.last_vbar = vbar;
    app.last_hbars = hbars;

    // An open <select> dropdown overlays everything in the panel.
    render_select_menu(frame, app, inner);

    if let Some(input_area) = input_area {
        frame.render_widget(
            input_box(app, input_area.width.saturating_sub(2)),
            input_area,
        );
    }
    frame.render_widget(status_bar(app), status_area);

    // Fetch in flight: a tiny beating heart, animated by the run loop's
    // ticker. Lub, dub, rest — filled bold, hollow, filled again, then dim
    // through the diastole. It rides the entry box's right edge when the
    // box is shown, otherwise the status line's right end.
    if app.loading() {
        let area = match input_area {
            Some(a) if a.width > 8 => Some(Rect::new(a.right().saturating_sub(3), a.y + 1, 1, 1)),
            Some(_) => None,
            None if status_area.width > 4 => Some(Rect::new(
                status_area.right().saturating_sub(2),
                status_area.y,
                1,
                1,
            )),
            None => None,
        };
        if let Some(area) = area {
            let (glyph, style) = match app.spinner % 6 {
                0 => (
                    "♥",
                    Style::new()
                        .fg(theme::NEON_PINK)
                        .add_modifier(Modifier::BOLD),
                ),
                1 => ("♡", Style::new().fg(theme::NEON_PINK)),
                2 => ("♥", Style::new().fg(theme::NEON_PINK)),
                _ => ("♡", Style::new().fg(theme::DIM)),
            };
            frame.render_widget(Paragraph::new(Span::styled(glyph, style)), area);
        }
    }
}

/// Draw the open `<select>` dropdown as a bordered popup anchored to its
/// field, the highlighted option inverted. Records the drawn rect (and keeps
/// the option scroll in sync with the highlight) for the mouse hit-test.
fn render_select_menu(frame: &mut Frame, app: &mut App, inner: Rect) {
    let Some(menu) = &app.select_menu else {
        app.last_select_rect = None;
        return;
    };
    let browser_scroll = app.browser.as_ref().map_or(0, |g| g.scroll);
    let n = menu.options.len();
    // Width = widest label (clamped), plus the border; height = the visible
    // option rows (capped), plus the border.
    let label_w = menu
        .options
        .iter()
        .map(|(l, _)| l.chars().count())
        .max()
        .unwrap_or(4);
    let body_w = label_w
        .clamp(6, (inner.width.saturating_sub(2)).max(6) as usize)
        .min(40);
    let w = (body_w + 2) as u16;
    let vis = n.clamp(1, 12);
    let h = (vis + 2) as u16;
    // Keep the highlight inside the visible window.
    let scroll = if menu.highlight < menu.scroll {
        menu.highlight
    } else if menu.highlight >= menu.scroll + vis {
        menu.highlight + 1 - vis
    } else {
        menu.scroll
    };
    // Anchor under the field; flip above if it would overflow the panel.
    let field_y = inner.y as isize + menu.anchor_row as isize - browser_scroll as isize;
    let mut y = field_y + 1;
    if y + h as isize > inner.bottom() as isize {
        y = field_y - h as isize;
    }
    let y = y.clamp(
        inner.y as isize,
        (inner.bottom() as isize - h as isize).max(inner.y as isize),
    ) as u16;
    let max_x = inner.right().saturating_sub(w).max(inner.x);
    let x = inner.x.saturating_add(menu.anchor_col).min(max_x);
    let rect = Rect::new(x, y, w.min(inner.width), h.min(inner.height));

    let lines: Vec<Line> = menu
        .options
        .iter()
        .enumerate()
        .skip(scroll)
        .take(vis)
        .map(|(i, (label, _))| {
            let mut text: String = label.chars().take(body_w).collect();
            while text.chars().count() < body_w {
                text.push(' ');
            }
            let style = if i == menu.highlight {
                Style::new().fg(theme::BG).bg(theme::NEON_CYAN)
            } else {
                Style::new().fg(theme::TEXT)
            };
            Line::styled(text, style)
        })
        .collect();
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(theme::NEON_PINK))
        .style(Style::new().bg(theme::BG));
    frame.render_widget(Clear, rect);
    frame.render_widget(Paragraph::new(lines).block(block), rect);

    if let Some(m) = app.select_menu.as_mut() {
        m.scroll = scroll;
    }
    app.last_select_rect = Some(rect);
}

/// The visible slice of a document, gopherus-style: the cursor line is
/// highlighted when it carries a link.
/// The find-match char ranges at a given location, each tagged with whether
/// it is the active match. Already in document order (so sorted).
fn find_ranges(find: Option<&FindState>, loc: FindLoc) -> Vec<(usize, usize, bool)> {
    let Some(f) = find else {
        return Vec::new();
    };
    let cur = f
        .current
        .and_then(|c| f.matches.get(c))
        .map(|m| (m.loc, m.start));
    f.matches
        .iter()
        .filter(|m| m.loc == loc)
        .map(|m| (m.start, m.end, cur == Some((m.loc, m.start))))
        .collect()
}

/// Split `text` into styled spans: `base` everywhere, BOLD on each match, and
/// REVERSED+BOLD on the active one. `ranges` = (start, end, is_current) char
/// offsets, sorted and non-overlapping.
fn match_spans(text: &str, base: Style, ranges: &[(usize, usize, bool)]) -> Vec<Span<'static>> {
    let chars: Vec<char> = text.chars().collect();
    let mut spans = Vec::new();
    let mut pos = 0usize;
    for &(s, e, current) in ranges {
        let s = s.min(chars.len());
        let e = e.min(chars.len()).max(s);
        if s > pos {
            spans.push(Span::styled(chars[pos..s].iter().collect::<String>(), base));
        }
        let mstyle = if current {
            base.add_modifier(Modifier::REVERSED | Modifier::BOLD)
        } else {
            base.add_modifier(Modifier::BOLD)
        };
        spans.push(Span::styled(chars[s..e].iter().collect::<String>(), mstyle));
        pos = e;
    }
    if pos < chars.len() {
        spans.push(Span::styled(chars[pos..].iter().collect::<String>(), base));
    }
    spans
}

fn browser_lines<'a>(g: &'a BrowserView, height: usize, find: Option<&FindState>) -> Vec<Line<'a>> {
    if g.doc.laid_out() {
        return browser_rows(g, height, find);
    }
    let end = (g.scroll + height).min(g.doc.lines.len());
    g.doc.lines[g.scroll..end]
        .iter()
        .enumerate()
        .map(|(i, line)| {
            let mut style = match (line.link.is_some(), line.kind) {
                (true, Kind::Dir | Kind::GemLink) => Style::new()
                    .fg(theme::NEON_CYAN)
                    .add_modifier(Modifier::BOLD),
                (true, Kind::Document) => Style::new().fg(theme::NEON_GREEN),
                (true, Kind::Search) => Style::new().fg(theme::PASTEL_GREEN),
                (true, _) => Style::new().fg(theme::NEON_PINK),
                (_, Kind::Error) => Style::new().fg(theme::NEON_PINK),
                (_, Kind::Heading(1)) => Style::new()
                    .fg(theme::NEON_PINK)
                    .add_modifier(Modifier::BOLD),
                (_, Kind::Heading(2)) => Style::new()
                    .fg(theme::NEON_CYAN)
                    .add_modifier(Modifier::BOLD),
                (_, Kind::Heading(_)) => Style::new().fg(theme::NEON_CYAN),
                (_, Kind::Quote) => Style::new().fg(theme::DIM),
                (_, Kind::Pre) => Style::new().fg(theme::NEON_GREEN),
                _ => Style::new().fg(theme::TEXT),
            };
            if g.selected == Some(g.scroll + i) && line.link.is_some() {
                style = style.add_modifier(Modifier::REVERSED | Modifier::BOLD);
            }
            let ranges = find_ranges(find, FindLoc::Line(g.scroll + i));
            if ranges.is_empty() {
                Line::styled(line.text.as_str(), style)
            } else {
                Line::from(match_spans(&line.text, style, &ranges))
            }
        })
        .collect()
}

use crate::layout::visible_col;

/// The base cyberpunk colour for an item's `kind` (before emphasis, selection,
/// carousel-disabled, and find highlighting are layered on). Shared by the
/// scrolling document rows and the pinned fixed layer.
fn item_kind_style(kind: crate::layout::ItemKind) -> Style {
    use crate::layout::ItemKind;
    match kind {
        ItemKind::Link => Style::new().fg(theme::NEON_CYAN),
        ItemKind::Heading(1) => Style::new()
            .fg(theme::NEON_PINK)
            .add_modifier(Modifier::BOLD),
        ItemKind::Heading(2) => Style::new()
            .fg(theme::NEON_CYAN)
            .add_modifier(Modifier::BOLD),
        ItemKind::Heading(_) => Style::new().fg(theme::NEON_CYAN),
        ItemKind::Quote => Style::new().fg(theme::DIM),
        ItemKind::Pre => Style::new().fg(theme::NEON_GREEN),
        ItemKind::Form => Style::new().fg(theme::PASTEL_GREEN),
        ItemKind::Image => Style::new().fg(theme::DIM).add_modifier(Modifier::ITALIC),
        ItemKind::Border => Style::new().fg(theme::DIM),
        ItemKind::Text => Style::new().fg(theme::TEXT),
    }
}

/// Render an HTTP laid-out doc: each visible row is a sequence of
/// positioned item spans, padded to each item's start column. The
/// selected `(row, item)` is highlighted.
pub(crate) fn browser_rows<'a>(
    g: &'a BrowserView,
    height: usize,
    find: Option<&FindState>,
) -> Vec<Line<'a>> {
    use crate::layout::NO_NODE;
    // The selected link's source node: every item sharing it (a link that
    // wrapped across rows) highlights as one unit. Read through `effective_row`
    // so a selection on scroll-region content resolves to its buffer item.
    let sel_node = g.sel_item.and_then(|(r, i)| {
        crate::layout::effective_row(&g.doc.rows, &g.doc.regions, r)
            .items
            .get(i)
            .map(|it| it.node)
    });
    let carousels = &g.doc.carousels;
    let end = (g.scroll + height).min(g.doc.rows.len());
    (g.scroll..end)
        .map(|row_idx| {
            // Merge any scroll-region buffer window over this row's reserved
            // band (a vertical inner-scroll viewport draws `buffer[voffset+…]`
            // here, clipped to its band) before placement, so region content
            // styles/highlights through the same path as page content.
            let row = crate::layout::effective_row(&g.doc.rows, &g.doc.regions, row_idx);
            // Each item's on-screen start column (carousel clip + gap-fill +
            // overlap-append), shared with the hit-test so the drawn position
            // and the clickable position always agree.
            let placed = crate::layout::visual_columns(&row, carousels, row_idx);
            let mut spans: Vec<Span> = Vec::with_capacity(placed.len() * 2);
            let mut col = 0u16;
            for (i, scol, vis_w, cut) in placed {
                let item = &row.items[i];
                if scol > col {
                    spans.push(Span::raw(" ".repeat((scol - col) as usize)));
                }
                // The visible slice of the item after carousel windowing: a wide
                // `white-space:pre` line clipped to its scroll band shows only
                // the columns in view (`cut` shaved off the left, `vis_w` wide);
                // an unclipped item is its whole self, borrowed.
                let clipped = cut > 0 || vis_w < item.width;
                let text: std::borrow::Cow<str> = if clipped && !item.text.is_empty() {
                    std::borrow::Cow::Owned(crate::layout::slice_display(
                        &item.text,
                        cut,
                        vis_w as usize,
                    ))
                } else {
                    std::borrow::Cow::Borrowed(item.text.as_str())
                };
                // Paint suppression (`opacity:0`): the item is fully laid out
                // but painted BLANK — reserve its width with spaces and skip all
                // styling/selection/text (its image, if any, is skipped in the
                // image pass too). Geometry is untouched (the item still carries
                // its real `col`/`width`/`height`), so measurement APIs report
                // the true box while the cells render empty.
                if item.invisible {
                    spans.push(Span::raw(" ".repeat(vis_w as usize)));
                    col = scol + vis_w;
                    continue;
                }
                let mut style = item_kind_style(item.kind);
                // A generated carousel scroll control greys out when it can't
                // page that way (the spec's `:disabled` end state).
                if let Some(crate::doc::Link::CarouselScroll(dir)) = &item.link {
                    let active = carousels
                        .iter()
                        .filter(|c| c.end > row_idx)
                        .min_by_key(|c| c.start.abs_diff(row_idx))
                        .is_some_and(|c| c.can_scroll(i32::from(*dir)));
                    if !active {
                        style = Style::new().fg(theme::DIM);
                    }
                }
                // Emphasis is orthogonal to kind: a link or heading can
                // also carry bold/italic/underline/strike from tags or CSS.
                if item.emph.bold {
                    style = style.add_modifier(Modifier::BOLD);
                }
                if item.emph.italic {
                    style = style.add_modifier(Modifier::ITALIC);
                }
                if item.emph.underline {
                    style = style.add_modifier(Modifier::UNDERLINED);
                }
                if item.emph.strike {
                    style = style.add_modifier(Modifier::CROSSED_OUT);
                }
                let selected = item.is_interactive()
                    && match sel_node {
                        // Highlight all pieces of the selected link (it may
                        // have wrapped); fall back to the exact item when
                        // the selection has no source node.
                        Some(n) if n != NO_NODE => item.node == n,
                        _ => g.sel_item == Some((row_idx, i)),
                    };
                if selected {
                    style = style.add_modifier(Modifier::REVERSED | Modifier::BOLD);
                }
                // Only resolve the (possibly region-buffer) origin while a
                // find is open — this runs per item per frame. A horizontally
                // CLIPPED carousel item is skipped: its find ranges are computed
                // against the full item text, and offsetting them into the
                // visible slice is deferred (a rare case — searching inside a
                // scrolled code strip).
                let ranges = if find.is_some() && !clipped {
                    let loc =
                        match crate::layout::item_origin(&g.doc.rows, &g.doc.regions, row_idx, i) {
                            crate::layout::ItemOrigin::Doc => FindLoc::Item {
                                row: row_idx,
                                item: i,
                            },
                            crate::layout::ItemOrigin::Region {
                                region,
                                brow,
                                bitem,
                            } => FindLoc::Region {
                                region,
                                brow,
                                bitem,
                            },
                        };
                    find_ranges(find, loc)
                } else {
                    Vec::new()
                };
                if ranges.is_empty() {
                    // Owned (not `as_str`): a scroll-region row's items live in a
                    // freshly merged row (`effective_row`'s `Cow::Owned`), so the
                    // span can't borrow from it. `match_spans` already owns.
                    spans.push(Span::styled(text.clone().into_owned(), style));
                } else {
                    spans.extend(match_spans(&text, style, &ranges));
                }
                // An item can reserve more columns than its text fills — an
                // inline image carries an empty string but a real W×H box (the
                // pixels are overlaid in the second pass). Pad the remainder
                // with spaces so the row's visible width tracks the item's
                // VISIBLE width (`vis_w` — clipped to the band for a carousel
                // wide run); without this an image collapses to zero width and
                // every following item slides left UNDER it (the header logo
                // painting over the nav links, an avatar over its post title).
                let text_w = crate::layout::display_width(&text) as u16;
                if vis_w > text_w {
                    spans.push(Span::raw(" ".repeat((vis_w - text_w) as usize)));
                }
                col = scol + vis_w;
            }
            Line::from(spans)
        })
        .collect()
}

/// Draw the PINNED fixed layer over the scrolling document: each captured
/// `position:fixed` box (a sidebar/header rail) paints at a FIXED screen
/// position — NOT offset by scroll — so the document scrolls beneath it. Rows
/// are placed at their box-relative columns; the box is clipped to the panel.
/// Decoded images inside a rail overlay their reserved box (a fixed avatar/logo)
/// just like the scrolling document's inline-image pass.
fn render_fixed_layer(
    frame: &mut Frame,
    g: &BrowserView,
    inner: Rect,
    find: Option<&FindState>,
    protocols: &std::collections::HashMap<
        crate::app::EncKey,
        ratatui_image::sliced::SlicedProtocol,
    >,
) {
    use ratatui_image::sliced::{SignedPosition, SlicedImage};
    for (fi, item) in g.doc.fixed.iter().enumerate() {
        let sx = inner.x.saturating_add(item.col);
        if sx >= inner.right() {
            continue;
        }
        let w = inner.right() - sx;
        for (r, row) in item.rows.iter().enumerate() {
            let sy = inner.y as usize + item.row as usize + r;
            if sy >= inner.bottom() as usize {
                break; // rows past the panel bottom are clipped
            }
            // The hovered/selected fixed item (this fixed box + row) highlights.
            let sel = match g.sel_fixed {
                Some((sfi, sr, si)) if sfi == fi && sr == r => Some(si),
                _ => None,
            };
            frame.render_widget(
                Paragraph::new(fixed_row_line(row, sel, find, fi, r)),
                Rect::new(sx, sy as u16, w, 1),
            );
        }
        // Overlay the rail's decoded images at their box-relative positions.
        // The panel `Rect` (the rail's on-screen span) clips a tall/wide image
        // to the rail, exactly like the region-image pass clips to its band.
        let panel_top = i32::from(inner.y) + i32::from(item.row);
        let panel_bot = i32::from(inner.bottom());
        if panel_top >= panel_bot {
            continue;
        }
        let panel = Rect::new(sx, panel_top as u16, w, (panel_bot - panel_top) as u16);
        for (r, row) in item.rows.iter().enumerate() {
            for it in &row.items {
                let Some(url) = &it.image else { continue };
                if it.invisible {
                    continue; // opacity:0 — reserve the box, draw nothing
                }
                if u32::from(it.col) >= u32::from(w) {
                    continue; // past the rail's right edge
                }
                let key = crate::app::EncKey::for_item(url, it);
                let Some(proto) = protocols.get(&key) else {
                    continue;
                };
                let position = SignedPosition::from((it.col as i16, r as i16));
                crate::app::IMG_RENDERS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                frame.render_widget(SlicedImage::new(proto, position), panel);
            }
        }
    }
}

/// A pinned fixed-layer row as a styled `Line`: items placed at their
/// box-relative columns (gap-filled), coloured by kind + emphasis. `sel` is the
/// hovered/selected item's index in `row.items` (highlighted reversed+bold).
/// `layer`/`row_idx` name this row for find-match highlighting.
fn fixed_row_line(
    row: &crate::layout::Row,
    sel: Option<usize>,
    find: Option<&FindState>,
    layer: usize,
    row_idx: usize,
) -> Line<'static> {
    // Keep the original index (the hit-test / `sel` addresses `row.items[i]`)
    // while placing left-to-right by column.
    let mut items: Vec<(usize, &crate::layout::Item)> = row.items.iter().enumerate().collect();
    items.sort_by_key(|(_, it)| it.col);
    let mut spans: Vec<Span> = Vec::new();
    let mut col = 0u16;
    for (idx, it) in items {
        if it.col > col {
            spans.push(Span::raw(" ".repeat((it.col - col) as usize)));
        }
        let mut style = item_kind_style(it.kind);
        if it.emph.bold {
            style = style.add_modifier(Modifier::BOLD);
        }
        if it.emph.italic {
            style = style.add_modifier(Modifier::ITALIC);
        }
        if it.emph.underline {
            style = style.add_modifier(Modifier::UNDERLINED);
        }
        if it.emph.strike {
            style = style.add_modifier(Modifier::CROSSED_OUT);
        }
        if sel == Some(idx) {
            style = style.add_modifier(Modifier::REVERSED | Modifier::BOLD);
        }
        let ranges = find_ranges(
            find,
            FindLoc::Fixed {
                layer,
                row: row_idx,
                item: idx,
            },
        );
        if ranges.is_empty() {
            spans.push(Span::styled(it.text.clone(), style));
        } else {
            spans.extend(match_spans(&it.text, style, &ranges));
        }
        let text_w = crate::layout::display_width(&it.text) as u16;
        if it.width > text_w {
            spans.push(Span::raw(" ".repeat((it.width - text_w) as usize)));
        }
        col = it.col + it.width;
    }
    Line::from(spans)
}

/// Overlay encoded inline images onto their reserved boxes within the
/// browser viewport. `inner` is the content rect (inside the border);
/// row `r` of the visible slice maps to screen row `inner.y + r`, the
/// item's `col` to `inner.x + col`. An image only draws when its whole
/// box fits the viewport (the stateless widget won't clip — keeps sixel
/// from corrupting); a partly-scrolled image waits, alt text in its place.
fn render_inline_images(
    frame: &mut Frame,
    g: &BrowserView,
    inner: ratatui::layout::Rect,
    protocols: &std::collections::HashMap<
        crate::app::EncKey,
        ratatui_image::sliced::SlicedProtocol,
    >,
) {
    use ratatui_image::sliced::{SignedPosition, SlicedImage};
    let vh = inner.height;
    let carousels = &g.doc.carousels;
    let end = (g.scroll + vh as usize).min(g.doc.rows.len());
    // Back-scan above the viewport: a tall image whose top scrolled off the top
    // still reaches into view; `SlicedImage` clips it (a negative `y` skips the
    // off-screen rows — sixel bands — and the bottom is dropped).
    let start = g.scroll.saturating_sub(crate::layout::MAX_IMAGE_LOOKBACK);
    for (off, row) in g.doc.rows[start..end].iter().enumerate() {
        let doc_row = start + off;
        for item in &row.items {
            let Some(url) = &item.image else { continue };
            // Paint suppression (`opacity:0`): the image box is reserved (its
            // rows still spacer-pad the flow) but no pixels are drawn.
            if item.invisible {
                continue;
            }
            // Carousel offset/clip: a strip image scrolled out of its band
            // doesn't draw — snapping keeps whole cards, so it's never cut.
            let Some(scol) = visible_col(carousels, doc_row, item) else {
                continue;
            };
            let key = crate::app::EncKey::for_item(url, item);
            let Some(proto) = protocols.get(&key) else {
                continue;
            };
            // Position the box's top-left relative to the content rect; `y` may
            // be negative (scrolled above the top). One scroll-independent
            // encode serves every position, so a partly-visible image renders
            // at the same scale as a fully-visible one (no resize-on-scroll).
            let position =
                SignedPosition::from((scol as i16, (doc_row as isize - g.scroll as isize) as i16));
            crate::app::IMG_RENDERS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            frame.render_widget(SlicedImage::new(proto, position), inner);
        }
    }
    render_region_images(frame, g, inner, protocols);
}

/// Draw the images held in each vertical scroll region's buffer (the reserved
/// doc rows are blank, so the pass above never sees them). Each region's
/// windowed buffer images are rendered into the region's on-screen BAND `Rect`
/// — not the full content area — so the sliced widget clips them to the band:
/// its top edge when an image is scrolled partly off the region's top, its
/// bottom edge, and the scrollport width. That clipping is what keeps a region
/// image (a chat avatar, a thumbnail in a scroll panel) from bleeding into the
/// page content above/below or past the region's right edge.
fn render_region_images(
    frame: &mut Frame,
    g: &BrowserView,
    inner: ratatui::layout::Rect,
    protocols: &std::collections::HashMap<
        crate::app::EncKey,
        ratatui_image::sliced::SlicedProtocol,
    >,
) {
    use ratatui::layout::Rect;
    use ratatui_image::sliced::{SignedPosition, SlicedImage};
    let inner_top = i32::from(inner.y);
    let inner_bot = i32::from(inner.y) + i32::from(inner.height);
    let inner_right = i32::from(inner.x) + i32::from(inner.width);
    for rg in &g.doc.regions {
        // The band's on-screen rows (the reserved doc rows move with the page
        // scroll), clamped to the content area.
        let band_top = inner_top + rg.start_row as i32 - g.scroll as i32;
        let band_bot = band_top + i32::from(rg.height);
        let vis_top = band_top.max(inner_top);
        let vis_bot = band_bot.min(inner_bot);
        if vis_bot <= vis_top {
            continue; // band scrolled entirely off-screen
        }
        let band_x = i32::from(inner.x) + i32::from(rg.left);
        if band_x >= inner_right {
            continue; // band off the right edge
        }
        let band_w = i32::from(rg.width).min(inner_right - band_x) as u16;
        let band = Rect {
            x: band_x as u16,
            y: vis_top as u16,
            width: band_w,
            height: (vis_bot - vis_top) as u16,
        };
        // Window the buffer: the visible rows plus the lookback (a tall image
        // whose top scrolled above the band still reaches down into it — a
        // negative `y` clips its top, exactly like the document pass).
        let top = rg.voffset.saturating_sub(crate::layout::MAX_IMAGE_LOOKBACK);
        let bot = rg.voffset + rg.height as usize;
        for br in top..bot {
            let Some(brow) = rg.buffer.get(br) else {
                continue;
            };
            for item in &brow.items {
                let Some(url) = &item.image else { continue };
                // Paint suppression (`opacity:0`): reserve the box, draw nothing.
                if item.invisible {
                    continue;
                }
                if u32::from(item.col) >= u32::from(band_w) {
                    continue; // past the scrollport's right edge
                }
                let key = crate::app::EncKey::for_item(url, item);
                let Some(proto) = protocols.get(&key) else {
                    continue;
                };
                // Position relative to the visible band's top-left; the
                // `band_top - vis_top` term is the negative offset when the
                // band's top is clipped, so rows shift up correctly.
                let pos_y = (band_top - vis_top) + (br as i32 - rg.voffset as i32);
                let position = SignedPosition::from((item.col as i16, pos_y as i16));
                crate::app::IMG_RENDERS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                frame.render_widget(SlicedImage::new(proto, position), band);
            }
        }
    }
}

/// Draw a scroll-position indicator on the panel's right border when the
/// document is taller than the viewport. The thumb's size and position track
/// `scroll` against the total row/line count, like a real browser's scrollbar.
/// Rendered over the right border (between the corners) so it costs no content
/// width; absent entirely when the whole document fits.
fn render_browser_scrollbar(
    frame: &mut Frame,
    g: &BrowserView,
    session_area: Rect,
    inner: Rect,
) -> Option<crate::app::ScrollTrack> {
    // A locked-viewport page (Twitch and every SPA app shell) carries its whole
    // scroll in a PRINCIPAL region, not in `rows` — the document itself barely
    // overflows. The main scrollbar then tracks THAT region (its buffer height
    // vs its clientHeight, positioned at its voffset), because it IS the page's
    // scroll. Otherwise the ordinary document scroll drives it.
    let (total, viewport, position, principal) = if let Some(rg) = g.doc.principal_region() {
        (rg.buffer.len(), rg.height as usize, rg.voffset, true)
    } else {
        let total = if g.doc.laid_out() {
            g.doc.rows.len()
        } else {
            g.doc.lines.len()
        };
        (total, inner.height as usize, g.scroll, false)
    };
    if total <= viewport {
        return None; // fits — no scrollbar, the border stays whole
    }
    // `content_length` is the SCROLLABLE range (max scroll = total − viewport),
    // not the row count, so the thumb reaches both ends exactly: top at
    // `scroll == 0`, bottom at the last scroll position.
    let mut state = ScrollbarState::new(total - viewport)
        .position(position)
        .viewport_content_length(viewport);
    let bar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
        .thumb_symbol("█")
        .thumb_style(Style::new().fg(theme::NEON_CYAN))
        .track_symbol(Some("│"))
        .track_style(Style::new().fg(theme::DIM))
        .begin_symbol(None)
        .end_symbol(None);
    // The right border column, excluding the corner rows (vertical margin 1).
    let area = session_area.inner(Margin {
        vertical: 1,
        horizontal: 0,
    });
    frame.render_stateful_widget(bar, area, &mut state);
    // The clickable/draggable track is the border column the bar drew in.
    Some(crate::app::ScrollTrack {
        rect: Rect::new(area.right().saturating_sub(1), area.y, 1, area.height),
        principal,
        content: total,
        viewport,
    })
}

/// Draw a horizontal scrollbar under each on-screen carousel that overflows its
/// band, and return the drawn tracks for the mouse hit-test. The bar sits on the
/// carousel band's BOTTOM row (the conventional place for a horizontal
/// scrollbar), spanning the band columns; the thumb tracks the strip offset.
/// Only carousels whose bottom row is currently on screen get one.
fn render_carousel_scrollbars(
    frame: &mut Frame,
    g: &BrowserView,
    inner: Rect,
) -> Vec<crate::app::CarouselTrack> {
    let mut tracks = Vec::new();
    for (ci, c) in g.doc.carousels.iter().enumerate() {
        if c.max_offset() == 0 || c.end == 0 {
            continue; // nothing to scroll to
        }
        // Draw the bar on the row just BELOW the band (doc row `end`), clipped
        // to the band's columns — so it never overlays the strip's own content
        // (a single-row code block would otherwise be hidden by its own bar). It
        // only covers the band-column slice of the following row.
        let doc_row = c.end;
        if doc_row < g.scroll {
            continue;
        }
        let sy = inner.y as usize + (doc_row - g.scroll);
        if sy >= inner.bottom() as usize {
            continue; // off-screen vertically
        }
        // The band's on-screen column span, clamped to the panel.
        let bx = inner.x.saturating_add(c.left).min(inner.right());
        let bw = c.view_width().min(inner.right().saturating_sub(bx));
        if bw == 0 {
            continue;
        }
        let rect = Rect::new(bx, sy as u16, bw, 1);
        let mut state = ScrollbarState::new((c.width.saturating_sub(c.view_width())) as usize)
            .position(c.offset as usize)
            .viewport_content_length(c.view_width() as usize);
        let bar = Scrollbar::new(ScrollbarOrientation::HorizontalBottom)
            .thumb_symbol("█")
            .thumb_style(Style::new().fg(theme::NEON_CYAN))
            .track_symbol(Some("─"))
            .track_style(Style::new().fg(theme::DIM))
            .begin_symbol(None)
            .end_symbol(None);
        frame.render_stateful_widget(bar, rect, &mut state);
        tracks.push(crate::app::CarouselTrack { car: ci, rect });
    }
    tracks
}

/// Status-bar / strip badge for the active browser protocol.
fn protocol_badge(g: &BrowserView) -> &'static str {
    match &g.doc.url {
        Link::Gopher(_) => " GOPHER ",
        Link::Gemini(_) => " GEMINI ",
        Link::Http(_) => " WWW ",
        Link::OneShot(url) => match url.scheme {
            crate::oneshot::Scheme::Finger => " FINGER ",
            crate::oneshot::Scheme::Whois => " WHOIS ",
            crate::oneshot::Scheme::Dict => " DICT ",
        },
        Link::JsClick { .. } => " WWW ",
        // Form controls, carousel buttons, and media representations never
        // appear as a document's own URL.
        Link::Form { .. } | Link::CarouselScroll(_) | Link::Media(_) => " WWW ",
        Link::External(_) => " NET ",
    }
}

/// Status-bar text for a selected form control.
fn form_status(g: &BrowserView, form: usize, field: usize) -> String {
    use crate::doc::{FieldKind, FormMethod};
    let Some(f) = g.doc.forms.get(form) else {
        return String::from(" form ");
    };
    let Some(field) = f.fields.get(field) else {
        return String::from(" form ");
    };
    match &field.kind {
        FieldKind::Submit => {
            let method = match f.method {
                FormMethod::Get => "GET",
                FormMethod::Post => "POST",
            };
            format!(" {method} {} — Enter submits ", f.action)
        }
        FieldKind::Checkbox => format!(" {} — Enter toggles ", field.name),
        FieldKind::Radio => format!(" {} — Enter selects ", field.name),
        FieldKind::Select(_) => format!(" {} — Enter cycles options ", field.name),
        _ => format!(" {} — Enter edits ", field.name),
    }
}

/// First visible char of the input window: keeps the cursor in view
/// (riding the right edge once the text outgrows the field) while
/// showing as much text as fits.
fn window_start(cursor: usize, len: usize, avail: usize) -> usize {
    // The +1s make room for the cursor's virtual cell past the end.
    (cursor + 1)
        .saturating_sub(avail)
        .min((len + 1).saturating_sub(avail))
}

fn input_box(app: &App, width: u16) -> Paragraph<'_> {
    // Browsing and character mode capture keystrokes, so the field
    // renders as a dimmed strip rather than shifting the layout around.
    if app.mode == Mode::Session
        && let Some((badge, text)) = strip_content(app)
    {
        let line = Line::from(vec![
            Span::styled(
                badge,
                Style::new()
                    .fg(theme::BG)
                    .bg(theme::DIM)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(text, Style::new().fg(theme::DIM)),
        ]);
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(theme::DIM))
            .style(Style::new().bg(theme::BG));
        return Paragraph::new(line).block(block);
    }

    // The search prompt doubles as the form-field editor, the identity-name
    // prompt for capsules that ask for a certificate, and the masked secret
    // prompt (gemini status 11, HTML password fields).
    let editing_field = matches!(app.search_target, Some(Link::Form { .. }));
    let minting_identity = app.cert_for.is_some();
    let masked = app.mode == Mode::Search && app.masked_input;
    let (prompt, accent) = match app.mode {
        Mode::Session => ("❯ ", theme::NEON_GREEN),
        Mode::Command => ("trust> ", theme::PASTEL_GREEN),
        Mode::Search if masked => ("secret> ", theme::PASTEL_GREEN),
        Mode::Search if minting_identity => ("name> ", theme::PASTEL_GREEN),
        Mode::Search if editing_field => ("input> ", theme::PASTEL_GREEN),
        Mode::Search => ("search> ", theme::PASTEL_GREEN),
        Mode::Find => ("find> ", theme::PASTEL_GREEN),
    };

    // Window the text horizontally so the cursor (and the prompt) stay
    // visible however long the line grows; render per-char so the
    // cursor block and any Shift-selection can style their cells.
    let chars: Vec<char> = app.input.chars().collect();
    let avail = (width as usize)
        .saturating_sub(prompt.chars().count())
        .max(1);
    let start = window_start(app.cursor, chars.len(), avail);
    let selection = app.selection();
    let mut spans = vec![Span::styled(
        prompt,
        Style::new().fg(accent).add_modifier(Modifier::BOLD),
    )];
    for i in start..(start + avail).min(chars.len() + 1) {
        // One virtual cell past the end hosts the cursor block.
        let ch = if i == start && start > 0 {
            '…' // more text off to the left
        } else if masked && i < chars.len() {
            '•' // sensitive input: never echo the secret
        } else {
            chars.get(i).copied().unwrap_or(' ')
        };
        let mut style = Style::new().fg(theme::NEON_CYAN);
        if selection.is_some_and(|(lo, hi)| i >= lo && i < hi) {
            style = style.bg(theme::DIM);
        }
        if i == app.cursor {
            style = style.add_modifier(Modifier::REVERSED);
        }
        spans.push(Span::styled(ch.to_string(), style));
    }
    let line = Line::from(spans);

    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(accent))
        .style(Style::new().bg(theme::BG));
    let block = match app.mode {
        Mode::Session => block,
        Mode::Command => block.title(Line::styled(
            " COMMAND ",
            Style::new()
                .fg(theme::BG)
                .bg(theme::PASTEL_GREEN)
                .add_modifier(Modifier::BOLD),
        )),
        Mode::Search => block.title(Line::styled(
            if masked {
                " SECRET "
            } else if minting_identity {
                " IDENTITY "
            } else if editing_field {
                " INPUT "
            } else {
                " SEARCH "
            },
            Style::new()
                .fg(theme::BG)
                .bg(theme::PASTEL_GREEN)
                .add_modifier(Modifier::BOLD),
        )),
        Mode::Find => block.title(Line::styled(
            " FIND ",
            Style::new()
                .fg(theme::BG)
                .bg(theme::PASTEL_GREEN)
                .add_modifier(Modifier::BOLD),
        )),
    };

    Paragraph::new(line).block(block)
}

/// Badge and hint for the dimmed strip when keys bypass the input field.
/// Only the character-at-a-time strip remains: browsing and the image
/// viewer drop the input box entirely (see `draw`'s collapse path), so
/// their badges/hints live solely on the status line.
fn strip_content(app: &App) -> Option<(&'static str, &'static str)> {
    app.char_mode().then_some((
        " CHAR ",
        " keys go directly to remote · server echoes · Tab/Ctrl-] cmds",
    ))
}

fn status_bar(app: &App) -> Paragraph<'_> {
    let (label, color) = if app.viewer.is_some() {
        (" IMG ", theme::NEON_PINK)
    } else if let Some(g) = &app.browser {
        (protocol_badge(g), theme::NEON_CYAN)
    } else if app.connected {
        (" LINK:ONLINE ", theme::NEON_GREEN)
    } else {
        (" LINK:DOWN ", theme::NEON_PINK)
    };
    let laid_out = app.browser.as_ref().map(|g| g.doc.laid_out());
    let hint = match (app.mode, laid_out, app.char_mode()) {
        (Mode::Session, _, _) if app.viewer.is_some() => "· ← Esc close · Tab cmds",
        (Mode::Session, Some(true), _) => {
            "· ↑↓←→ move · Enter follow · ⌫ back · Esc stop · Tab cmds"
        }
        (Mode::Session, Some(false), _) => "· ↑↓ scroll · → follow · ← back · Esc stop · Tab cmds",
        (Mode::Session, None, true) => "· keys go to remote · Tab/Ctrl-] cmds",
        (Mode::Session, None, false) => "· Enter send · Tab/Esc cmds",
        (Mode::Command, ..) => "· Enter run · Esc/Tab back · help · open <url>/close/quit",
        (Mode::Search, ..) if app.masked_input => "· Enter send · Esc cancel · typing is hidden",
        (Mode::Search, ..) if app.cert_for.is_some() => "· Enter mints the identity · Esc cancel",
        (Mode::Search, ..) if matches!(app.search_target, Some(Link::Form { .. })) => {
            "· Enter set · Esc cancel"
        }
        (Mode::Search, ..) => "· Enter search · Esc cancel",
        (Mode::Find, ..) => "· Enter/↓ next · Shift-Enter/↑ prev · Esc close",
    };
    let mut spans = vec![Span::styled(
        label,
        Style::new()
            .fg(theme::BG)
            .bg(color)
            .add_modifier(Modifier::BOLD),
    )];
    if app.connected && app.tls {
        spans.push(Span::styled(
            " TLS ",
            Style::new()
                .fg(theme::BG)
                .bg(theme::NEON_GREEN)
                .add_modifier(Modifier::BOLD),
        ));
    }
    if app.encoding == Encoding::Cp437 {
        spans.push(Span::styled(
            " CP437 ",
            Style::new().fg(theme::BG).bg(theme::DIM),
        ));
    }
    let scrollback = app.vt.screen().scrollback();
    if app.browser.is_none() && scrollback > 0 {
        spans.push(Span::styled(
            format!(" SCROLL ↑{scrollback} "),
            Style::new()
                .fg(theme::BG)
                .bg(theme::AMBER)
                .add_modifier(Modifier::BOLD),
        ));
    }
    // While browsing, show the selected link instead of the connection
    // status, the way gopherus does — unless a fetch just went wrong,
    // which must not hide behind the selection hint.
    let selection = app
        .selected_link()
        .filter(|_| !app.notice && app.viewer.is_none() && app.mode != Mode::Find);
    let middle = match (&app.viewer, &selection) {
        // While viewing an image: its dimensions and type (unless a
        // notice — e.g. a failed re-encode — needs the bar).
        (Some(v), _) if !app.notice => format!(" {} — {} ", v.url, v.info),
        (_, Some(Link::Form { form, field })) => {
            form_status(app.browser.as_ref().unwrap(), *form, *field)
        }
        (_, Some(link)) => format!(" → {link} "),
        _ => format!(" {} ", app.status),
    };
    spans.push(Span::styled(middle, Style::new().fg(theme::NEON_CYAN)));
    spans.push(Span::styled(hint, Style::new().fg(theme::DIM)));
    Paragraph::new(Line::from(spans)).style(Style::new().bg(theme::BG))
}

#[cfg(test)]
mod tests {
    use super::window_start;

    #[test]
    fn input_window_keeps_cursor_and_tail_visible() {
        // Short text: no scrolling.
        assert_eq!(window_start(3, 5, 20), 0);
        // Cursor at the end of long text: the tail (and cursor cell)
        // fills the field, latest chars visible.
        assert_eq!(window_start(30, 30, 10), 21);
        // Cursor moved into the middle: it rides the window edge.
        assert_eq!(window_start(15, 30, 10), 6);
        // Back at the start: window follows all the way home.
        assert_eq!(window_start(0, 30, 10), 0);
        // Degenerate width never panics or hides the cursor.
        assert_eq!(window_start(4, 4, 1), 4);
    }
}
