//! RFC 7838 §§2–3, 5–6, 9.4 (RFC Editor snapshot 2026-09-06).
//! Parse only authenticated HTTPS advertisements for the final `h3` ALPN.
//! Alternatives change the connection endpoint, never the HTTP origin.

use super::{FetchTiming, Headers};
use std::time::{Duration, SystemTime};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Alternative {
    pub(super) host: String,
    pub(super) port: u16,
}

impl Alternative {
    pub(super) fn authority(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
    pub(super) fn dns_host(&self) -> &str {
        self.host.trim_matches(['[', ']'])
    }
}

// HTTP quoted-string, including quoted-pair; commas and semicolons inside
// quotes are not separators. Reject dangling escapes/unbalanced quotes.
fn split(value: &str, separator: u8) -> Option<Vec<&str>> {
    let mut result = Vec::new();
    let (mut quoted, mut escaped, mut start) = (false, false, 0);
    for (i, byte) in value.bytes().enumerate() {
        if escaped {
            escaped = false;
            continue;
        }
        match byte {
            b'\\' if quoted => escaped = true,
            b'"' => quoted = !quoted,
            b if b == separator && !quoted => {
                result.push(value[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    if quoted || escaped {
        return None;
    }
    result.push(value[start..].trim());
    Some(result)
}

fn token(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

fn unquote(value: &str) -> Option<String> {
    let value = value.strip_prefix('"')?.strip_suffix('"')?;
    let mut result = Vec::with_capacity(value.len());
    let mut escaped = false;
    for b in value.bytes() {
        if b < 0x20 && b != b'\t' || b == 0x7f {
            return None;
        }
        if escaped {
            result.push(b);
            escaped = false;
        } else if b == b'\\' {
            escaped = true;
        } else if b == b'"' {
            return None;
        } else {
            result.push(b);
        }
    }
    if escaped {
        return None;
    }
    String::from_utf8(result).ok()
}

fn authority(value: &str, origin: &str) -> Option<Alternative> {
    let (host, port) = value.rsplit_once(':')?;
    if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let port = port.parse().ok().filter(|port| *port != 0)?;
    let host = if host.is_empty() { origin } else { host };
    // URI hosts, not URLs, credentials, paths, zones, or backslash aliases.
    if host.bytes().any(|b| b <= 0x20 || b"/@?#\\".contains(&b)) {
        return None;
    }
    let host = url::Host::parse(host).ok()?.to_string();
    Some(Alternative { host, port })
}

/// A received field replaces prior alternatives even when it contains none
/// we support. `clear` wins over other entries, including a mixed bad reply.
pub(super) fn parse(value: &str, origin: &str, age: Duration) -> Option<(Alternative, Duration)> {
    let entries = split(value, b',')?;
    if entries.contains(&"clear") {
        return None;
    }
    for entry in entries {
        let Some(parts) = split(entry, b';') else {
            continue;
        };
        let Some((protocol, endpoint)) = parts[0].split_once('=') else {
            continue;
        };
        if protocol != "h3" {
            continue;
        }
        let Some(endpoint) = unquote(endpoint).and_then(|value| authority(&value, origin)) else {
            continue;
        };
        let (mut max_age, mut seen_age, mut valid) = (86400u64, false, true);
        for parameter in parts.iter().skip(1) {
            let Some((name, value)) = parameter.split_once('=') else {
                valid = false;
                break;
            };
            let name = name.trim();
            let value = value.trim();
            let value = if value.starts_with('"') {
                unquote(value)
            } else {
                token(value).then(|| value.to_owned())
            };
            let Some(value) = value.filter(|_| token(name)) else {
                valid = false;
                break;
            };
            if name.eq_ignore_ascii_case("ma") {
                if seen_age || value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
                    valid = false;
                    break;
                }
                seen_age = true;
                // RFC 9111 §1.2.2: saturate overflowing delta-seconds.
                max_age = value.parse().unwrap_or(u64::MAX);
            }
            // Unknown parameters MUST be ignored. `persist` is a hint only;
            // this in-memory cache never survives a browser restart.
        }
        // A finite local retention cap also avoids Instant overflow; clients
        // may evict alternatives before their advertised freshness expires.
        let remaining = Duration::from_secs(max_age)
            .saturating_sub(age)
            .min(Duration::from_secs(7 * 86400));
        if valid && !remaining.is_zero() {
            return Some((endpoint, remaining));
        }
    }
    None
}

/// RFC 9111 §4.2.3: subtract age, transit and body-consumption time. Called
/// on a network response, never when replaying a response from our cache.
pub(super) fn response_age(headers: &Headers, timing: &FetchTiming, now: SystemTime) -> Duration {
    let age = headers
        .get("age")
        .and_then(|s| {
            (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
                .then(|| s.parse::<u64>().unwrap_or(u64::MAX))
        })
        .unwrap_or(0);
    let apparent = headers
        .get("date")
        .and_then(|s| httpdate::parse_http_date(s).ok())
        .and_then(|date| now.duration_since(date).ok())
        .unwrap_or_default();
    let delay_ms = (crate::performance::now_ms() - timing.request_start).max(0.0);
    apparent
        .max(Duration::from_secs(age).saturating_add(Duration::from_secs_f64(delay_ms / 1000.0)))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn selected(value: &str) -> Option<(String, u64)> {
        parse(value, "example.com", Duration::from_secs(30))
            .map(|(a, ttl)| (a.authority(), ttl.as_secs()))
    }
    #[test]
    fn http3_advertisements_obey_alpn_preference_freshness_and_quoting() {
        assert_eq!(
            selected("h3-29=\":443\", h3=\":8443\"; ma=60; x=\"a,b;c\", h3=\":443\""),
            Some(("example.com:8443".into(), 30))
        );
        assert_eq!(
            selected("h3=\"alt.example:443\"; ma=\"90\"; persist=2"),
            Some(("alt.example:443".into(), 60))
        );
        assert_eq!(
            selected("h3=\"[::1]:443\""),
            Some(("[::1]:443".into(), 86370))
        );
        assert_eq!(
            selected("h3=\":443\"; ma=0, h3=\":444\"; ma=40"),
            Some(("example.com:444".into(), 10))
        );
        assert_eq!(selected("h3=\":443\"; ma=30"), None);
        assert_eq!(selected("h3=\":443\", clear"), None);
        assert_eq!(
            selected("h3=\":443\"; x=\"clear\""),
            Some(("example.com:443".into(), 86370))
        );
        for bad in [
            "%683=\":443\"",
            "H3=\":443\"",
            "h3=\":0\"",
            "h3=\"user@host:443\"",
            "h3=\"host/path:443\"",
            "h3=\"host:443\"; ma=10; ma=80",
            "h3=\":443\"; ma=-1",
            "h3=\":443\"; x=\"dangling\\\"",
        ] {
            assert_eq!(selected(bad), None, "{bad}");
        }
    }
}
