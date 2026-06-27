//! Boa's lexer cursor that manages the input byte stream.

use crate::source::{ReadChar, UTF8Input};
use boa_ast::{LinearPosition, Position, PositionGroup, SourceText};
use std::io::{self, Error, ErrorKind};

/// Cursor over the source code.
#[derive(Debug)]
pub(super) struct Cursor<R> {
    iter: R,
    pos: Position,
    module: bool,
    strict: bool,
    peeked: [Option<u32>; 4],
    /// Code points pushed back in front of `peeked`/`iter` (TRust lazy parsing).
    /// The lazy body scan consumes through the cursor destructively; when it
    /// bails, the consumed prefix is replayed here so an eager re-parse re-reads
    /// it seamlessly (the unconsumed suffix is still in `peeked`/`iter`). Stored
    /// reversed, so `pop()` yields the next code point. Empty on the hot path.
    pushback: Vec<u32>,
    source_collector: SourceText,
}

impl<R> Cursor<R> {
    /// Gets the current position of the cursor in the source code.
    #[inline]
    pub(super) fn pos_group(&self) -> PositionGroup {
        PositionGroup::new(self.pos, self.linear_pos())
    }

    /// Gets the current position of the cursor in the source code.
    #[inline]
    pub(super) const fn pos(&self) -> Position {
        self.pos
    }

    /// Gets the current linear position of the cursor in the source code.
    #[inline]
    pub(super) fn linear_pos(&self) -> LinearPosition {
        self.source_collector.cur_linear_position()
    }

    pub(super) fn take_source(&mut self) -> SourceText {
        let replace_with = SourceText::with_capacity(0);
        std::mem::replace(&mut self.source_collector, replace_with)
    }

    /// Advances the position to the next column.
    fn next_column(&mut self) {
        let current_line = self.pos.line_number();
        let next_column = self.pos.column_number() + 1;
        self.pos = Position::new(current_line, next_column);
    }

    /// Advances the position to the next line.
    fn next_line(&mut self) {
        let next_line = self.pos.line_number() + 1;
        self.pos = Position::new(next_line, 1);
    }

    /// Returns if strict mode is currently active.
    pub(super) const fn strict(&self) -> bool {
        self.strict
    }

    /// Sets the current strict mode.
    pub(super) fn set_strict(&mut self, strict: bool) {
        self.strict = strict;
    }

    /// Returns if the module mode is currently active.
    pub(super) const fn module(&self) -> bool {
        self.module
    }

    /// Sets the current goal symbol to module.
    pub(super) fn set_module(&mut self, module: bool) {
        self.module = module;
        self.strict = module;
    }
}

impl<R: ReadChar> Cursor<R> {
    /// Creates a new Lexer cursor.
    pub(super) fn new(inner: R) -> Self {
        Self {
            iter: inner,
            pos: Position::new(1, 1),
            strict: false,
            module: false,
            peeked: [None; 4],
            pushback: Vec::new(),
            source_collector: SourceText::default(),
        }
    }

    /// Peeks the next n code points, the maximum number of peeked is 4 (n <= 4).
    ///
    /// Returns an owned array (so a non-empty lazy-parse `pushback`, which sits
    /// in front of `peeked`, can be merged in without aliasing `peeked`). The
    /// hot path (`pushback` empty) fills and copies `peeked` exactly as before.
    pub(super) fn peek_n(&mut self, n: u8) -> Result<[Option<u32>; 4], Error> {
        let n = n as usize;
        debug_assert!(n <= 4);
        let pb = self.pushback.len();
        // Slots beyond what `pushback` covers come from `peeked`/`iter`.
        let peeked_have = self.peeked.iter().filter(|c| c.is_some()).count();
        let peeked_need = n.saturating_sub(pb);
        for i in peeked_have..peeked_need {
            self.peeked[i] = self.iter.next_char()?;
        }
        let mut out = [None; 4];
        for (i, slot) in out.iter_mut().enumerate().take(n) {
            *slot = if i < pb {
                // `pushback` is stored reversed: its last element is next.
                self.pushback.get(pb - 1 - i).copied()
            } else {
                self.peeked[i - pb]
            };
        }
        Ok(out)
    }

    /// Peeks the next UTF-8 character in u32 code point.
    pub(super) fn peek_char(&mut self) -> Result<Option<u32>, Error> {
        if let Some(&c) = self.pushback.last() {
            return Ok(Some(c));
        }
        if let Some(c) = self.peeked[0] {
            return Ok(Some(c));
        }

        let next = self.iter.next_char()?;
        self.peeked[0] = next;
        Ok(next)
    }

