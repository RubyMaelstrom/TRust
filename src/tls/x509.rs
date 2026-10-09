//! The few X.509 certificate fields TRust's trust-on-first-use policies show
//! and compare: validity, subject common name and subject alternative names.
//! RFC Editor snapshot 2026-09-06: RFC 5280 §4.1 (Certificate, TBSCertificate),
//! §4.1.2.5 (Validity; UTCTime §4.1.2.5.1, GeneralizedTime §4.1.2.5.2),
//! §4.1.2.6 (Subject) and §4.2.1.6 (Subject Alternative Name); X.690 DER
//! definite-length encoding. Name matching follows RFC 9525 §§6.3–6.4, with
//! the RFC 6125 §6.4.4 common-name fallback for certificates that carry no
//! subject alternative names (common among Gemini's self-signed capsules).
//!
//! This reader never decides trust by itself: a malformed or unusual
//! certificate simply yields fewer facts.

/// One DER element: its tag and contents.
struct Element<'a> {
    tag: u8,
    body: &'a [u8],
}

/// Read the element at the start of `input`; return it and what follows.
fn element(input: &[u8]) -> Option<(Element<'_>, &[u8])> {
    let (&tag, rest) = input.split_first()?;
    // Multi-byte tag numbers never occur in the fields read here.
    if tag & 0x1f == 0x1f {
        return None;
    }
    let (&first, rest) = rest.split_first()?;
    let (length, rest) = if first < 0x80 {
        (usize::from(first), rest)
    } else {
        let count = usize::from(first & 0x7f);
        // DER forbids the indefinite form; 4 octets bound any certificate.
        if count == 0 || count > 4 || rest.len() < count {
            return None;
        }
        let length = rest[..count]
            .iter()
            .fold(0usize, |n, b| (n << 8) | usize::from(*b));
        (length, &rest[count..])
    };
    if rest.len() < length {
        return None;
    }
    Some((
        Element {
            tag,
            body: &rest[..length],
        },
        &rest[length..],
    ))
}

fn children(mut body: &[u8]) -> impl Iterator<Item = Element<'_>> {
    std::iter::from_fn(move || {
        let (item, rest) = element(body)?;
        body = rest;
        Some(item)
    })
}

const SEQUENCE: u8 = 0x30;
const SET: u8 = 0x31;
const OID: u8 = 0x06;
const UTC_TIME: u8 = 0x17;
const GENERALIZED_TIME: u8 = 0x18;
const OCTET_STRING: u8 = 0x04;
const EXTENSIONS: u8 = 0xa3;
/// id-at-commonName 2.5.4.3
const COMMON_NAME: &[u8] = &[0x55, 0x04, 0x03];
/// id-ce-subjectAltName 2.5.29.17
const SUBJECT_ALT_NAME: &[u8] = &[0x55, 0x1d, 0x11];

/// Facts about one certificate. Times are seconds since the Unix epoch.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Certificate {
    pub not_before: Option<i64>,
    pub not_after: Option<i64>,
    pub common_name: Option<String>,
    pub dns_names: Vec<String>,
    pub ip_addresses: Vec<std::net::IpAddr>,
}

/// Days since 1970-01-01 for a proleptic Gregorian date (H. Hinnant's
/// days_from_civil).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// RFC 5280 §4.1.2.5.1–2: `YYMMDDHHMMSSZ` or `YYYYMMDDHHMMSSZ`.
fn time(item: &Element<'_>) -> Option<i64> {
    let text = std::str::from_utf8(item.body).ok()?;
    let digits = text.strip_suffix('Z')?;
    if !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let (year, rest) = match (item.tag, digits.len()) {
        (UTC_TIME, 12) => {
            let yy: i64 = digits[..2].parse().ok()?;
            (if yy >= 50 { 1900 + yy } else { 2000 + yy }, &digits[2..])
        }
        (GENERALIZED_TIME, 14) => (digits[..4].parse().ok()?, &digits[4..]),
        _ => return None,
    };
    let field = |n: usize| -> Option<i64> { rest.get(n * 2..n * 2 + 2)?.parse().ok() };
    let (month, day, hour, minute, second) =
        (field(0)?, field(1)?, field(2)?, field(3)?, field(4)?);
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    Some(days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second)
}

