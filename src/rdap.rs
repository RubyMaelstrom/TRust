//! RDAP discovery and readable registration records, with the original JSON
//! retained separately. HTTP remains the transport, including direct links.
//! RFC Editor snapshot 2026-09-06: RFC 9224 §§3–8 (bootstrap matching/cache),
//! RFC 9082 §3.1 (paths), RFC 7480 §§4–5 (HTTP), RFC 9111 §4.2 (freshness).
//! `about:rdap?query=…` is TRust's internal action address, not a wire protocol.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime};

use serde_json::Value;
use url::Url;

use crate::doc::{Doc, Kind, Link};
use crate::http;
use crate::registration::{self, Record, Section};

mod record;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Page {
    pub url: Url,
    pub status: u16,
    pub raw: Arc<[u8]>,
    pub record: Arc<Record>,
    pub section: Section,
    pub view: crate::text_reply::View,
    json: Arc<str>,
}

pub fn is_response(response: &http::Response) -> bool {
    response
        .content_type
        .split(';')
        .next()
        .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("application/rdap+json"))
}

impl Page {
    pub fn view_action(
        &mut self,
        action: &str,
        enabled: Option<bool>,
    ) -> Result<&'static str, &'static str> {
        if action == "changes" {
            return Err("Change comparison is available for Finger and WHOIS replies.");
        }
        crate::text_reply::view_action(&mut self.view, action, enabled, false)
    }

    pub fn from_response(response: http::Response) -> Self {
        let source = response.url.as_str();
        let parsed = if response.body.len() <= LIMIT {
            serde_json::from_slice::<Value>(&response.body)
                .ok()
                .filter(Value::is_object)
        } else {
            None
        };
        let mut record = parsed.as_ref().map(|json| record::parse(json, &response.url)).unwrap_or_else(|| {
            let mut record = Record { title: "RDAP response".into(), ..Default::default() };
            record.notice(if response.body.len() > LIMIT { "Reply exceeds the 1 MiB structured display limit. The received data remains available to save." } else { "The server did not return a valid RDAP JSON object. The original reply remains available." });
            record
        });
        if response.status >= 400 {
            record.notice(&format!("HTTP {}", response.status));
        }
        if let Some((_, retry)) = response
            .headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case("retry-after"))
        {
            record.notice(&format!("Retry-After: {retry}"));
        }
        record.add(
            Section::Details,
            "Response URL",
            source,
            source,
            Some(Link::Http(response.url.clone())),
        );
        record.add(
            Section::Details,
            "HTTP status",
            &response.status.to_string(),
            source,
            None,
        );
        let json = parsed
            .as_ref()
            .and_then(pretty)
            .unwrap_or_else(|| String::from_utf8_lossy(&response.body).into_owned());
        Self {
            url: response.url,
            status: response.status,
            raw: response.body.into(),
            record: Arc::new(record),
            section: Section::Summary,
            view: crate::text_reply::View {
                wrap: true,
                ..Default::default()
            },
            json: json.into(),
        }
    }

    pub fn export(&self) -> crate::download::DownloadOffer {
        crate::download::DownloadOffer::from_bytes(
            self.url.clone(),
            "rdap.json".into(),
            self.raw.to_vec(),
        )
    }
}

