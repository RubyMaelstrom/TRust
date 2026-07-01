//! Fallible function-body pre-scan — the lazy-parse "syntax pass".
//!
//! [`scan_function_body`] takes the source as code points and the index of a
//! function body's opening `{` and returns the index just past its matching
//! `}` — WITHOUT building an AST — together with the identifier references in
//! the body (for closed-over-binding capture) and whether the body's directive
//! prologue declares `"use strict"`. It is the cheap boundary-finder a lazy
//! parser uses to *defer* a function body it may never need to compile.
//!
//! It is deliberately FALLIBLE (SpiderMonkey's syntax-parser model): it returns
//! [`None`] ("bail") on any construct whose tokenization it cannot resolve with
//! certainty — an ambiguous regex-vs-division after `}`, an unterminated
//! literal, a bracket mismatch, EOF before the close — AND on any construct
//! whose binding semantics it cannot capture by textual reference replay: a
//! `with` statement or an unqualified `eval` reference anywhere in the body
//! (both make enclosing bindings reachable in ways the reference superset does
//! not list, so the caller must parse them eagerly). **A bail is never wrong,
//! only slower**: the caller falls back to a full eager parse. The contract the
//! fuzz tests enforce is therefore one-directional — for any real function this
//! returns EITHER `None` OR *exactly* the body span the full parser produces,
//! NEVER a different span.
//!
//! The scan logic lives in one generic core ([`scan_core`]) over a
//! [`CpCursor`]; the slice entry point below drives it for the fuzz harness, and
//! `boa_parser`'s lexer cursor drives the same core to skip bodies in place
//! (so the fuzz proof covers both). The scan works in flat code points (the
//! lexer counts columns by code point, so a caller can map a span `Position`
//! to/from an index with the same newline rules — see the `lazy_scan` fuzz
//! harness in TRust).

/// A forward code-point cursor with one-ahead lookahead, consumed left to
/// right, driving the lazy body [`scan_core`].
///
/// `peek(0)` is the next code point to be consumed, `peek(1)` the one after; the
/// core never looks further. `bump` consumes one code point and returns it. An
/// implementation that also tracks source text / position (the lexer cursor) has
/// those updated by `bump`, so a successful scan leaves the cursor exactly past
/// the matching `}`.
pub trait CpCursor {
    /// The code point `n` ahead (0 = next to consume), or `None` at EOF. `n` is
    /// always 0 or 1.
    fn peek(&mut self, n: usize) -> Option<u32>;
    /// Consume and return the next code point, or `None` at EOF.
    fn bump(&mut self) -> Option<u32>;
}

/// The result of a successful body scan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BodyScan {
    /// Index one past the matching `}` — equal to the body span's exclusive end
    /// (`FunctionBody::span().end()` mapped to a flat code-point index). Only
    /// meaningful for the slice entry point; the lexer cursor reads its position
    /// directly.
    pub end: usize,
    /// The code points of each identifier reference in the body, a conservative
    /// SUPERSET of its free variables (member-access property names after
    /// `.`/`?.` and keywords are excluded; object-literal keys, labels, and the
    /// body's own locals are NOT — over-collecting only over-escapes a binding,
    /// which is safe). Consumed by capturing lazy parse to replay the outer
    /// scope's escape analysis without re-walking the body.
    pub idents: Vec<Box<[u32]>>,
    /// Whether the body's directive prologue contains an exact `"use strict"`
    /// directive (no escapes), so the deferred function's effective strictness
    /// is correct before its body is re-parsed.
    pub body_strict: bool,
}

/// What the most recently scanned token implies about a following `/`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Slash {
    /// A `/` begins a regular-expression literal (expression position).
    Regex,
    /// A `/` is the division / `/=` operator (after a value).
    Div,
    /// Position is right after a `}` whose role (block end vs. object/function
    /// expression end) is unknown, so a following `/` is genuinely ambiguous.
    AfterBrace,
}

/// One open bracket on the nesting stack.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Bracket {
    Brace,
    Paren,
    Square,
    /// A `${` inside a template literal: its matching `}` resumes the template.
    TemplateSub,
}

