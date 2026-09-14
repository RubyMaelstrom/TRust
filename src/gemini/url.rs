//! RFC 3986 §§3, 5.2 (RFC Editor snapshot 2026-09-06), and Gemini 0.24.1
//! Requests. URI components remain separate; only the request serializer may
//! expose a sensitive query. Display/Debug are safe for browser chrome.

use std::fmt;

#[derive(Clone, PartialEq, Eq)]
pub struct GeminiUrl {
    pub host: String,
    pub port: u16,
    pub path: String,
    query: Option<String>,
    fragment: Option<String>,
    sensitive: bool,
}

impl fmt::Display for GeminiUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.serialize(!self.sensitive, true))
    }
}

impl fmt::Debug for GeminiUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("GeminiUrl").field(&self.to_string()).finish()
    }
}

impl GeminiUrl {
    /// Construct a target from connection coordinates. `request` validates
    /// even constructed/mutated targets before any network activity.
    pub fn new(host: impl Into<String>, port: u16, resource: impl Into<String>) -> Self {
        let host = host.into();
        let host = if let Ok(ip) = host.trim_matches(['[', ']']).parse::<std::net::Ipv6Addr>() {
            ip.to_string()
        } else {
            ::url::Host::parse(&host).map_or(host, |h| h.to_string().to_ascii_lowercase())
        };
        let resource = resource.into();
        let (path, query, fragment) = components(&resource);
        Self {
            host,
            port,
            path: if path.is_empty() {
                "/".into()
            } else {
                path.into()
            },
            query: query.map(str::to_owned),
            fragment: fragment.map(str::to_owned),
            sensitive: false,
        }
    }

    pub fn parse(input: &str) -> Option<Self> {
        let (scheme, rest) = input.split_once("://")?;
        if !scheme.eq_ignore_ascii_case("gemini")
            || input.len() > 16384
            || input.chars().any(|c| c.is_control() || c.is_whitespace())
        {
            return None;
        }
        let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let authority = &rest[..end];
        if authority.is_empty() || authority.contains('@') {
            return None;
        }
        let (host, port) = if let Some(ip) = authority.strip_prefix('[') {
            let (ip, tail) = ip.split_once(']')?;
            let ip = ip.parse::<std::net::Ipv6Addr>().ok()?;
            let port = if tail.is_empty() {
                1965
            } else {
                parse_port(tail.strip_prefix(':')?)?
            };
            (ip.to_string(), port)
        } else {
            let (host, port) = match authority.split_once(':') {
                Some((host, port)) => (host, parse_port(port)?),
                None => (authority, 1965),
            };
            let host = url::Host::parse(host)
                .ok()?
                .to_string()
                .to_ascii_lowercase();
            if host.is_empty() || host.contains(['/', '\\', ':']) {
                return None;
            }
            (host, port)
        };
        let (path, query, fragment) = components(&rest[end..]);
        Some(Self {
            host,
            port,
            path: if path.is_empty() {
                "/".into()
            } else {
                uri_component(path, true)?
            },
            query: query.map(|q| uri_component(q, false)).transpose_option()?,
            fragment: fragment
                .map(|q| uri_component(q, false))
                .transpose_option()?,
            sensitive: false,
        })
    }

    fn serialize(&self, query: bool, fragment: bool) -> String {
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        let port = if self.port == 1965 {
            String::new()
        } else {
            format!(":{}", self.port)
        };
        let mut out = format!("gemini://{host}{port}{}", self.path);
        if query && let Some(query) = &self.query {
            out.push('?');
            out.push_str(query);
        }
        if fragment && let Some(fragment) = &self.fragment {
            out.push('#');
            out.push_str(fragment);
        }
        out
    }

    pub fn request(&self) -> Result<Vec<u8>, String> {
        let wire = self.serialize(true, false);
        if wire.len() > 1024 {
            return Err(
                "Gemini request exceeds the 1,024-byte URI limit (after percent-encoding).".into(),
            );
        }
        // Do not let public connection/path fields bypass URI validation or
        // silently change the resource which the user chose.
        let valid = Self::parse(&wire).is_some_and(|u| u.serialize(true, false) == wire);
        if !valid {
            return Err("Invalid Gemini request URI.".into());
        }
        Ok(format!("{wire}\r\n").into_bytes())
    }

    pub fn with_input(&self, input: &str, sensitive: bool) -> Result<Self, String> {
        let mut next = self.clone();
        next.query = Some(super::encode_query(input));
        next.fragment = None;
        next.sensitive = sensitive;
        next.request()?;
        Ok(next)
    }

    /// Drop secrets before committing documents, history, exports or bookmarks.
    pub fn public_url(&self) -> Self {
        let mut public = self.clone();
        if public.sensitive {
            public.query = None;
            public.fragment = None;
        }
        public.sensitive = false;
        public
    }

    pub(super) fn inherit_sensitivity(&mut self, base: &Self) {
        if base.sensitive {
            self.sensitive = true;
        }
    }