// Pretty-print through a bounded writer: indentation of deeply nested JSON
// must not multiply a small network response into an unbounded allocation.
fn pretty(value: &Value) -> Option<String> {
    struct Bounded(Vec<u8>);
    impl std::io::Write for Bounded {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if self.0.len().saturating_add(bytes.len()) > LIMIT * 2 {
                return Err(std::io::Error::other("JSON display limit"));
            }
            self.0.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut output = Bounded(Vec::new());
    serde_json::to_writer_pretty(&mut output, value).ok()?;
    String::from_utf8(output.0).ok()
}

pub fn render(page: Page, width: usize) -> Doc {
    render_with_columns(page, width, width >= 52)
}

pub fn render_with_columns(page: Page, width: usize, columns: bool) -> Doc {
    let width = width.max(2);
    let mut lines = Vec::new();
    registration::line(
        &mut lines,
        Kind::Heading(1),
        &format!("RDAP · {}", registration::clean(&page.record.title)),
        None,
        width,
    );
    if page.section != Section::Summary {
        registration::line(
            &mut lines,
            Kind::OtherLink,
            "‹ Record summary",
            Some(Section::Summary.action()),
            width,
        );
    }
    registration::line(&mut lines, Kind::Text, "", None, width);
    if page.section == Section::Raw {
        let (text, clipped) = crate::text_reply::display_text(page.json.as_bytes(), false);
        for line in text.lines() {
            registration::line(
                &mut lines,
                Kind::Pre,
                line,
                None,
                if page.view.wrap { width } else { usize::MAX },
            );
        }
        if clipped || lines.len() >= crate::text_reply::MAX_ROWS - 2 {
            lines.truncate(crate::text_reply::MAX_ROWS - 8);
            registration::line(
                &mut lines,
                Kind::Info,
                "Display truncated; S saves the complete original JSON.",
                None,
                width,
            );
        }
    } else {
        page.record
            .render_with_columns(page.section, width, columns, &mut lines);
    }
    registration::navigation(&mut lines, page.section, true, width);
    registration::line(
        &mut lines,
        Kind::Quote,
        "S save original JSON · W wrap",
        None,
        width,
    );
    let mut doc = Doc::from_lines(
        Link::Http(page.url.clone()),
        lines,
        page.raw.to_vec(),
        width,
        false,
        Some("application/rdap+json".into()),
    );
    doc.rdap = Some(page);
    doc
}

const LIMIT: usize = 1024 * 1024;
const PREFIX: &str = "about:rdap?";

#[derive(Clone, Debug, PartialEq, Eq)]
enum Query {
    Domain(String),
    Ip(IpAddr, Option<u8>),
    Asn(u32),
}

impl Query {
    fn parse(input: &str) -> Option<Self> {
        let input = input.trim();
        if input.is_empty() || input.len() > 1024 || input.chars().any(char::is_control) {
            return None;
        }
        if let Ok(ip) = input.parse() {
            return Some(Self::Ip(ip, None));
        }
        if let Some((address, length)) = input.split_once('/') {
            let ip: IpAddr = address.parse().ok()?;
            let length: u8 = length.parse().ok()?;
            if length > bits(ip) {
                return None;
            }
            return Some(Self::Ip(ip, Some(length)));
        }
        let number = input
            .get(..2)
            .filter(|prefix| prefix.eq_ignore_ascii_case("as"))
            .map_or(input, |_| &input[2..]);
        if !number.is_empty() && number.bytes().all(|ch| ch.is_ascii_digit()) {
            return number.parse().ok().map(Self::Asn);
        }
        let url::Host::Domain(domain) = url::Host::parse(input.trim_end_matches('.')).ok()? else {
            return None;
        };
        if domain.is_empty()
            || domain.split('.').any(|label| {
                label.is_empty()
                    || label.len() > 63
                    || label.starts_with('-')
                    || label.ends_with('-')
                    || !label
                        .bytes()
                        .all(|ch| ch.is_ascii_alphanumeric() || ch == b'-')
            })
        {
            return None;
        }
        Some(Self::Domain(domain.to_ascii_lowercase()))
    }

    fn file(&self) -> &'static str {
        match self {
            Self::Domain(_) => "dns",
            Self::Ip(IpAddr::V4(_), _) => "ipv4",
            Self::Ip(IpAddr::V6(_), _) => "ipv6",
            Self::Asn(_) => "asn",
        }
    }

    fn rank(&self, entry: &str) -> Option<usize> {
        match self {
            Self::Domain(domain) => {
                if entry.is_empty() {
                    Some(0)
                } else if domain == entry
                    || domain
                        .strip_suffix(entry)
                        .is_some_and(|prefix| prefix.ends_with('.'))
                {
                    Some(entry.split('.').count())
                } else {
                    None
                }
            }
            Self::Ip(ip, query_length) => {
                let (network, length) = entry.split_once('/')?;
                let network: IpAddr = network.parse().ok()?;
                let length: u8 = length.parse().ok()?;
                if ip.is_ipv4() != network.is_ipv4() || length > query_length.unwrap_or(bits(*ip)) {
                    return None;
                }
                let shift = u32::from(bits(*ip) - length);
                let mask = if shift == 128 { 0 } else { u128::MAX << shift };
                ((number(*ip) & mask) == (number(network) & mask)).then_some(length as usize)
            }
            Self::Asn(asn) => {
                let (first, last) = entry.split_once('-')?;
                let first: u32 = first.parse().ok()?;
                let last: u32 = last.parse().ok()?;
                (first <= *asn && *asn <= last).then_some(0)
            }
        }
    }

    fn append(&self, mut base: Url) -> Url {
        {
            let mut path = base.path_segments_mut().expect("HTTP base");
            path.pop_if_empty();
            match self {
                Self::Domain(domain) => {
                    path.push("domain").push(domain);
                }
                Self::Ip(ip, prefix) => {
                    path.push("ip").push(&ip.to_string());
                    if let Some(prefix) = prefix {
                        path.push(&prefix.to_string());
                    }
                }
                Self::Asn(asn) => {
                    path.push("autnum").push(&asn.to_string());
                }
            }
        }
        base
    }
}