    pub(super) fn next_if(&mut self, c: u32) -> io::Result<bool> {
        if self.peek_char()? == Some(c) {
            self.next_char()?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Applies the predicate to the next character and returns the result.
    /// Returns false if the next character is not a valid ascii or there is no next character.
    /// Otherwise returns the result from the predicate on the ascii in char
    ///
    /// The buffer is not incremented.
    pub(super) fn next_is_ascii_pred<F>(&mut self, pred: &F) -> io::Result<bool>
    where
        F: Fn(char) -> bool,
    {
        Ok(match self.peek_char()? {
            Some(byte) if (0..=0x7F).contains(&byte) =>
            {
                #[allow(clippy::cast_possible_truncation)]
                pred(char::from(byte as u8))
            }
            Some(_) | None => false,
        })
    }

    /// Fills the buffer with all bytes until the stop byte is found.
    /// Returns error when reaching the end of the buffer.
    ///
    /// Note that all bytes up until the stop byte are added to the buffer, including the byte right before.
    pub(super) fn take_until(&mut self, stop: u32, buf: &mut Vec<u32>) -> io::Result<()> {
        loop {
            if self.next_if(stop)? {
                return Ok(());
            } else if let Some(c) = self.next_char()? {
                buf.push(c);
            } else {
                return Err(Error::new(
                    ErrorKind::UnexpectedEof,
                    format!("Unexpected end of file when looking for character {stop}"),
                ));
            }
        }
    }

    /// Fills a mutable slice up to the ends while characters are alphabetic. Returns
    /// the number of characters read, or `N+1` if the buffer was filled but there were
    /// still characters after.
    pub(super) fn take_array_alphabetic<const N: usize>(
        &mut self,
        arr: &mut [u32; N],
    ) -> io::Result<usize> {
        for (i, out) in arr.iter_mut().enumerate() {
            match self.peek_char()? {
                // A..Z | a..z
                Some(0x41..=0x5A | 0x61..=0x7A) => {
                    *out = self.next_char()?.expect("Already checked.");
                }
                _ => return Ok(i),
            }
        }
        // Check the next character and return N+1 if it's alphabetic.
        match self.peek_char() {
            // A..Z | a..z
            Ok(Some(0x41..=0x5A | 0x61..=0x7A)) => Ok(N + 1),
            _ => Ok(N),
        }
    }

    /// Retrieves the next UTF-8 character.
    pub(crate) fn next_char(&mut self) -> Result<Option<u32>, Error> {
        let ch = if let Some(c) = self.pushback.pop() {
            Some(c)
        } else if let Some(c) = self.peeked[0] {
            self.peeked[0] = None;
            self.peeked.rotate_left(1);
            Some(c)
        } else {
            self.iter.next_char()?
        };

        if let Some(ch) = ch {
            self.source_collector.collect_code_point(ch);
        }

        match ch {
            Some(0xD) => {
                // Try to take a newline if it's next, for windows "\r\n" newlines
                // Otherwise, treat as a Mac OS9 bare '\r' newline. The following
                // '\n' may sit in `pushback` (lazy-parse replay) ahead of
                // `peeked`/`iter`.
                if self.peek_char()? == Some(0xA) {
                    if self.pushback.last().copied() == Some(0xA) {
                        self.pushback.pop();
                    } else {
                        self.peeked[0] = None;
                        self.peeked.rotate_left(1);
                    }
                    self.source_collector.collect_code_point(0xA);
                }
                self.next_line();
            }
            // '\n' | '\u{2028}' | '\u{2029}'
            Some(0xA | 0x2028 | 0x2029) => self.next_line(),
            Some(_) => self.next_column(),
            _ => {}
        }

        Ok(ch)
    }

    /// Attempt to skip an eligible function body **in place** (TRust lazy
    /// parsing), positioned just after the body's opening `{`. On success the
    /// cursor is advanced past the matching `}` (source text collected, position
    /// updated as for a normal lex) and the captured reference superset +
    /// directive strictness are returned. On a bail — the scanner's, or a body
    /// shorter than `min_len` code units — the cursor is restored exactly to
    /// where it started (so the caller parses the body eagerly) and [`None`] is
    /// returned.
    pub(super) fn scan_lazy_function_body(&mut self, min_len: usize) -> Option<LazyBodyScan> {
        let start_pos = self.pos;
        let start_linear = self.source_collector.cur_linear_position();

        let mut idents: Vec<Box<[u32]>> = Vec::new();
        let scanned = {
            let mut adapter = LazyScanAdapter { cursor: self };
            crate::lazy_scan::scan_body_after_open(&mut adapter, &mut idents)
        };

        let end_linear = self.source_collector.cur_linear_position();
        let long_enough = end_linear.pos().saturating_sub(start_linear.pos()) >= min_len;

        match scanned {
            Some(body_strict) if long_enough => Some(LazyBodyScan {
                idents,
                body_strict,
                end_pos: self.pos,
                end_linear,
            }),
            _ => {
                self.rollback_lazy_scan(start_pos, start_linear);
                None
            }
        }
    }

    /// Restore the cursor after a bailed [`scan_lazy_function_body`]: replay the
    /// code points it speculatively consumed (now sitting in `source_collector`
    /// past `start_linear`) via `pushback`, truncate the collected source, and
    /// restore the position. An eager re-parse then re-reads the body from
    /// `pushback` (its prefix) and `iter` (its suffix), seamlessly.
    fn rollback_lazy_scan(&mut self, start_pos: Position, start_linear: LinearPosition) {
        let tail: Vec<u16> = self
            .source_collector
            .get_code_points_from_pos(start_linear)
            .to_vec();
        self.source_collector.truncate(start_linear.pos());
        self.pos = start_pos;
        // Decode the UTF-16 tail to code points, pushing them in front. Stored
        // reversed so `pop()` yields the original order; an existing `pushback`
        // (none, in practice — bails do not nest) stays further ahead.
        let mut decoded: Vec<u32> = Vec::with_capacity(tail.len());
        let mut i = 0;
        while i < tail.len() {
            let u1 = tail[i];
            if (0xD800..=0xDBFF).contains(&u1) {
                if let Some(&u2) = tail.get(i + 1)
                    && (0xDC00..=0xDFFF).contains(&u2)
                {
                    let cp =
                        0x1_0000 + ((u32::from(u1 - 0xD800)) << 10) + u32::from(u2 - 0xDC00);
                    decoded.push(cp);
                    i += 2;
                    continue;
                }
            }
            decoded.push(u32::from(u1));
            i += 1;
        }
        decoded.reverse();
        // Append to any pre-existing pushback (nested rollback: an inner
        // function rolls back while an enclosing function's pushback is still
        // pending). `pushback` is reverse-stored — `pop()` from the END yields
        // the next forward char — so this body's code points, which are EARLIER
        // in source than the enclosing remainder already in `pushback`, go at the
        // end and are replayed first. An empty existing `pushback` is the common
        // (non-nested) case.
        self.pushback.extend(decoded);
    }
}

/// The result of a successful in-place lazy body skip (TRust lazy parsing).
pub(crate) struct LazyBodyScan {
    /// The captured reference superset, as code points (interned by the caller).
    pub(crate) idents: Vec<Box<[u32]>>,
    /// Whether the body's directive prologue declared `"use strict"`.
    pub(crate) body_strict: bool,
    /// The position just past the matching `}` (the body span's end).
    pub(crate) end_pos: Position,
    /// The linear position just past the matching `}`.
    pub(crate) end_linear: LinearPosition,
}

/// Adapts the lexer [`Cursor`] to the lazy-scan [`CpCursor`] (TRust lazy
/// parsing). `peek`/`bump` go through the cursor's own buffered reads, so source
/// collection and position tracking happen exactly as for a normal lex; an I/O
/// error is treated as end-of-input (a bail).
struct LazyScanAdapter<'a, R> {
    cursor: &'a mut Cursor<R>,
}

impl<R: ReadChar> crate::lazy_scan::CpCursor for LazyScanAdapter<'_, R> {
    #[inline]
    fn peek(&mut self, n: usize) -> Option<u32> {
        match n {
            0 => self.cursor.peek_char().ok().flatten(),
            _ => self
                .cursor
                .peek_n((n + 1) as u8)
                .ok()
                .and_then(|a| a[n]),
        }
    }
    #[inline]
    fn bump(&mut self) -> Option<u32> {
        self.cursor.next_char().ok().flatten()
    }
}

impl<'a> From<&'a [u8]> for Cursor<UTF8Input<&'a [u8]>> {
    fn from(input: &'a [u8]) -> Self {
        Self::new(UTF8Input::new(input))
    }
}
