//! RFC 2045 §5.1 / RFC 2046 §4.1.2, local RFC snapshot 2026-09-06.
//! Gemini 0.24.1 overrides the text charset default to UTF-8.

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediaType {
    pub essence: String,
    pub charset: Option<String>,
    pub lang: Option<String>,
}

impl MediaType {
    pub fn parse(value: &str) -> Result<Self, String> {
        parse(value).ok_or_else(|| "Malformed Gemini media type.".into())
    }

    pub fn is_text(&self) -> bool {
        self.essence.starts_with("text/")
    }
    pub fn is_image(&self) -> bool {
        self.essence.starts_with("image/")
    }

    /// Decodes a text body in its `charset` (UTF-8 by default). While the
    /// body is `loading`, an incomplete trailing sequence waits for more
    /// bytes instead of decoding as U+FFFD. A leading byte order mark is
    /// ignored, as Gemini 0.24.1 "Response bodies" asks of clients.
    pub fn decode(&self, bytes: &[u8], loading: bool) -> Result<String, String> {
        let charset = self.charset.as_deref().unwrap_or("utf-8");
        let text = decode_charset(charset, bytes, loading).ok_or_else(|| {
            format!(
                "Unsupported charset: {charset}. Save the original source to decode it elsewhere."
            )
        })?;
        Ok(match text.strip_prefix('\u{feff}') {
            Some(rest) => rest.to_string(),
            None => text,
        })
    }
}

/// IANA character set names for US-ASCII and ISO-8859-1 (RFC 2046 §4.1.2
/// defers to the IANA registry). The WHATWG Encoding Standard folds both into
/// windows-1252, which assigns printable characters to 0x80–0x9F; MIME text
/// must decode them exactly.
const US_ASCII: &[&str] = &[
    "us-ascii",
    "ascii",
    "ansi_x3.4-1968",
    "ansi_x3.4-1986",
    "iso-ir-6",
    "iso_646.irv:1991",
    "iso646-us",
    "us",
    "ibm367",
    "cp367",
    "csascii",
];
const LATIN_1: &[&str] = &[
    "iso-8859-1",
    "iso_8859-1",
    "iso_8859-1:1987",
    "iso-ir-100",
    "latin1",
    "latin-1",
    "l1",
    "ibm819",
    "cp819",
    "csisolatin1",
    "iso8859-1",
    "iso88591",
];

/// Decodes `bytes` in the IANA-named `charset` (already lowercased), or
/// `None` when TRust has no decoder for it.
///
/// The WHATWG Encoding Standard's label table (encoding_rs) supplies every
/// other encoding, with three corrections for MIME semantics:
/// - "UTF-16" and "UTF-32" without an explicit byte order follow the byte
///   order mark and are otherwise big-endian (RFC 2781 §4.3; Unicode
///   D99/D101), where WHATWG maps "utf-16" to little-endian.
/// - ISO-8859 labels that WHATWG resolves to a Windows superset keep
///   0x80–0x9F as the C1 controls those standards assign.
/// - Labels that WHATWG maps to its "replacement" encoding are unsupported
///   rather than one U+FFFD.
fn decode_charset(charset: &str, bytes: &[u8], loading: bool) -> Option<String> {
    if US_ASCII.contains(&charset) {
        return Some(
            bytes
                .iter()
                .map(|&b| if b < 0x80 { char::from(b) } else { '\u{fffd}' })
                .collect(),
        );
    }
    if LATIN_1.contains(&charset) {
        return Some(bytes.iter().map(|&b| char::from(b)).collect());
    }
    match charset {
        "utf-16" | "csutf16" => {
            let (encoding, bom) = match bytes {
                [0xff, 0xfe, ..] => (encoding_rs::UTF_16LE, 2),
                [0xfe, 0xff, ..] => (encoding_rs::UTF_16BE, 2),
                [_] if loading => return Some(String::new()),
                _ => (encoding_rs::UTF_16BE, 0),
            };
            return Some(decode_with(encoding, &bytes[bom..], loading));
        }
        "utf-32" | "csutf32" => return Some(decode_utf32(bytes, None, loading)),
        "utf-32be" | "csutf32be" => return Some(decode_utf32(bytes, Some(true), loading)),
        "utf-32le" | "csutf32le" => return Some(decode_utf32(bytes, Some(false), loading)),
        _ => {}
    }
    let encoding = encoding_rs::Encoding::for_label_no_replacement(charset.as_bytes())?;
    let text = decode_with(encoding, bytes, loading);
    let windows_superset = [
        (
            encoding_rs::WINDOWS_1252,
            &["windows-1252", "cp1252", "x-cp1252"][..],
        ),
        (
            encoding_rs::WINDOWS_1254,
            &["windows-1254", "cp1254", "x-cp1254"],
        ),
        (encoding_rs::WINDOWS_874, &["windows-874", "dos-874"]),
    ]
    .iter()
    .any(|(windows, own_labels)| encoding == *windows && !own_labels.contains(&charset));
    if !windows_superset {
        return Some(text);
    }
    // Single-byte decoders emit exactly one scalar value per byte.
    Some(
        text.chars()
            .zip(bytes)
            .map(|(c, &b)| {
                if (0x80..=0x9f).contains(&b) {
                    char::from(b)
                } else {
                    c
                }
            })
            .collect(),
    )
}

