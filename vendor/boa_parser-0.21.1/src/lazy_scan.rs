//! Fallible function-body pre-scan — the lazy-parse "syntax pass".
//!
//! [`scan_function_body`] takes the source as code points and the index of a
//! function body's opening `{` and returns the index just past its matching
//! `}` — WITHOUT building an AST — together with the identifier references in
//! the body (for closed-over-binding capture). It is the cheap boundary-finder
//! a lazy parser uses to *defer* a function body it may never need to compile.
//!
//! It is deliberately FALLIBLE (SpiderMonkey's syntax-parser model): it returns
//! [`None`] ("bail") on any construct whose tokenization it cannot resolve with
//! certainty — an ambiguous regex-vs-division after `}`, an unterminated
//! literal, a bracket mismatch, or EOF before the close. **A bail is never
//! wrong, only slower**: the caller falls back to a full eager parse. The
//! contract the fuzz tests enforce is therefore one-directional — for any real
//! function this returns EITHER `None` OR *exactly* the body span the full
//! parser produces, NEVER a different span.
//!
//! The scan works in flat code-point indices (the lexer counts columns by code
//! point, so a caller can map a span `Position` to/from an index with the same
//! newline rules — see the `lazy_scan` fuzz harness in TRust).

/// The result of a successful body scan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BodyScan {
    /// Index one past the matching `}` — equal to the body span's exclusive end
    /// (`FunctionBody::span().end()` mapped to a flat code-point index).
    pub end: usize,
    /// `(start, end)` code-point ranges of identifier references in the body, a
    /// conservative SUPERSET of its free variables (member-access property
    /// names after `.`/`?.` are excluded; keywords are excluded; object-literal
    /// keys and labels are NOT excluded — over-collecting only over-escapes a
    /// binding, which is safe). Consumed by capturing lazy parse to replay the
    /// outer scope's escape analysis without re-walking the body.
    pub idents: Vec<(usize, usize)>,
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

/// Whether `word` is a reserved word / keyword (so it is not collected as a
/// captured identifier reference).
fn is_keyword(word: &str) -> bool {
    matches!(
        word,
        "await"
            | "break"
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
            | "yield"
            | "let"
            | "static"
            | "async"
            | "get"
            | "set"
            | "of"
    )
}

