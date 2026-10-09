//! RFC 2229 §3.10.1 OPTION MIME: text responses carry a MIME header (RFC
//! 2045), possibly empty, then a blank line. "The client MUST be able to
//! parse Content-Type and Content-transfer-encoding headers, and MUST be
//! able to ignore textual responses which have an unsupported content or
//! encoding." The defaults are `text/plain; charset=utf-8` with `8bit`.
//!
//! RFC Editor snapshot 2026-09-06: RFC 2045 §5.1 (Content-Type), §6.1–6.2
//! (Content-Transfer-Encoding), §6.7 (quoted-printable), §6.8 (base64);
//! RFC 2046 §5.1.1 (multipart common syntax); RFC 2183 §2.3 (filename).
//! Text parts become the definition's text in any charset the WHATWG
//! Encoding Standard labels name; other parts become attachments the user
//! can save. Everything decoded is bounded by the 1 MiB reply limit.

use std::sync::Arc;

const MAX_DEPTH: usize = 4;
const MAX_PARTS: usize = 64;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Header {
    /// Lowercased `type/subtype`.
    pub content_type: String,
    pub params: Vec<(String, String)>,
    /// Lowercased Content-Transfer-Encoding.
    pub transfer: String,
    /// RFC 2183 §2.3 filename, else the Content-Type `name` parameter.
    pub filename: Option<String>,
}

impl Header {
    /// RFC 2229 §3.10.1: an empty header means the text default.
    pub fn text_default() -> Self {
        Self {
            content_type: "text/plain".into(),
            params: vec![("charset".into(), "utf-8".into())],
            transfer: "8bit".into(),
            filename: None,
        }
    }

    pub fn param(&self, name: &str) -> Option<&str> {
        self.params
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// Plain UTF-8 text that can be shown as it arrives.
    pub fn streams_as_utf8(&self) -> bool {
        self.content_type == "text/plain"
            && matches!(self.transfer.as_str(), "7bit" | "8bit" | "binary")
            && self.param("charset").is_none_or(|c| {
                c.eq_ignore_ascii_case("utf-8") || c.eq_ignore_ascii_case("us-ascii")
            })
    }
}

/// Parse `name: value` lines (RFC 822 folding allowed). Unknown fields are
/// ignored; missing ones take the RFC 2045 defaults.
pub fn parse_header(lines: &[Vec<u8>]) -> Header {
    let mut fields: Vec<(String, String)> = Vec::new();
    for line in lines {
        let text = String::from_utf8_lossy(line);
        if text.starts_with([' ', '\t']) {
            if let Some((_, value)) = fields.last_mut() {
                value.push(' ');
                value.push_str(text.trim());
            }
            continue;
        }
        if let Some((name, value)) = text.split_once(':') {
            fields.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
        }
    }
    let field = |name: &str| {
        fields
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, v)| v.as_str())
    };
    if fields.is_empty() {
        return Header::text_default();
    }
    let (content_type, params) = match field("content-type") {
        Some(value) => {
            let (essence, params) = parameters(value);
            if essence.contains('/') {
                (essence, params)
            } else {
                // RFC 2045 §5.2: an invalid Content-Type means the default.
                (
                    "text/plain".into(),
                    vec![("charset".into(), "us-ascii".into())],
                )
            }
        }
        None => (
            "text/plain".into(),
            vec![("charset".into(), "us-ascii".into())],
        ),
    };
    let transfer = field("content-transfer-encoding")
        .map_or_else(|| "7bit".into(), |v| v.trim().to_ascii_lowercase());
    let filename = field("content-disposition")
        .and_then(|value| {
            let (_, params) = parameters(value);
            params
                .into_iter()
                .find(|(key, _)| key == "filename")
                .map(|(_, v)| v)
        })
        .or_else(|| {
            params
                .iter()
                .find(|(key, _)| key == "name")
                .map(|(_, v)| v.clone())
        });
    Header {
        content_type,
        params,
        transfer,
        filename,
    }
}

/// RFC 2045 §5.1: `type/subtype *(; attribute=value)`, values as tokens or
/// quoted strings.
fn parameters(value: &str) -> (String, Vec<(String, String)>) {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut escaped = false;
    for c in value.chars() {
        if escaped {
            current.push(c);
            escaped = false;
        } else if quoted && c == '\\' {
            escaped = true;
        } else if c == '"' {
            quoted = !quoted;
        } else if c == ';' && !quoted {
            parts.push(std::mem::take(&mut current));
        } else {
            current.push(c);
        }
    }
    parts.push(current);
    let mut parts = parts.into_iter();
    let essence = parts.next().unwrap_or_default().trim().to_ascii_lowercase();
    let params = parts
        .filter_map(|part| {
            let (key, value) = part.split_once('=')?;
            Some((key.trim().to_ascii_lowercase(), value.trim().to_string()))
        })
        .take(32)
        .collect();
    (essence, params)
}