fn bits(ip: IpAddr) -> u8 {
    if ip.is_ipv4() { 32 } else { 128 }
}
fn number(ip: IpAddr) -> u128 {
    match ip {
        IpAddr::V4(ip) => u32::from(ip) as u128,
        IpAddr::V6(ip) => u128::from(ip),
    }
}

pub fn action(query: &str) -> Option<Link> {
    Query::parse(query)?;
    let mut address = Url::parse("about:rdap").expect("internal address");
    address.query_pairs_mut().append_pair("query", query);
    Some(Link::External(address.into()))
}

/// An explicitly typed RDAP link (or reload) keeps the RDAP Accept header and
/// presenter even when a service uses the generic application/json type.
pub fn direct_action(url: &Url) -> Link {
    let mut action = Url::parse("about:rdap").expect("internal address");
    action.query_pairs_mut().append_pair("url", url.as_str());
    Link::External(action.into())
}

fn direct_query(address: &str) -> Option<Url> {
    let action = Url::parse(address).ok()?;
    let pairs: Vec<_> = action.query_pairs().collect();
    if !is_action(address) || pairs.len() != 1 || pairs[0].0 != "url" {
        return None;
    }
    let url = Url::parse(&pairs[0].1).ok()?;
    (matches!(url.scheme(), "http" | "https")
        && url.host().is_some()
        && url.username().is_empty()
        && url.password().is_none())
    .then_some(url)
}

pub fn is_action(address: &str) -> bool {
    address.starts_with(PREFIX)
}

pub fn command_target(arguments: &str, current: Option<&str>) -> Result<Link, String> {
    let words = crate::command::quoted_arguments(arguments)?;
    let query = match words.as_slice() {
        [] => current.ok_or("usage: rdap <domain|IP|CIDR|AS-number>")?,
        [query] => query,
        _ => return Err("usage: rdap <domain|IP|CIDR|AS-number>".into()),
    };
    action(query).ok_or_else(|| "RDAP supports a domain, IP address, CIDR, or AS number.".into())
}

fn action_query(address: &str) -> Option<Query> {
    if !is_action(address) {
        return None;
    }
    let url = Url::parse(address).ok()?;
    let mut query = url.query_pairs().filter(|(name, _)| name == "query");
    let value = query.next()?.1;
    if query.next().is_some() {
        return None;
    }
    Query::parse(&value)
}

