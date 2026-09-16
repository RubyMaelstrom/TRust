//! Renderer-neutral graphical view of TRust's existing VT state.
//!
//! Telnet terminal content is inherently a cell grid. This module is the sole
//! legitimate desktop cell renderer; HTML never passes through it.

use crate::core::{CssPoint, CssSize};
#[cfg(test)]
use crate::core::{Key, KeyInput, KeyState};
use crate::render::{
    DecorationStyle, DisplayCommand, PagePaint, PaintColor, PaintLine, TextDecorationPaint,
};
use crate::text::{self, TextStyle};

pub use crate::terminal::Encoding;

pub struct TerminalView {
    pub terminal: crate::terminal::Terminal,
    cell_width: f32,
    cell_height: f32,
    rows: Vec<CachedRow>,
    paint: PagePaint,
    painted: Option<(u64, u8)>,
    blink_origin: std::time::Instant,
    blinking: bool,
    rapid: bool,
    cursor_active: bool,
}

#[derive(Default)]
struct CachedRow {
    cells: Vec<vt100::Cell>,
    highlighted: Vec<bool>,
    blink_on: bool,
    rapid_on: bool,
    reverse: bool,
    commands: Vec<DisplayCommand>,
}

impl TerminalView {
    pub fn new(rows: u16, cols: u16) -> Self {
        let metrics = text::shape("0", &terminal_text_style());
        Self {
            terminal: crate::terminal::Terminal::new(rows, cols),
            rows: Vec::new(),
            paint: PagePaint::default(),
            painted: None,
            blink_origin: std::time::Instant::now(),
            blinking: false,
            rapid: false,
            cursor_active: true,
            cell_width: metrics.advance.max(1.0),
            cell_height: metrics
                .line_height
                .max(crate::theme::TERMINAL_FONT_SIZE_CSS_PX * 1.2),
        }
    }

    pub fn size_for_viewport(&self, viewport: CssSize) -> (u16, u16) {
        crate::terminal::bounded_size(
            (viewport.width / self.cell_width)
                .floor()
                .clamp(1.0, f32::from(u16::MAX)) as u16,
            (viewport.height / self.cell_height)
                .floor()
                .clamp(1.0, f32::from(u16::MAX)) as u16,
        )
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.terminal.resize(cols, rows);
    }

    pub fn process(&mut self, bytes: &[u8]) -> Vec<Vec<u8>> {
        self.terminal.process(bytes)
    }
    pub fn scroll(&mut self, lines: i32) {
        self.terminal.scroll(lines);
    }

    pub fn cell_size(&self) -> (f32, f32) {
        (self.cell_width, self.cell_height)
    }

    pub fn set_cursor_active(&mut self, active: bool) {
        if self.cursor_active != active {
            self.cursor_active = active;
            self.painted = None;
        }
    }

    pub fn animation_delay(&self) -> Option<std::time::Duration> {
        let period = if self.rapid { 100 } else { 500 };
        self.blinking.then(|| {
            std::time::Duration::from_millis(
                period - (self.blink_origin.elapsed().as_millis() % u128::from(period)) as u64,
            )
        })
    }

    /// Retain unchanged cell rows and the complete display list at rest.
    /// Selection geometry remains tied to individual terminal cells, including
    /// fallback-font glyphs whose natural advances differ from the cell width.
    pub fn paint(&mut self) -> &PagePaint {
        self.paint_at(self.blink_origin.elapsed().as_millis())
    }