/// True for ECMAScript `WhiteSpace` plus the BOM, excluding line terminators
/// (which are handled separately for position, though the scan ignores lines).
fn is_ws(c: u32) -> bool {
    matches!(
        c,
        0x09 | 0x0B | 0x0C | 0x20 | 0xA0 | 0xFEFF | 0x1680 | 0x2000
            ..=0x200A | 0x202F | 0x205F | 0x3000
    )
}

fn is_line_term(c: u32) -> bool {
    matches!(c, 0x0A | 0x0D | 0x2028 | 0x2029)
}

/// ASCII identifier-start / continue, `$`, `_`, and (conservatively) any
/// non-ASCII code point — JS allows many Unicode letters in identifiers, and
/// treating all non-ASCII as an identifier char only ever affects the
/// regex/division classification or over-collects an identifier, both safe.
fn is_ident_start(c: u32) -> bool {
    matches!(c, 0x41..=0x5A | 0x61..=0x7A | 0x24 | 0x5F) || c >= 0x80
}

fn is_ident_part(c: u32) -> bool {
    is_ident_start(c) || is_digit(c)
}

/// ASCII decimal digit (`0`–`9`) as a code point.
fn is_digit(c: u32) -> bool {
    (0x30..=0x39).contains(&c)
}

/// Keywords after which a `/` starts a regular expression (they expect an
/// expression to follow). Every other keyword (`this`, `super`, `true`,
/// `false`, `null`) yields a value, so a following `/` is division.
fn keyword_allows_regex(word: &str) -> bool {
    matches!(
        word,
        "return"
            | "typeof"
            | "instanceof"
            | "in"
            | "of"
            | "new"
            | "delete"
            | "void"
            | "do"
            | "else"
            | "yield"
            | "await"
            | "case"
            | "throw"
    )
}

/// Whether `word` is an ALWAYS-reserved word (so it can never be a captured
/// identifier reference and is not collected).
///
/// Only unconditional reserved words belong here. CONTEXTUAL keywords —
/// `get`/`set`/`of`/`as`/`from`/`async`, and the strict/generator/module-only
/// reserved `let`/`static`/`yield`/`await` — are all legal identifiers in some
/// context (minifiers really do emit `function of(e){…}`), so they MUST be
/// collected: not collecting an identifier that IS a reference under-escapes the
/// enclosing binding, which resolves the wrong slot at delazify (react-core's
/// `of` "not a callable" bug). Collecting a word that turns out to be a keyword
/// only ever OVER-escapes a (non-existent) binding — the safe direction.
fn is_keyword(word: &str) -> bool {
    matches!(
        word,
        "break"
            | "case"
            | "catch"
            | "class"
            | "const"
            | "continue"
            | "debugger"
            | "default"
            | "delete"
            | "do"
            | "else"
            | "enum"
            | "export"
            | "extends"
            | "false"
            | "finally"
            | "for"
            | "function"
            | "if"
            | "import"
            | "in"
            | "instanceof"
            | "new"
            | "null"
            | "return"
            | "super"
            | "switch"
            | "this"
            | "throw"
            | "true"
            | "try"
            | "typeof"
            | "var"
            | "void"
            | "while"
            | "with"
    )
}

/// A code-point cursor over a flat slice — the slice entry point's adapter (and
/// what the fuzz harness exercises).
struct SliceCursor<'a> {
    src: &'a [u32],
    i: usize,
}

impl CpCursor for SliceCursor<'_> {
    #[inline]
    fn peek(&mut self, n: usize) -> Option<u32> {
        self.src.get(self.i + n).copied()
    }
    #[inline]
    fn bump(&mut self) -> Option<u32> {
        let c = self.src.get(self.i).copied();
        if c.is_some() {
            self.i += 1;
        }
        c
    }
}

/// Drive the body [`scan_core`] over an arbitrary [`CpCursor`] — the lexer's
/// own cursor, to skip a body *in place*. `c` must be positioned just after the
/// body's opening `{`. Pushes each captured identifier's code points into
/// `idents` and returns the directive-prologue strictness on success (leaving
/// `c` just past the matching `}`), or [`None`] to bail (having consumed an
/// arbitrary prefix — the caller restores it). Shares its state machine, and so
/// the fuzz proof, with [`scan_function_body`].
#[must_use]
pub fn scan_body_after_open<C: CpCursor>(c: &mut C, idents: &mut Vec<Box<[u32]>>) -> Option<bool> {
    scan_core(c, idents)
}

