//! Renderer-neutral graphical view of TRust's existing VT state.
//!
//! Telnet terminal content is inherently a cell grid. This module is the sole
//! legitimate desktop cell renderer; HTML never passes through it.

use crate::core::{CssPoint, CssSize, Key, KeyInput, KeyState};
use crate::render::{
    DecorationStyle, DisplayCommand, PagePaint, PaintColor, PaintLine, TextDecorationPaint,
};
use crate::text::{self, TextStyle};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Encoding {
    #[default]
    Utf8,
    Cp437,
}

pub struct TerminalView {
    parser: vt100::Parser,
    pub encoding: Encoding,
    cell_width: f32,
    cell_height: f32,
}

impl TerminalView {
    pub fn new(rows: u16, cols: u16) -> Self {
        let metrics = text::shape("0", &terminal_text_style());
        Self {
            parser: vt100::Parser::new(rows.max(1), cols.max(1), 10_000),
            encoding: Encoding::Utf8,
            cell_width: metrics.advance.max(1.0),
            cell_height: metrics
                .line_height
                .max(crate::theme::TERMINAL_FONT_SIZE_CSS_PX * 1.2),
        }
    }

    pub fn size_for_viewport(&self, viewport: CssSize) -> (u16, u16) {
        (
            (viewport.width / self.cell_width)
                .floor()
                .clamp(1.0, f32::from(u16::MAX)) as u16,
            (viewport.height / self.cell_height)
                .floor()
                .clamp(1.0, f32::from(u16::MAX)) as u16,
        )
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.parser.screen_mut().set_size(rows.max(1), cols.max(1));
    }

    /// Process application bytes and return terminal-device replies for the
    /// common ANSI/VT probes used by MUDs and BBSes.
    pub fn process(&mut self, bytes: &[u8]) -> Vec<Vec<u8>> {
        let decoded;
        let bytes = if self.encoding == Encoding::Cp437 {
            decoded = crate::cp437::decode(bytes);
            decoded.as_slice()
        } else {
            bytes
        };
        self.parser.process(bytes);
        let mut replies = Vec::new();
        if bytes.windows(4).any(|window| window == b"\x1b[6n") {
            let (row, col) = self.parser.screen().cursor_position();
            replies.push(format!("\x1b[{};{}R", row + 1, col + 1).into_bytes());
        }
        if bytes.windows(4).any(|window| window == b"\x1b[5n") {
            replies.push(b"\x1b[0n".to_vec());
        }
        if bytes.windows(3).any(|window| window == b"\x1b[c") {
            replies.push(b"\x1b[?1;2c".to_vec());
        }
        replies
    }

    pub fn scroll(&mut self, lines: i32) {
        let current = self.parser.screen().scrollback();
        let next = if lines > 0 {
            current.saturating_add(lines as usize)
        } else {
            current.saturating_sub(lines.unsigned_abs() as usize)
        };
        self.parser.screen_mut().set_scrollback(next);
    }