fn common_name(name: &Element<'_>) -> Option<String> {
    // Name ::= SEQUENCE OF RelativeDistinguishedName (SET OF
    // AttributeTypeAndValue); the most specific (last) CN is the name.
    let mut found = None;
    for rdn in children(name.body).filter(|e| e.tag == SET) {
        for attribute in children(rdn.body).filter(|e| e.tag == SEQUENCE) {
            let mut parts = children(attribute.body);
            if let (Some(oid), Some(value)) = (parts.next(), parts.next())
                && oid.tag == OID
                && oid.body == COMMON_NAME
            {
                // PrintableString, UTF8String, IA5String, TeletexString...
                found = Some(String::from_utf8_lossy(value.body).into_owned());
            }
        }
    }
    found
}

fn alternative_names(certificate: &mut Certificate, extensions: &Element<'_>) {
    let Some((list, _)) = element(extensions.body) else {
        return;
    };
    for extension in children(list.body).filter(|e| e.tag == SEQUENCE) {
        let mut parts = children(extension.body);
        let Some(oid) = parts.next() else { continue };
        if oid.tag != OID || oid.body != SUBJECT_ALT_NAME {
            continue;
        }
        // extnValue is the last OCTET STRING (critical BOOLEAN may precede).
        let Some(value) = parts.filter(|e| e.tag == OCTET_STRING).last() else {
            continue;
        };
        let Some((names, _)) = element(value.body) else {
            continue;
        };
        for name in children(names.body).take(256) {
            match name.tag {
                // dNSName [2] IA5String
                0x82 => {
                    if let Ok(dns) = std::str::from_utf8(name.body) {
                        certificate.dns_names.push(dns.to_ascii_lowercase());
                    }
                }
                // iPAddress [7] OCTET STRING
                0x87 => match name.body.len() {
                    4 => certificate.ip_addresses.push(std::net::IpAddr::from(
                        <[u8; 4]>::try_from(name.body).unwrap(),
                    )),
                    16 => certificate.ip_addresses.push(std::net::IpAddr::from(
                        <[u8; 16]>::try_from(name.body).unwrap(),
                    )),
                    _ => {}
                },
                _ => {}
            }
        }
    }
}

/// RFC 5280 §4.1: read validity and names from a DER certificate.
pub fn parse(der: &[u8]) -> Option<Certificate> {
    let (certificate, _) = element(der)?;
    if certificate.tag != SEQUENCE {
        return None;
    }
    let (tbs, _) = element(certificate.body)?;
    if tbs.tag != SEQUENCE {
        return None;
    }
    let mut fields = children(tbs.body).peekable();
    // version [0] EXPLICIT is optional.
    if fields.peek().is_some_and(|e| e.tag == 0xa0) {
        fields.next();
    }
    let _serial = fields.next()?;
    let _signature = fields.next()?;
    let _issuer = fields.next()?;
    let validity = fields.next()?;
    let subject = fields.next()?;
    let _key = fields.next()?;
    let mut parsed = Certificate::default();
    if validity.tag == SEQUENCE {
        let mut times = children(validity.body);
        parsed.not_before = times.next().as_ref().and_then(time);
        parsed.not_after = times.next().as_ref().and_then(time);
    }
    if subject.tag == SEQUENCE {
        parsed.common_name = common_name(&subject);
    }
    for field in fields {
        if field.tag == EXTENSIONS {
            alternative_names(&mut parsed, &field);
        }
    }
    Some(parsed)
}