/// Scan a function body starting at `open` (which must index a `{`), returning
/// the matching close, the identifier references, and the directive-prologue
/// strictness, or [`None`] to bail. See the module docs for the safety
/// contract. This is the slice entry point used by the fuzz harness; the lexer
/// cursor drives the same [`scan_core`].
#[must_use]
pub fn scan_function_body(src: &[u32], open: usize) -> Option<BodyScan> {
    if src.get(open).copied() != Some(u32::from(b'{')) {
        return None;
    }
    let mut cur = SliceCursor {
        src,
        i: open + 1, // the opening `{` is accounted for by the initial Brace
    };
    let mut idents = Vec::new();
    let body_strict = scan_core(&mut cur, &mut idents)?;
    Some(BodyScan {
        end: cur.i,
        idents,
        body_strict,
    })
}

/// The lazy-body scan state machine, shared by the slice entry point and the
/// lexer cursor. `c` is positioned just after the body's opening `{` (so the
/// bracket stack starts with one [`Bracket::Brace`]). On success the function
/// returns the directive-prologue strictness, pushes each captured identifier's
/// code points into `idents`, and leaves `c` positioned just past the matching
/// `}`. On any uncertainty — or a `with`/unqualified-`eval` the reference
/// superset cannot model — it returns [`None`] (a bail), having consumed an
/// arbitrary prefix (the caller restores it).
#[allow(clippy::too_many_lines)]
fn scan_core<C: CpCursor>(c: &mut C, idents: &mut Vec<Box<[u32]>>) -> Option<bool> {
    let body_strict = scan_directive_prologue(c)?;

    let mut stack: Vec<Bracket> = Vec::with_capacity(16);
    stack.push(Bracket::Brace);
    let mut slash = Slash::Regex; // after `{`, an expression may begin
    // Whether the previously scanned significant token was `.` or `?.`, so the
    // next identifier is a member-access property name (not a variable ref).
    let mut after_dot = false;

    while let Some(ch) = c.peek(0) {
        // Whitespace and line terminators: skip, leaving `slash`/`after_dot`.
        if is_ws(ch) || is_line_term(ch) {
            c.bump();
            continue;
        }

        // Comments.
        if ch == u32::from(b'/') {
            match c.peek(1) {
                Some(c1) if c1 == u32::from(b'/') => {
                    c.bump();
                    c.bump();
                    while c.peek(0).is_some_and(|x| !is_line_term(x)) {
                        c.bump();
                    }
                    continue;
                }
                Some(c1) if c1 == u32::from(b'*') => {
                    c.bump();
                    c.bump();
                    if !skip_block_comment(c) {
                        return None; // unterminated block comment
                    }
                    continue;
                }
                _ => {
                    // Division vs. regular expression.
                    match slash {
                        Slash::Regex => {
                            if !scan_regex(c) {
                                return None;
                            }
                            slash = Slash::Div;
                            after_dot = false;
                            continue;
                        }
                        Slash::Div => {
                            // `/` or `/=` operator → expression follows.
                            c.bump();
                            if c.peek(0) == Some(u32::from(b'=')) {
                                c.bump();
                            }
                            slash = Slash::Regex;
                            after_dot = false;
                            continue;
                        }
                        // `}` then `/`: cannot tell regex from division. Bail.
                        Slash::AfterBrace => return None,
                    }
                }
            }
        }

        // String literals.
        if ch == u32::from(b'"') || ch == u32::from(b'\'') {
            if !scan_string(c) {
                return None;
            }
            slash = Slash::Div;
            after_dot = false;
            continue;
        }

        // Template literals (incl. nested `${ ... }`).
        if ch == u32::from(b'`') {
            match scan_template(c, &mut stack)? {
                TemplateStep::Complete => slash = Slash::Div,
                // Entered a `${` — a TemplateSub was pushed; resume normal
                // scanning of the substitution expression.
                TemplateStep::Substitution => slash = Slash::Regex,
            }
            after_dot = false;
            continue;
        }

        // Identifiers / keywords.
        if is_ident_start(ch) {
            let mut run: Vec<u32> = Vec::new();
            run.push(c.bump().expect("peeked"));
            while c.peek(0).is_some_and(is_ident_part) {
                run.push(c.bump().expect("peeked"));
            }
            // A `\uXXXX` escape inside the identifier — be conservative and bail
            // rather than mis-bound it (rare in real code).
            if c.peek(0) == Some(u32::from(b'\\')) {
                return None;
            }
            let word = cp_slice_to_string(&run);
            // A `with` statement, or any unqualified `eval` reference, makes
            // enclosing bindings reachable beyond the captured superset — bail.
            if word == "with" || (!after_dot && word == "eval") {
                return None;
            }
            slash = if keyword_allows_regex(&word) {
                Slash::Regex
            } else {
                Slash::Div
            };
            if !after_dot && !is_keyword(&word) {
                idents.push(run.into_boxed_slice());
            }
            after_dot = false;
            continue;
        }

        // Numbers (a digit, or `.` immediately before a digit).
        if is_digit(ch) || (ch == u32::from(b'.') && c.peek(1).is_some_and(is_digit)) {
            let first = c.bump().expect("peeked");
            // A radix literal (`0x`/`0b`/`0o`) has no exponent, so a following
            // sign is never part of it; only a DECIMAL exponent takes a sign.
            let is_radix = first == u32::from(b'0')
                && matches!(c.peek(0), Some(0x78 | 0x58 | 0x62 | 0x42 | 0x6F | 0x4F));
            let mut prev = first;
            while let Some(p) = c.peek(0) {
                if p == u32::from(b'+') || p == u32::from(b'-') {
                    // A `+`/`-` continues the number ONLY as a decimal exponent
                    // sign — i.e. immediately after `e`/`E` in a non-radix
                    // literal. Otherwise it is a subtraction/addition operator:
                    // stop, so `31-eg` scans as `31`, `-`, `eg` and the free
                    // reference `eg` is still captured. (Swallowing it — `e` is a
                    // hex digit, so `31-e` looked like a number — dropped `eg`
                    // from a deferred function's captured set, leaving the
                    // enclosing binding un-escaped: the react-lib `sX` delazify
                    // "not a callable" bug. An under-collected reference is the
                    // one unsafe direction — cf. the spread-scan fix.)
                    if !is_radix && matches!(prev, 0x65 | 0x45) {
                        prev = c.bump().expect("peeked");
                        continue;
                    }
                    break;
                }
                if is_number_part(p) {
                    prev = c.bump().expect("peeked");
                    continue;
                }
                break;
            }
            slash = Slash::Div;
            after_dot = false;
            continue;
        }

        // Punctuators.
        c.bump();
        match ch {
            _ if ch == u32::from(b'{') => {
                stack.push(Bracket::Brace);
                slash = Slash::Regex;
                after_dot = false;
            }
            _ if ch == u32::from(b'}') => match stack.pop() {
                Some(Bracket::TemplateSub) => {
                    // Resume the enclosing template literal after the `}`.
                    match scan_template_chars(c, &mut stack)? {
                        TemplateStep::Complete => slash = Slash::Div,
                        TemplateStep::Substitution => slash = Slash::Regex,
                    }
                    after_dot = false;
                }
                Some(Bracket::Brace) => {
                    if stack.is_empty() {
                        return Some(body_strict); // matched the body's `}`
                    }
                    slash = Slash::AfterBrace;
                    after_dot = false;
                }
                _ => return None, // mismatched `}`
            },
            _ if ch == u32::from(b'(') => {
                stack.push(Bracket::Paren);
                slash = Slash::Regex;
                after_dot = false;
            }
            _ if ch == u32::from(b')') => {
                if stack.pop() != Some(Bracket::Paren) {
                    return None;
                }
                slash = Slash::Div;
                after_dot = false;
            }
            _ if ch == u32::from(b'[') => {
                stack.push(Bracket::Square);
                slash = Slash::Regex;
                after_dot = false;
            }
            _ if ch == u32::from(b']') => {
                if stack.pop() != Some(Bracket::Square) {
                    return None;
                }
                slash = Slash::Div;
                after_dot = false;
            }
            _ if ch == u32::from(b'.') => {
                // `.` member access vs. `...` spread/rest. The first `.` was just
                // consumed (above); `...` is the only multi-dot token, so two more
                // dots mean spread/rest. A spread/rest's following identifier is a
                // real reference (`[...arr]`, `f(...args)`) — or a rest-parameter
                // *local*, which is only over-collected (safe) — so it must NOT be
                // treated as a member-access property name, or the capture-replay
                // superset drops it and the enclosing binding it captures is never
                // marked escaping. A single `.` is member access: skip the next
                // identifier. (`?.` is handled at `?`; `.5` numbers above.)
                slash = Slash::Regex;
                if c.peek(0) == Some(u32::from(b'.')) && c.peek(1) == Some(u32::from(b'.')) {
                    c.bump(); // 2nd dot
                    c.bump(); // 3rd dot
                    after_dot = false;
                } else {
                    after_dot = true;
                }
            }
            _ if ch == u32::from(b'?') => {
                // `?.` optional chaining → next identifier is a property name.
                if c.peek(0) == Some(u32::from(b'.')) {
                    c.bump();
                    slash = Slash::Regex;
                    after_dot = true;
                } else {
                    slash = Slash::Regex;
                    after_dot = false;
                }
            }
            // Any other operator/punctuator: an expression may follow.
            _ => {
                slash = Slash::Regex;
                after_dot = false;
            }
        }
    }

    None // EOF before the body closed
}

