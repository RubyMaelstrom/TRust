//! Referrer Policy §8.1–8.4 (recorded W3C snapshot cc435b05, 2026-09-06).
//! This operates on request metadata, not a reconstruction from a destination
//! Document's URL. Redirects carry the already-reduced referrer forward.
use url::{Host, Url};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum ReferrerPolicy {
    NoReferrer,
    NoReferrerWhenDowngrade,
    Origin,
    OriginWhenCrossOrigin,
    SameOrigin,
    StrictOrigin,
    #[default]
    StrictOriginWhenCrossOrigin,
    UnsafeUrl,
}

impl ReferrerPolicy {
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "no-referrer" => Self::NoReferrer,
            "no-referrer-when-downgrade" => Self::NoReferrerWhenDowngrade,
            "origin" => Self::Origin,
            "origin-when-cross-origin" => Self::OriginWhenCrossOrigin,
            "same-origin" => Self::SameOrigin,
            "strict-origin" => Self::StrictOrigin,
            "strict-origin-when-cross-origin" => Self::StrictOriginWhenCrossOrigin,
            "unsafe-url" => Self::UnsafeUrl,
            _ => return None,
        })
    }

    pub fn from_headers(headers: &[(String, String)]) -> Option<Self> {
        // §8.1 keeps the last recognized policy across the header list.
        headers
            .iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case("referrer-policy"))
            .flat_map(|(_, value)| value.split(','))
            .filter_map(|value| Self::parse(value.trim_matches([' ', '\t'])))
            .next_back()
    }

    pub fn determine(self, source: &Url, destination: &Url) -> Option<Url> {
        // Fetch's local schemes have no referrer. This browser-owned HTTP
        // navigation boundary has no other non-HTTP referrer sources.
        if !matches!(source.scheme(), "http" | "https") || self == Self::NoReferrer {
            return None;
        }
        let mut full = source.clone();
        full.set_fragment(None);
        let _ = full.set_username("");
        let _ = full.set_password(None);
        let mut origin = full.clone();
        origin.set_path("/");
        origin.set_query(None);
        if full.as_str().len() > 4096 {
            full = origin.clone();
        }
        let same = source.origin() == destination.origin();
        let downgrade = trustworthy(source) && !trustworthy(destination);
        match self {
            Self::NoReferrer => None,
            Self::UnsafeUrl => Some(full),
            Self::Origin => Some(origin),
            Self::SameOrigin => same.then_some(full),
            Self::OriginWhenCrossOrigin => Some(if same { full } else { origin }),
            Self::NoReferrerWhenDowngrade => (!downgrade).then_some(full),
            Self::StrictOrigin => (!downgrade).then_some(origin),
            Self::StrictOriginWhenCrossOrigin => {
                if same {
                    Some(full)
                } else {
                    (!downgrade).then_some(origin)
                }
            }
        }
    }
}

fn trustworthy(url: &Url) -> bool {
    // Secure Contexts §3.1. Do not grant the optional localhost-name trust
    // exemption without a resolver guarantee; numeric loopback is unambiguous.
    matches!(url.scheme(), "https" | "wss" | "file")
        || match url.host() {
            Some(Host::Ipv4(ip)) => ip.is_loopback(),
            Some(Host::Ipv6(ip)) => ip.is_loopback(),
            _ => false,
        }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn referrer_policy_sanitizes_and_applies_all_eight_policies() {
        let source = Url::parse("https://user:secret@example.test/path?q=1#fragment").unwrap();
        let same = Url::parse("https://example.test/next").unwrap();
        let cross = Url::parse("https://other.test/").unwrap();
        let insecure = Url::parse("http://other.test/").unwrap();
        let full = Some("https://example.test/path?q=1");
        let origin = Some("https://example.test/");
        for (policy, expected) in [
            (ReferrerPolicy::NoReferrer, [None, None, None]),
            (ReferrerPolicy::NoReferrerWhenDowngrade, [full, full, None]),
            (ReferrerPolicy::Origin, [origin, origin, origin]),
            (
                ReferrerPolicy::OriginWhenCrossOrigin,
                [full, origin, origin],
            ),
            (ReferrerPolicy::SameOrigin, [full, None, None]),
            (ReferrerPolicy::StrictOrigin, [origin, origin, None]),
            (
                ReferrerPolicy::StrictOriginWhenCrossOrigin,
                [full, origin, None],
            ),
            (ReferrerPolicy::UnsafeUrl, [full, full, full]),
        ] {
            for (destination, expected) in [&same, &cross, &insecure].into_iter().zip(expected) {
                assert_eq!(
                    policy
                        .determine(&source, destination)
                        .as_ref()
                        .map(Url::as_str),
                    expected,
                    "{policy:?}"
                );
            }
        }
        let long = Url::parse(&format!("https://example.test/{}", "x".repeat(4096))).unwrap();
        assert_eq!(
            ReferrerPolicy::UnsafeUrl
                .determine(&long, &same)
                .as_ref()
                .map(Url::as_str),
            origin
        );
        let loopback = Url::parse("http://127.0.0.1:1234/").unwrap();
        assert!(
            ReferrerPolicy::default()
                .determine(&source, &loopback)
                .is_some()
        );
        assert!(
            ReferrerPolicy::default()
                .determine(&loopback, &insecure)
                .is_none()
        );
        let data = Url::parse("data:text/html,hello").unwrap();
        assert!(ReferrerPolicy::UnsafeUrl.determine(&data, &same).is_none());
        assert_eq!(
            ReferrerPolicy::from_headers(&[(
                "Referrer-Policy".into(),
                "origin, nonsense, no-referrer, UNKNOWN".into()
            )]),
            Some(ReferrerPolicy::NoReferrer)
        );
    }
}
