//! Terminal frontend adapter for the shared terminal cells.

use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
};

use crate::terminal::Terminal;

pub fn paint(frame: &mut Frame, terminal: &Terminal, area: Rect, cursor: bool) {
    let screen = terminal.screen();
    let (rows, cols) = screen.size();
    for row in 0..rows.min(area.height) {
        for col in 0..cols.min(area.width) {
            let Some(cell) = screen.cell(row, col) else {
                continue;
            };
            if cell.is_wide_continuation() {
                continue;
            }
            let (mut foreground, mut background) = (
                crate::terminal_view::color_rgb(cell.fgcolor(), false),
                crate::terminal_view::color_rgb(cell.bgcolor(), true),
            );
            if cell.inverse() ^ screen.reverse_video() {
                std::mem::swap(&mut foreground, &mut background);
            }
            if cell.dim() {
                foreground = foreground.map(|component| (u16::from(component) * 2 / 3) as u8);
            }
            if terminal.highlighted(row, col) {
                foreground = [0, 0, 0];
                background = [255, 190, 60];
            }
            let mut modifiers = Modifier::empty();
            if cell.bold() {
                modifiers |= Modifier::BOLD;
            }
            if cell.italic() {
                modifiers |= Modifier::ITALIC;
            }
            if cell.underline() {
                modifiers |= Modifier::UNDERLINED;
            }
            if cell.strike() {
                modifiers |= Modifier::CROSSED_OUT;
            }
            if cell.rapid_blink() {
                modifiers |= Modifier::RAPID_BLINK;
            } else if cell.blink() {
                modifiers |= Modifier::SLOW_BLINK;
            }
            let style = Style::reset()
                .fg(Color::Rgb(foreground[0], foreground[1], foreground[2]))
                .bg(Color::Rgb(background[0], background[1], background[2]))
                .add_modifier(modifiers);
            let width = if cell.is_wide() { 2 } else { 1 };
            let symbol = if cell.conceal() || !cell.has_contents() {
                " "
            } else {
                cell.contents()
            };
            let x = area.x + col;
            let y = area.y + row;
            if width == 2 && col + 1 >= area.width {
                continue;
            }
            frame.buffer_mut()[(x, y)]
                .set_symbol(symbol)
                .set_style(style);
            if width == 2 {
                frame.buffer_mut()[(x + 1, y)]
                    .set_symbol(" ")
                    .set_style(style);
            }
        }
    }
    if cursor && !screen.hide_cursor() && screen.scrollback() == 0 {
        let (row, col) = screen.cursor_position();
        if row < area.height && area.width > 0 {
            frame.set_cursor_position((
                area.x + col.min(cols - 1).min(area.width - 1),
                area.y + row,
            ));
        }
    }
}