/// Scan a leading directive prologue (a run of string-literal statements at the
/// start of the body), returning whether an exact `"use strict"` directive
/// (no escapes) is present. Consumes the prologue. Conservative: only `;`- and
/// `}`-terminated directives count, so exotic ASI/continuation forms are
/// *under*-detected (the deferred function's effective strictness is recovered
/// exactly when its body is re-parsed; only the pre-first-call strict flag could
/// be momentarily sloppy). Never over-detects. Returns [`None`] on an
/// unterminated comment in the prologue (a bail).
fn scan_directive_prologue<C: CpCursor>(c: &mut C) -> Option<bool> {
    let mut strict = false;
    loop {
        if !skip_trivia(c)? {
            return Some(strict);
        }
        let Some(q) = c.peek(0) else {
            return Some(strict);
        };
        if q != u32::from(b'"') && q != u32::from(b'\'') {
            return Some(strict);
        }
        // Read the string, capturing its raw content and whether it used any
        // escape (a `"use strict"` is NOT a use-strict directive).
        let mut content: Vec<u32> = Vec::new();
        let mut had_escape = false;
        c.bump(); // opening quote
        loop {
            match c.bump() {
                None => return None, // unterminated
                Some(ch) if ch == q => break,
                Some(ch) if ch == u32::from(b'\\') => {
                    had_escape = true;
                    if c.bump().is_none() {
                        return None;
                    }
                }
                Some(ch) if is_line_term(ch) => return None, // bare newline in string
                Some(ch) => content.push(ch),
            }
        }
        let is_use_strict = !had_escape && cp_slice_to_string(&content) == "use strict";
        // Skip non-newline whitespace and comments to find the terminator.
        skip_inline_trivia(c)?;
        match c.peek(0) {
            Some(s) if s == u32::from(b';') => {
                c.bump();
                strict |= is_use_strict;
                // Continue: there may be further directives.
            }
            Some(s) if s == u32::from(b'}') => {
                // Last directive, no trailing `;`. Leave the `}` for the main
                // loop to match the body close.
                strict |= is_use_strict;
                return Some(strict);
            }
            _ => {
                // The string was an expression, not a directive (or the prologue
                // ended). The main loop continues after it.
                return Some(strict);
            }
        }
    }
}

