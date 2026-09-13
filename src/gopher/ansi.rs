//! Desktop Gopher color annotations, never terminal instructions.
//!
//! RFC 1436's menu format does not define colors. This optional presentation
//! extension follows ECMA-48 (5th ed.) §§5.4, 8.3.117 and xterm's SGR indexed/
//! direct-color extensions: https://invisible-island.net/xterm/ctlseqs/ctlseqs.html
//! Only color rendition is interpreted; cursor operations, OSC, and other
//! controls still pass through the common discard-only display sanitizer.

use crate::text_reply::{self, DisplayEvent};
use std::ops::Range;

const MAX_SPANS: usize = 16_384;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Colors {
    pub foreground: Option<[u8; 3]>,
    pub background: Option<[u8; 3]>,
    pub inverse: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Span {
    pub range: Range<usize>,
    pub colors: Colors,
}

#[derive(Clone, Copy, Debug)]
enum Color {
    Indexed(u8),
    Rgb([u8; 3]),
}

impl Color {
    fn rgb(self, bright: bool) -> [u8; 3] {
        match self {
            Self::Rgb(rgb) => rgb,
            Self::Indexed(index) => {
                let (r, g, b) = crate::terminal_view::ansi_color(
                    index + if bright && index < 8 { 8 } else { 0 },
                );
                [r, g, b]
            }
        }
    }
}

#[derive(Default)]
struct Rendition {
    foreground: Option<Color>,
    background: Option<Color>,
    bright: bool,
    inverse: bool,
}

impl Rendition {
    fn colors(&self) -> Colors {
        Colors {
            foreground: self.foreground.map(|c| c.rgb(self.bright)),
            background: self.background.map(|c| c.rgb(false)),
            inverse: self.inverse,
        }
    }

    fn apply(&mut self, parameters: &str) {
        // A CSI with intermediates or private parameters is not SGR. Bound
        // parameter work without truncating a sequence into a different SGR.
        if parameters.len() > 256
            || !parameters
                .bytes()
                .all(|b| b.is_ascii_digit() || matches!(b, b';' | b':'))
        {
            return;
        }
        let mut parts = parameters.split(';');
        while let Some(part) = parts.next() {
            if part.contains(':') {
                let fields: Vec<_> = part.split(':').collect();
                let target = fields[0].parse::<u16>().ok();
                if matches!(target, Some(38 | 48)) {
                    let mode = fields.get(1).and_then(|p| p.parse::<u16>().ok());
                    let color = match (mode, &fields[2..]) {
                        (Some(5), [index]) => number(index).map(Color::Indexed),
                        (Some(2), [r, g, b] | [_, r, g, b]) => rgb(r, g, b),
                        _ => None,
                    };
                    self.set_color(target == Some(38), color);
                }
                continue;
            }
            // ECMA-48 §5.4.2: an omitted parameter has the default value (0).
            let value = if part.is_empty() {
                Some(0)
            } else {
                part.parse::<u16>().ok()
            };
            match value {
                Some(0) => *self = Self::default(),
                Some(1) => self.bright = true,
                Some(22) => self.bright = false,
                Some(7) => self.inverse = true,
                Some(27) => self.inverse = false,
                Some(30..=37) => self.foreground = Some(Color::Indexed(value.unwrap() as u8 - 30)),
                Some(40..=47) => self.background = Some(Color::Indexed(value.unwrap() as u8 - 40)),
                Some(90..=97) => {
                    self.foreground = Some(Color::Indexed(value.unwrap() as u8 - 90 + 8))
                }
                Some(100..=107) => {
                    self.background = Some(Color::Indexed(value.unwrap() as u8 - 100 + 8))
                }
                Some(39) => self.foreground = None,
                Some(49) => self.background = None,
                Some(38 | 48 | 58) => {
                    // Consume a whole extended color, even when unsupported
                    // (58 = underline color) or out of range. RGB components
                    // must never accidentally become reset or inverse codes.
                    let color = match parts.next().and_then(|p| p.parse::<u16>().ok()) {
                        Some(5) => parts.next().and_then(number).map(Color::Indexed),
                        Some(2) => {
                            let r = parts.next();
                            let g = parts.next();
                            let b = parts.next();
                            r.zip(g).zip(b).and_then(|((r, g), b)| rgb(r, g, b))
                        }
                        _ => break,
                    };
                    if value != Some(58) {
                        self.set_color(value == Some(38), color);
                    }
                }
                _ => {}
            }
        }
    }

    fn set_color(&mut self, foreground: bool, color: Option<Color>) {
        if let Some(color) = color {
            if foreground {
                self.foreground = Some(color);
            } else {
                self.background = Some(color);
            }
        }
    }
}

fn number(part: &str) -> Option<u8> {
    if part.is_empty() {
        None
    } else {
        part.parse().ok()
    }
}

fn rgb(r: &str, g: &str, b: &str) -> Option<Color> {
    Some(Color::Rgb([number(r)?, number(g)?, number(b)?]))
}

/// One parser per displayed response: rendition carries across physical
/// lines, but menu selectors/hosts and client-generated notices are excluded.
pub(crate) struct Parser {
    rendition: Rendition,
    remaining: usize,
}

impl Default for Parser {
    fn default() -> Self {
        Self {
            rendition: Rendition::default(),
            remaining: MAX_SPANS,
        }
    }
}

impl Parser {
    pub fn line(&mut self, raw: &[u8]) -> (String, bool, Vec<Span>) {
        if self.remaining == 0 {
            let (text, clipped) = text_reply::display_text(raw, false);
            return (text, clipped, Vec::new());
        }
        let mut changes = vec![(0, self.rendition.colors())];
        let mut spans: Vec<Span> = Vec::new();
        let mut change = 0;
        let (text, clipped) = text_reply::display_text_observed(raw, false, |event| match event {
            DisplayEvent::Sgr { offset, parameters } => {
                self.rendition.apply(parameters);
                let colors = self.rendition.colors();
                if changes.last().is_some_and(|&(at, _)| at == offset) {
                    changes.last_mut().unwrap().1 = colors;
                } else if changes
                    .last()
                    .is_none_or(|&(_, previous)| previous != colors)
                {
                    changes.push((offset, colors));
                }
            }
            DisplayEvent::Grapheme { source, output } => {
                while change + 1 < changes.len() && changes[change + 1].0 <= source {
                    change += 1;
                }
                let colors = changes[change].1;
                if colors == Colors::default() || output.is_empty() {
                    return;
                }
                if let Some(previous) = spans.last_mut()
                    && previous.range.end == output.start
                    && previous.colors == colors
                {
                    previous.range.end = output.end;
                } else if self.remaining > 0 {
                    spans.push(Span {
                        range: output,
                        colors,
                    });
                    self.remaining -= 1;
                }
            }
        });
        (text, clipped, spans)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(spans: &[Span], byte: usize) -> Colors {
        spans
            .iter()
            .find(|span| span.range.contains(&byte))
            .map_or(Colors::default(), |s| s.colors)
    }

    #[test]
    fn gopher_ansi_palettes_truecolor_and_resets() {
        let mut parser = Parser::default();
        let (text, clipped, spans) = parser.line(b"\x1b[31;44mA\x1b[1mB\x1b[22;39mC\x1b[49mD\x1b[38;5;196;48;5;232mE\x1b[38;2;27;75;105mF\x1b[mG");
        assert_eq!(text, "ABCDEFG");
        assert!(!clipped);
        assert_eq!(at(&spans, 0).foreground, Some([170, 0, 0]));
        assert_eq!(at(&spans, 1).foreground, Some([255, 85, 85]));
        assert_eq!(
            at(&spans, 2),
            Colors {
                foreground: None,
                background: Some([0, 0, 170]),
                inverse: false
            }
        );
        assert_eq!(at(&spans, 3), Colors::default());
        assert_eq!(
            at(&spans, 4),
            Colors {
                foreground: Some([255, 0, 0]),
                background: Some([8, 8, 8]),
                inverse: false
            }
        );
        assert_eq!(at(&spans, 5).foreground, Some([27, 75, 105]));
        assert_eq!(at(&spans, 6), Colors::default());
    }

    #[test]
    fn gopher_ansi_colon_colors_leading_zeroes_and_independent_defaults() {
        let (_, _, spans) = Parser::default()
            .line(b"\x1b[038:02::001:002:003;48:5:021mA\x1b[39mB\x1b[;93;107mC\x1b[7mD\x1b[27mE");
        assert_eq!(
            at(&spans, 0),
            Colors {
                foreground: Some([1, 2, 3]),
                background: Some([0, 0, 255]),
                inverse: false
            }
        );
        assert_eq!(at(&spans, 1).foreground, None);
        assert_eq!(at(&spans, 1).background, Some([0, 0, 255]));
        assert_eq!(at(&spans, 2).foreground, Some([255, 255, 85]));
        assert_eq!(at(&spans, 2).background, Some([255, 255, 255]));
        assert!(at(&spans, 3).inverse);
        assert!(!at(&spans, 4).inverse);
    }

    #[test]
    fn gopher_ansi_invalid_and_non_color_controls_remain_inert() {
        let raw = b"\x1b[31mA\x1b[38;2;999;0;7mB\x1b[48:2:0:300:0:0mC\x1b[?0mD\x1b[0 mE\x1b[2JF\x1b]0;title\x07G\x1bP\x1b[32mhidden\x1b\\H\x1b[58;2;0;0;0mI\x1b[38;2;0;0";
        let (text, clipped, spans) = Parser::default().line(raw);
        assert_eq!(text, "ABCDEFGHI");
        assert!(!clipped);
        assert_eq!(
            text_reply::display_text(raw, false),
            (text.clone(), clipped)
        );
        for byte in 0..text.len() {
            assert_eq!(
                at(&spans, byte),
                Colors {
                    foreground: Some([170, 0, 0]),
                    ..Colors::default()
                }
            );
        }
    }

    #[test]
    fn gopher_ansi_tabs_graphemes_and_line_continuation() {
        let mut parser = Parser::default();
        let raw = "\x1b[31me\x1b[32m\u{301}\t界\r\nZ".as_bytes();
        let (text, clipped, spans) = parser.line(raw);
        assert_eq!(text, "e\u{301}       界\nZ");
        assert_eq!(
            text_reply::display_text(raw, false),
            (text.clone(), clipped)
        );
        assert_eq!(at(&spans, 0).foreground, Some([170, 0, 0]));
        assert_eq!(at(&spans, 3).foreground, Some([0, 170, 0]));
        assert_eq!(at(&spans, text.len() - 1).foreground, Some([0, 170, 0]));
        let (_, _, next) = parser.line(b"next\x1b[0m");
        assert_eq!(at(&next, 0).foreground, Some([0, 170, 0]));
        assert!(parser.line(b"default").2.is_empty());
    }

    #[test]
    fn gopher_ansi_color_budget_preserves_the_remaining_text() {
        let mut parser = Parser {
            remaining: 2,
            ..Parser::default()
        };
        let raw = b"\x1b[31mR\x1b[32mG\x1b[33mY";
        let (text, _, spans) = parser.line(raw);
        assert_eq!(text, "RGY");
        assert_eq!(spans.len(), 2);
        let (text, _, spans) = parser.line(raw);
        assert_eq!(text, "RGY");
        assert!(spans.is_empty());
    }

    #[test]
    fn gopher_ansi_annotations_are_desktop_only_and_exclude_selectors() {
        let url = super::super::GopherUrl::parse("gopher://example.test").unwrap();
        let raw = b"i\x1b[38;2;27;75;105m\xe2\xa0\x80\x1b[0m\tfake\thost\t70\r\n1plain\t/\x1b[32m\texample.test\t70\r\niend\tfake\thost\t70\r\n.\r\n".to_vec();
        let terminal = super::super::render(&url, raw.clone().into(), 80);
        let desktop = super::super::render_desktop(&url, raw.into());
        assert_eq!(terminal.lines.len(), desktop.lines.len());
        for (t, d) in terminal.lines.iter().zip(&desktop.lines) {
            assert_eq!(t.text, d.text);
            assert_eq!(t.link, d.link);
        }
        assert!(terminal.gopher.unwrap().colors.is_empty());
        let colors = desktop.gopher.unwrap().colors;
        assert_eq!(at(&colors[0], 0).foreground, Some([27, 75, 105]));
        assert!(colors[1].is_empty());
        assert!(colors[2].is_empty());
    }
}
