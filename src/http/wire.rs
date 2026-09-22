//! Shared HTTP/2 and HTTP/3 field semantics (RFC 9113 §8; RFC 9114 §4).
//! Framing engines validate pseudo-headers and sequencing; browser policy
//! supplies the same ordinary request fields to both transports.

use super::{Headers, Request};
use http_wire::{HeaderMap, HeaderName, HeaderValue, Version};

pub(super) const HEADER_BYTES: u32 = 256 * 1024;
pub(super) const HEADER_FIELDS: usize = 256;

pub(super) struct Error {
    pub(super) message: String,
    pub(super) malformed: bool,
}

impl Error {
    fn malformed(message: &str) -> Self {
        Self {
            message: message.into(),
            malformed: true,
        }
    }
    fn limit(message: &str) -> Self {
        Self {
            message: message.into(),
            malformed: false,
        }
    }
}

pub(super) fn request(
    request: &Request,
    origin: Option<&str>,
    download: bool,
    version: Version,
) -> Result<http_wire::Request<()>, Error> {
    // Strip userinfo/fragment without decoding or re-encoding the path.
    let url = &request.url;
    let uri = format!(
        "{}://{}{}",
        url.scheme(),
        &url[url::Position::BeforeHost..url::Position::AfterPort],
        &url[url::Position::BeforePath..url::Position::AfterQuery]
    );
    let mut wire = http_wire::Request::builder()
        .method(request.method.as_str())
        .uri(uri)
        .version(version)
        .body(())
        .map_err(|_| Error::malformed("invalid HTTP request"))?;
    for (name, value) in super::request_headers(request, origin) {
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| Error::malformed("invalid HTTP request header name"))?;
        let value = if download && name == "accept-encoding" {
            "identity"
        } else {
            value.trim_matches([' ', '\t'])
        };
        let mut value = HeaderValue::from_str(value)
            .map_err(|_| Error::malformed("invalid HTTP request header value"))?;
        // HPACK honors never-indexed; the HTTP/3 encoder uses stateless
        // QPACK (no dynamic table) for all fields, including credentials.
        value.set_sensitive(matches!(
            name.as_str(),
            "cookie" | "authorization" | "proxy-authorization"
        ));
        wire.headers_mut().append(name, value);
    }
    Ok(wire)
}

pub(super) fn response_headers(fields: &HeaderMap) -> Result<(Headers, Vec<String>), Error> {
    if fields.len() > HEADER_FIELDS {
        return Err(Error::limit("too many HTTP response headers"));
    }
    let mut headers = Headers::new();
    let mut cookies = Vec::new();
    let mut bytes = 0usize;
    for (name, value) in fields {
        let name = name.as_str();
        let raw = value.as_bytes();
        if matches!(
            name,
            "connection"
                | "proxy-connection"
                | "keep-alive"
                | "transfer-encoding"
                | "upgrade"
                | "te"
        ) || raw.first().is_some_and(|b| matches!(b, b' ' | b'\t'))
            || raw.last().is_some_and(|b| matches!(b, b' ' | b'\t'))
            || raw.iter().any(|b| (*b < 0x20 && *b != b'\t') || *b == 0x7f)
        {
            return Err(Error::malformed("malformed HTTP response header"));
        }
        bytes = bytes.saturating_add(name.len() + raw.len() + 32);
        if bytes > HEADER_BYTES as usize {
            return Err(Error::limit("HTTP response headers exceed size limit"));
        }
        let value = String::from_utf8_lossy(raw);
        if name == "set-cookie" {
            cookies.push(value.to_string());
        }
        headers
            .entry(name.to_string())
            .and_modify(|existing: &mut String| {
                existing.push_str(", ");
                existing.push_str(&value);
            })
            .or_insert_with(|| value.into_owned());
    }
    Ok((headers, cookies))
}