fn decode_with(encoding: &'static encoding_rs::Encoding, bytes: &[u8], loading: bool) -> String {
    let mut decoder = encoding.new_decoder_without_bom_handling();
    let mut text = String::with_capacity(bytes.len());
    let mut read = 0;
    loop {
        let (result, consumed, _) = decoder.decode_to_string(&bytes[read..], &mut text, !loading);
        read += consumed;
        match result {
            encoding_rs::CoderResult::InputEmpty => return text,
            encoding_rs::CoderResult::OutputFull => text.reserve(bytes.len() - read + 16),
        }
    }
}

/// UTF-32 (Unicode D99–D101): `big_endian` is the explicit byte order, or
/// `None` to follow a byte order mark and default to big-endian.
fn decode_utf32(bytes: &[u8], big_endian: Option<bool>, loading: bool) -> String {
    let (big_endian, bytes) = match (big_endian, bytes) {
        (Some(order), _) => (order, bytes),
        (None, [0xff, 0xfe, 0, 0, rest @ ..]) => (false, rest),
        (None, [0, 0, 0xfe, 0xff, rest @ ..]) => (true, rest),
        (None, _) => (true, bytes),
    };
    let whole = bytes.len() - bytes.len() % 4;
    let mut text: String = bytes[..whole]
        .chunks_exact(4)
        .map(|unit| {
            let unit = [unit[0], unit[1], unit[2], unit[3]];
            let scalar = if big_endian {
                u32::from_be_bytes(unit)
            } else {
                u32::from_le_bytes(unit)
            };
            char::from_u32(scalar).unwrap_or('\u{fffd}')
        })
        .collect();
    if whole < bytes.len() && !loading {
        text.push('\u{fffd}');
    }
    text
}

fn token(b: u8) -> bool {
    b.is_ascii_graphic() && !b"()<>@,;:\\\"/[]?=".contains(&b)
}