    pub fn query(&self) -> Option<&str> {
        self.query.as_deref()
    }
    pub fn fragment(&self) -> Option<&str> {
        self.fragment.as_deref()
    }

    pub(super) fn resolve(&self, reference: &str, redirect: bool) -> Option<Self> {
        if reference.starts_with("//") {
            let mut next = Self::parse(&format!("gemini:{reference}"))?;
            next.path = remove_dot_segments(&next.path);
            return Some(next);
        }
        let (path, query, fragment) = components(reference);
        if !path.starts_with('/')
            && path
                .split('/')
                .next()
                .is_some_and(|segment| segment.contains(':'))
        {
            return None;
        }
        let mut next = self.clone();
        next.fragment = fragment
            .map(|s| uri_component(s, false))
            .transpose_option()?;
        next.query = if path.is_empty() && query.is_none() && !redirect {
            self.query.clone()
        } else {
            query.map(|s| uri_component(s, false)).transpose_option()?
        };
        if !path.is_empty() {
            let path = uri_component(path, true)?;
            let merged = if path.starts_with('/') {
                path
            } else {
                let directory = self.path.rsplit_once('/').map_or("/", |(d, _)| d);
                format!("{directory}/{path}")
            };
            next.path = remove_dot_segments(&merged);
        }
        // Sensitivity follows inherited query data, never an unrelated link.
        next.sensitive = self.sensitive && path.is_empty() && query.is_none() && !redirect;
        Some(next)
    }
}

/// RFC 3986 URI-reference character repertoire; validate escapes without
/// decoding delimiters. Component-specific parsing/resolution follows this.
pub(super) fn valid_reference(value: &str) -> bool {
    let mut bytes = value.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'%' {
            if !bytes.next().is_some_and(|b| b.is_ascii_hexdigit())
                || !bytes.next().is_some_and(|b| b.is_ascii_hexdigit())
            {
                return false;
            }
        } else if !(byte.is_ascii_alphanumeric() || b"-._~:/?#[]@!$&'()*+,;=".contains(&byte)) {
            return false;
        }
    }
    true
}

fn parse_port(port: &str) -> Option<u16> {
    if port.is_empty() {
        return Some(1965);
    }
    port.bytes()
        .all(|b| b.is_ascii_digit())
        .then(|| port.parse().ok())
        .flatten()
}

trait TransposeOption<T> {
    fn transpose_option(self) -> Option<Option<T>>;
}
impl<T> TransposeOption<T> for Option<Option<T>> {
    fn transpose_option(self) -> Option<Option<T>> {
        match self {
            None => Some(None),
            Some(v) => v.map(Some),
        }
    }
}

fn components(value: &str) -> (&str, Option<&str>, Option<&str>) {
    let (rest, fragment) = value
        .split_once('#')
        .map_or((value, None), |(p, f)| (p, Some(f)));
    let (path, query) = rest
        .split_once('?')
        .map_or((rest, None), |(p, q)| (p, Some(q)));
    (path, query, fragment)
}

/// Accept IRIs at the address surface by encoding non-ASCII UTF-8. Reject
/// control/space characters, invalid escapes, and delimiters in the wrong component.
fn uri_component(value: &str, path: bool) -> Option<String> {
    let mut out = String::new();
    let mut bytes = value.bytes();
    while let Some(b) = bytes.next() {
        match b {
            b'%' => {
                let (a, b) = (bytes.next()?, bytes.next()?);
                if !a.is_ascii_hexdigit() || !b.is_ascii_hexdigit() {
                    return None;
                }
                out.push('%');
                out.push(a as char);
                out.push(b as char);
            }
            b if b >= 128 => out.push_str(&format!("%{b:02X}")),
            b if b.is_ascii_alphanumeric()
                || b"-._~!$&'()*+,;=:@/".contains(&b)
                || (!path && b == b'?') =>
            {
                out.push(b as char)
            }
            _ => return None,
        }
    }
    Some(out)
}

/// Literal translation of RFC 3986 §5.2.4. Empty path segments are significant.
pub(super) fn remove_dot_segments(mut input: &str) -> String {
    let mut output = String::new();
    while !input.is_empty() {
        if let Some(rest) = input
            .strip_prefix("../")
            .or_else(|| input.strip_prefix("./"))
        {
            input = rest;
        } else if input.starts_with("/./") {
            input = &input[2..];
        } else if input == "/." {
            input = "/";
        } else if input.starts_with("/../") || input == "/.." {
            input = if input == "/.." { "/" } else { &input[3..] };
            output.truncate(output.rfind('/').unwrap_or(0));
        } else if input == "." || input == ".." {
            input = "";
        } else {
            let start = usize::from(input.starts_with('/'));
            let end = input[start..].find('/').map_or(input.len(), |i| start + i);
            output.push_str(&input[..end]);
            input = &input[end..];
        }
    }
    output
}

