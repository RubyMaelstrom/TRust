//! Rendering: cyberpunk chrome around the emulated remote screen, or the
//! gopher browser panel when one is open.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph};
use tui_term::widget::PseudoTerminal;

use crate::app::{App, BrowserView, Encoding, Mode};
use crate::doc::{Kind, Link};

pub mod theme {
    use ratatui::style::Color;

    pub const NEON_PINK: Color = Color::Rgb(0xff, 0x2b, 0xd6);
    pub const NEON_CYAN: Color = Color::Rgb(0x00, 0xff, 0xf9);
    pub const NEON_GREEN: Color = Color::Rgb(0x39, 0xff, 0x14);
    pub const AMBER: Color = Color::Rgb(0xff, 0xb0, 0x00);
    pub const DIM: Color = Color::Rgb(0x6e, 0x4e, 0x9e);
    pub const TEXT: Color = Color::Rgb(0xc8, 0xc8, 0xdc);
    pub const BG: Color = Color::Rgb(0x0b, 0x02, 0x21);
}

pub fn draw(frame: &mut Frame, app: &mut App) {
    let [session_area, input_area, status_area] = Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(3),
        Constraint::Length(1),
    ])
    .areas(frame.area());

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

    match (&app.viewer, &app.browser) {
        (Some(v), _) => {
            frame.render_widget(block, session_area);
            // Center the scaled image in the panel; the protocol was
            // encoded to fit it, but clamp anyway (a resize may not
            // have re-encoded yet).
            let size = v.protocol.size();
            let image_area = ratatui::layout::Rect::new(
                inner.x + inner.width.saturating_sub(size.width) / 2,
                inner.y + inner.height.saturating_sub(size.height) / 2,
                size.width.min(inner.width),
                size.height.min(inner.height),
            );
            frame.render_widget(ratatui_image::Image::new(&v.protocol), image_area);
        }
        (None, Some(g)) => {
            let doc = Paragraph::new(browser_lines(g, inner.height as usize)).block(block);
            frame.render_widget(doc, session_area);
        }
        (None, None) => {
            let term = PseudoTerminal::new(app.vt.screen()).block(block);
            frame.render_widget(term, session_area);
        }
    }

    frame.render_widget(
        input_box(app, input_area.width.saturating_sub(2)),
        input_area,
    );
    frame.render_widget(status_bar(app), status_area);

    // Fetch in flight: a tiny beating heart at the right end of the
    // entry bar, animated by the run loop's ticker. Lub, dub, rest —
    // filled bold, hollow, filled again, then dim through the diastole.
    if app.loading() && app.mode == Mode::Session && input_area.width > 8 {
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
        let area = ratatui::layout::Rect::new(
            input_area.right().saturating_sub(3),
            input_area.y + 1,
            1,
            1,
        );
        frame.render_widget(Paragraph::new(Span::styled(glyph, style)), area);
    }
}

/// The visible slice of a document, gopherus-style: the cursor line is
/// highlighted when it carries a link.
fn browser_lines(g: &BrowserView, height: usize) -> Vec<Line<'_>> {
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
                (true, Kind::Search | Kind::Input) => Style::new().fg(theme::AMBER),
                (true, Kind::Button) => Style::new()
                    .fg(theme::NEON_GREEN)
                    .add_modifier(Modifier::BOLD),
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
            Line::styled(line.text.as_str(), style)
        })
        .collect()
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
        // Form controls never appear as a document's own URL.
        Link::Form { .. } => " WWW ",
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

    // The search prompt doubles as the form-field editor and the
    // identity-name prompt for capsules that ask for a certificate.
    let editing_field = matches!(app.search_target, Some(Link::Form { .. }));
    let minting_identity = app.cert_for.is_some();
    let (prompt, accent) = match app.mode {
        Mode::Session => ("❯ ", theme::NEON_GREEN),
        Mode::Command => ("trust> ", theme::AMBER),
        Mode::Search if minting_identity => ("name> ", theme::AMBER),
        Mode::Search if editing_field => ("input> ", theme::AMBER),
        Mode::Search => ("search> ", theme::AMBER),
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
                .bg(theme::AMBER)
                .add_modifier(Modifier::BOLD),
        )),
        Mode::Search => block.title(Line::styled(
            if minting_identity {
                " IDENTITY "
            } else if editing_field {
                " INPUT "
            } else {
                " SEARCH "
            },
            Style::new()
                .fg(theme::BG)
                .bg(theme::AMBER)
                .add_modifier(Modifier::BOLD),
        )),
    };

    Paragraph::new(line).block(block)
}

/// Badge and hint for the dimmed strip when keys bypass the input field.
fn strip_content(app: &App) -> Option<(&'static str, &'static str)> {
    if app.viewer.is_some() {
        Some((" IMG ", " ← / Esc close"))
    } else if let Some(g) = &app.browser {
        Some((
            protocol_badge(g),
            " ↑↓ scroll · → follow · ← back · Esc terminal",
        ))
    } else if app.char_mode() {
        Some((" CHAR ", " keys go directly to remote · server echoes"))
    } else {
        None
    }
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
    let hint = match (app.mode, app.browser.is_some(), app.char_mode()) {
        (Mode::Session, ..) if app.viewer.is_some() => "· ← Esc close · Ctrl-] commands",
        (Mode::Session, true, _) => "· ↑↓ → ← navigate · Ctrl-] commands",
        (Mode::Session, false, true) => "· CHAR mode · Ctrl-] commands",
        (Mode::Session, false, false) => "· Enter send · Esc/Ctrl-] commands",
        (Mode::Command, ..) => "· Enter run · Esc back · open/close/mode/send/set/status/quit",
        (Mode::Search, ..) if app.cert_for.is_some() => "· Enter mints the identity · Esc cancel",
        (Mode::Search, ..) if matches!(app.search_target, Some(Link::Form { .. })) => {
            "· Enter set · Esc cancel"
        }
        (Mode::Search, ..) => "· Enter search · Esc cancel",
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
        .browser
        .as_ref()
        .and_then(|g| g.selected.and_then(|i| g.doc.lines.get(i)))
        .and_then(|l| l.link.as_ref())
        .filter(|_| !app.notice && app.viewer.is_none());
    let middle = match (&app.viewer, selection) {
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