    pub fn paint(&self) -> PagePaint {
        let screen = self.parser.screen();
        let (rows, cols) = screen.size();
        let mut paint = PagePaint {
            width: f32::from(cols) * self.cell_width,
            height: f32::from(rows) * self.cell_height,
            background: Some(theme_color(crate::theme::BG)),
            ..PagePaint::default()
        };
        let style_base = terminal_text_style();
        let base_metrics = text::shape("0", &style_base);
        for row in 0..rows {
            paint.lines.push(PaintLine {
                rect: crate::render::CssRect::new(
                    0.0,
                    f32::from(row) * self.cell_height,
                    paint.width,
                    self.cell_height,
                ),
                baseline: f32::from(row) * self.cell_height + base_metrics.baseline,
                ascent: base_metrics.ascent,
                descent: base_metrics.descent,
            });
            for col in 0..cols {
                let Some(cell) = screen.cell(row, col) else {
                    continue;
                };
                if cell.is_wide_continuation() {
                    continue;
                }
                let mut foreground = terminal_color(cell.fgcolor(), false);
                let mut background = terminal_color(cell.bgcolor(), true);
                if cell.inverse() {
                    std::mem::swap(&mut foreground, &mut background);
                }
                let x = f32::from(col) * self.cell_width;
                let y = f32::from(row) * self.cell_height;
                if background != theme_color(crate::theme::BG) {
                    paint.primitives.push(DisplayCommand::FillRect {
                        rect: crate::render::CssRect::new(
                            x,
                            y,
                            self.cell_width * if cell.is_wide() { 2.0 } else { 1.0 },
                            self.cell_height,
                        ),
                        color: background,
                    });
                }
                let text = if cell.has_contents() {
                    cell.contents()
                } else {
                    " "
                };
                let mut style = style_base.clone();
                // The configured Foot primary face is already bold. SGR 1
                // therefore retains that face (as Foot does when no separate
                // `font-bold` override is configured) instead of falling back
                // to the normal-weight JetBrains Mono face for unstyled cells.
                style.weight = if cell.bold() {
                    700.0
                } else {
                    style_base.weight
                };
                style.italic = cell.italic();
                style.underline = cell.underline();
                let shaped = text::shape(text, &style);
                paint.primitives.push(DisplayCommand::GlyphRun {
                    origin: CssPoint::new(x, y),
                    shaped,
                    color: foreground,
                    decoration: TextDecorationPaint {
                        color: foreground,
                        style: DecorationStyle::Solid,
                    },
                    clip: None,
                    node: usize::from(row) * usize::from(cols) + usize::from(col),
                    link: None,
                });
            }
        }
        if !screen.hide_cursor() {
            let (row, col) = screen.cursor_position();
            paint.primitives.push(DisplayCommand::FillRect {
                rect: crate::render::CssRect::new(
                    f32::from(col) * self.cell_width,
                    f32::from(row) * self.cell_height + self.cell_height - 2.0,
                    self.cell_width,
                    2.0,
                ),
                color: PaintColor::Rgba(220, 232, 245, 255),
            });
        }
        paint
    }

    pub fn encode_key(input: &KeyInput) -> Option<Vec<u8>> {
        if input.state != KeyState::Pressed {
            return None;
        }
        let bytes = match &input.key {
            Key::Character(text) if input.modifiers.control => {
                let character = text.bytes().next()?;
                vec![character.to_ascii_uppercase() & 0x1f]
            }
            Key::Character(text) => text.as_bytes().to_vec(),
            Key::Enter => b"\r\n".to_vec(),
            Key::Backspace => vec![0x7f],
            Key::Tab => vec![b'\t'],
            Key::Escape => vec![0x1b],
            Key::ArrowUp => b"\x1b[A".to_vec(),
            Key::ArrowDown => b"\x1b[B".to_vec(),
            Key::ArrowRight => b"\x1b[C".to_vec(),
            Key::ArrowLeft => b"\x1b[D".to_vec(),
            Key::Home => b"\x1b[H".to_vec(),
            Key::End => b"\x1b[F".to_vec(),
            Key::Delete => b"\x1b[3~".to_vec(),
            Key::PageUp => b"\x1b[5~".to_vec(),
            Key::PageDown => b"\x1b[6~".to_vec(),
            _ => return None,
        };
        Some(bytes)
    }
}

fn terminal_color(color: vt100::Color, background: bool) -> PaintColor {
    match color {
        // ECMA-48 SGR 39/49 restore the implementation's default rendition;
        // TRust's defaults are its shared terminal palette. Explicit indexed
        // and RGB SGR colors below remain authoritative.
        vt100::Color::Default if background => theme_color(crate::theme::BG),
        vt100::Color::Default => theme_color(crate::theme::TEXT),
        vt100::Color::Rgb(r, g, b) => PaintColor::Rgba(r, g, b, 255),
        vt100::Color::Idx(index) => {
            let (r, g, b) = ansi_color(index);
            PaintColor::Rgba(r, g, b, 255)
        }
    }
}

fn terminal_text_style() -> TextStyle {
    TextStyle {
        family: String::from(crate::theme::TERMINAL_FONT_FAMILY),
        size: crate::theme::TERMINAL_FONT_SIZE_CSS_PX,
        weight: crate::theme::TERMINAL_FONT_WEIGHT,
        ..TextStyle::default()
    }
}

