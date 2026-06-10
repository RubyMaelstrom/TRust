//! Rendering: cyberpunk chrome around the emulated remote screen, or the
//! gopher browser panel when one is open.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph};
use tui_term::widget::PseudoTerminal;

use crate::app::{App, Encoding, GopherView, Mode};

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

    let (title, border_color) = match &app.gopher {
        Some(g) => (format!("░▒▓ TRUST :: {} ▓▒░", g.doc.url), theme::NEON_CYAN),
        None => (
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

    match &app.gopher {
        Some(g) => {
            let doc = Paragraph::new(gopher_lines(g, inner.height as usize)).block(block);
            frame.render_widget(doc, session_area);
        }
        None => {
            let term = PseudoTerminal::new(app.vt.screen()).block(block);
            frame.render_widget(term, session_area);
        }
    }

    frame.render_widget(input_box(app), input_area);
    frame.render_widget(status_bar(app), status_area);
}

/// The visible slice of a gopher document, gopherus-style: the cursor
/// line is highlighted when it carries a link.
fn gopher_lines(g: &GopherView, height: usize) -> Vec<Line<'_>> {
    let end = (g.scroll + height).min(g.doc.lines.len());
    g.doc.lines[g.scroll..end]
        .iter()
        .enumerate()
        .map(|(i, line)| {
            let mut style = match (line.link.is_some(), line.kind) {
                (true, '1') => Style::new()
                    .fg(theme::NEON_CYAN)
                    .add_modifier(Modifier::BOLD),
                (true, '0') => Style::new().fg(theme::NEON_GREEN),
                (true, '7') => Style::new().fg(theme::AMBER),
                (true, _) => Style::new().fg(theme::NEON_PINK),
                (false, '3') => Style::new().fg(theme::NEON_PINK),
                _ => Style::new().fg(theme::TEXT),
            };
            if g.selected == Some(g.scroll + i) && line.link.is_some() {
                style = style.add_modifier(Modifier::REVERSED | Modifier::BOLD);
            }
            Line::styled(line.text.as_str(), style)
        })
        .collect()
}

fn input_box(app: &App) -> Paragraph<'_> {
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

    let (prompt, accent) = match app.mode {
        Mode::Session => ("❯ ", theme::NEON_GREEN),
        Mode::Command => ("trust> ", theme::AMBER),
        Mode::Search => ("search> ", theme::AMBER),
    };

    // Split the text around the cursor so the char under it renders inverted.
    let byte_cursor = app
        .input
        .char_indices()
        .nth(app.cursor)
        .map_or(app.input.len(), |(i, _)| i);
    let before = &app.input[..byte_cursor];
    let mut rest = app.input[byte_cursor..].chars();
    let at = rest.next().map_or(' ', |c| c);
    let after: String = rest.collect();

    let line = Line::from(vec![
        Span::styled(prompt, Style::new().fg(accent).add_modifier(Modifier::BOLD)),
        Span::styled(before, Style::new().fg(theme::NEON_CYAN)),
        Span::styled(
            at.to_string(),
            Style::new()
                .fg(theme::NEON_CYAN)
                .add_modifier(Modifier::REVERSED),
        ),
        Span::styled(after, Style::new().fg(theme::NEON_CYAN)),
    ]);

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
            " SEARCH ",
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
    if app.gopher.is_some() {
        Some((" GOPHER ", " ↑↓ scroll · → follow · ← back · Esc terminal"))
    } else if app.char_mode() {
        Some((" CHAR ", " keys go directly to remote · server echoes"))
    } else {
        None
    }
}

fn status_bar(app: &App) -> Paragraph<'_> {
    let (label, color) = if app.gopher.is_some() {
        (" GOPHER ", theme::NEON_CYAN)
    } else if app.connected {
        (" LINK:ONLINE ", theme::NEON_GREEN)
    } else {
        (" LINK:DOWN ", theme::NEON_PINK)
    };
    let hint = match (app.mode, app.gopher.is_some(), app.char_mode()) {
        (Mode::Session, true, _) => "· ↑↓ → ← navigate · Ctrl-] commands",
        (Mode::Session, false, true) => "· CHAR mode · Ctrl-] commands",
        (Mode::Session, false, false) => "· Enter send · Ctrl-] commands",
        (Mode::Command, ..) => "· Enter run · Esc back · open/close/mode/send/set/status/quit",
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
    if app.gopher.is_none() && scrollback > 0 {
        spans.push(Span::styled(
            format!(" SCROLL ↑{scrollback} "),
            Style::new()
                .fg(theme::BG)
                .bg(theme::AMBER)
                .add_modifier(Modifier::BOLD),
        ));
    }
    // While browsing, show the selected link instead of the connection
    // status, the way gopherus does.
    let middle = match app
        .gopher
        .as_ref()
        .and_then(|g| g.selected.and_then(|i| g.doc.lines.get(i)))
        .and_then(|l| l.link.as_ref())
    {
        Some(link) => format!(" → {link} "),
        None => format!(" {} ", app.status),
    };
    spans.push(Span::styled(middle, Style::new().fg(theme::NEON_CYAN)));
    spans.push(Span::styled(hint, Style::new().fg(theme::DIM)));
    Paragraph::new(Line::from(spans)).style(Style::new().bg(theme::BG))
}