pub(super) fn normalize(resource: &str) -> String {
    let (path, query, fragment) = components(resource);
    let mut out = remove_dot_segments(path);
    if let Some(query) = query {
        out.push('?');
        out.push_str(query);
    }
    if let Some(fragment) = fragment {
        out.push('#');
        out.push_str(fragment);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3986_reference_corpus_and_component_boundaries() {
        let base = GeminiUrl::parse("gemini://a/b/c/d;p?q").unwrap();
        for (reference, resource) in [
            ("g", "/b/c/g"),
            ("./g", "/b/c/g"),
            ("g/", "/b/c/g/"),
            ("/g", "/g"),
            ("?y", "/b/c/d;p?y"),
            ("g?y", "/b/c/g?y"),
            ("#s", "/b/c/d;p?q#s"),
            ("g#s", "/b/c/g#s"),
            ("g?y#s", "/b/c/g?y#s"),
            (";x", "/b/c/;x"),
            ("g;x", "/b/c/g;x"),
            ("g;x?y#s", "/b/c/g;x?y#s"),
            ("", "/b/c/d;p?q"),
            (".", "/b/c/"),
            ("./", "/b/c/"),
            ("..", "/b/"),
            ("../", "/b/"),
            ("../g", "/b/g"),
            ("../..", "/"),
            ("../../g", "/g"),
            ("../../../g", "/g"),
            ("../../../../g", "/g"),
            ("/./g", "/g"),
            ("/../g", "/g"),
            ("g.", "/b/c/g."),
            (".g", "/b/c/.g"),
            ("g..", "/b/c/g.."),
            ("..g", "/b/c/..g"),
            ("./../g", "/b/g"),
            ("./g/.", "/b/c/g/"),
            ("g/./h", "/b/c/g/h"),
            ("g/../h", "/b/c/h"),
            ("g;x=1/./y", "/b/c/g;x=1/y"),
            ("g;x=1/../y", "/b/c/y"),
            ("g?y/./x", "/b/c/g?y/./x"),
            ("g?y/../x", "/b/c/g?y/../x"),
            ("g#s/./x", "/b/c/g#s/./x"),
            ("g#s/../x", "/b/c/g#s/../x"),
            ("/a//b", "/a//b"),
            ("/%2e/%2e%2e/x", "/%2e/%2e%2e/x"),
        ] {
            assert_eq!(
                base.resolve(reference, false).unwrap().to_string(),
                format!("gemini://a{resource}"),
                "{reference}"
            );
        }
        assert_eq!(
            base.resolve("#s", true).unwrap().to_string(),
            "gemini://a/b/c/d;p#s",
            "redirects do not inherit a query"
        );
        assert_eq!(
            base.resolve("//other:1966/x", false).unwrap().to_string(),
            "gemini://other:1966/x"
        );
    }

    #[test]
    fn validates_requests_and_serializes_ipv6_and_fragments() {
        let base = GeminiUrl::parse("gemini://a/dir/page").unwrap();
        assert_eq!(
            base.resolve("//other/a/../b", false).unwrap().to_string(),
            "gemini://other/b"
        );
        assert!(base.resolve("1bad:reference", false).is_none());
        assert!(GeminiUrl::new("EXAMPLE.ORG", 1965, "/").request().is_ok());
        let url = GeminiUrl::parse("GEMINI://[::1]:1966/é?q#fragment").unwrap();
        assert_eq!(url.host, "::1");
        assert_eq!(url.request().unwrap(), b"gemini://[::1]:1966/%C3%A9?q\r\n");
        assert_eq!(
            GeminiUrl::parse("gemini://EXAMPLE.org#frag").unwrap().host,
            "example.org"
        );
        for invalid in [
            "gemini://",
            "gemini://:1965/",
            "gemini://u@e/",
            "gemini://e:bad/",
            "gemini://[::1/",
            "gemini://e/a\r\nsecond",
            "gemini://e/%zz",
            "gemini://e/a b",
        ] {
            assert!(GeminiUrl::parse(invalid).is_none(), "{invalid:?}");
        }
        let base = GeminiUrl::parse("gemini://e/").unwrap();
        let overhead = base.request().unwrap().len() - 2 + 1;
        assert!(base.with_input(&"a".repeat(1024 - overhead), false).is_ok());
        assert!(
            base.with_input(&"a".repeat(1025 - overhead), false)
                .is_err()
        );
        let invalid = GeminiUrl::new("e", 1965, "/x\r\nsecond");
        assert!(invalid.request().is_err());
    }

    #[test]
    fn secret_query_only_reaches_request_serialization() {
        let base = GeminiUrl::parse("gemini://e/input?previous#old").unwrap();
        let secret = base.with_input("private value", true).unwrap();
        assert_eq!(
            secret.request().unwrap(),
            b"gemini://e/input?private%20value\r\n"
        );
        assert_eq!(secret.to_string(), "gemini://e/input");
        assert!(!format!("{secret:?}").contains("private"));
        assert_eq!(secret.public_url().query(), None);
        assert_eq!(secret.resolve("next", false).unwrap().query(), None);
    }
}