    fn paint_at(&mut self, elapsed_ms: u128) -> &PagePaint {
        let blink_on = (elapsed_ms / 500).is_multiple_of(2);
        let rapid_on = (elapsed_ms / 100).is_multiple_of(2);
        let phase = u8::from(self.blinking && blink_on) | (u8::from(self.rapid && rapid_on) << 1);
        if self.painted == Some((self.terminal.revision, phase)) {
            return &self.paint;
        }
        let screen = self.terminal.screen();
        let (rows, cols) = screen.size();
        let mut paint = PagePaint {
            width: f32::from(cols) * self.cell_width,
            height: f32::from(rows) * self.cell_height,
            background: Some(theme_color(if screen.reverse_video() {
                crate::theme::TEXT
            } else {
                crate::theme::BG
            })),
            ..PagePaint::default()
        };
        self.rows.resize_with(usize::from(rows), CachedRow::default);
        let style_base = terminal_text_style();
        let base_metrics = text::shape("0", &style_base);
        let mut blinking_text = false;
        let mut rapid_text = false;
        for row in 0..rows {
            let y = f32::from(row) * self.cell_height;
            paint.lines.push(PaintLine {
                rect: crate::render::CssRect::new(0.0, y, paint.width, self.cell_height),
                baseline: y + base_metrics.baseline,
                ascent: base_metrics.ascent,
                descent: base_metrics.descent,
            });
            let cached = &mut self.rows[usize::from(row)];
            let row_blinks =
                (0..cols).any(|col| screen.cell(row, col).is_some_and(|cell| cell.blink()));
            blinking_text |= row_blinks;
            let row_rapid =
                (0..cols).any(|col| screen.cell(row, col).is_some_and(|cell| cell.rapid_blink()));
            rapid_text |= row_rapid;
            let unchanged = cached.cells.len() == usize::from(cols)
                && cached.reverse == screen.reverse_video()
                && (!row_blinks || cached.blink_on == blink_on)
                && (!row_rapid || cached.rapid_on == rapid_on)
                && (0..cols).all(|col| {
                    screen.cell(row, col) == cached.cells.get(usize::from(col))
                        && cached.highlighted[usize::from(col)]
                            == self.terminal.highlighted(row, col)
                });
            if !unchanged {
                cached.cells.clear();
                cached.highlighted.clear();
                cached.commands.clear();
                cached.blink_on = blink_on;
                cached.rapid_on = rapid_on;
                cached.reverse = screen.reverse_video();
                for col in 0..cols {
                    let Some(cell) = screen.cell(row, col) else {
                        continue;
                    };
                    cached.cells.push(cell.clone());
                    let highlighted = self.terminal.highlighted(row, col);
                    cached.highlighted.push(highlighted);
                    if cell.is_wide_continuation() {
                        continue;
                    }
                    let mut foreground = terminal_color(cell.fgcolor(), false);
                    let mut background = terminal_color(cell.bgcolor(), true);
                    if cell.inverse() ^ screen.reverse_video() {
                        std::mem::swap(&mut foreground, &mut background);
                    }
                    if cell.dim()
                        && let PaintColor::Rgba(r, g, b, a) = foreground
                    {
                        let dim = |value: u8| (u16::from(value) * 2 / 3) as u8;
                        foreground = PaintColor::Rgba(dim(r), dim(g), dim(b), a);
                    }
                    if highlighted {
                        foreground = PaintColor::Rgba(0, 0, 0, 255);
                        background = PaintColor::Rgba(255, 190, 60, 255);
                    }
                    let x = f32::from(col) * self.cell_width;
                    let rect = crate::render::CssRect::new(
                        x,
                        y,
                        self.cell_width * if cell.is_wide() { 2.0 } else { 1.0 },
                        self.cell_height,
                    );
                    if Some(background) != paint.background {
                        cached.commands.push(DisplayCommand::FillRect {
                            rect,
                            color: background,
                        });
                    }
                    // Never export concealed text into scene copy/search data.
                    if cell.conceal()
                        || (cell.blink()
                            && !if cell.rapid_blink() {
                                rapid_on
                            } else {
                                blink_on
                            })
                        || !cell.has_contents()
                    {
                        continue;
                    }
                    let mut style = style_base.clone();
                    if cell.bold() {
                        style.weight = 700.0;
                    }
                    style.italic = cell.italic();
                    style.underline = cell.underline();
                    style.strikethrough = cell.strike();
                    let shaped = text::shape(cell.contents(), &style);
                    cached.commands.push(DisplayCommand::GlyphRun {
                        origin: CssPoint::new(x, y),
                        shaped,
                        color: foreground,
                        decoration: TextDecorationPaint {
                            color: foreground,
                            style: if cell.double_underline() {
                                DecorationStyle::Double
                            } else {
                                DecorationStyle::Solid
                            },
                        },
                        shadows: Vec::new(),
                        clip: Some(rect),
                        node: usize::from(row) * usize::from(cols) + usize::from(col),
                        link: None,
                    });
                }
            }
            paint.primitives.extend(cached.commands.iter().cloned());
        }
        let cursor_style = screen.cursor_style();
        let cursor_blinks = cursor_style == 0 || cursor_style % 2 == 1;
        let cursor_visible =
            self.cursor_active && !screen.hide_cursor() && screen.scrollback() == 0;
        self.blinking = blinking_text || (cursor_visible && cursor_blinks);
        self.rapid = rapid_text;
        if cursor_visible && (!cursor_blinks || blink_on) {
            let (row, col) = screen.cursor_position();
            let (x, y) = (
                f32::from(col.min(cols - 1)) * self.cell_width,
                f32::from(row) * self.cell_height,
            );
            let (rect, alpha) = match cursor_style {
                3 | 4 => (
                    crate::render::CssRect::new(
                        x,
                        y + self.cell_height - 2.0,
                        self.cell_width,
                        2.0,
                    ),
                    255,
                ),
                5 | 6 => (
                    crate::render::CssRect::new(x, y, 2.0, self.cell_height),
                    255,
                ),
                _ => (
                    crate::render::CssRect::new(x, y, self.cell_width, self.cell_height),
                    110,
                ),
            };
            paint.primitives.push(DisplayCommand::FillRect {
                rect,
                color: PaintColor::Rgba(220, 232, 245, alpha),
            });
        }
        self.painted = Some((
            self.terminal.revision,
            u8::from(self.blinking && blink_on) | (u8::from(self.rapid && rapid_on) << 1),
        ));
        self.paint = paint;
        &self.paint
    }