struct Parser<'a> {
    remaining: &'a str,
}
impl Parser<'_> {
    fn spaces(&mut self) -> Option<()> {
        loop {
            self.remaining = self.remaining.trim_start_matches([' ', '\t']);
            if !self.remaining.starts_with('(') {
                return Some(());
            }
            let mut depth = 0;
            let mut escaped = false;
            let mut end = None;
            for (i, c) in self.remaining.char_indices() {
                if escaped {
                    escaped = false;
                    continue;
                }
                match c {
                    '\\' => escaped = true,
                    '(' => {
                        depth += 1;
                        if depth > 8 {
                            return None;
                        }
                    }
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            end = Some(i + 1);
                            break;
                        }
                    }
                    c if c.is_control() && c != '\t' => return None,
                    _ => {}
                }
            }
            self.remaining = &self.remaining[end?..];
        }
    }
    fn take(&mut self, c: char) -> Option<()> {
        self.spaces()?;
        self.remaining = self.remaining.strip_prefix(c)?;
        Some(())
    }
    fn token(&mut self) -> Option<String> {
        self.spaces()?;
        let n = self.remaining.bytes().take_while(|&b| token(b)).count();
        if n == 0 {
            return None;
        }
        let value = self.remaining[..n].to_string();
        self.remaining = &self.remaining[n..];
        Some(value)
    }
    fn value(&mut self) -> Option<String> {
        self.spaces()?;
        if !self.remaining.starts_with('"') {
            return self.token();
        }
        let mut value = String::new();
        let mut escaped = false;
        for (i, c) in self.remaining[1..].char_indices() {
            if c.is_control() && c != '\t' {
                return None;
            }
            if escaped {
                value.push(c);
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                self.remaining = &self.remaining[i + 2..];
                return Some(value);
            } else {
                value.push(c);
            }
        }
        None
    }
}