/// Skip whitespace, line terminators, and comments. Returns `Some(true)` if more
/// input remains, `Some(false)` at EOF, [`None`] on an unterminated block
/// comment.
fn skip_trivia<C: CpCursor>(c: &mut C) -> Option<bool> {
    loop {
        match c.peek(0) {
            None => return Some(false),
            Some(ch) if is_ws(ch) || is_line_term(ch) => {
                c.bump();
            }
            Some(ch) if ch == u32::from(b'/') => match c.peek(1) {
                Some(c1) if c1 == u32::from(b'/') => {
                    c.bump();
                    c.bump();
                    while c.peek(0).is_some_and(|x| !is_line_term(x)) {
                        c.bump();
                    }
                }
                Some(c1) if c1 == u32::from(b'*') => {
                    c.bump();
                    c.bump();
                    if !skip_block_comment(c) {
                        return None;
                    }
                }
                _ => return Some(true),
            },
            Some(_) => return Some(true),
        }
    }
}

/// Like [`skip_trivia`] but does not cross line terminators (used to find a
/// directive's terminator without ASI guesswork).
fn skip_inline_trivia<C: CpCursor>(c: &mut C) -> Option<()> {
    loop {
        match c.peek(0) {
            Some(ch) if is_ws(ch) => {
                c.bump();
            }
            Some(ch) if ch == u32::from(b'/') => match c.peek(1) {
                Some(c1) if c1 == u32::from(b'/') => {
                    c.bump();
                    c.bump();
                    while c.peek(0).is_some_and(|x| !is_line_term(x)) {
                        c.bump();
                    }
                }
                Some(c1) if c1 == u32::from(b'*') => {
                    c.bump();
                    c.bump();
                    if !skip_block_comment(c) {
                        return None;
                    }
                }
                _ => return Some(()),
            },
            _ => return Some(()),
        }
    }
}

