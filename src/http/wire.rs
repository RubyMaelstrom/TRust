//! Shared HTTP/2 and HTTP/3 field semantics (RFC 9113 §8; RFC 9114 §4).
//! Framing engines validate pseudo-headers and sequencing; browser policy
//! supplies the same ordinary request fields to both transports.

use super::{Headers, Request};
use http_wire::{HeaderMap, HeaderName, HeaderValue, Version};

/// Bound on a decoded field section, counted as RFC 9113 §6.5.2 sizes
/// SETTINGS_MAX_HEADER_LIST_SIZE: name plus value octets plus 32 octets per
/// field line. The per-line overhead also bounds the number of lines (at most
/// 8,192), so no separate field count applies: HTTP sets no limit on how many
/// field lines a section holds (RFC 9110 §5.4), and real responses exceed a
/// few hundred, e.g. CDNs that append one timing field per hop.
pub(super) const HEADER_BYTES: u32 = 256 * 1024;

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
        // Header values are byte sequences (Fetch #header-value).
        let mut value = HeaderValue::from_bytes(&super::isomorphic_encode(value))
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
    let mut headers = Headers::new();
    let mut cookies = Vec::new();
    let mut bytes = 0usize;
    for (name, value) in fields {
        let name = name.as_str();
        let raw = value.as_bytes();
        // RFC 9113 §8.2.1 / RFC 9114 §4.2: a field value MUST NOT contain
        // NUL, LF or CR, nor start or end with SP or HTAB, and
        // connection-specific fields make the message malformed. Other bytes,
        // obs-text included, are kept as they are (Fetch #header-value).
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
            || raw.iter().any(|b| matches!(b, 0 | b'\n' | b'\r'))
        {
            return Err(Error::malformed("malformed HTTP response header"));
        }
        bytes = bytes.saturating_add(name.len() + raw.len() + 32);
        if bytes > HEADER_BYTES as usize {
            return Err(Error::limit("HTTP response headers exceed size limit"));
        }
        let value = super::isomorphic_decode(raw);
        if name == "set-cookie" {
            cookies.push(super::header_value_utf8(&value));
        }
        headers.append(name.to_string(), value);
    }
    Ok((headers, cookies))
}
