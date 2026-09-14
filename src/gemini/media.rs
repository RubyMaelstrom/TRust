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

    pub fn decode(&self, bytes: &[u8], loading: bool) -> Result<String, String> {
        match self.charset.as_deref().unwrap_or("utf-8") {
            "utf-8" | "utf8" => {
                let bytes = if loading {
                    match std::str::from_utf8(bytes) {
                        Err(e) if e.error_len().is_none() => &bytes[..e.valid_up_to()],
                        _ => bytes,
                    }
                } else {
                    bytes
                };
                let text = String::from_utf8_lossy(bytes);
                Ok(text.strip_prefix('\u{feff}').unwrap_or(&text).to_string())
            }
            "us-ascii" | "ascii" => Ok(bytes
                .iter()
                .map(|&b| if b < 128 { char::from(b) } else { '\u{fffd}' })
                .collect()),
            "iso-8859-1" | "latin1" | "latin-1" => {
                Ok(bytes.iter().map(|&b| char::from(b)).collect())
            }
            charset => Err(format!(
                "Unsupported charset: {charset}. Save the original source to decode it elsewhere."
            )),
        }
    }
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
}