/// Consume a block comment body after its opening `/*`. Returns whether it was
/// terminated (`*/` found).
fn skip_block_comment<C: CpCursor>(c: &mut C) -> bool {
    while let Some(ch) = c.bump() {
        if ch == u32::from(b'*') && c.peek(0) == Some(u32::from(b'/')) {
            c.bump();
            return true;
        }
    }
    false
}

/// Number continuation chars (decimal, hex/bin/oct prefixes & digits, exponent,
/// `_` separators, BigInt `n`, and the decimal point). Over-consuming an
/// attached `.member` chain is brace- and capture-safe (numbers hold no
/// brackets, and a member name is not a captured variable).
///
/// NOTE: the exponent SIGN (`+`/`-`) is deliberately NOT here — it is consumed
/// contextually by the number loop (only right after `e`/`E` in a decimal
/// literal). Listing it unconditionally swallowed the operator in `31-eg`
/// (`e` is a hex digit, so `31-e` looked numeric), dropping the reference `eg`
/// and UNDER-escaping its enclosing binding — the react-lib `sX` delazify bug.
fn is_number_part(c: u32) -> bool {
    // `A-F`/`a-f` already cover the `B`/`b`/`E`/`e` of binary prefixes and
    // exponents; only the radix letters outside that range are listed.
    matches!(
        c,
        0x30..=0x39          // 0-9
            | 0x41..=0x46    // A-F
            | 0x61..=0x66    // a-f
            | 0x58 | 0x78    // X x (hex prefix)
            | 0x4F | 0x6F    // O o (octal prefix)
            | 0x6E           // n (BigInt suffix)
            | 0x2E           // .
            | 0x5F           // _ (numeric separator)
    )
}

/// Consume a string literal whose opening quote `q` is the next code point;
/// returns whether it was terminated.
fn scan_string<C: CpCursor>(c: &mut C) -> bool {
    let q = c.bump().expect("opening quote");
    while let Some(ch) = c.bump() {
        if ch == u32::from(b'\\') {
            // Line continuations and all escapes: skip the escaped unit. (A
            // `\<CR><LF>` continuation skips the CR here and the LF next loop —
            // harmless.)
            if c.bump().is_none() {
                return false;
            }
            continue;
        }
        if ch == q {
            return true;
        }
        // A bare line terminator inside a non-template string is a syntax error.
        if is_line_term(ch) {
            return false;
        }
    }
    false
}

/// Consume a regular-expression literal whose opening `/` is the next code
/// point; returns whether it was well-formed (and consumes trailing flags).
fn scan_regex<C: CpCursor>(c: &mut C) -> bool {
    c.bump(); // opening `/`
    let mut in_class = false; // inside a `[...]` character class
    while let Some(ch) = c.bump() {
        if is_line_term(ch) {
            return false; // a regex literal cannot span lines
        }
        if ch == u32::from(b'\\') {
            if c.bump().is_none() {
                return false; // escaped char (incl. `\/`, `\]`)
            }
            continue;
        }
        if ch == u32::from(b'[') {
            in_class = true;
        } else if ch == u32::from(b']') {
            in_class = false;
        } else if ch == u32::from(b'/') && !in_class {
            // End of the body; consume identifier-part flags.
            while c.peek(0).is_some_and(is_ident_part) {
                c.bump();
            }
            return true;
        }
    }
    false
}