fn parse(value: &str) -> Option<MediaType> {
    let mut parser = Parser { remaining: value };
    let top = parser.token()?.to_ascii_lowercase();
    parser.take('/')?;
    let subtype = parser.token()?.to_ascii_lowercase();
    let mut result = MediaType {
        essence: format!("{top}/{subtype}"),
        charset: None,
        lang: None,
    };
    loop {
        parser.spaces()?;
        if parser.remaining.is_empty() {
            return Some(result);
        }
        parser.take(';')?;
        let attribute = parser.token()?.to_ascii_lowercase();
        parser.take('=')?;
        let value = parser.value()?;
        match attribute.as_str() {
            "charset" if result.charset.is_none() => {
                result.charset = Some(value.to_ascii_lowercase())
            }
            "lang" if result.lang.is_none() => result.lang = Some(value),
            _ => {} // Unknown parameters have no effect on rendering.
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mime_tokens_comments_quoted_parameters_and_charsets() {
        let mime = MediaType::parse(
            "TEXT/GEMINI (comment); unknown=\"a;b\\\"c\"; CHARSET=\"UTF-8\"; lang=\"en,de\"",
        )
        .unwrap();
        assert_eq!(mime.essence, "text/gemini");
        assert_eq!(mime.lang.as_deref(), Some("en,de"));
        assert_eq!(
            mime.decode(b"\xef\xbb\xbf# Heading", false).unwrap(),
            "# Heading"
        );
        assert_eq!(
            MediaType::parse("text/plain; charset=ISO-8859-1")
                .unwrap()
                .decode(b"caf\xe9", false)
                .unwrap(),
            "café"
        );
        assert!(
            MediaType::parse("text/plain; charset=unknown")
                .unwrap()
                .decode(b"data", false)
                .is_err()
        );
        assert!(
            MediaType::parse("text/plain; charset=iso-2022-kr")
                .unwrap()
                .decode(b"data", false)
                .is_err(),
            "WHATWG's replacement encoding is not a decoder"
        );
        for invalid in [
            "",
            "text",
            "text/",
            "text/plain;",
            "text/plain; charset=",
            "text/plain; charset=\"UTF-8",
            "text/plain (unclosed",
        ] {
            assert!(MediaType::parse(invalid).is_err(), "{invalid}");
        }
    }

    fn decode(charset: &str, bytes: &[u8], loading: bool) -> Result<String, String> {
        MediaType::parse(&format!("text/gemini; charset={charset}"))
            .unwrap()
            .decode(bytes, loading)
    }

    #[test]
    fn mime_charsets_decode_with_their_registered_semantics() {
        // Legacy multi-byte and single-byte encodings come from the WHATWG
        // label table.
        assert_eq!(
            decode("Shift_JIS", b"\x93\xfa\x96\x7b", false).unwrap(),
            "日本"
        );
        assert_eq!(
            decode("EUC-KR", b"\xc7\xd1\xb1\xdb", false).unwrap(),
            "한글"
        );
        assert_eq!(
            decode("GB18030", b"\xd6\xd0\xce\xc4", false).unwrap(),
            "中文"
        );
        assert_eq!(decode("koi8-r", b"\xf0\xd2\xc9", false).unwrap(), "При");
        assert_eq!(
            decode("windows-1251", b"\xcf\xf0\xe8", false).unwrap(),
            "При"
        );
        // windows-1252 keeps its printable 0x80–0x9F; ISO-8859-1, ISO-8859-9
        // and ISO-8859-11 keep the C1 controls their standards assign.
        assert_eq!(decode("windows-1252", b"\x80", false).unwrap(), "€");
        assert_eq!(decode("latin1", b"\x80\xe9", false).unwrap(), "\u{80}é");
        assert_eq!(decode("iso-8859-9", b"\x80\xfe", false).unwrap(), "\u{80}ş");
        assert_eq!(decode("windows-1254", b"\x80\xfe", false).unwrap(), "€ş");
        assert_eq!(decode("tis-620", b"\x85\xa1", false).unwrap(), "\u{85}ก");
        assert_eq!(decode("us-ascii", b"a\xe9", false).unwrap(), "a\u{fffd}");
        assert_eq!(decode("csASCII", b"ok", false).unwrap(), "ok");
    }

    #[test]
    fn unicode_charsets_follow_their_byte_order_rules() {
        // RFC 2781 §4.3: unmarked "UTF-16" is big-endian; a mark selects the
        // order and is not content.
        assert_eq!(decode("UTF-16", b"\x00h\x00i", false).unwrap(), "hi");
        assert_eq!(
            decode("UTF-16", b"\xff\xfeh\x00i\x00", false).unwrap(),
            "hi"
        );
        assert_eq!(
            decode("UTF-16", b"\xfe\xff\x00h\x00i", false).unwrap(),
            "hi"
        );
        assert_eq!(decode("UTF-16LE", b"h\x00i\x00", false).unwrap(), "hi");
        assert_eq!(decode("UTF-16BE", b"\xfe\xff\x00h", false).unwrap(), "h");
        assert_eq!(decode("UTF-32", b"\x00\x00\x00h", false).unwrap(), "h");
        assert_eq!(
            decode("UTF-32", b"\xff\xfe\x00\x00h\x00\x00\x00", false).unwrap(),
            "h"
        );
        assert_eq!(
            decode("UTF-32LE", b"\x00\xf6\x01\x00", false).unwrap(),
            "😀"
        );
        assert_eq!(
            decode("UTF-32BE", b"\x00\x11\x00\x00", false).unwrap(),
            "\u{fffd}"
        );
    }

    #[test]
    fn incomplete_trailing_sequences_wait_while_loading() {
        for (charset, bytes, partial, done) in [
            ("utf-8", &b"ok\xe6\x97"[..], "ok", "ok\u{fffd}"),
            ("shift_jis", b"ok\x93", "ok", "ok\u{fffd}"),
            ("utf-16be", b"\x00o\x00k\x00", "ok", "ok\u{fffd}"),
            ("utf-32le", b"o\x00\x00\x00k\x00", "o", "o\u{fffd}"),
        ] {
            assert_eq!(decode(charset, bytes, true).unwrap(), partial, "{charset}");
            assert_eq!(decode(charset, bytes, false).unwrap(), done, "{charset}");
        }
        // A lone first byte cannot pick a UTF-16 byte order yet.
        assert_eq!(decode("utf-16", b"\xff", true).unwrap(), "");
    }
}
