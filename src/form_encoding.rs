//! Legacy output encodings for form submission and URL query strings.
//!
//! A document whose character encoding is not UTF-8 submits forms and parses
//! the query of its URLs in that encoding. Sources, from the local standards
//! library (WHATWG HTML e5071a20c856, URL 55d6699, Encoding a985b62a9b45):
//!
//! - Encoding #encode and #encode-or-fail, #get-an-output-encoding: the
//!   "html" error mode replaces a code point the encoder cannot represent by
//!   the numeric character reference `&#N;`. `encoding_rs` implements every
//!   encoder, including ISO-2022-JP's return to ASCII before such a reference.
//! - URL #string-percent-encode-after-encoding and
//!   #concept-urlencoded-serializer: an unencodable code point becomes
//!   `%26%23N%3B` (the reference, percent-encoded).
//! - URL #concept-basic-url-parser "encoding" (the query encoding override,
//!   used only for special schemes other than ws/wss) and HTML
//!   #encoding-parsing-a-url.
//! - HTML #picking-an-encoding-for-the-form (accept-charset), the
//!   `application/x-www-form-urlencoded`, `multipart/form-data` and
//!   `text/plain` submission bodies (#submit-body).

use std::borrow::Cow;
use std::fmt::Write as _;

use encoding_rs::{EncoderResult, Encoding, UTF_8};

/// Encoding #concept-encoding-get, by label; `None` for an unknown label.
pub(crate) fn encoding_for_label(label: &str) -> Option<&'static Encoding> {
    Encoding::for_label(label.trim().as_bytes())
}

/// Encoding #get-an-output-encoding: replacement, UTF-16BE and UTF-16LE
/// encode as UTF-8.
pub(crate) fn output_encoding(encoding: &'static Encoding) -> &'static Encoding {
    encoding.output_encoding()
}

/// Encoding #encode: `text` in `encoding`'s output encoding with the "html"
/// error mode.
pub(crate) fn encode_html<'a>(text: &'a str, encoding: &'static Encoding) -> Cow<'a, [u8]> {
    let encoding = output_encoding(encoding);
    if encoding == UTF_8 {
        return Cow::Borrowed(text.as_bytes());
    }
    let (bytes, _, _) = encoding.encode(text);
    bytes
}

/// URL #application-x-www-form-urlencoded-percent-encode-set: everything
/// except ASCII alphanumerics, `*`, `-`, `.` and `_`.
fn in_form_urlencoded_set(byte: u8) -> bool {
    !(byte.is_ascii_alphanumeric() || matches!(byte, b'*' | b'-' | b'.' | b'_'))
}

/// URL #string-percent-encode-after-encoding with the
/// `application/x-www-form-urlencoded` percent-encode set (so spaceAsPlus):
/// an unencodable code point becomes "%26%23", its decimal value and "%3B".
pub(crate) fn form_urlencode_after_encoding(encoding: &'static Encoding, input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    encode_or_fail_each(
        output_encoding(encoding),
        input,
        &mut output,
        |bytes, output| {
            for &byte in bytes {
                if byte == b' ' {
                    output.push('+');
                } else if in_form_urlencoded_set(byte) {
                    let _ = write!(output, "%{byte:02X}");
                } else {
                    output.push(char::from(byte));
                }
            }
        },
        |c, output| {
            let _ = write!(output, "%26%23{}%3B", u32::from(c));
        },
    );
    output
}

/// Encoding #encode-or-fail, repeated until the input is consumed as URL
/// #string-percent-encode-after-encoding does: `bytes` receives each run of
/// encoder output and `unmappable` each code point the encoder cannot encode.
fn encode_or_fail_each<T>(
    encoding: &'static Encoding,
    input: &str,
    sink: &mut T,
    mut bytes: impl FnMut(&[u8], &mut T),
    mut unmappable: impl FnMut(char, &mut T),
) {
    if encoding == UTF_8 {
        bytes(input.as_bytes(), sink);
        return;
    }
    let mut encoder = encoding.new_encoder();
    let mut buffer = [0u8; 1024];
    let mut remaining = input;
    loop {
        let (result, read, written) =
            encoder.encode_from_utf8_without_replacement(remaining, &mut buffer, true);
        bytes(&buffer[..written], sink);
        remaining = &remaining[read..];
        match result {
            EncoderResult::InputEmpty => break,
            EncoderResult::OutputFull => {}
            EncoderResult::Unmappable(c) => unmappable(c, sink),
        }
    }
}