/// Outcome of scanning a template segment.
enum TemplateStep {
    /// The template ended (closing backtick consumed).
    Complete,
    /// A `${` opened a substitution (a `TemplateSub` was pushed); scanning
    /// resumes in normal mode.
    Substitution,
}

/// Scan a template literal whose opening backtick is the next code point.
fn scan_template<C: CpCursor>(c: &mut C, stack: &mut Vec<Bracket>) -> Option<TemplateStep> {
    c.bump(); // opening backtick
    scan_template_chars(c, stack)
}

/// Scan template characters until the closing backtick or the next `${`
/// substitution. Handles `\` escapes; bails on EOF.
fn scan_template_chars<C: CpCursor>(
    c: &mut C,
    stack: &mut Vec<Bracket>,
) -> Option<TemplateStep> {
    while let Some(ch) = c.peek(0) {
        if ch == u32::from(b'\\') {
            c.bump();
            if c.bump().is_none() {
                return None;
            }
            continue;
        }
        if ch == u32::from(b'`') {
            c.bump();
            return Some(TemplateStep::Complete);
        }
        if ch == u32::from(b'$') && c.peek(1) == Some(u32::from(b'{')) {
            c.bump();
            c.bump();
            stack.push(Bracket::TemplateSub);
            return Some(TemplateStep::Substitution);
        }
        c.bump();
    }
    None // unterminated template
}