/// Scan a function body starting at `open` (which must index a `{`), returning
/// the matching close and the identifier references, or [`None`] to bail. See
/// the module docs for the safety contract.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn scan_function_body(src: &[u32], open: usize) -> Option<BodyScan> {
    if src.get(open).copied() != Some(u32::from(b'{')) {
        return None;
    }

    let mut stack: Vec<Bracket> = Vec::with_capacity(16);
    stack.push(Bracket::Brace);
    let mut idents: Vec<(usize, usize)> = Vec::new();
    let mut slash = Slash::Regex; // after `{`, an expression may begin
    // Whether the previously scanned significant token was `.` or `?.`, so the
    // next identifier is a member-access property name (not a variable ref).
    let mut after_dot = false;
    let mut i = open + 1;

    while i < src.len() {
        let c = src[i];

        // Whitespace and line terminators: skip, leaving `slash`/`after_dot`.
        if is_ws(c) || is_line_term(c) {
            i += 1;
            continue;
        }

        // Comments.
        if c == u32::from(b'/') {
            match src.get(i + 1).copied() {
                Some(c1) if c1 == u32::from(b'/') => {
                    i += 2;
                    while i < src.len() && !is_line_term(src[i]) {
                        i += 1;
                    }
                    continue;
                }
                Some(c1) if c1 == u32::from(b'*') => {
                    i += 2;
                    let mut closed = false;
                    while i + 1 < src.len() {
                        if src[i] == u32::from(b'*') && src[i + 1] == u32::from(b'/') {
                            i += 2;
                            closed = true;
                            break;
                        }
                        i += 1;
                    }
                    if !closed {
                        return None; // unterminated block comment
                    }
                    continue;
                }
                _ => {
                    // Division vs. regular expression.
                    match slash {
                        Slash::Regex => {
                            i = scan_regex(src, i)?;
                            slash = Slash::Div;
                            after_dot = false;
                            continue;
                        }
                        Slash::Div => {
                            // `/` or `/=` operator → expression follows.
                            i += if src.get(i + 1).copied() == Some(u32::from(b'=')) {
                                2
                            } else {
                                1
                            };
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
        if c == u32::from(b'"') || c == u32::from(b'\'') {
            i = scan_string(src, i, c)?;
            slash = Slash::Div;
            after_dot = false;
            continue;
        }

        // Template literals (incl. nested `${ ... }`).
        if c == u32::from(b'`') {
            match scan_template(src, i, &mut stack)? {
                TemplateStep::Complete(next) => {
                    i = next;
                    slash = Slash::Div;
                }
                TemplateStep::Substitution(next) => {
                    // Entered a `${` — `stack` got a TemplateSub pushed; resume
                    // normal scanning of the substitution expression.
                    i = next;
                    slash = Slash::Regex;
                }
            }
            after_dot = false;
            continue;
        }

        // Identifiers / keywords.
        if is_ident_start(c) {
            let start = i;
            i += 1;
            while i < src.len() && is_ident_part(src[i]) {
                i += 1;
            }
            // A `\uXXXX` escape inside the identifier — be conservative and bail
            // rather than mis-bound it (rare in real code).
            if i < src.len() && src[i] == u32::from(b'\\') {
                return None;
            }
            let word = cp_slice_to_string(&src[start..i]);
            slash = if keyword_allows_regex(&word) {
                Slash::Regex
            } else {
                Slash::Div
            };
            if !after_dot && !is_keyword(&word) {
                idents.push((start, i));
            }
            after_dot = false;
            continue;
        }

        // Numbers (a digit, or `.` immediately before a digit).
        if is_digit(c) || (c == u32::from(b'.') && src.get(i + 1).copied().is_some_and(is_digit)) {
            i += 1;
            while i < src.len() && is_number_part(src[i]) {
                i += 1;
            }
            slash = Slash::Div;
            after_dot = false;
            continue;
        }

        // Punctuators.
        match c {
            _ if c == u32::from(b'{') => {
                stack.push(Bracket::Brace);
                slash = Slash::Regex;
                after_dot = false;
            }
            _ if c == u32::from(b'}') => match stack.pop() {
                Some(Bracket::TemplateSub) => {
                    // Resume the enclosing template literal after the `}`.
                    match scan_template_tail(src, i + 1, &mut stack)? {
                        TemplateStep::Complete(next) => {
                            i = next;
                            slash = Slash::Div;
                            after_dot = false;
                            continue;
                        }
                        TemplateStep::Substitution(next) => {
                            i = next;
                            slash = Slash::Regex;
                            after_dot = false;
                            continue;
                        }
                    }
                }
                Some(Bracket::Brace) => {
                    if stack.is_empty() {
                        return Some(BodyScan { end: i + 1, idents });
                    }
                    slash = Slash::AfterBrace;
                    after_dot = false;
                }
                _ => return None, // mismatched `}`
            },
            _ if c == u32::from(b'(') => {
                stack.push(Bracket::Paren);
                slash = Slash::Regex;
                after_dot = false;
            }
            _ if c == u32::from(b')') => {
                if stack.pop() != Some(Bracket::Paren) {
                    return None;
                }
                slash = Slash::Div;
                after_dot = false;
            }
            _ if c == u32::from(b'[') => {
                stack.push(Bracket::Square);
                slash = Slash::Regex;
                after_dot = false;
            }
            _ if c == u32::from(b']') => {
                if stack.pop() != Some(Bracket::Square) {
                    return None;
                }
                slash = Slash::Div;
                after_dot = false;
            }
            _ if c == u32::from(b'.') => {
                // `.` or `...`; the next identifier is a property name. (`?.` is
                // handled at `?`.)
                slash = Slash::Regex;
                after_dot = true;
                i += 1;
                continue;
            }
            _ if c == u32::from(b'?') => {
                // `?.` optional chaining → next identifier is a property name.
                if src.get(i + 1).copied() == Some(u32::from(b'.')) {
                    i += 2;
                    slash = Slash::Regex;
                    after_dot = true;
                    continue;
                }
                slash = Slash::Regex;
                after_dot = false;
            }
            // Any other operator/punctuator: an expression may follow.
            _ => {
                slash = Slash::Regex;
                after_dot = false;
            }
        }
        i += 1;
    }

    None // EOF before the body closed
}

/// Number continuation chars (decimal, hex/bin/oct prefixes & digits, exponent,
/// `_` separators, BigInt `n`, and the decimal point). Over-consuming an
/// attached `.member` chain is brace- and capture-safe (numbers hold no
/// brackets, and a member name is not a captured variable).
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
            | 0x2B | 0x2D    // + - (exponent sign; harmless mid-run for valid src)
    )
}

/// Consume a string literal beginning at `open` (the quote `q`); return the
/// index just past the closing quote, or [`None`] if unterminated.
fn scan_string(src: &[u32], open: usize, q: u32) -> Option<usize> {
    let mut i = open + 1;
    while i < src.len() {
        let c = src[i];
        if c == u32::from(b'\\') {
            // Line continuations and all escapes: skip the escaped unit. (A
            // `\<CR><LF>` continuation skips the CR here and the LF next loop —
            // harmless.)
            i += 2;
            continue;
        }
        if c == q {
            return Some(i + 1);
        }
        // A bare line terminator inside a non-template string is a syntax error.
        if is_line_term(c) {
            return None;
        }
        i += 1;
    }
    None
}

/// Consume a regular-expression literal beginning at `open` (the `/`); return
/// the index just past the trailing flags, or [`None`] if malformed.
fn scan_regex(src: &[u32], open: usize) -> Option<usize> {
    let mut i = open + 1;
    let mut in_class = false; // inside a `[...]` character class
    while i < src.len() {
        let c = src[i];
        if is_line_term(c) {
            return None; // a regex literal cannot span lines
        }
        if c == u32::from(b'\\') {
            i += 2; // escaped char (incl. `\/`, `\]`)
            continue;
        }
        if c == u32::from(b'[') {
            in_class = true;
        } else if c == u32::from(b']') {
            in_class = false;
        } else if c == u32::from(b'/') && !in_class {
            // End of the body; consume identifier-part flags.
            i += 1;
            while i < src.len() && is_ident_part(src[i]) {
                i += 1;
            }
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Outcome of scanning a template segment.
enum TemplateStep {
    /// The template ended (closing backtick consumed); index just past it.
    Complete(usize),
    /// A `${` opened a substitution (a `TemplateSub` was pushed); index just
    /// past the `${`, scanning resumes in normal mode.
    Substitution(usize),
}

/// Scan a template literal beginning at `open` (the backtick).
fn scan_template(src: &[u32], open: usize, stack: &mut Vec<Bracket>) -> Option<TemplateStep> {
    scan_template_chars(src, open + 1, stack)
}

/// Resume scanning a template's character run after a substitution's closing
/// `}` (which has already been popped from `stack`). `at` is the index right
/// after that `}`.
fn scan_template_tail(src: &[u32], at: usize, stack: &mut Vec<Bracket>) -> Option<TemplateStep> {
    scan_template_chars(src, at, stack)
}

/// Scan template characters from `at` until the closing backtick or the next
/// `${` substitution. Handles `\` escapes; bails on EOF.
fn scan_template_chars(src: &[u32], at: usize, stack: &mut Vec<Bracket>) -> Option<TemplateStep> {
    let mut i = at;
    while i < src.len() {
        let c = src[i];
        if c == u32::from(b'\\') {
            i += 2;
            continue;
        }
        if c == u32::from(b'`') {
            return Some(TemplateStep::Complete(i + 1));
        }
        if c == u32::from(b'$') && src.get(i + 1).copied() == Some(u32::from(b'{')) {
            stack.push(Bracket::TemplateSub);
            return Some(TemplateStep::Substitution(i + 2));
        }
        i += 1;
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
    fn unterminated_bails() {
        assert_eq!(scan("{ var s = 'oops; }"), None);
        assert_eq!(scan("{ return 1; "), None);
        assert_eq!(scan("{ /* nope } "), None);
    }

    #[test]
    fn collects_identifier_references_skipping_members() {
        let v = cps("{ return foo + bar.baz; }");
        let open = v.iter().position(|&c| c == u32::from(b'{')).unwrap();
        let scan = scan_function_body(&v, open).unwrap();
        let names: Vec<String> = scan
            .idents
            .iter()
            .map(|&(a, b)| super::cp_slice_to_string(&v[a..b]))
            .collect();
        // `foo` and `bar` are references; `baz` is a member (after `.`),
        // `return` is a keyword.
        assert_eq!(names, vec!["foo", "bar"]);
    }
}