fn endpoints(registry: &Value, query: &Query) -> Result<Vec<Url>, String> {
    let mut longest = None;
    let mut urls = Vec::new();
    for service in registry
        .get("services")
        .and_then(Value::as_array)
        .ok_or("Invalid RDAP bootstrap registry.")?
    {
        let Some(pair) = service.as_array().filter(|pair| pair.len() == 2) else {
            continue;
        };
        let Some(entries) = pair[0].as_array() else {
            continue;
        };
        let Some(rank) = entries
            .iter()
            .filter_map(Value::as_str)
            .filter_map(|entry| query.rank(entry))
            .max()
        else {
            continue;
        };
        if longest.is_some_and(|longest| rank < longest) {
            continue;
        }
        if longest != Some(rank) {
            urls.clear();
            longest = Some(rank);
        }
        if let Some(candidates) = pair[1].as_array() {
            for candidate in candidates.iter().filter_map(Value::as_str) {
                let Ok(base) = Url::parse(candidate) else {
                    continue;
                };
                if !matches!(base.scheme(), "http" | "https")
                    || base.host().is_none()
                    || !base.username().is_empty()
                    || base.password().is_some()
                    || base.query().is_some()
                    || base.fragment().is_some()
                    || !base.path().ends_with('/')
                {
                    continue;
                }
                if urls.len() < 16 && !urls.contains(&base) {
                    urls.push(base);
                }
            }
        }
    }
    if urls.is_empty() {
        return Err("No authoritative RDAP service is listed for this query.".into());
    }
    urls.sort_by_key(|url| url.scheme() != "https");
    Ok(urls.into_iter().map(|url| query.append(url)).collect())
}

struct Cached {
    registry: Arc<Value>,
    expires: Instant,
}
// Only the four fixed IANA namespaces can enter this map. No per-query cache,
// polling task, or disk persistence is needed.
static CACHE: LazyLock<Mutex<HashMap<&'static str, Cached>>> = LazyLock::new(Default::default);

fn freshness(headers: &[(String, String)], received: SystemTime, delay: Duration) -> Duration {
    let header = |name: &str| {
        headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    };
    let mut max_age = None;
    for value in headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("cache-control"))
        .flat_map(|(_, value)| value.split(','))
    {
        let (name, value) = value
            .trim()
            .split_once('=')
            .map_or((value.trim(), None), |(name, value)| {
                (name.trim(), Some(value.trim().trim_matches('"')))
            });
        if name.eq_ignore_ascii_case("no-store") || name.eq_ignore_ascii_case("no-cache") {
            return Duration::ZERO;
        }
        if name.eq_ignore_ascii_case("max-age") {
            if max_age.is_some() {
                return Duration::ZERO;
            }
            let Some(seconds) = value.and_then(|value| value.parse::<u64>().ok()) else {
                return Duration::ZERO;
            };
            max_age = Some(Duration::from_secs(seconds));
        }
    }
    if header("vary").is_some_and(|value| {
        value.split(',').any(|name| {
            !matches!(
                name.trim().to_ascii_lowercase().as_str(),
                "accept" | "accept-encoding"
            )
        })
    }) {
        return Duration::ZERO;
    }
    let date = header("date")
        .and_then(|date| httpdate::parse_http_date(date).ok())
        .unwrap_or(received);
    let lifetime = match max_age {
        Some(lifetime) => lifetime,
        None => match header("expires") {
            Some(expires) => httpdate::parse_http_date(expires)
                .ok()
                .and_then(|expires| expires.duration_since(date).ok())
                .unwrap_or_default(),
            // A short heuristic for an otherwise cacheable 200 registry.
            None => Duration::from_secs(600),
        },
    };
    let age = match header("age") {
        Some(age) => match age.trim().parse::<u64>() {
            Ok(seconds) => Duration::from_secs(seconds),
            Err(_) => return Duration::ZERO,
        },
        None => Duration::ZERO,
    };
    let initial_age = received
        .duration_since(date)
        .unwrap_or_default()
        .max(age.saturating_add(delay));
    lifetime
        .saturating_sub(initial_age)
        .min(Duration::from_secs(86400))
}