/// Decode a code-point slice into a `String` for keyword comparison. Identifier
/// text is overwhelmingly ASCII; non-ASCII falls back through `char::from_u32`.
fn cp_slice_to_string(cps: &[u32]) -> String {
    cps.iter().filter_map(|&c| char::from_u32(c)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cps(s: &str) -> Vec<u32> {
        s.chars().map(u32::from).collect()
    }

    /// Scan from the first `{` in `s` and return the matched end index, or None.
    fn scan(s: &str) -> Option<usize> {
        let v = cps(s);
        let open = v.iter().position(|&c| c == u32::from(b'{'))?;
        scan_function_body(&v, open).map(|b| b.end)
    }

    #[test]
    fn plain_body() {
        // `{ return 1; }` — end is just past the final `}`.
        assert_eq!(scan("{ return 1; }"), Some(13));
    }

    #[test]
    fn nested_braces_and_objects() {
        let s = "{ var o = { a: { b: 1 } }; return o; }";
        assert_eq!(scan(s), Some(s.chars().count()));
    }

    #[test]
    fn brace_in_string_is_ignored() {
        let s = r#"{ var s = "}"; return s; }"#;
        assert_eq!(scan(s), Some(s.chars().count()));
    }

    #[test]
    fn brace_in_template_and_substitution() {
        let s = "{ var t = `a${ {x:1}.x }b`; return t; }";
        assert_eq!(scan(s), Some(s.chars().count()));
    }

    #[test]
    fn nested_template() {
        let s = "{ return `a${ `b${ 1 }c` }d`; }";
        assert_eq!(scan(s), Some(s.chars().count()));
    }

    #[test]
    fn regex_with_braces_inside() {
        let s = r"{ var re = /a{1,2}[/}]/g; return re; }";
        assert_eq!(scan(s), Some(s.chars().count()));
    }

    #[test]
    fn division_not_regex() {
        let s = "{ return a / b / c; }";
        assert_eq!(scan(s), Some(s.chars().count()));
    }

    #[test]
    fn regex_after_keyword() {
        let s = "{ return /x}y/.test(z); }";
        assert_eq!(scan(s), Some(s.chars().count()));
    }

    #[test]
    fn line_comment_with_brace() {
        let s = "{ // }\n return 1; }";
        assert_eq!(scan(s), Some(s.chars().count()));
    }

    #[test]
    fn block_comment_with_brace() {
        let s = "{ /* } */ return 1; }";
        assert_eq!(scan(s), Some(s.chars().count()));
    }

    #[test]
    fn ambiguous_slash_after_brace_bails() {
        // `}` then `/` is genuinely ambiguous — must bail, not guess.
        let s = "{ if (a) {} /b/.test(c); }";
        assert_eq!(scan(s), None);
    }

    #[test]
    fn with_statement_bails() {
        assert_eq!(scan("{ with (o) { x; } }"), None);
    }

    #[test]
    fn unqualified_eval_bails() {
        assert_eq!(scan("{ return eval(s); }"), None);
        // A member `.eval` is fine (not a direct eval).
        assert_eq!(
            scan("{ return o.eval(s); }"),
            Some("{ return o.eval(s); }".chars().count())
        );
    }

    #[test]
    fn unterminated_bails() {
        assert_eq!(scan("{ var s = 'oops; }"), None);
        assert_eq!(scan("{ return 1; "), None);
        assert_eq!(scan("{ /* nope } "), None);
    }

    #[test]
    fn detects_use_strict_directive() {
        let v = cps(r#"{ "use strict"; return 1; }"#);
        let open = v.iter().position(|&c| c == u32::from(b'{')).unwrap();
        assert!(scan_function_body(&v, open).unwrap().body_strict);

        let v = cps(r#"{ 'use strict' }"#);
        let open = v.iter().position(|&c| c == u32::from(b'{')).unwrap();
        assert!(scan_function_body(&v, open).unwrap().body_strict);

        // Not a directive: it is the start of an expression.
        let v = cps(r#"{ "use strict".length; }"#);
        let open = v.iter().position(|&c| c == u32::from(b'{')).unwrap();
        assert!(!scan_function_body(&v, open).unwrap().body_strict);

        // Escaped: a directive must be the exact code points with no escapes.
        let v = cps(r#"{ "use\x20strict"; }"#);
        let open = v.iter().position(|&c| c == u32::from(b'{')).unwrap();
        assert!(!scan_function_body(&v, open).unwrap().body_strict);
    }

    #[test]
    fn collects_identifier_references_skipping_members() {
        let v = cps("{ return foo + bar.baz; }");
        let open = v.iter().position(|&c| c == u32::from(b'{')).unwrap();
        let scan = scan_function_body(&v, open).unwrap();
        let names: Vec<String> = scan
            .idents
            .iter()
            .map(|cps| super::cp_slice_to_string(cps))
            .collect();
        // `foo` and `bar` are references; `baz` is a member (after `.`),
        // `return` is a keyword.
        assert_eq!(names, vec!["foo", "bar"]);
    }

    fn names_of(s: &str) -> Vec<String> {
        let v = cps(s);
        let open = v.iter().position(|&c| c == u32::from(b'{')).unwrap();
        scan_function_body(&v, open)
            .unwrap()
            .idents
            .iter()
            .map(|cps| super::cp_slice_to_string(cps))
            .collect()
    }

    #[test]
    fn subtraction_of_identifier_after_number_is_captured() {
        // `31-eg` is `31`, `-`, `eg` — NOT a number. `e` is a hex digit, so a
        // sign-swallowing number scan consumed `31-e` and dropped the reference
        // `eg`, under-escaping it (the react-lib fiber-lane `sX` delazify bug).
        assert_eq!(names_of("{ return 31-eg(l); }"), vec!["eg", "l"]);
        // A genuine decimal exponent DOES take a sign and holds no identifier.
        assert_eq!(names_of("{ return 1e-5 + x; }"), vec!["x"]);
        assert_eq!(names_of("{ return 1.5e+10 * y; }"), vec!["y"]);
        // A hex literal ending in `e`/`E` has no exponent: `0x1e-foo` is a
        // subtraction, so `foo` must survive.
        assert_eq!(names_of("{ return 0x1e-foo; }"), vec!["foo"]);
        // Addition of an identifier after a number, likewise.
        assert_eq!(names_of("{ return 3+ab; }"), vec!["ab"]);
    }

    #[test]
    fn contextual_keywords_used_as_identifiers_are_captured() {
        // `of`/`get`/`set`/`async`/`let`/`static`/`yield`/`await` are legal
        // identifiers in some context (minifiers emit `function of(e){…}`), so a
        // reference to one must be collected — dropping it under-escapes the
        // enclosing binding (react-core's `of` delazify bug). True reserved
        // words (`return`, `new`, `typeof`, …) stay uncollected.
        assert_eq!(names_of("{ return of(x); }"), vec!["of", "x"]);
        assert_eq!(names_of("{ return get + set; }"), vec!["get", "set"]);
        assert_eq!(
            names_of("{ return async(let, static); }"),
            vec!["async", "let", "static"]
        );
        assert_eq!(names_of("{ return typeof of; }"), vec!["of"]);
    }
}
