//! The terminal/session engine shared by the TUI and desktop adapters.
//!
//! Frontends own focus and presentation. This module alone interprets terminal
//! bytes, answers devices queries, and decides how remote input is encoded.
//! RFC 854/856/857/1184 govern the Telnet input policy; ECMA-48 and xterm's
//! published control sequences govern the VT and keyboard contract.

use std::collections::HashSet;

use crate::core::{Key, KeyInput, KeyState, Modifiers};
use crate::telnet::{Event, op_command, op_option};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Encoding {
    #[default]
    Utf8,
    Cp437,
}

impl Encoding {
    pub fn encode(self, text: &str) -> Result<Vec<u8>, String> {
        match self {
            Self::Utf8 => Ok(text.as_bytes().to_vec()),
            Self::Cp437 => crate::cp437::encode(text).map_err(|character| {
                format!("CP437 cannot represent {character:?}; use COMMAND: set encoding utf8")
            }),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputMode {
    Character,
    Line,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseAction {
    Press(u8),
    Release(u8),
    Motion(Option<u8>),
    Wheel(bool),
}

/// Frontends apply these actions to their native line editors. The negotiated
/// policy is shared; an untrapped control remains in the line until Enter.
#[derive(Debug, PartialEq, Eq)]
pub enum LineControl {
    EraseCharacter,
    EraseLine,
    EraseWord,
    Insert(String),
    Send(Vec<u8>),
}

#[derive(Default)]
pub struct Callbacks {
    pub bells: usize,
    pub title: String,
    replies: Vec<Vec<u8>>,
}

impl vt100::Callbacks for Callbacks {
    fn audible_bell(&mut self, _: &mut vt100::Screen) {
        self.bells = self.bells.saturating_add(1);
    }
    fn visual_bell(&mut self, screen: &mut vt100::Screen) {
        self.audible_bell(screen);
    }

    fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        self.title = String::from_utf8_lossy(title)
            .chars()
            .filter(|c| !c.is_control())
            .take(256)
            .collect();
    }

    fn unhandled_escape(&mut self, _: &mut vt100::Screen, i1: Option<u8>, _: Option<u8>, byte: u8) {
        if i1.is_none() && byte == b'Z' {
            self.replies.push(b"\x1b[?1;2c".to_vec());
        }
    }

    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        i1: Option<u8>,
        i2: Option<u8>,
        params: &[&[u16]],
        final_byte: char,
    ) {
        if i2.is_some() || params.iter().any(|p| p.len() != 1) {
            return;
        }
        let first = params.first().and_then(|p| p.first()).copied().unwrap_or(0);
        match (i1, final_byte, first, params.len()) {
            (None | Some(b'?'), 'n', 6, 1) => {
                // Called at dispatch, before any later text in the SAME read.
                let (row, col) = screen.reported_cursor_position();
                self.replies.push(
                    format!(
                        "\x1b[{}{};{}R",
                        if i1.is_some() { "?" } else { "" },
                        row + 1,
                        col + 1
                    )
                    .into_bytes(),
                );
            }
            (None, 'n', 5, 1) => self.replies.push(b"\x1b[0n".to_vec()),
            (None, 'c', 0, 0 | 1) => self.replies.push(b"\x1b[?1;2c".to_vec()),
            (None, 't', 18, 1) => {
                let (rows, cols) = screen.size();
                self.replies
                    .push(format!("\x1b[8;{rows};{cols}t").into_bytes());
            }
            _ => {}
        }
    }
}

pub struct Terminal {
    parser: vt100::Parser<Callbacks>,
    pub encoding: Encoding,
    pub crlf: bool,
    pub input_mode: Option<InputMode>,
    pub remote_options: HashSet<u8>,
    pub local_options: HashSet<u8>,
    pub linemode_active: bool,
    pub linemode_edit: bool,
    pub line_mode: u8,
    pub disabled_slc: u32,
    pub revision: u64,
    pub report: Option<String>,
    pub search: String,
    matches: Vec<Vec<(usize, u16, u16)>>,
    match_index: usize,
    search_dirty: bool,
}

pub const SCROLLBACK_LINES: usize = 10_000;
pub const MAX_COLS: u16 = 1024;
pub const MAX_ROWS: u16 = 512;

pub fn bounded_size(cols: u16, rows: u16) -> (u16, u16) {
    (cols.clamp(1, MAX_COLS), rows.clamp(1, MAX_ROWS))
}

impl Terminal {
    pub fn encode_mouse(
        &self,
        row: u16,
        col: u16,
        action: MouseAction,
        modifiers: Modifiers,
    ) -> Option<Vec<u8>> {
        use vt100::{MouseProtocolEncoding as Encoding, MouseProtocolMode as Mode};
        let screen = self.screen();
        let mode = screen.mouse_protocol_mode();
        if mode == Mode::None || screen.scrollback() > 0 || modifiers.shift {
            return None;
        }
        let (button, released) = match action {
            MouseAction::Press(button) if button < 3 => (button, false),
            MouseAction::Release(button) if mode != Mode::Press && button < 3 => (button, true),
            MouseAction::Motion(button)
                if mode == Mode::AnyMotion || (mode == Mode::ButtonMotion && button.is_some()) =>
            {
                (32 + button.unwrap_or(3), false)
            }
            MouseAction::Wheel(up) if mode != Mode::Press => (if up { 64 } else { 65 }, false),
            _ => return None,
        };
        let flags = if mode == Mode::Press {
            0
        } else {
            8 * u8::from(modifiers.alt || modifiers.meta) + 16 * u8::from(modifiers.control)
        };
        let code = button + flags;
        let (x, y) = (u32::from(col) + 1, u32::from(row) + 1);
        match screen.mouse_protocol_encoding() {
            Encoding::Sgr => Some(
                format!("\x1b[<{code};{x};{y}{}", if released { 'm' } else { 'M' }).into_bytes(),
            ),
            encoding => {
                let code = if released { 3 + flags } else { code };
                let mut bytes = b"\x1b[M".to_vec();
                if encoding == Encoding::Default {
                    if x > 223 || y > 223 {
                        return None;
                    }
                    bytes.extend([code + 32, (x + 32) as u8, (y + 32) as u8]);
                } else {
                    if x > 2015 || y > 2015 {
                        return None;
                    }
                    for value in [u32::from(code) + 32, x + 32, y + 32] {
                        let mut encoded = [0; 4];
                        bytes.extend_from_slice(
                            char::from_u32(value)?.encode_utf8(&mut encoded).as_bytes(),
                        );
                    }
                }
                Some(bytes)
            }
        }
    }

    pub fn new(rows: u16, cols: u16) -> Self {
        let (cols, rows) = bounded_size(cols, rows);
        Self {
            parser: vt100::Parser::new_with_callbacks(
                rows,
                cols,
                SCROLLBACK_LINES,
                Callbacks::default(),
            ),
            encoding: Encoding::Utf8,
            crlf: false,
            input_mode: None,
            remote_options: HashSet::new(),
            local_options: HashSet::new(),
            linemode_active: false,
            linemode_edit: false,
            line_mode: 0,
            disabled_slc: 0,
            revision: 0,
            report: None,
            search: String::new(),
            matches: Vec::new(),
            match_index: 0,
            search_dirty: false,
        }
    }

    pub fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }
    pub fn screen_mut(&mut self) -> &mut vt100::Screen {
        self.revision = self.revision.wrapping_add(1);
        self.parser.screen_mut()
    }
    pub fn callbacks(&self) -> &Callbacks {
        self.parser.callbacks()
    }

    pub fn resize(&mut self, cols: u16, rows: u16) -> (u16, u16) {
        let (cols, rows) = bounded_size(cols, rows);
        if self.screen().size() != (rows, cols) {
            self.screen_mut().set_size(rows, cols);
            self.search_dirty = true;
        }
        (cols, rows)
    }

    pub fn process(&mut self, data: &[u8]) -> Vec<Vec<u8>> {
        if data.is_empty() {
            return Vec::new();
        }
        self.revision = self.revision.wrapping_add(1);
        self.search_dirty = true;
        match self.encoding {
            Encoding::Utf8 => self.parser.process(data),
            Encoding::Cp437 => self.parser.process(&crate::cp437::decode(data)),
        }
        std::mem::take(&mut self.parser.callbacks_mut().replies)
    }

    pub fn observe(&mut self, event: &Event) {
        match *event {
            Event::Negotiation { command, option } => match command {
                op_command::WILL => {
                    self.remote_options.insert(option);
                }
                op_command::WONT => {
                    self.remote_options.remove(&option);
                }
                op_command::DO => {
                    self.local_options.insert(option);
                }
                op_command::DONT => {
                    self.local_options.remove(&option);
                }
                _ => {}
            },
            Event::LineMode { active, mode } => {
                self.linemode_active = active;
                self.line_mode = mode;
                self.linemode_edit = mode & 1 != 0;
            }
            Event::Slc { disabled } => self.disabled_slc = disabled,
            Event::Closed(_) => self.reset_negotiation(),
            _ => {}
        }
    }

    pub fn reset_negotiation(&mut self) {
        self.remote_options.clear();
        self.local_options.clear();
        self.linemode_active = false;
        self.linemode_edit = false;
        self.line_mode = 0;
        self.disabled_slc = 0;
    }

    pub fn remote_echo(&self) -> bool {
        self.remote_options.contains(&op_option::ECHO)
    }

    pub fn char_mode(&self, connected: bool) -> bool {
        connected
            && match self.input_mode {
                Some(InputMode::Character) => true,
                Some(InputMode::Line) => false,
                // RFC 1184 §2.2: ECHO never overrides the negotiated EDIT bit.
                None if self.linemode_active => !self.linemode_edit,
                None => self.remote_echo(),
            }
    }

    pub fn encode_line(&self, text: &str) -> Result<Vec<u8>, String> {
        use unicode_segmentation::UnicodeSegmentation as _;
        use unicode_width::UnicodeWidthStr as _;
        let expanded;
        let text = if self.linemode_active && self.line_mode & 8 != 0 {
            let mut column = self.screen().cursor_position().1;
            let cols = self.screen().size().1;
            expanded = text
                .graphemes(true)
                .fold(String::new(), |mut output, grapheme| {
                    if grapheme == "\t" {
                        let stop = self.screen().tab_stop_after(column);
                        output.extend(std::iter::repeat_n(
                            ' ',
                            usize::from(stop.saturating_sub(column)),
                        ));
                        column = stop;
                    } else {
                        if column >= cols {
                            column = 0;
                        }
                        output.push_str(grapheme);
                        column = column.saturating_add(grapheme.width() as u16);
                    }
                    output
                });
            expanded.as_str()
        } else {
            text
        };
        let mut bytes = self.encoding.encode(text)?;
        // RFC 1123 §3.2.7: BINARY has no NVT end-of-line convention.
        // Our line editor completes a line with LF; only NVT expands it to
        // CR LF (RFC 1184 §5.2). In binary mode an added CR is literal data.
        if !self.local_options.contains(&op_option::BINARY) {
            bytes.push(b'\r');
        }
        bytes.push(b'\n');
        Ok(bytes)
    }

    pub fn encode_paste(&self, text: &str) -> Result<Vec<u8>, String> {
        // xterm's default paste newline conversion, followed by RFC 854 NVT
        // encoding in the transport. One command preserves marker atomicity.
        let normalized = text.replace("\r\n", "\r").replace('\n', "\r");
        let mut bytes = self.encoding.encode(&normalized)?;
        if self.screen().bracketed_paste() {
            let mut wrapped = b"\x1b[200~".to_vec();
            wrapped.append(&mut bytes);
            wrapped.extend_from_slice(b"\x1b[201~");
            bytes = wrapped;
        }
        Ok(bytes)
    }

    /// Prepare a line-mode paste without mutating the editor. The caller only
    /// commits the returned draft after the complete wire batch is accepted.
    pub fn line_paste(
        &self,
        draft: &str,
        selection: std::ops::Range<usize>,
        text: &str,
    ) -> Result<(Vec<u8>, String), String> {
        let normalized: String = text
            .replace("\r\n", "\n")
            .replace('\r', "\n")
            .chars()
            .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
            .collect();
        let mut combined = draft.to_owned();
        if selection.start > selection.end
            || !combined.is_char_boundary(selection.start)
            || !combined.is_char_boundary(selection.end)
        {
            return Err("Invalid terminal input selection".into());
        }
        combined.replace_range(selection, &normalized);
        let Some(last) = combined.rfind('\n') else {
            return Ok((Vec::new(), combined));
        };
        let mut bytes = Vec::new();
        for line in combined[..last].split('\n') {
            bytes.extend(self.encode_line(line)?);
        }
        Ok((bytes, combined[last + 1..].to_owned()))
    }

    /// RFC 1184 TRAPSIG. The fixed SLC values are exported by the transport.
    pub fn signal(&self, bytes: &[u8]) -> Option<u8> {
        if !self.linemode_active || self.line_mode & 2 == 0 {
            return None;
        }
        let (function, command) = match bytes {
            [3] => (3, op_command::IP),
            [28] => (7, op_command::ABORT),
            [26] => (9, op_command::SUSP),
            [4] => (8, op_command::EOF),
            _ => return None,
        };
        (self.disabled_slc & (1 << function) == 0).then_some(command)
    }

    pub fn slc_enabled(&self, function: u8) -> bool {
        !self.linemode_active || self.disabled_slc & (1u32 << function) == 0
    }

    pub fn line_control(&self, bytes: Vec<u8>) -> LineControl {
        match bytes.as_slice() {
            [127] if self.slc_enabled(10) => LineControl::EraseCharacter,
            [21] if self.slc_enabled(11) => LineControl::EraseLine,
            [23] if self.slc_enabled(12) => LineControl::EraseWord,
            _ if self.signal(&bytes).is_some() => LineControl::Send(bytes),
            _ if self.linemode_active && self.linemode_edit => {
                // This path handles encoded ASCII control keys, not text or
                // function-key escape strings. RFC 1184 §2.2 EDIT/TRAPSIG.
                LineControl::Insert(String::from_utf8_lossy(&bytes).into_owned())
            }
            [9] => LineControl::Insert("\t".into()),
            _ => LineControl::Send(bytes),
        }
    }

    pub fn previous_grapheme(text: &str, cursor: usize) -> usize {
        use unicode_segmentation::UnicodeSegmentation as _;
        text.get(..cursor)
            .unwrap_or_default()
            .grapheme_indices(true)
            .next_back()
            .map_or(0, |(byte, _)| byte)
    }

    pub fn encode_key(&self, input: &KeyInput) -> Result<Option<Vec<u8>>, String> {
        if input.state != KeyState::Pressed || input.composing {
            return Ok(None);
        }
        let modifiers = input.modifiers;
        let parameter = 1
            + u8::from(modifiers.shift)
            + 2 * u8::from(modifiers.alt)
            + 4 * u8::from(modifiers.control)
            + 8 * u8::from(modifiers.meta);
        if input.location == 3 && self.screen().application_keypad() {
            let final_byte = match input.code.as_str() {
                "Numpad0" => Some('p'),
                "Numpad1" => Some('q'),
                "Numpad2" => Some('r'),
                "Numpad3" => Some('s'),
                "Numpad4" => Some('t'),
                "Numpad5" => Some('u'),
                "Numpad6" => Some('v'),
                "Numpad7" => Some('w'),
                "Numpad8" => Some('x'),
                "Numpad9" => Some('y'),
                "NumpadDecimal" => Some('n'),
                "NumpadDivide" => Some('o'),
                "NumpadMultiply" => Some('j'),
                "NumpadSubtract" => Some('m'),
                "NumpadAdd" => Some('k'),
                "NumpadEnter" => Some('M'),
                "NumpadEqual" => Some('X'),
                "NumpadComma" => Some('l'),
                _ => None,
            };
            if let Some(final_byte) = final_byte {
                return Ok(Some(format!("\x1bO{final_byte}").into_bytes()));
            }
        }
        let cursor = |final_byte| {
            if parameter > 1 {
                format!("\x1b[1;{parameter}{final_byte}").into_bytes()
            } else {
                format!(
                    "\x1b{}{final_byte}",
                    if self.screen().application_cursor() {
                        'O'
                    } else {
                        '['
                    }
                )
                .into_bytes()
            }
        };
        let tilde = |number| {
            if parameter > 1 {
                format!("\x1b[{number};{parameter}~").into_bytes()
            } else {
                format!("\x1b[{number}~").into_bytes()
            }
        };
        let mut bytes = match &input.key {
            Key::Character(text) if modifiers.control => {
                let mut chars = text.chars();
                let Some(character) = chars.next() else {
                    return Ok(None);
                };
                if chars.next().is_some() || !character.is_ascii() {
                    return Ok(None);
                }
                let upper = character.to_ascii_uppercase();
                match upper {
                    '@'..='_' => vec![upper as u8 & 0x1f],
                    ' ' | '2' => vec![0],
                    '?' | '8' => vec![127],
                    '3'..='7' => vec![upper as u8 - b'3' + 27],
                    _ => return Ok(None),
                }
            }
            Key::Character(text) => self.encoding.encode(text)?,
            Key::Enter if self.crlf => b"\r\n".to_vec(),
            Key::Enter => vec![b'\r'],
            Key::Backspace => vec![127],
            Key::Tab if modifiers.shift => b"\x1b[Z".to_vec(),
            Key::Tab if self.linemode_active && self.line_mode & 8 != 0 => {
                let col = self.screen().cursor_position().1;
                vec![b' '; usize::from(self.screen().tab_stop_after(col).saturating_sub(col))]
            }
            Key::Tab => vec![b'\t'],
            Key::Escape => vec![27],
            Key::ArrowUp => cursor('A'),
            Key::ArrowDown => cursor('B'),
            Key::ArrowRight => cursor('C'),
            Key::ArrowLeft => cursor('D'),
            Key::Home => cursor('H'),
            Key::End => cursor('F'),
            Key::Delete => tilde(3),
            Key::PageUp => tilde(5),
            Key::PageDown => tilde(6),
            Key::Other(name) if name == "Insert" => tilde(2),
            Key::Other(name) => {
                let Some(number) = name.strip_prefix('F').and_then(|n| n.parse::<u8>().ok()) else {
                    return Ok(None);
                };
                match number {
                    1..=4 => {
                        let final_byte = char::from(b'P' + number - 1);
                        if parameter == 1 {
                            format!("\x1bO{final_byte}").into_bytes()
                        } else {
                            format!("\x1b[1;{parameter}{final_byte}").into_bytes()
                        }
                    }
                    5..=20 => tilde(
                        [
                            15, 17, 18, 19, 20, 21, 23, 24, 25, 26, 28, 29, 31, 32, 33, 34,
                        ][usize::from(number - 5)],
                    ),
                    _ => return Ok(None),
                }
            }
        };
        if modifiers.alt && matches!(input.key, Key::Character(_)) {
            bytes.insert(0, 27);
        }
        Ok(Some(bytes))
    }

    /// Echo only accepted user input, never protocol/device replies. ECHO is
    /// independent of EDIT, including when line input must remain hidden.
    pub fn echo_input(&mut self, bytes: &[u8]) {
        if self.remote_echo() {
            return;
        }
        let bytes = bytes
            .strip_prefix(b"\x1b[200~")
            .and_then(|bytes| bytes.strip_suffix(b"\x1b[201~"))
            .unwrap_or(bytes);
        let mut echo = Vec::with_capacity(bytes.len());
        let mut previous_cr = false;
        for &byte in bytes {
            match byte {
                b'\r' => echo.extend_from_slice(b"\r\n"),
                b'\n' | 0 if previous_cr => {}
                // A locally edited line still echoes a complete newline
                // when BINARY sends its terminating LF without an NVT CR.
                b'\n' if !self.char_mode(true) => echo.extend_from_slice(b"\r\n"),
                8 | 127 => echo.extend_from_slice(b"\x08 \x08"),
                0..=31 if byte != b'\t' && byte != b'\n' && self.line_mode & 16 == 0 => {
                    echo.extend([b'^', byte + 64]);
                }
                _ => echo.push(byte),
            }
            previous_cr = byte == b'\r';
        }
        self.process(&echo);
    }

    pub fn scroll(&mut self, lines: i32) {
        let current = self.screen().scrollback();
        let next = if lines > 0 {
            current.saturating_add(lines as usize)
        } else {
            current.saturating_sub(lines.unsigned_abs() as usize)
        };
        self.screen_mut().set_scrollback(next);
    }

    pub fn scroll_key(&mut self, input: &KeyInput, connected: bool) -> bool {
        if input.state != KeyState::Pressed
            || input.modifiers.control
            || input.modifiers.alt
            || input.modifiers.meta
            || (connected && !input.modifiers.shift)
        {
            return false;
        }
        let page = i32::from(self.screen().size().0.saturating_sub(1).max(1));
        let lines = match input.key {
            Key::PageUp => page,
            Key::PageDown => -page,
            Key::Home => i32::MAX,
            Key::End => i32::MIN,
            _ => return false,
        };
        self.scroll(lines);
        true
    }

    pub fn visible_text(&self) -> String {
        let screen = self.screen();
        let (rows, cols) = screen.size();
        let mut output = String::new();
        for row in 0..rows {
            let mut line = String::new();
            for col in 0..cols {
                let Some(cell) = screen.cell(row, col) else {
                    continue;
                };
                if cell.is_wide_continuation() {
                    continue;
                }
                if cell.conceal() || !cell.has_contents() {
                    line.push(' ');
                    if cell.is_wide() {
                        line.push(' ');
                    }
                } else {
                    line.push_str(cell.contents());
                }
            }
            if !screen.row_wrapped(row) {
                line.truncate(line.trim_end().len());
            }
            output.push_str(&line);
            if !screen.row_wrapped(row) {
                output.push('\n');
            }
        }
        output.truncate(output.trim_end_matches('\n').len());
        output
    }

    pub fn transcript(&self) -> String {
        let mut result = String::new();
        for (line, wrapped) in self.screen().transcript_rows() {
            result.push_str(&line);
            if !wrapped {
                result.push('\n');
            }
        }
        result.truncate(result.trim_end_matches('\n').len());
        result
    }

    pub fn find(&mut self, query: &str) -> String {
        use unicode_width::UnicodeWidthStr as _;
        self.search = query.to_owned();
        self.matches.clear();
        self.match_index = 0;
        self.search_dirty = false;
        self.revision = self.revision.wrapping_add(1);
        if query.is_empty() {
            return "Terminal search cleared".into();
        }
        let rows = self.screen().transcript_rows();
        let mut logical = String::new();
        let mut boundaries = Vec::new();
        for (row, (text, wrapped)) in rows.iter().enumerate() {
            boundaries.push((row, logical.len(), logical.len() + text.len()));
            logical.push_str(text);
            if *wrapped && row + 1 < rows.len() {
                continue;
            }
            for (byte, matched) in logical.match_indices(query) {
                let end = byte + matched.len();
                let mut segments = Vec::new();
                for &(row, start, stop) in &boundaries {
                    if byte < stop && end > start {
                        segments.push((
                            row,
                            logical[start..byte.max(start)].width() as u16,
                            logical[start..end.min(stop)].width() as u16,
                        ));
                    }
                }
                if !segments.is_empty() {
                    self.matches.push(segments);
                }
                if self.matches.len() == 10_000 {
                    break;
                }
            }
            if self.matches.len() == 10_000 {
                break;
            }
            logical.clear();
            boundaries.clear();
        }
        self.reveal_match()
    }

    pub fn find_next(&mut self, backwards: bool) -> String {
        // Rebuild on explicit search navigation, never on the network/paint
        // hot path. Changed cells must not retain stale highlights.
        if self.search_dirty {
            let index = self.match_index;
            self.find(&self.search.clone());
            self.match_index = index.min(self.matches.len().saturating_sub(1));
        }
        if self.matches.is_empty() {
            return "No terminal search matches".into();
        }
        self.match_index = if backwards {
            (self.match_index + self.matches.len() - 1) % self.matches.len()
        } else {
            (self.match_index + 1) % self.matches.len()
        };
        self.reveal_match()
    }

    fn reveal_match(&mut self) -> String {
        let Some(&(row, _, _)) = self
            .matches
            .get(self.match_index)
            .and_then(|segments| segments.first())
        else {
            return "No terminal search matches".into();
        };
        let top = row.saturating_sub(usize::from(self.screen().size().0) / 2);
        let offset = self.screen().history_len().saturating_sub(top);
        self.screen_mut().set_scrollback(offset);
        format!(
            "Match {}/{} · COMMAND: find-next / find-prev / live",
            self.match_index + 1,
            self.matches.len()
        )
    }

    pub fn highlighted(&self, row: u16, col: u16) -> bool {
        let absolute = self
            .screen()
            .history_len()
            .saturating_sub(self.screen().scrollback())
            + usize::from(row);
        !self.search_dirty
            && self.matches.get(self.match_index).is_some_and(|segments| {
                segments
                    .iter()
                    .any(|&(line, start, end)| line == absolute && (start..end).contains(&col))
            })
    }
}

pub enum Control {
    Message(String),
    Send(crate::telnet::Command, String),
    Copy(String),
    Reconnect,
    Close,
}

impl Terminal {
    /// Shared COMMAND vocabulary; both frontends apply the same settings and
    /// return the same responses. Clipboard/navigation remain frontend actions.
    pub fn control(&mut self, line: &str, connected: bool) -> Option<Result<Control, String>> {
        let mut parts = line.split_whitespace();
        let verb = parts.next()?;
        let rest = line
            .split_once(char::is_whitespace)
            .map_or("", |(_, text)| text.trim());
        let message = |text: &str| Ok(Control::Message(text.into()));
        Some(match verb {
            "mode" | "m" => match parts.next() {
                Some("char" | "character") => {
                    self.input_mode = Some(InputMode::Character);
                    if self.linemode_active {
                        self.linemode_edit = false;
                        self.line_mode &= !1;
                    }
                    Ok(Control::Send(
                        crate::telnet::Command::LineModeRequest { edit: false },
                        "Input mode: character".into(),
                    ))
                }
                Some("line") => {
                    self.input_mode = Some(InputMode::Line);
                    if self.linemode_active {
                        self.linemode_edit = true;
                        self.line_mode |= 1;
                    }
                    Ok(Control::Send(
                        crate::telnet::Command::LineModeRequest { edit: true },
                        "Input mode: line".into(),
                    ))
                }
                Some("auto") => {
                    self.input_mode = None;
                    message("Input mode follows Telnet negotiation")
                }
                _ => Err("usage: mode character|line|auto".into()),
            },
            "toggle" | "t" if rest == "crlf" => {
                self.crlf = !self.crlf;
                message(if self.crlf {
                    "Enter sends CR LF"
                } else {
                    "Enter sends CR (CR NUL in NVT mode)"
                })
            }
            "set" if parts.next() == Some("encoding") => match parts.next() {
                Some("utf8" | "utf-8") => {
                    self.encoding = Encoding::Utf8;
                    message("Terminal encoding: UTF-8")
                }
                Some("cp437") => {
                    self.encoding = Encoding::Cp437;
                    message("Terminal encoding: CP437")
                }
                _ => Err("usage: set encoding utf8|cp437".into()),
            },
            "status" | "st" | "help" | "?" => {
                let (rows, cols) = self.screen().size();
                self.report = Some(format!(
                    "TELNET\n{} · {} columns × {} rows\nInput: {}{}\nRemote echo: {}\nEncoding: {:?}\nEnter: {}\nScrollback: {} lines retained\n\nmode character|line|auto\nset encoding utf8|cp437 · toggle crlf\nfind <text> · find-next · find-prev · live\ncopy screen|scrollback · clear scrollback\nsend ip|ao|ayt|brk|eof|susp|escape\nreconnect · close\n\nCtrl-] returns to the session",
                    if connected {
                        "Connected"
                    } else {
                        "Disconnected"
                    },
                    cols,
                    rows,
                    if self.char_mode(connected) {
                        "character"
                    } else {
                        "line"
                    },
                    if self.input_mode.is_some() {
                        " (forced)"
                    } else {
                        ""
                    },
                    if self.remote_echo() { "yes" } else { "no" },
                    self.encoding,
                    if self.crlf {
                        "CR LF"
                    } else {
                        "CR / NVT CR NUL"
                    },
                    self.screen().history_len()
                ));
                message("Telnet status · Ctrl-] returns to the session")
            }
            "find" => {
                let message = self.find(rest);
                Ok(Control::Message(message))
            }
            "find-next" => {
                let message = self.find_next(false);
                Ok(Control::Message(message))
            }
            "find-prev" => {
                let message = self.find_next(true);
                Ok(Control::Message(message))
            }
            "live" => {
                self.screen_mut().set_scrollback(0);
                self.matches.clear();
                message("Following live terminal output")
            }
            "scroll" => match rest {
                "top" => {
                    self.screen_mut().set_scrollback(usize::MAX);
                    message("Oldest retained output")
                }
                "bottom" => {
                    self.screen_mut().set_scrollback(0);
                    message("Following live terminal output")
                }
                _ => match rest.parse::<i32>() {
                    Ok(lines) => {
                        self.scroll(lines);
                        message("Terminal scrollback")
                    }
                    Err(_) => Err("usage: scroll <lines>|top|bottom".into()),
                },
            },
            "clear" if rest == "scrollback" => {
                self.screen_mut().clear_scrollback();
                self.matches.clear();
                message("Scrollback cleared")
            }
            "copy" => match rest {
                "" | "screen" => Ok(Control::Copy(self.visible_text())),
                "scrollback" => Ok(Control::Copy(self.transcript())),
                _ => Err("usage: copy screen|scrollback".into()),
            },
            "reconnect" => Ok(Control::Reconnect),
            "close" | "c" => Ok(Control::Close),
            "send" if rest == "escape" => Ok(Control::Send(
                crate::telnet::Command::Send(vec![29]),
                "Sent escape character".into(),
            )),
            "send" => {
                let code = match rest {
                    "ip" => Some(op_command::IP),
                    "brk" | "break" => Some(op_command::BRK),
                    "ao" => Some(op_command::AO),
                    "ayt" => Some(op_command::AYT),
                    "ec" => Some(op_command::EC),
                    "el" => Some(op_command::EL),
                    "ga" => Some(op_command::GA),
                    "nop" => Some(op_command::NOP),
                    "eof" => Some(op_command::EOF),
                    "susp" => Some(op_command::SUSP),
                    "abort" => Some(op_command::ABORT),
                    _ => None,
                };
                code.map(|code| {
                    Control::Send(
                        crate::telnet::Command::SendIac(code),
                        format!("Sent IAC {}", rest.to_uppercase()),
                    )
                })
                .ok_or_else(|| {
                    "usage: send ip|ao|ayt|brk|ec|el|ga|nop|eof|susp|abort|escape".into()
                })
            }
            _ => return None,
        })
    }
}

/// Only the event vocabulary differs between frontends; encoding does not.
pub fn from_crossterm(event: crossterm::event::KeyEvent) -> KeyInput {
    use crossterm::event::{KeyCode as C, KeyModifiers as M};
    let mut modifiers = Modifiers {
        shift: event.modifiers.contains(M::SHIFT),
        control: event.modifiers.contains(M::CONTROL),
        alt: event.modifiers.contains(M::ALT),
        meta: event.modifiers.contains(M::SUPER),
    };
    let key = match event.code {
        C::Char(c) => Key::Character(c.to_string()),
        C::Enter => Key::Enter,
        C::Esc => Key::Escape,
        C::Backspace => Key::Backspace,
        C::Delete => Key::Delete,
        C::Tab => Key::Tab,
        C::BackTab => {
            modifiers.shift = true;
            Key::Tab
        }
        C::Up => Key::ArrowUp,
        C::Down => Key::ArrowDown,
        C::Right => Key::ArrowRight,
        C::Left => Key::ArrowLeft,
        C::Home => Key::Home,
        C::End => Key::End,
        C::PageUp => Key::PageUp,
        C::PageDown => Key::PageDown,
        C::Insert => Key::Other("Insert".into()),
        C::F(n) => Key::Other(format!("F{n}")),
        _ => Key::Other(String::new()),
    };
    KeyInput {
        key,
        code: String::new(),
        location: 0,
        state: if event.kind == crossterm::event::KeyEventKind::Release {
            KeyState::Released
        } else {
            KeyState::Pressed
        },
        modifiers,
        repeat: event.kind == crossterm::event::KeyEventKind::Repeat,
        composing: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queries_are_streaming_ordered_and_do_not_answer_status_reports() {
        for split in 0..=26 {
            let mut terminal = Terminal::new(24, 80);
            let bytes = b"abc\x1b[6nxyz\x1b[6n\x1b[5n\x1b[0c\x1b[n";
            let split = split.min(bytes.len());
            let mut replies = terminal.process(&bytes[..split]);
            replies.extend(terminal.process(&bytes[split..]));
            assert_eq!(
                replies,
                [
                    b"\x1b[1;4R".to_vec(),
                    b"\x1b[1;7R".to_vec(),
                    b"\x1b[0n".to_vec(),
                    b"\x1b[?1;2c".to_vec()
                ]
            );
        }
    }

    #[test]
    fn dec_graphics_hvp_index_tabs_wrap_and_attributes() {
        let mut terminal = Terminal::new(6, 20);
        terminal.process(b"\x1b(0lqk\x1b(B\x1b[3;5fX\x1bDY\x1bEZ");
        assert_eq!(terminal.screen().cell(0, 0).unwrap().contents(), "┌");
        assert_eq!(terminal.screen().cell(0, 1).unwrap().contents(), "─");
        assert_eq!(terminal.screen().cell(2, 4).unwrap().contents(), "X");
        assert_eq!(terminal.screen().cell(3, 5).unwrap().contents(), "Y");
        assert_eq!(terminal.screen().cell(4, 0).unwrap().contents(), "Z");
        terminal.process(b"\x1b[H\x1b[3g\x1b[6G\x1bH\r\tT");
        assert_eq!(terminal.screen().cell(0, 5).unwrap().contents(), "T");
        terminal.process(b"\x1b[?7l\x1b[2;20HAB\x1b[8;9;2mS");
        let cell = terminal.screen().cell(1, 19).unwrap();
        assert_eq!(cell.contents(), "S");
        assert!(cell.conceal() && cell.strike() && cell.dim());
    }

    #[test]
    fn wide_cells_survive_resize_and_single_column_output() {
        for cols in [1, 2, 4, 80, 160] {
            let mut terminal = Terminal::new(3, cols);
            terminal.process(format!("\x1b[{}G界", cols.saturating_sub(1).max(1)).as_bytes());
            terminal.resize(cols.saturating_sub(1), 3);
            terminal.process("X界\x1b[2K".as_bytes());
            terminal.resize(cols, 1);
            terminal.process("界界\n界".as_bytes());
        }
    }

    #[test]
    fn graphemes_are_joined_across_packets_without_advancing_extra_cells() {
        for text in [
            "👩\u{200d}💻",
            "👨\u{200d}👩\u{200d}👧\u{200d}👦",
            "🇬🇧",
            "👍🏽",
            "❤️",
        ] {
            let mut terminal = Terminal::new(2, 30);
            for byte in text.as_bytes() {
                terminal.process(&[*byte]);
            }
            terminal.process(b"X");
            assert_eq!(terminal.screen().cell(0, 0).unwrap().contents(), text);
            assert_eq!(terminal.screen().cell(0, 2).unwrap().contents(), "X");
            assert_eq!(terminal.screen().cursor_position(), (0, 3));
        }
    }

    #[test]
    fn echo_does_not_override_edit_and_cp437_encodes_both_directions() {
        let mut terminal = Terminal::new(2, 20);
        terminal.observe(&Event::Negotiation {
            command: op_command::WILL,
            option: op_option::ECHO,
        });
        terminal.observe(&Event::LineMode {
            active: true,
            mode: 3,
        });
        assert!(!terminal.char_mode(true));
        assert_eq!(terminal.signal(&[3]), Some(op_command::IP));
        terminal.encoding = Encoding::Cp437;
        assert_eq!(terminal.encode_line("é╔").unwrap(), b"\x82\xc9\r\n");
        assert!(terminal.encode_line("😀").is_err());
    }

    #[test]
    fn terminal_line_endings_follow_outbound_binary_negotiation() {
        let mut terminal = Terminal::new(3, 40);
        terminal.observe(&Event::LineMode {
            active: true,
            mode: 11,
        });
        assert_eq!(terminal.encode_line("reader").unwrap(), b"reader\r\n");

        // RFC 856: the two BINARY directions are independent. Receiving
        // binary data does not change how the local editor transmits lines.
        terminal.observe(&Event::Negotiation {
            command: op_command::WILL,
            option: op_option::BINARY,
        });
        assert_eq!(terminal.encode_line("reader").unwrap(), b"reader\r\n");
        terminal.observe(&Event::Negotiation {
            command: op_command::DO,
            option: op_option::BINARY,
        });
        assert_eq!(terminal.encode_line("reader").unwrap(), b"reader\n");
        assert_eq!(terminal.encode_line("").unwrap(), b"\n");

        // A user-selected line editor has the same binary transparency as
        // negotiated LINEMODE. RFC 1123 §3.2.7 has no binary EOL convention.
        terminal.observe(&Event::LineMode {
            active: false,
            mode: 0,
        });
        terminal.input_mode = Some(InputMode::Line);
        assert_eq!(terminal.encode_line("reader").unwrap(), b"reader\n");
        terminal.observe(&Event::Negotiation {
            command: op_command::DONT,
            option: op_option::BINARY,
        });
        assert_eq!(terminal.encode_line("reader").unwrap(), b"reader\r\n");
    }

    #[test]
    fn terminal_binary_line_echo_and_paste_keep_newline_geometry() {
        let mut terminal = Terminal::new(5, 40);
        terminal.observe(&Event::LineMode {
            active: true,
            mode: 11,
        });
        terminal.observe(&Event::Negotiation {
            command: op_command::DO,
            option: op_option::BINARY,
        });
        terminal.process(b"login: ");
        let bytes = terminal.encode_line("reader").unwrap();
        assert_eq!(bytes, b"reader\n");
        terminal.echo_input(&bytes);
        assert_eq!(terminal.visible_text(), "login: reader");
        assert_eq!(terminal.screen().cursor_position(), (1, 0));

        let (bytes, draft) = terminal.line_paste("", 0..0, "one\r\ntwo\nthree").unwrap();
        assert_eq!(bytes, b"one\ntwo\n");
        assert_eq!(draft, "three");
        terminal.echo_input(&bytes);
        assert_eq!(terminal.visible_text(), "login: reader\none\ntwo");
        assert_eq!(terminal.screen().cursor_position(), (3, 0));

        // Character-mode LF retains its independent cursor motion; local
        // line-editor echo must not change received VT control semantics.
        terminal.input_mode = Some(InputMode::Character);
        terminal.echo_input(b"x\ny");
        assert_eq!(terminal.screen().cursor_position(), (4, 2));
    }

    #[test]
    fn input_obeys_application_cursor_modifiers_and_bracketed_paste() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut terminal = Terminal::new(2, 20);
        terminal.process(b"\x1b[?1h\x1b[?2004h");
        assert_eq!(
            terminal
                .encode_key(&from_crossterm(KeyEvent::new(
                    KeyCode::Up,
                    KeyModifiers::NONE
                )))
                .unwrap()
                .unwrap(),
            b"\x1bOA"
        );
        assert_eq!(
            terminal
                .encode_key(&from_crossterm(KeyEvent::new(
                    KeyCode::Up,
                    KeyModifiers::CONTROL
                )))
                .unwrap()
                .unwrap(),
            b"\x1b[1;5A"
        );
        assert_eq!(
            terminal.encode_paste("a\nb").unwrap(),
            b"\x1b[200~a\rb\x1b[201~"
        );
        assert_eq!(
            terminal
                .encode_key(&from_crossterm(KeyEvent::new(
                    KeyCode::Char('é'),
                    KeyModifiers::CONTROL
                )))
                .unwrap(),
            None
        );
    }

    #[test]
    fn terminal_reflow_preserves_transcript_and_cursor_across_sizes() {
        let mut terminal = Terminal::new(3, 8);
        terminal.process(b"first\r\nabcdefghijklmnopqrst\r\nlast");
        let transcript = terminal.transcript();
        for (cols, rows) in [(5, 4), (12, 2), (40, 8), (8, 3)] {
            terminal.resize(cols, rows);
            assert_eq!(terminal.transcript(), transcript, "{cols}x{rows}");
            let (row, col) = terminal.screen().cursor_position();
            assert_eq!(
                terminal.screen().cell(row, col - 1).unwrap().contents(),
                "t"
            );
        }
    }

    #[test]
    fn terminal_single_row_wrap_and_wide_reflow_retain_logical_text() {
        let mut terminal = Terminal::new(1, 4);
        terminal.process(b"abcdefghijk");
        assert_eq!(terminal.transcript(), "abcdefghijk");
        terminal.resize(20, 2);
        assert_eq!(terminal.transcript(), "abcdefghijk");
        assert_eq!(terminal.screen().cursor_position(), (0, 11));

        let mut terminal = Terminal::new(3, 8);
        terminal.process("ab界cd界ef界gh".as_bytes());
        for cols in [4, 6, 16, 8] {
            terminal.resize(cols, 3);
            assert_eq!(terminal.transcript(), "ab界cd界ef界gh", "width {cols}");
        }
    }

    #[test]
    fn terminal_scrollback_anchor_and_alternate_screen_are_independent() {
        let mut terminal = Terminal::new(3, 10);
        for line in 0..20 {
            terminal.process(format!("{line:02}\r\n").as_bytes());
        }
        terminal.scroll(10);
        let reading = terminal.visible_text();
        terminal.process(b"live\r\n");
        assert_eq!(terminal.visible_text(), reading);
        terminal.screen_mut().set_scrollback(0);
        let primary = terminal.transcript();
        terminal.process(b"\x1b[?1049h\x1b[2;3HALT");
        terminal.resize(12, 4);
        assert_eq!(terminal.screen().cell(1, 2).unwrap().contents(), "A");
        terminal.process(b"\x1b[?1049l");
        assert_eq!(terminal.transcript(), primary);
    }

    #[test]
    fn terminal_search_crosses_soft_wraps_and_invalidates_changed_cells() {
        let mut terminal = Terminal::new(4, 5);
        terminal.process(b"abcdefghijklmnop");
        assert!(terminal.find("defgh").starts_with("Match 1/1"));
        for (row, col) in [(0, 3), (0, 4), (1, 0), (1, 1), (1, 2)] {
            assert!(terminal.highlighted(row, col));
        }
        terminal.process(b"\x1b[HXXXXX");
        assert!(!terminal.highlighted(0, 3));
        assert_eq!(terminal.find_next(false), "No terminal search matches");
    }

    #[test]
    fn terminal_line_controls_obey_slc_and_trapsig() {
        let mut terminal = Terminal::new(3, 20);
        terminal.observe(&Event::LineMode {
            active: true,
            mode: 1,
        });
        assert_eq!(
            terminal.line_control(vec![3]),
            LineControl::Insert("\x03".into())
        );
        assert_eq!(terminal.line_control(vec![21]), LineControl::EraseLine);
        terminal.observe(&Event::Slc {
            disabled: (1 << 11) | (1 << 3),
        });
        assert_eq!(
            terminal.line_control(vec![21]),
            LineControl::Insert("\x15".into())
        );
        terminal.observe(&Event::LineMode {
            active: true,
            mode: 3,
        });
        assert_eq!(terminal.signal(&[3]), None);
        assert_eq!(terminal.line_control(vec![4]), LineControl::Send(vec![4]));
        terminal.echo_input(b"\x1b[200~hello\rworld\x1b[201~");
        assert_eq!(terminal.visible_text(), "hello\nworld");
    }

    #[test]
    fn terminal_origin_reports_and_ind_ignore_newline_mode() {
        let mut terminal = Terminal::new(8, 20);
        terminal.process(b"\x1b[20h\x1b[3;5HX\x1bDY");
        assert_eq!(terminal.screen().cell(3, 5).unwrap().contents(), "Y");
        terminal.process(b"\x1b[3;7r\x1b[?6h\x1b[2;4H");
        assert_eq!(terminal.process(b"\x1b[6n"), [b"\x1b[2;4R".to_vec()]);
        terminal.process(b"\x1b[4d");
        assert_eq!(terminal.process(b"\x1b[6n"), [b"\x1b[4;4R".to_vec()]);
    }

    #[test]
    fn terminal_soft_tabs_use_programmable_stops_and_grapheme_columns() {
        let mut terminal = Terminal::new(3, 30);
        terminal.observe(&Event::LineMode {
            active: true,
            mode: 9,
        });
        terminal.process(b"\x1b[3g\x1b[6G\x1bH\r");
        assert_eq!(terminal.encode_line("ab\tX").unwrap(), b"ab   X\r\n");
        assert_eq!(
            terminal.encode_line("👩‍💻\tX").unwrap(),
            "👩‍💻   X\r\n".as_bytes()
        );
    }

    #[test]
    fn terminal_sgr_rates_double_underline_and_reverse_screen_are_independent() {
        let mut terminal = Terminal::new(2, 20);
        terminal.process(b"\x1b[5mA\x1b[6;21mB\x1b[25;24mC\x1b[?5h");
        assert!(terminal.screen().reverse_video());
        let a = terminal.screen().cell(0, 0).unwrap();
        let b = terminal.screen().cell(0, 1).unwrap();
        let c = terminal.screen().cell(0, 2).unwrap();
        assert!(a.blink() && !a.rapid_blink());
        assert!(b.blink() && b.rapid_blink() && b.double_underline());
        assert!(!c.blink() && !c.underline());
        terminal.process(b"\x1b[?5l");
        assert!(!terminal.screen().reverse_video());
        assert_eq!(terminal.visible_text(), "ABC");
    }

    #[test]
    fn terminal_utf8_replacement_characters_are_visible() {
        let mut terminal = Terminal::new(2, 20);
        terminal.process(b"A\xffB");
        terminal.process("�X".as_bytes());
        assert_eq!(terminal.visible_text(), "A�B�X");
    }

    #[test]
    fn terminal_mouse_reports_modes_modifiers_and_local_override() {
        let mut terminal = Terminal::new(24, 80);
        assert!(
            terminal
                .encode_mouse(0, 0, MouseAction::Press(0), Modifiers::default())
                .is_none()
        );
        terminal.process(b"\x1b[?1002h\x1b[?1006h");
        let modifiers = Modifiers {
            control: true,
            ..Modifiers::default()
        };
        assert_eq!(
            terminal
                .encode_mouse(4, 9, MouseAction::Press(0), modifiers)
                .unwrap(),
            b"\x1b[<16;10;5M"
        );
        assert_eq!(
            terminal
                .encode_mouse(4, 9, MouseAction::Release(0), modifiers)
                .unwrap(),
            b"\x1b[<16;10;5m"
        );
        assert_eq!(
            terminal
                .encode_mouse(0, 0, MouseAction::Motion(Some(0)), Modifiers::default())
                .unwrap(),
            b"\x1b[<32;1;1M"
        );
        assert!(
            terminal
                .encode_mouse(0, 0, MouseAction::Motion(None), modifiers)
                .is_none()
        );
        assert!(
            terminal
                .encode_mouse(
                    0,
                    0,
                    MouseAction::Press(0),
                    Modifiers {
                        shift: true,
                        ..modifiers
                    }
                )
                .is_none()
        );
    }

    #[test]
    fn terminal_concealed_wide_cells_preserve_copy_columns() {
        let mut terminal = Terminal::new(2, 20);
        terminal.process("A\x1b[8m界\x1b[28mZ".as_bytes());
        assert_eq!(terminal.visible_text(), "A  Z");
        assert_eq!(terminal.transcript(), "A  Z");
        assert!(terminal.find("Z").starts_with("Match 1/1"));
        assert!(terminal.highlighted(0, 3));
        assert!(!terminal.transcript().contains('界'));
    }

    #[test]
    fn terminal_adversarial_controls_resizes_and_wide_edits_keep_cell_invariants() {
        let mut terminal = Terminal::new(4, 8);
        let mut seed = 123u64;
        let sequences: &[&[u8]] = &[
            "界".as_bytes(),
            "👩‍💻".as_bytes(),
            b"x",
            b"\r\n",
            b"\x1b[65535P",
            b"\x1b[65535@",
            b"\x1b[65535L",
            b"\x1b[65535M",
            b"\x1b[65535S",
            b"\x1b[65535T",
            b"\x1b[2K",
            b"\x1b[?1049h",
            b"\x1b[?1049l",
            b"\x1b[?6h",
            b"\x1b[?6l",
            b"\x1b[2;3r",
            b"\x1b[r",
            b"\x1b[99;99H",
            b"\x1b[?7l",
            b"\x1b[?7h",
            b"\x1b[4h",
            b"\x1b[4l",
        ];
        for index in 0..2_000 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            if index % 7 == 0 {
                terminal.resize((seed % 15 + 1) as u16, ((seed >> 8) % 8 + 1) as u16);
            }
            terminal.process(sequences[(seed >> 16) as usize % sequences.len()]);
            let (rows, cols) = terminal.screen().size();
            for row in 0..rows {
                for col in 0..cols {
                    let cell = terminal.screen().cell(row, col).unwrap();
                    if cell.is_wide() {
                        assert!(
                            col + 1 < cols
                                && terminal
                                    .screen()
                                    .cell(row, col + 1)
                                    .unwrap()
                                    .is_wide_continuation(),
                            "iteration {index} at {row},{col}"
                        );
                    }
                    if cell.is_wide_continuation() {
                        assert!(
                            col > 0 && terminal.screen().cell(row, col - 1).unwrap().is_wide(),
                            "iteration {index} at {row},{col}"
                        );
                    }
                }
            }
        }
    }
}