/// URL #concept-urlencoded-serializer with an encoding.
pub(crate) fn urlencoded_serialize<'a>(
    pairs: impl IntoIterator<Item = (&'a str, &'a str)>,
    encoding: &'static Encoding,
) -> String {
    let encoding = output_encoding(encoding);
    let mut output = String::new();
    for (name, value) in pairs {
        if !output.is_empty() {
            output.push('&');
        }
        output.push_str(&form_urlencode_after_encoding(encoding, name));
        output.push('=');
        output.push_str(&form_urlencode_after_encoding(encoding, value));
    }
    output
}

/// HTML #picking-an-encoding-for-the-form: the first known label of a form's
/// `accept-charset` attribute (UTF-8 when it names none), otherwise the
/// document's character encoding; then its output encoding.
pub(crate) fn form_submission_encoding(
    accept_charset: Option<&str>,
    document_encoding: &'static Encoding,
) -> &'static Encoding {
    let encoding = match accept_charset {
        Some(input) => input
            .split(|c: char| c.is_ascii_whitespace())
            .filter(|label| !label.is_empty())
            .find_map(|label| Encoding::for_label(label.as_bytes()))
            .unwrap_or(UTF_8),
        None => document_encoding,
    };
    output_encoding(encoding)
}

/// HTML #encoding-parsing-a-url: the URL parser with `encoding` as the
/// query encoding override. The `url` crate applies the override only to
/// http, https, file and ftp URLs, as URL #concept-basic-url-parser requires
/// for every scheme that is special and neither ws nor wss.
pub(crate) fn encoding_parse_url(
    input: &str,
    base: Option<&url::Url>,
    encoding: &'static Encoding,
) -> Result<url::Url, url::ParseError> {
    let encoding = output_encoding(encoding);
    if encoding == UTF_8 {
        return url::Url::options().base_url(base).parse(input);
    }
    // URL #percent-encode-after-encoding with the special-query set: the
    // parser percent-encodes the returned bytes with that same set, which
    // leaves "%", digits and letters unchanged, so an unencodable code point
    // still reaches the query as "%26%23N%3B".
    fn query_encoder<F: for<'a> Fn(&'a str) -> Cow<'a, [u8]>>(encode: F) -> F {
        encode
    }
    let encode = query_encoder(move |text: &str| {
        let mut encoded = Vec::with_capacity(text.len());
        encode_or_fail_each(
            encoding,
            text,
            &mut encoded,
            |bytes, encoded| encoded.extend_from_slice(bytes),
            |c, encoded| encoded.extend_from_slice(format!("%26%23{}%3B", u32::from(c)).as_bytes()),
        );
        Cow::Owned(encoded)
    });
    url::Url::options()
        .base_url(base)
        .encoding_override(Some(&encode))
        .parse(input)
}