impl Certificate {
    /// RFC 9525 §6.3 (DNS-IDs, one complete left-most wildcard label) and
    /// §6.4 (IP-IDs); the common name only when no alternative name exists.
    pub fn names_host(&self, host: &str) -> bool {
        let host = host.trim_matches(['[', ']']).to_ascii_lowercase();
        if let Ok(ip) = host.parse::<std::net::IpAddr>() {
            return self.ip_addresses.contains(&ip)
                || (self.dns_names.is_empty()
                    && self.ip_addresses.is_empty()
                    && self.common_name.as_deref() == Some(host.as_str()));
        }
        let matches = |pattern: &str| {
            let pattern = pattern.trim_end_matches('.').to_ascii_lowercase();
            let host = host.trim_end_matches('.');
            match pattern.strip_prefix("*.") {
                Some(suffix) if !suffix.contains('*') && suffix.contains('.') => host
                    .split_once('.')
                    .is_some_and(|(label, rest)| !label.is_empty() && rest == suffix),
                _ => !pattern.contains('*') && pattern == host,
            }
        };
        if self.dns_names.is_empty() && self.ip_addresses.is_empty() {
            return self.common_name.as_deref().is_some_and(matches);
        }
        self.dns_names.iter().any(|name| matches(name))
    }

    /// Names this certificate presents, for display.
    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.dns_names.clone();
        names.extend(self.ip_addresses.iter().map(ToString::to_string));
        if names.is_empty()
            && let Some(name) = &self.common_name
        {
            names.push(name.clone());
        }
        names
    }
}

/// A UTC calendar date for display, `YYYY-MM-DD`.
pub fn date(seconds: i64) -> String {
    // civil_from_days (H. Hinnant)
    let days = seconds.div_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_validity_names_and_common_name_from_generated_certificates() {
        let mut params =
            rcgen::CertificateParams::new(vec!["capsule.example".into(), "*.gem.example".into()])
                .unwrap();
        params
            .subject_alt_names
            .push(rcgen::SanType::IpAddress(std::net::IpAddr::from([
                192, 0, 2, 7,
            ])));
        params.distinguished_name = rcgen::DistinguishedName::new();
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "Capsule");
        params.not_before = rcgen::date_time_ymd(2049, 12, 31);
        params.not_after = rcgen::date_time_ymd(2051, 1, 2);
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = params.self_signed(&key).unwrap();
        let parsed = parse(cert.der()).unwrap();
        // 2049 uses UTCTime and 2051 GeneralizedTime (RFC 5280 §4.1.2.5).
        assert_eq!(date(parsed.not_before.unwrap()), "2049-12-31");
        assert_eq!(date(parsed.not_after.unwrap()), "2051-01-02");
        assert_eq!(parsed.common_name.as_deref(), Some("Capsule"));
        assert!(parsed.names_host("CAPSULE.example"));
        assert!(parsed.names_host("a.gem.example"));
        assert!(!parsed.names_host("gem.example"));
        assert!(!parsed.names_host("a.b.gem.example"));
        assert!(parsed.names_host("192.0.2.7"));
        assert!(
            !parsed.names_host("Capsule"),
            "a CN is ignored when SANs exist"
        );
    }

    #[test]
    fn common_name_only_certificates_and_garbage_are_handled() {
        let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        params.distinguished_name = rcgen::DistinguishedName::new();
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "old.example");
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = params.self_signed(&key).unwrap();
        let parsed = parse(cert.der()).unwrap();
        assert!(parsed.names_host("old.example"));
        assert_eq!(parsed.names(), ["old.example"]);
        assert!(parse(b"").is_none());
        assert!(parse(&[0x30, 0x84, 0xff, 0xff, 0xff, 0xff]).is_none());
        assert!(parse(&cert.der()[..40]).is_none());
        assert_eq!(date(0), "1970-01-01");
        assert_eq!(date(951_782_400), "2000-02-29");
    }
}