/// RFC 2045 §6.8: decode, ignoring characters outside the alphabet.
pub fn base64(input: &[u8]) -> Vec<u8> {
    let value = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32)
    };
    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    let mut buffer = 0u32;
    let mut bits = 0;
    for &c in input {
        if c == b'=' {
            break;
        }
        let Some(v) = value(c) else { continue };
        buffer = (buffer << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    out
}

/// RFC 2045 §6.7: `=XX` octets, soft line breaks (`=` at the end of a
/// line), and trailing white space deleted from every encoded line.
pub fn quoted_printable(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut lines = input.split(|b| *b == b'\n').peekable();
    while let Some(line) = lines.next() {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let line = line.trim_ascii_end();
        let (line, soft) = match line.strip_suffix(b"=") {
            Some(rest) => (rest, true),
            None => (line, false),
        };
        let mut i = 0;
        while i < line.len() {
            if line[i] == b'='
                && let Some(hex) = line.get(i + 1..i + 3)
                && let Ok(text) = std::str::from_utf8(hex)
                && let Ok(byte) = u8::from_str_radix(text, 16)
            {
                out.push(byte);
                i += 3;
            } else {
                out.push(line[i]);
                i += 1;
            }
        }
        if !soft && lines.peek().is_some() {
            out.extend_from_slice(b"\r\n");
        }
    }
    out
}

fn transfer_decode(header: &Header, body: &[u8]) -> Result<Vec<u8>, String> {
    match header.transfer.as_str() {
        "7bit" | "8bit" | "binary" => Ok(body.to_vec()),
        "quoted-printable" => Ok(quoted_printable(body)),
        "base64" => Ok(base64(body)),
        other => Err(format!("unsupported transfer encoding {other}")),
    }
}

/// Decode text in its declared charset (WHATWG Encoding labels).
fn charset_text(header: &Header, bytes: &[u8]) -> Result<String, String> {
    let label = header.param("charset").unwrap_or("us-ascii");
    let encoding = encoding_rs::Encoding::for_label(label.as_bytes())
        .ok_or_else(|| format!("unsupported charset {label}"))?;
    let (text, _, _) = encoding.decode(bytes);
    Ok(text.replace("\r\n", "\n"))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attachment {
    pub content_type: String,
    pub filename: Option<String>,
    pub bytes: Arc<Vec<u8>>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Decoded {
    pub text: String,
    pub attachments: Vec<Attachment>,
    /// Parts that could not be decoded and were skipped (§3.10.1).
    pub skipped: Vec<String>,
}

/// Decode one MIME entity whose header has been parsed. `body` uses CRLF
/// line breaks, as received.
pub fn decode(header: &Header, body: &[u8]) -> Decoded {
    let mut decoded = Decoded::default();
    decode_into(header, body, 0, &mut decoded);
    decoded
}

fn decode_into(header: &Header, body: &[u8], depth: usize, out: &mut Decoded) {
    if header.content_type.starts_with("multipart/") {
        let Some(boundary) = header
            .param("boundary")
            .filter(|b| !b.is_empty() && b.len() <= 70)
        else {
            out.skipped
                .push(format!("{} without a boundary", header.content_type));
            return;
        };
        if depth >= MAX_DEPTH {
            out.skipped.push("deeply nested multipart".into());
            return;
        }
        for part in multipart_parts(body, boundary).into_iter().take(MAX_PARTS) {
            let split = part
                .windows(4)
                .position(|w| w == b"\r\n\r\n")
                .map(|at| (&part[..at], &part[at + 4..]));
            let (head, content) = match (part.starts_with(b"\r\n"), split) {
                // A part with no header fields at all.
                (true, _) => (&[][..], &part[2..]),
                (false, Some(split)) => split,
                (false, None) => (part, &[][..]),
            };
            let lines: Vec<Vec<u8>> = head
                .split(|b| *b == b'\n')
                .map(|l| l.strip_suffix(b"\r").unwrap_or(l).to_vec())
                .filter(|l| !l.is_empty())
                .collect();
            // RFC 2046 §5.1: a part without Content-Type is text/plain
            // in us-ascii.
            let part_header = if lines.is_empty() {
                Header {
                    params: vec![("charset".into(), "us-ascii".into())],
                    transfer: "7bit".into(),
                    ..Header::text_default()
                }
            } else {
                parse_header(&lines)
            };
            decode_into(&part_header, content, depth + 1, out);
        }
        return;
    }
    let bytes = match transfer_decode(header, body) {
        Ok(bytes) => bytes,
        Err(error) => {
            out.skipped
                .push(format!("{} ({error})", header.content_type));
            return;
        }
    };
    if header.content_type.starts_with("text/") && header.filename.is_none() {
        match charset_text(header, &bytes) {
            Ok(text) => {
                if !out.text.is_empty() && !out.text.ends_with('\n') {
                    out.text.push('\n');
                }
                out.text.push_str(&text);
            }
            Err(error) => out
                .skipped
                .push(format!("{} ({error})", header.content_type)),
        }
    } else {
        out.attachments.push(Attachment {
            content_type: header.content_type.clone(),
            filename: header.filename.clone(),
            bytes: Arc::new(bytes),
        });
    }
}

/// RFC 2046 §5.1.1: body parts between `--boundary` delimiter lines; the
/// preamble and epilogue are discarded.
fn multipart_parts<'a>(body: &'a [u8], boundary: &str) -> Vec<&'a [u8]> {
    let dash = format!("--{boundary}");
    let mut parts = Vec::new();
    let mut start: Option<usize> = None;
    let mut offset = 0usize;
    for line in body.split_inclusive(|b| *b == b'\n') {
        let content = line.strip_suffix(b"\n").unwrap_or(line);
        let content = content.strip_suffix(b"\r").unwrap_or(content);
        // Delimiter lines may carry transport padding (§5.1.1).
        let trimmed = content.trim_ascii_end();
        let close = trimmed == format!("{dash}--").as_bytes();
        if close || trimmed == dash.as_bytes() {
            if let Some(begin) = start {
                // The CRLF before a delimiter belongs to the delimiter.
                let end = offset.saturating_sub(2).max(begin);
                parts.push(&body[begin..end]);
            }
            if close {
                return parts;
            }
            start = Some(offset + line.len());
        }
        offset += line.len();
    }
    // An unterminated last part is still shown.
    if let Some(begin) = start
        && begin < body.len()
    {
        parts.push(&body[begin..]);
    }
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_and_default_headers_follow_rfc_2229_and_2045() {
        assert_eq!(parse_header(&[]), Header::text_default());
        assert!(Header::text_default().streams_as_utf8());
        let header = parse_header(&[
            b"Content-Type: text/plain;".to_vec(),
            b" charset=\"ISO-8859-1\"".to_vec(),
        ]);
        assert_eq!(header.param("charset"), Some("ISO-8859-1"));
        assert_eq!(header.transfer, "7bit");
        assert!(!header.streams_as_utf8());
        let decoded = decode(&header, b"caf\xe9\r\n");
        assert_eq!(decoded.text, "café\n");
    }

    #[test]
    fn transfer_encodings_decode_exactly() {
        assert_eq!(base64(b"aGVs\r\nbG8="), b"hello");
        assert_eq!(base64(b"YQ=="), b"a");
        assert_eq!(
            quoted_printable(b"caf=C3=A9 =\r\nsoft  \r\nhard=3D\r\n"),
            "café soft\r\nhard=\r\n".as_bytes()
        );
        let header = parse_header(&[
            b"Content-Type: text/plain; charset=utf-8".to_vec(),
            b"Content-Transfer-Encoding: quoted-printable".to_vec(),
        ]);
        assert_eq!(decode(&header, b"r=C3=A9sum=C3=A9\r\n").text, "résumé\n");
        let unsupported = parse_header(&[b"Content-Transfer-Encoding: x-uuencode".to_vec()]);
        assert!(!decode(&unsupported, b"begin").skipped.is_empty());
    }

    #[test]
    fn multipart_definitions_split_text_and_attachments() {
        let header = parse_header(&[b"Content-Type: multipart/mixed; boundary=\"b:1\"".to_vec()]);
        let body = b"preamble\r\n--b:1\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nA definition.\r\n--b:1  \r\nContent-Type: image/png\r\nContent-Transfer-Encoding: base64\r\nContent-Disposition: attachment; filename=\"glyph.png\"\r\n\r\niVBORw0K\r\n--b:1\r\n\r\nplain part\r\n--b:1--\r\nepilogue\r\n";
        let decoded = decode(&header, body);
        assert_eq!(decoded.text, "A definition.\nplain part");
        assert_eq!(decoded.attachments.len(), 1);
        let attachment = &decoded.attachments[0];
        assert_eq!(attachment.content_type, "image/png");
        assert_eq!(attachment.filename.as_deref(), Some("glyph.png"));
        assert_eq!(*attachment.bytes, b"\x89PNG\r\n");
        let missing = parse_header(&[b"Content-Type: multipart/mixed".to_vec()]);
        assert!(!decode(&missing, body).skipped.is_empty());
    }
}