async fn registry(file: &'static str) -> Result<Arc<Value>, String> {
    if let Some(registry) = CACHE
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get(file)
        .filter(|entry| Instant::now() < entry.expires)
        .map(|entry| entry.registry.clone())
    {
        return Ok(registry);
    }
    let url = Url::parse(&format!("https://data.iana.org/rdap/{file}.json"))
        .expect("fixed IANA endpoint");
    let mut request = http::Request::get(url);
    request
        .headers
        .push(("Accept".into(), "application/json".into()));
    let started = Instant::now();
    let response = http::fetch(&request).await?;
    let lifetime = freshness(&response.headers, SystemTime::now(), started.elapsed());
    if response.status != 200 || response.body.len() > LIMIT {
        return Err("Could not load a bounded IANA RDAP bootstrap registry.".into());
    }
    let registry: Arc<Value> = Arc::new(
        serde_json::from_slice(&response.body)
            .map_err(|error| format!("Invalid RDAP bootstrap JSON: {error}"))?,
    );
    if !lifetime.is_zero() {
        CACHE
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(
                file,
                Cached {
                    registry: registry.clone(),
                    expires: Instant::now() + lifetime,
                },
            );
    }
    Ok(registry)
}

pub async fn fetch_action(address: &str) -> Result<http::Response, String> {
    let direct = direct_query(address);
    let query = action_query(address);
    if direct.is_none() && query.is_none() {
        return Err("RDAP supports a domain, IP address, CIDR, or AS number.".into());
    }
    tokio::time::timeout(Duration::from_secs(15), async {
        let urls = if let Some(url) = direct {
            vec![url]
        } else {
            let query = query.expect("validated action");
            let registry = registry(query.file()).await?;
            endpoints(&registry, &query)?
        };
        let mut last_error = "RDAP service unavailable.".to_string();
        for url in urls {
            let mut request = http::Request::get(url);
            request.headers.push((
                "Accept".into(),
                "application/rdap+json, application/json".into(),
            ));
            match http::fetch(&request).await {
                Ok(response) => {
                    if response.body.len() > LIMIT {
                        return Err("RDAP reply exceeds the 1 MiB display limit.".into());
                    }
                    let mime = response
                        .content_type
                        .split(';')
                        .next()
                        .unwrap_or("")
                        .trim()
                        .to_ascii_lowercase();
                    if !(mime == "application/rdap+json" || mime == "application/json") {
                        return Err(format!(
                            "RDAP server returned HTTP {} with an unexpected content type.",
                            response.status
                        ));
                    }
                    return Ok(response);
                }
                Err(error) => last_error = error,
            }
        }
        Err(last_error)
    })
    .await
    .map_err(|_| "RDAP lookup timed out.".to_string())?
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    pub(crate) fn response(value: Value) -> http::Response {
        http::Response {
            url: Url::parse("https://rdap.example.test/domain/example.test").unwrap(),
            status: 200,
            content_type: "application/rdap+json".into(),
            headers: Vec::new(),
            body: serde_json::to_vec(&value).unwrap(),
            rendered: None,
            js: None,
            blobs: None,
            live: None,
            declarative_refresh: None,
            challenge: None,
            from_post: false,
            timing: None,
        }
    }

    fn text(doc: &Doc) -> String {
        doc.lines
            .iter()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn rdap_records_scope_contacts_events_and_dnssec_to_their_objects() {
        let response = response(json!({
            "objectClassName": "domain", "ldhName": "EXAMPLE.TEST", "unicodeName": "example.test",
            "status": ["client transfer prohibited"],
            "events": [{"eventAction": "registration", "eventDate": "2001-01-01T00:00:00Z"}],
            "secureDNS": {"zoneSigned": true, "delegationSigned": false},
            "nameservers": [{"objectClassName": "nameserver", "ldhName": "NS.EXAMPLE.TEST", "ipAddresses": {"v4": ["192.0.2.1"]}}],
            "entities": [{"objectClassName": "entity", "roles": ["registrar"], "handle": "123",
                "vcardArray": ["vcard", [["fn", {}, "text", "Registrar Inc"], ["adr", {"label":"Paris, France"}, "text", ["", "", ["One Street", "Floor 2"], "Paris", "", "", "FR"]]]],
                "events": [{"eventAction": "registration", "eventDate": "1990-01-01T00:00:00Z"}],
                "entities": [{"objectClassName":"entity", "roles":["abuse"], "vcardArray":["vcard", [["fn",{},"text",""], ["email",{},"text","abuse@example.test", "security@example.test"], ["tel",{},"uri","tel:+1234"]]]}]
            }],
            "vendor_extension": {"opaque": true}
        }));
        let raw = response.body.clone();
        let page = Page::from_response(response);
        let shown = text(&render(page.clone(), 100));
        for expected in [
            "Registrar Inc",
            "2001-01-01",
            "NS.EXAMPLE.TEST",
            "Unsigned delegation",
            "Zone signed",
            "Yes",
        ] {
            assert!(shown.contains(expected), "{expected}: {shown}");
        }
        assert!(
            !shown.contains("1990-01-01")
                && !shown.contains("vcardArray")
                && !shown.contains("Handle 123")
        );
        let mut contacts = page.clone();
        contacts.section = Section::Contacts;
        let contacts = render(contacts, 120);
        let shown = text(&contacts);
        for expected in [
            "registrar / abuse email",
            "abuse@example.test",
            "security@example.test",
            "Floor 2",
            "Paris, France",
        ] {
            assert!(shown.contains(expected), "{expected}: {shown}");
        }
        assert!(contacts.lines.iter().any(|line| {
            line.link
                .as_ref()
                .is_some_and(|link| link.to_string() == "mailto:abuse@example.test")
        }));
        let mut details = page.clone();
        details.section = Section::Details;
        let shown = text(&render(details, 120));
        assert!(
            shown.contains("1990-01-01T00:00:00Z")
                && shown.contains("vendor_extension")
                && shown.contains("192.0.2.1")
        );
        assert_eq!(page.export().body, raw);
    }

    #[test]
    fn rdap_missing_optional_fields_errors_and_truncation_remain_readable() {
        let page = Page::from_response(response(
            json!({"objectClassName":"domain", "ldhName":"example.test", "entities": [{"objectClassName":"entity", "vcardArray":["vcard",[["fn",{},"text",""]]]}]}),
        ));
        let shown = text(&render(page, 100));
        assert!(!shown.contains("Unsigned") && !shown.contains("redacted"));
        let mut response = response(
            json!({"errorCode":429, "title":"Too many requests", "description":["Please try later.", "Your query was not answered."], "notices":[{"type":"result set truncated due to authorization", "title":"Limited data", "description":["Only public fields are included."]}], "redacted":[{"name":{"type":"Registrant Name"}, "reason":{"description":"Privacy policy"}}]}),
        );
        response.status = 429;
        response.headers.push(("Retry-After".into(), "60".into()));
        let page = Page::from_response(response);
        let shown = text(&render(page.clone(), 100));
        for expected in [
            "429",
            "Too many requests",
            "Please try later.",
            "Your query was not answered.",
            "Only public fields",
            "redacted",
            "Retry-After: 60",
        ] {
            assert!(shown.contains(expected), "{expected}: {shown}");
        }
        let mut details = page;
        details.section = Section::Details;
        assert!(text(&render(details, 100)).contains("Privacy policy"));
    }

    #[test]
    fn rdap_network_asn_entity_and_nameserver_views_keep_the_right_identity() {
        for (value, expected) in [
            (
                json!({"objectClassName":"ip network", "name":"EXAMPLE-NET", "startAddress":"192.0.2.0", "endAddress":"192.0.2.255", "country":"NL"}),
                "192.0.2.0 – 192.0.2.255",
            ),
            (
                json!({"objectClassName":"autnum", "startAutnum":64496, "endAutnum":64500, "name":"EXAMPLE-AS"}),
                "AS64496 – AS64500",
            ),
            (
                json!({"objectClassName":"entity", "handle":"EXAMPLE", "vcardArray":["vcard",[["fn",{},"text","Example Organization"]]]}),
                "Example Organization",
            ),
            (
                json!({"objectClassName":"nameserver", "ldhName":"ns.example.test"}),
                "ns.example.test",
            ),
        ] {
            assert!(
                text(&render(Page::from_response(response(value)), 100)).contains(expected),
                "{expected}"
            );
        }
    }

    #[test]
    fn rdap_uses_link_targets_and_ignores_unsafe_schemes() {
        let page = Page::from_response(response(
            json!({"objectClassName":"domain", "ldhName":"example.test", "links":[
                {"value":"https://rdap.example.test/base/", "rel":"related", "href":"record", "type":"application/rdap+json"},
                {"value":"https://rdap.example.test/base/", "rel":"related", "href":"javascript:alert(1)"}
            ]}),
        ));
        let links: Vec<_> = page
            .record
            .fields
            .iter()
            .filter_map(|field| field.link.as_ref())
            .map(ToString::to_string)
            .collect();
        assert!(
            links.contains(
                &direct_action(&Url::parse("https://rdap.example.test/base/record").unwrap())
                    .to_string()
            )
        );
        assert!(!links.iter().any(|link| link.contains("javascript:")));
        assert!(direct_query("about:rdap?url=file%3A%2F%2F%2Fetc%2Fpasswd").is_none());
        assert!(
            direct_query("about:rdap?url=https%3A%2F%2Fa.test%2F&query=example.test").is_none()
        );
    }

    #[test]
    fn rdap_deep_entities_and_malformed_json_have_bounded_fallbacks() {
        let mut value = json!({"objectClassName":"entity", "handle":"bottom"});
        for _ in 0..20 {
            value = json!({"objectClassName":"entity", "entities":[value]});
        }
        let page = Page::from_response(response(value));
        assert!(page.record.clipped);
        let mut response = response(json!({}));
        response.body = b"{broken\x1b[31m".to_vec();
        let page = Page::from_response(response);
        assert!(
            text(&render(page.clone(), 20))
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .contains("valid RDAP JSON")
        );
        let mut page = page;
        page.section = Section::Raw;
        let doc = render(page, 20);
        assert!(!text(&doc).contains('\x1b'));
        assert!(doc.lines.len() <= crate::text_reply::MAX_ROWS);
        assert_eq!(doc.rdap.unwrap().export().body, b"{broken\x1b[31m");
    }

    #[test]
    fn rdap_bootstrap_uses_label_boundaries_longest_match_and_https() {
        let registry = json!({"services": [
            [["com"], ["https://root.test/rdap/"]],
            [["example.com"], ["http://specific.test/base/", "https://specific.test/base/"]],
            [["goodexample.com"], ["https://wrong.test/"]]
        ]});
        let urls = endpoints(&registry, &Query::parse("A.EXAMPLE.COM.").unwrap()).unwrap();
        assert_eq!(
            urls[0].as_str(),
            "https://specific.test/base/domain/a.example.com"
        );
        let urls = endpoints(&registry, &Query::parse("badexample.com").unwrap()).unwrap();
        assert_eq!(urls[0].host_str(), Some("root.test"));
        assert!(endpoints(&registry, &Query::parse("example.net").unwrap()).is_err());
        assert_eq!(
            Query::parse("bücher.example"),
            Some(Query::Domain("xn--bcher-kva.example".into()))
        );
        assert!(action("-T inetnum 192.0.2.1").is_none());
    }

    #[test]
    fn rdap_bootstrap_matches_ip_prefixes_asn_ranges_and_preserves_paths() {
        let registry = json!({"services": [
            [["192.0.0.0/16"], ["https://broad.test/"]],
            [["192.0.2.0/24"], ["https://specific.test/rdap/"]],
            [["2001:db8::/32"], ["https://ipv6.test/"]],
            [["64496-64511"], ["https://asn.test/"]]
        ]});
        for (input, expected) in [
            ("192.0.2.1", "https://specific.test/rdap/ip/192.0.2.1"),
            ("192.0.0.0/16", "https://broad.test/ip/192.0.0.0/16"),
            ("2001:db8::1", "https://ipv6.test/ip/2001:db8::1"),
            ("AS64496", "https://asn.test/autnum/64496"),
        ] {
            let query = Query::parse(input).unwrap();
            assert_eq!(endpoints(&registry, &query).unwrap()[0].as_str(), expected);
            let Link::External(address) = action(input).unwrap() else {
                panic!()
            };
            assert_eq!(action_query(&address), Some(query));
        }
        for input in [
            "192.0.2.1/33",
            "2001:db8::/129",
            "fe80::1%eth0",
            "example.com/path",
            "bad name.test",
            "AS4294967296",
        ] {
            assert!(Query::parse(input).is_none(), "{input}");
        }
    }

    #[test]
    fn rdap_cache_respects_expiry_age_and_restrictive_directives() {
        let date = "Sun, 06 Sep 2026 12:00:00 GMT";
        let now = httpdate::parse_http_date(date).unwrap() + Duration::from_secs(20);
        let headers = |control: &str| {
            vec![
                ("Date".into(), date.into()),
                ("Cache-Control".into(), control.into()),
                ("Age".into(), "30".into()),
            ]
        };
        assert_eq!(
            freshness(&headers("max-age=60"), now, Duration::from_secs(2)),
            Duration::from_secs(28)
        );
        for control in [
            "no-store",
            "max-age=60, no-cache",
            "max-age=bad",
            "max-age=60, max-age=120",
        ] {
            assert!(
                freshness(&headers(control), now, Duration::ZERO).is_zero(),
                "{control}"
            );
        }
        assert_eq!(
            freshness(
                &[
                    ("Date".into(), date.into()),
                    ("Expires".into(), "Sun, 06 Sep 2026 12:01:00 GMT".into())
                ],
                now,
                Duration::ZERO
            ),
            Duration::from_secs(40)
        );
    }

    #[tokio::test]
    async fn rdap_action_uses_accept_follows_redirects_and_preserves_json() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!(
            "http://127.0.0.1:{}/",
            listener.local_addr().unwrap().port()
        );
        let registry = json!({"services": [[["example.test"], [base]]]});
        let previous = CACHE.lock().unwrap().insert(
            "dns",
            Cached {
                registry: Arc::new(registry),
                expires: Instant::now() + Duration::from_secs(30),
            },
        );
        let task = tokio::spawn(async move {
            let mut requests = Vec::new();
            for response in [
                "HTTP/1.1 302 Found\r\nLocation: /record/final\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string(),
                {
                    let body = r#"{"objectClassName":"domain","ldhName":"EXAMPLE.TEST"}"#;
                    format!("HTTP/1.1 200 OK\r\nContent-Type: application/rdap+json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
                }
            ] {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut buf = [0; 1024];
                while !request.ends_with(b"\r\n\r\n") {
                    let n = socket.read(&mut buf).await.unwrap();
                    assert_ne!(n, 0);
                    request.extend_from_slice(&buf[..n]);
                    assert!(request.len() < 16384);
                }
                requests.push(String::from_utf8(request).unwrap());
                socket.write_all(response.as_bytes()).await.unwrap();
            }
            requests
        });
        let result = fetch_action(&action("example.test").unwrap().to_string()).await;
        {
            let mut cache = CACHE.lock().unwrap();
            cache.remove("dns");
            if let Some(previous) = previous {
                cache.insert("dns", previous);
            }
        }
        let response = result.unwrap();
        assert_eq!(response.url.path(), "/record/final");
        assert_eq!(
            response.body,
            br#"{"objectClassName":"domain","ldhName":"EXAMPLE.TEST"}"#
        );
        let doc = render(Page::from_response(response), 80);
        assert!(text(&doc).contains("EXAMPLE.TEST"));
        assert!(!text(&doc).contains("objectClassName"));
        let requests = task.await.unwrap();
        assert!(requests[0].starts_with("GET /domain/example.test HTTP/1.1\r\n"));
        assert!(requests[1].starts_with("GET /record/final HTTP/1.1\r\n"));
        for request in requests {
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains("accept: application/rdap+json, application/json\r\n")
            );
        }
    }
}