/// HTML #convert-to-a-list-of-name-value-pairs and the multipart algorithm:
/// every CR not followed by LF and every LF not preceded by CR becomes CRLF.
pub(crate) fn normalize_newlines(text: &str) -> Cow<'_, str> {
    if !text.contains(['\r', '\n']) {
        return Cow::Borrowed(text);
    }
    let mut output = String::with_capacity(text.len() + 8);
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                output.push_str("\r\n");
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
            }
            '\n' => output.push_str("\r\n"),
            c => output.push(c),
        }
    }
    Cow::Owned(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use encoding_rs::{EUC_JP, ISO_2022_JP, REPLACEMENT, SHIFT_JIS, UTF_16LE, WINDOWS_1252};

    #[test]
    fn html_error_mode_writes_numeric_character_references() {
        // Encoding #encode: "html" error mode.
        assert_eq!(
            &*encode_html("caf\u{e9} \u{3042}", WINDOWS_1252),
            b"caf\xe9 &#12354;"
        );
        assert_eq!(&*encode_html("\u{3042}", SHIFT_JIS), b"\x82\xa0");
        assert_eq!(&*encode_html("\u{1F600}", SHIFT_JIS), b"&#128512;");
        // ISO-2022-JP returns to ASCII before the reference.
        assert_eq!(
            &*encode_html("\u{3042}\u{1F600}", ISO_2022_JP),
            b"\x1b$B$\"\x1b(B&#128512;"
        );
        // UTF-16 and replacement output as UTF-8.
        assert_eq!(&*encode_html("\u{e9}", UTF_16LE), "\u{e9}".as_bytes());
        assert_eq!(&*encode_html("\u{e9}", REPLACEMENT), "\u{e9}".as_bytes());
    }

    #[test]
    fn urlencoded_serializer_encodes_and_escapes_references() {
        // URL #concept-urlencoded-serializer and #string-percent-encode-after-encoding.
        assert_eq!(
            urlencoded_serialize(
                [("q", "caf\u{e9} au lait"), ("x", "a&b=c*-._~")],
                WINDOWS_1252
            ),
            "q=caf%E9+au+lait&x=a%26b%3Dc*-._%7E"
        );
        assert_eq!(
            urlencoded_serialize([("k", "\u{3042}")], WINDOWS_1252),
            "k=%26%2312354%3B"
        );
        assert_eq!(
            urlencoded_serialize([("k", "&#12354;")], WINDOWS_1252),
            "k=%26%2312354%3B"
        );
        assert_eq!(
            urlencoded_serialize([("k", "\u{3042}")], EUC_JP),
            "k=%A4%A2"
        );
        assert_eq!(urlencoded_serialize([("k", "\u{e9}")], UTF_8), "k=%C3%A9");
        assert_eq!(
            urlencoded_serialize([("k", "\u{e9}")], UTF_16LE),
            "k=%C3%A9"
        );
    }

    #[test]
    fn form_encoding_follows_accept_charset_then_the_document() {
        // HTML #picking-an-encoding-for-the-form.
        assert_eq!(form_submission_encoding(None, SHIFT_JIS), SHIFT_JIS);
        assert_eq!(
            form_submission_encoding(Some("bogus  euc-jp utf-8"), UTF_8),
            EUC_JP
        );
        assert_eq!(form_submission_encoding(Some(" bogus "), SHIFT_JIS), UTF_8);
        assert_eq!(form_submission_encoding(Some(""), SHIFT_JIS), UTF_8);
        assert_eq!(form_submission_encoding(None, UTF_16LE), UTF_8);
        assert_eq!(
            form_submission_encoding(Some("latin1"), UTF_8),
            WINDOWS_1252
        );
    }

    #[test]
    fn encoding_parse_applies_the_query_encoding_to_special_schemes() {
        // HTML #encoding-parsing-a-url and URL's query state.
        let base = url::Url::parse("http://example.com/dir/page").unwrap();
        let parse = |input, encoding| {
            encoding_parse_url(input, Some(&base), encoding)
                .unwrap()
                .to_string()
        };
        assert_eq!(
            parse("?q=\u{e9}", WINDOWS_1252),
            "http://example.com/dir/page?q=%E9"
        );
        assert_eq!(
            parse("a\u{e9}?q=\u{e9}#\u{e9}", WINDOWS_1252),
            "http://example.com/dir/a%C3%A9?q=%E9#%C3%A9"
        );
        assert_eq!(
            parse("?q=\u{3042}", WINDOWS_1252),
            "http://example.com/dir/page?q=%26%2312354%3B"
        );
        assert_eq!(
            parse("?q=\u{3042}", SHIFT_JIS),
            "http://example.com/dir/page?q=%82%A0"
        );
        assert_eq!(
            parse("?q='\u{e9}", UTF_8),
            "http://example.com/dir/page?q=%27%C3%A9"
        );
        // ws, wss and non-special schemes always use UTF-8.
        assert_eq!(parse("ws://h/?q=\u{e9}", WINDOWS_1252), "ws://h/?q=%C3%A9");
        assert_eq!(parse("foo:/x?q=\u{e9}", WINDOWS_1252), "foo:/x?q=%C3%A9");
        assert_eq!(
            parse("https://h/?q=\u{e9}", WINDOWS_1252),
            "https://h/?q=%E9"
        );
    }

    #[test]
    fn newlines_normalize_to_crlf() {
        assert_eq!(normalize_newlines("a\rb\nc\r\nd"), "a\r\nb\r\nc\r\nd");
        assert_eq!(normalize_newlines("\n\r"), "\r\n\r\n");
    }
}