const fn theme_color(rgb: crate::theme::Rgb) -> PaintColor {
    PaintColor::Rgba(rgb[0], rgb[1], rgb[2], 255)
}

fn ansi_color(index: u8) -> (u8, u8, u8) {
    const ANSI: [(u8, u8, u8); 16] = [
        (0, 0, 0),
        (170, 0, 0),
        (0, 170, 0),
        (170, 85, 0),
        (0, 0, 170),
        (170, 0, 170),
        (0, 170, 170),
        (170, 170, 170),
        (85, 85, 85),
        (255, 85, 85),
        (85, 255, 85),
        (255, 255, 85),
        (85, 85, 255),
        (255, 85, 255),
        (85, 255, 255),
        (255, 255, 255),
    ];
    if index < 16 {
        return ANSI[index as usize];
    }
    if index < 232 {
        let value = index - 16;
        let component = |value: u8| if value == 0 { 0 } else { 55 + value * 40 };
        return (
            component(value / 36),
            component((value / 6) % 6),
            component(value % 6),
        );
    }
    let gray = 8 + (index - 232) * 10;
    (gray, gray, gray)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgba(rgb: crate::theme::Rgb) -> PaintColor {
        PaintColor::Rgba(rgb[0], rgb[1], rgb[2], 255)
    }

    #[test]
    fn vt_cells_paint_through_the_shared_display_list() {
        let mut terminal = TerminalView::new(4, 12);
        terminal.process(b"\x1b[31mred\x1b[0m");
        let paint = terminal.paint();
        assert!(paint.primitives.iter().any(|command| matches!(
            command,
            DisplayCommand::GlyphRun { shaped, .. } if shaped.text == "r"
        )));
    }

    #[test]
    fn vt_defaults_use_the_shared_terminal_face_and_palette() {
        let style = terminal_text_style();
        assert_eq!(style.family, crate::theme::TERMINAL_FONT_FAMILY);
        assert_eq!(style.size, crate::theme::TERMINAL_FONT_SIZE_CSS_PX);
        assert_eq!(style.weight, crate::theme::TERMINAL_FONT_WEIGHT);

        let mut terminal = TerminalView::new(1, 2);
        terminal.process(b"x");
        let paint = terminal.paint();
        assert_eq!(paint.background, Some(rgba(crate::theme::BG)));
        assert!(paint.primitives.iter().any(|command| matches!(
            command,
            DisplayCommand::GlyphRun { shaped, color, .. }
                if shaped.text == "x" && *color == rgba(crate::theme::TEXT)
        )));
    }

    #[test]
    fn explicit_ansi_color_still_overrides_the_trust_default() {
        let mut terminal = TerminalView::new(1, 2);
        terminal.process(b"\x1b[31mx");
        assert!(terminal.paint().primitives.iter().any(|command| matches!(
            command,
            DisplayCommand::GlyphRun { shaped, color, .. }
                if shaped.text == "x" && *color == PaintColor::Rgba(170, 0, 0, 255)
        )));
    }

    #[test]
    fn terminal_keys_are_encoded_without_crossterm() {
        let input = KeyInput {
            key: Key::ArrowUp,
            state: KeyState::Pressed,
            modifiers: Default::default(),
            repeat: false,
            composing: false,
        };
        assert_eq!(TerminalView::encode_key(&input), Some(b"\x1b[A".to_vec()));
    }

    #[test]
    fn cp437_bbs_bytes_become_unicode_before_graphical_paint() {
        let mut terminal = TerminalView::new(2, 8);
        terminal.encoding = Encoding::Cp437;
        terminal.process(b"\xC9\xCD\xBB");
        let text = terminal
            .paint()
            .primitives
            .into_iter()
            .filter_map(|command| match command {
                DisplayCommand::GlyphRun { shaped, .. } => Some(shaped.text),
                _ => None,
            })
            .collect::<String>();
        assert!(text.starts_with("╔═╗"), "{text:?}");
    }
}