    #[cfg(test)]
    pub fn encode_key(input: &KeyInput) -> Option<Vec<u8>> {
        crate::terminal::Terminal::new(24, 80)
            .encode_key(input)
            .ok()
            .flatten()
    }
}

fn terminal_color(color: vt100::Color, background: bool) -> PaintColor {
    let [r, g, b] = color_rgb(color, background);
    PaintColor::Rgba(r, g, b, 255)
}

pub(crate) fn color_rgb(color: vt100::Color, background: bool) -> [u8; 3] {
    match color {
        vt100::Color::Default if background => crate::theme::BG,
        vt100::Color::Default => crate::theme::TEXT,
        vt100::Color::Rgb(r, g, b) => [r, g, b],
        vt100::Color::Idx(index) => {
            let (r, g, b) = ansi_color(index);
            [r, g, b]
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

pub(crate) fn ansi_color(index: u8) -> (u8, u8, u8) {
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
    fn telnet_bookmark_keys_keep_control_and_alt_characters() {
        let mut input = KeyInput {
            key: Key::Character("b".into()),
            code: String::new(),
            location: 0,
            state: KeyState::Pressed,
            modifiers: Default::default(),
            repeat: false,
            composing: false,
        };
        input.modifiers.control = true;
        assert_eq!(TerminalView::encode_key(&input), Some(b"\x02".to_vec()));
        input.modifiers.control = false;
        input.modifiers.alt = true;
        assert_eq!(TerminalView::encode_key(&input), Some(b"\x1bb".to_vec()));
        input.modifiers.control = true;
        assert_eq!(TerminalView::encode_key(&input), Some(b"\x1b\x02".to_vec()));
    }

    #[test]
    fn terminal_keys_are_encoded_without_crossterm() {
        let input = KeyInput {
            key: Key::ArrowUp,
            code: String::new(),
            location: 0,
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
        terminal.terminal.encoding = Encoding::Cp437;
        terminal.process(b"\xC9\xCD\xBB");
        let text = terminal
            .paint()
            .primitives
            .iter()
            .filter_map(|command| match command {
                DisplayCommand::GlyphRun { shaped, .. } => Some(shaped.text.as_str()),
                _ => None,
            })
            .collect::<String>();
        assert!(text.starts_with("╔═╗"), "{text:?}");
    }

    #[test]
    fn terminal_graphics_conceal_dim_strike_cursor_and_row_retention() {
        let mut view = TerminalView::new(3, 20);
        view.process(b"\x1b[?25lA\x1b[2mB\x1b[8mSECRET\x1b[0;9mC");
        let glyphs: Vec<_> = view
            .paint()
            .primitives
            .iter()
            .filter_map(|command| {
                if let DisplayCommand::GlyphRun {
                    shaped,
                    color,
                    clip,
                    ..
                } = command
                {
                    Some((shaped.text.clone(), *color, *clip))
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(
            glyphs
                .iter()
                .map(|(text, _, _)| text.as_str())
                .collect::<String>(),
            "ABC"
        );
        assert_ne!(glyphs[0].1, glyphs[1].1, "dim has a different foreground");
        assert!(glyphs.iter().all(|(_, _, clip)| clip.is_some()));
        assert!(
            view.animation_delay().is_none(),
            "hidden cursor and static text have no animation timer"
        );
        let first_row = view.rows[0].commands.as_ptr();
        let paint = view.paint().primitives.as_ptr();
        assert_eq!(view.paint().primitives.as_ptr(), paint);
        view.process(b"\x1b[2;1HX");
        view.paint();
        assert_eq!(view.rows[0].commands.as_ptr(), first_row);
        view.process(b"\x1b[?25h\x1b[6 q");
        let cursor = view.paint().primitives.last().unwrap();
        assert!(matches!(cursor, DisplayCommand::FillRect { rect, .. } if rect.width == 2.0));
        view.process(b"\r\n1\r\n2\r\n3");
        view.scroll(1);
        assert!(view.paint().primitives.iter().all(|command| !matches!(
            command,
            DisplayCommand::FillRect {
                color: PaintColor::Rgba(220, 232, 245, _),
                ..
            }
        )));
    }

    #[test]
    fn terminal_painters_preserve_blink_rates_and_reverse_screen_changes() {
        let mut view = TerminalView::new(2, 10);
        view.process(b"\x1b[?25l\x1b[5mA\x1b[6;21mB");
        let paint = view.paint_at(620);
        assert!(paint.primitives.iter().any(|command| matches!(command,
            DisplayCommand::GlyphRun { shaped, decoration, .. } if shaped.text == "B" && decoration.style == DecorationStyle::Double)));
        assert!(paint.primitives.iter().all(|command| !matches!(command,
            DisplayCommand::GlyphRun { shaped, .. } if shaped.text == "A")));
        assert!(view.animation_delay().unwrap() <= std::time::Duration::from_millis(100));
        view.process(b"\x1b[?5h");
        assert_eq!(
            view.paint().background,
            Some(theme_color(crate::theme::TEXT))
        );
        view.process(b"\x1b[?5l");
        assert_eq!(view.paint().background, Some(theme_color(crate::theme::BG)));

        let mut tui = ratatui::Terminal::new(ratatui::backend::TestBackend::new(10, 2)).unwrap();
        tui.draw(|frame| crate::terminal_tui::paint(frame, &view.terminal, frame.area(), false))
            .unwrap();
        use ratatui::style::Modifier;
        assert!(
            tui.backend().buffer()[(0, 0)]
                .modifier
                .contains(Modifier::SLOW_BLINK)
        );
        assert!(
            tui.backend().buffer()[(1, 0)]
                .modifier
                .contains(Modifier::RAPID_BLINK | Modifier::UNDERLINED)
        );
    }

    #[test]
    fn terminal_inactive_cursor_has_no_animation_deadline() {
        let mut view = TerminalView::new(2, 20);
        view.set_cursor_active(false);
        assert!(view.paint().primitives.is_empty());
        assert!(view.animation_delay().is_none());
        view.set_cursor_active(true);
        view.process(b"\x1b[6 q");
        assert!(
            view.paint()
                .primitives
                .iter()
                .any(|command| matches!(command, DisplayCommand::FillRect { .. }))
        );
        view.set_cursor_active(false);
        assert!(view.paint().primitives.is_empty());
    }
}
