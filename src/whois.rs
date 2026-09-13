//! WHOIS transactions and their human-readable presentation.
//!
//! RFC 3912 §§2, 4 (RFC Editor snapshot 2026-09-06): one CRLF-terminated
//! request, reply through EOF, no universal encoding or response schema.
//! Referrals are service conventions, not RFC 3912 protocol fields. IANA's
//! documented `refer:` is a referral; its `whois:` describes a service.
//! https://www.iana.org/help/whois

use std::borrow::Cow;
use std::collections::HashSet;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use tokio::time::{Instant, timeout_at};

use crate::doc::{Doc, DocLine, Kind, Link};
use crate::oneshot::{OneShotUrl, Scheme};
use crate::text_reply::{self, MAX_LINKS, MAX_RESPONSE, MAX_ROWS};

mod record;

const MAX_HOPS: usize = 4;
const FETCH_TIMEOUT: Duration = Duration::from_secs(15);
pub const USAGE: &str = "usage: whois <query or \"quoted query\"> [server[:port]]";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HopState {
    Connecting,
    Receiving,
    Complete,
    Incomplete,
}

impl HopState {
    fn label(self) -> &'static str {
        match self {
            Self::Connecting => "connecting",
            Self::Receiving => "receiving",
            Self::Complete => "complete",
            Self::Incomplete => "incomplete",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hop {
    pub target: OneShotUrl,
    pub body: Arc<[u8]>,
    pub state: HopState,
    pub elapsed_ms: u64,
    pub notice: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Reply {
    pub hops: Vec<Hop>,
    /// The transaction has stopped; individual hops still say whether EOF
    /// completed their response or an error interrupted it.
    pub finished: bool,
    pub notice: Option<String>,
}

impl Reply {
    /// A complete response supplied by a caller rather than the streaming client.
    pub fn from_bytes(target: OneShotUrl, body: Vec<u8>) -> Self {
        Self {
            hops: vec![Hop {
                target,
                body: body.into(),
                state: HopState::Complete,
                elapsed_ms: 0,
                notice: None,
            }],
            finished: true,
            notice: None,
        }
    }

    pub fn bytes(&self) -> usize {
        self.hops.iter().map(|hop| hop.body.len()).sum()
    }

    pub fn successful(&self) -> bool {
        self.finished
            && self.notice.is_none()
            && !self.hops.is_empty()
            && self.hops.iter().all(|hop| hop.state == HopState::Complete)
    }

    pub fn stop(&mut self, reason: &str) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.notice = Some(reason.to_string());
        if let Some(hop) = self.hops.last_mut() {
            hop.state = HopState::Incomplete;
            hop.notice = Some(reason.to_string());
        }
    }

    /// An explicitly separated transcript. Per-server exports use `Hop.body`
    /// directly, without changing a single byte or inserting these headings.
    pub fn transcript(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(self.bytes() + self.hops.len() * 256);
        for hop in &self.hops {
            bytes.extend_from_slice(
                format!("% WHOIS {} — {}\r\n", hop.target, hop.state.label()).as_bytes(),
            );
            bytes.extend_from_slice(&hop.body);
            bytes.extend_from_slice(b"\r\n\r\n");
            if let Some(notice) = &hop.notice {
                bytes.extend_from_slice(format!("% {notice}\r\n").as_bytes());
            }
        }
        if let Some(notice) = &self.notice {
            bytes.extend_from_slice(format!("% Lookup: {notice}\r\n").as_bytes());
        }
        bytes
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Encoding {
    /// Prefer valid UTF-8, with Latin-1 for other bytes. This is a display
    /// heuristic: RFC 3912 does not carry charset metadata. Users can override
    /// it, including ambiguous Latin-1 byte sequences that are valid UTF-8.
    #[default]
    Auto,
    Utf8,
    Latin1,
}

impl Encoding {
    pub fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "utf8" | "utf-8" => Some(Self::Utf8),
            "latin1" | "latin-1" | "iso-8859-1" => Some(Self::Latin1),
            _ => None,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Utf8 => "UTF-8",
            Self::Latin1 => "Latin-1",
        }
    }
    fn decode(self, raw: &[u8], loading: bool) -> (Cow<'_, str>, &'static str) {
        let utf8 = std::str::from_utf8(raw);
        let incomplete = utf8
            .as_ref()
            .err()
            .filter(|error| loading && error.error_len().is_none());
        if self == Self::Utf8 || (self == Self::Auto && (utf8.is_ok() || incomplete.is_some())) {
            let raw = incomplete.map_or(raw, |error| &raw[..error.valid_up_to()]);
            (String::from_utf8_lossy(raw), "UTF-8")
        } else {
            (
                Cow::Owned(raw.iter().map(|byte| char::from(*byte)).collect()),
                "Latin-1",
            )
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Page {
    pub reply: Reply,
    pub view: text_reply::View,
    pub encoding: Encoding,
    pub previous: Option<Arc<Reply>>,
    pub section: crate::registration::Section,
}

impl Page {
    pub fn export(
        &self,
        url: &OneShotUrl,
        server: Option<usize>,
    ) -> Result<crate::download::DownloadOffer, String> {
        let body = if let Some(index) = server {
            self.reply
                .hops
                .get(index.checked_sub(1).ok_or("Server numbers start at 1.")?)
                .ok_or("No such server in this lookup.")?
                .body
                .to_vec()
        } else if self.reply.hops.len() == 1 {
            self.reply.hops[0].body.to_vec()
        } else {
            self.reply.transcript()
        };
        let name: String = url
            .query
            .chars()
            .map(|ch| {
                if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-') {
                    ch
                } else {
                    '_'
                }
            })
            .take(64)
            .collect();
        let suffix = server.map_or_else(String::new, |n| format!("-server-{n}"));
        let filename = format!(
            "whois-{name}{suffix}{}.txt",
            if self.reply.successful() {
                ""
            } else {
                "-partial"
            }
        );
        Ok(crate::download::DownloadOffer::from_bytes(
            url::Url::parse(&url.to_string()).map_err(|error| error.to_string())?,
            filename,
            body,
        ))
    }

    pub fn new(reply: Reply) -> Self {
        let view = text_reply::View {
            loading: !reply.finished,
            notice: reply.notice.clone(),
            ..Default::default()
        };
        Self {
            reply,
            view,
            encoding: Encoding::Auto,
            previous: None,
            section: Default::default(),
        }
    }

    pub fn update(&mut self, reply: Reply) {
        self.view.loading = !reply.finished;
        self.view.notice = reply.notice.clone();
        self.reply = reply;
    }

    pub fn refreshed(reply: Reply, old: &Self) -> Self {
        let mut page = Self::new(reply);
        page.view.wrap = old.view.wrap;
        page.view.horizontal = old.view.horizontal;
        page.encoding = old.encoding;
        page.section = old.section;
        page.previous = if old.reply.successful() {
            Some(Arc::new(old.reply.clone()))
        } else {
            old.previous.clone()
        };
        page.view.changes = old.view.changes && page.previous.is_some();
        page
    }

    pub fn view_action(
        &mut self,
        action: &str,
        enabled: Option<bool>,
    ) -> Result<&'static str, &'static str> {
        text_reply::view_action(&mut self.view, action, enabled, self.previous.is_some())
    }

    pub fn stop(&mut self, reason: &str) {
        self.reply.stop(reason);
        self.view.loading = false;
        self.view.notice = self.reply.notice.clone();
    }
}

pub fn parse_url(input: &str) -> Option<OneShotUrl> {
    text_reply::parse_url(input, Scheme::Whois)
}

/// Interpret only an endpoint here; a server argument cannot smuggle a
/// different query, credentials, URI query, or fragment into a referral.
pub fn server_target(server: &str, query: &str) -> Option<OneShotUrl> {
    if server.chars().any(char::is_whitespace) || server.contains(['?', '#']) {
        return None;
    }
    let address = if server.contains("://") {
        server.to_string()
    } else if server.parse::<std::net::Ipv6Addr>().is_ok() {
        format!("whois://[{server}]")
    } else {
        format!("whois://{server}")
    };
    let mut target = parse_url(&address)?;
    if !target.query.is_empty() || target.port == 0 {
        return None;
    }
    text_reply::validate_query(query, "WHOIS").ok()?;
    target.query = query.to_string();
    Some(target)
}

pub fn command_target(arguments: &str) -> Result<OneShotUrl, String> {
    let words = crate::command::quoted_arguments(arguments)?;
    let (query, server) = match words.as_slice() {
        [query] => (query.as_str(), crate::oneshot::WHOIS_DEFAULT),
        [query, server] => (query.as_str(), server.as_str()),
        [option, server, query] if matches!(option.as_str(), "-h" | "--server") => {
            (query.as_str(), server.as_str())
        }
        [option, server, separator, query]
            if matches!(option.as_str(), "-h" | "--server") && separator == "--" =>
        {
            (query.as_str(), server.as_str())
        }
        _ => return Err(USAGE.to_string()),
    };
    server_target(server, query).ok_or_else(|| "Invalid WHOIS query or server address.".to_string())
}

fn endpoint(target: &OneShotUrl) -> (String, u16) {
    (
        target.host.trim_end_matches('.').to_ascii_lowercase(),
        target.port,
    )
}

/// Prefer explicit IANA referrals. Registry/registrar WHOIS fields are useful
/// referrals for an object lookup, but an advertised IANA `whois:` is not.
fn referral(hop: &Hop) -> Result<Option<OneShotUrl>, String> {
    let text = String::from_utf8_lossy(&hop.body);
    let mut candidates = Vec::new();
    for line in text.lines() {
        let Some((key, value)) = line.trim().split_once(':') else {
            continue;
        };
        let priority = match key.trim().to_ascii_lowercase().as_str() {
            "refer" => 0,
            "referralserver" => 1,
            "registrar whois server" | "whois server" => 2,
            _ => continue,
        };
        if candidates.len() == 16 {
            break;
        }
        candidates.push((priority, value.trim()));
    }
    candidates.sort_by_key(|(priority, _)| *priority);
    for (priority, server) in candidates {
        if server.is_empty() {
            continue;
        }
        if server.contains("://") && !server.to_ascii_lowercase().starts_with("whois://") {
            return Err(
                "Referral uses an unsupported protocol; its address remains in the reply.".into(),
            );
        }
        let target = server_target(server, &hop.target.query).ok_or_else(|| {
            "Invalid WHOIS referral address; the received answer is retained.".to_string()
        })?;
        // A registrar may identify its own service in its response. This is
        // not another hop. Explicit refer: self-loops are diagnosed by caller.
        if priority > 0 && endpoint(&target) == endpoint(&hop.target) {
            continue;
        }
        return Ok(Some(target));
    }
    Ok(None)
}

pub async fn fetch(url: &OneShotUrl) -> Result<Reply, String> {
    fetch_updates(url, |_| async { true }).await
}

pub async fn fetch_updates<F, Fut>(url: &OneShotUrl, publish: F) -> Result<Reply, String>
where
    F: FnMut(Reply) -> Fut,
    Fut: Future<Output = bool>,
{
    exchange(url, FETCH_TIMEOUT, publish).await
}

async fn exchange<F, Fut>(
    url: &OneShotUrl,
    limit: Duration,
    mut publish: F,
) -> Result<Reply, String>
where
    F: FnMut(Reply) -> Fut,
    Fut: Future<Output = bool>,
{
    text_reply::validate_query(&url.query, "WHOIS")?;
    if url.scheme != Scheme::Whois {
        return Err("Not a WHOIS target.".into());
    }
    let deadline = Instant::now() + limit;
    let mut reply = Reply::default();
    let mut visited = HashSet::new();
    let mut target = url.clone();
    loop {
        if !visited.insert(endpoint(&target)) {
            reply.notice = Some("Referral cycle detected; received answers are retained.".into());
            break;
        }
        let started = Instant::now();
        reply.hops.push(Hop {
            target: target.clone(),
            body: Arc::from([]),
            state: HopState::Connecting,
            elapsed_ms: 0,
            notice: None,
        });
        match timeout_at(deadline, publish(reply.clone())).await {
            Ok(true) => {}
            Ok(false) => return Err("WHOIS request cancelled".into()),
            Err(_) => {
                reply.stop("Incomplete reply: request timed out");
                break;
            }
        }
        let base = reply.clone();
        let remaining = MAX_RESPONSE.saturating_sub(reply.bytes());
        let result = text_reply::exchange(&target, deadline, remaining, |partial| {
            let mut update = base.clone();
            let hop = update.hops.last_mut().expect("active hop");
            hop.body = Arc::from(partial.body);
            hop.state = HopState::Receiving;
            hop.elapsed_ms = started.elapsed().as_millis() as u64;
            publish(update)
        })
        .await;
        let hop = reply.hops.last_mut().expect("active hop");
        hop.elapsed_ms = started.elapsed().as_millis() as u64;
        match result {
            Ok(received) => {
                hop.body = Arc::from(received.body);
                hop.notice = received.notice;
                hop.state = if hop.notice.is_none() {
                    HopState::Complete
                } else {
                    HopState::Incomplete
                };
            }
            Err(error) => {
                hop.state = HopState::Incomplete;
                hop.notice = Some(error);
            }
        }
        if let Some(notice) = &hop.notice {
            reply.notice = Some(notice.clone());
            break;
        }
        let next = match referral(hop) {
            Ok(next) => next,
            Err(error) => {
                reply.notice = Some(error);
                break;
            }
        };
        let Some(next) = next else {
            break;
        };
        if reply.hops.len() == MAX_HOPS {
            reply.notice = Some(format!(
                "Stopped at {MAX_HOPS} servers; received answers are retained."
            ));
            break;
        }
        if reply.bytes() >= MAX_RESPONSE {
            reply.notice = Some(
                "Stopped at the 1 MiB transaction limit; received answers are retained.".into(),
            );
            break;
        }
        target = next;
    }
    reply.finished = true;
    Ok(reply)
}

pub fn status(url: &OneShotUrl, reply: &Reply) -> String {
    let source = reply
        .hops
        .last()
        .map(|hop| format!("{} · {}", authority(&hop.target), hop.state.label()))
        .unwrap_or_else(|| "connecting".into());
    format!(
        "WHOIS {} — {source} · {} bytes · W wrap · D changes · E encoding · S save",
        url.query,
        reply.bytes()
    )
}

fn authority(target: &OneShotUrl) -> String {
    let host = if target.host.contains(':') {
        format!("[{}]", target.host)
    } else {
        target.host.clone()
    };
    if target.port == 43 {
        host
    } else {
        format!("{host}:{}", target.port)
    }
}

/// Recognized labels get light emphasis, without asserting a universal WHOIS
/// schema. Unknown and repeated fields, comments, and continuations remain.
pub fn field_prefix(line: &str) -> Option<usize> {
    let (key, _) = line.split_once(':')?;
    match key.trim().to_ascii_lowercase().as_str() {
        "domain"
        | "domain name"
        | "registry domain id"
        | "registrar"
        | "registrar iana id"
        | "domain status"
        | "status"
        | "created"
        | "changed"
        | "creation date"
        | "updated date"
        | "registry expiry date"
        | "registrar registration expiration date"
        | "name server"
        | "nserver"
        | "dnssec"
        | "netrange"
        | "netname"
        | "nettype"
        | "inetnum"
        | "inet6num"
        | "cidr"
        | "aut-num"
        | "as-name"
        | "origin"
        | "originas"
        | "orgname"
        | "org-name"
        | "organisation"
        | "organization"
        | "orgid"
        | "country"
        | "source"
        | "contact"
        | "person"
        | "role"
        | "nic-hdl"
        | "admin-c"
        | "tech-c"
        | "mnt-by"
        | "abuse-mailbox"
        | "refer"
        | "whois"
        | "whois server"
        | "registrar whois server"
        | "referralserver" => Some(key.len() + 1),
        _ => None,
    }
}

fn record_link(word: &str, source: &OneShotUrl, handle: bool) -> Option<Link> {
    let word = word.trim_matches([',', ';', '(', ')']);
    let ip = word.parse::<std::net::IpAddr>().is_ok()
        || word.split_once('/').is_some_and(|(ip, bits)| {
            ip.parse::<std::net::IpAddr>()
                .ok()
                .zip(bits.parse::<u8>().ok())
                .is_some_and(|(ip, bits)| bits <= if ip.is_ipv4() { 32 } else { 128 })
        });
    let asn = word
        .get(..2)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("as"))
        && word
            .get(2..)
            .is_some_and(|number| !number.is_empty() && number.parse::<u32>().is_ok());
    let nic = handle
        && word.len() <= 128
        && word.contains('-')
        && word
            .bytes()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == b'-');
    if ip || asn || nic {
        Some(Link::OneShot(OneShotUrl {
            query: word.to_string(),
            ..source.clone()
        }))
    } else {
        None
    }
}

fn line_links(line: &str, source: &OneShotUrl) -> Vec<Link> {
    let mut links = text_reply::links(line);
    if let Some((key, value)) = line.split_once(':') {
        let key = key.trim().to_ascii_lowercase();
        if matches!(
            key.as_str(),
            "refer" | "whois" | "whois server" | "registrar whois server" | "referralserver"
        ) && let Some(target) = server_target(value.trim(), &source.query)
        {
            links.push(Link::OneShot(target));
        }
        // Interpret identifiers only in fields that define them. A phone
        // number or incidental prose must not become an inferred lookup.
        let handle = matches!(
            key.as_str(),
            "nic-hdl" | "admin-c" | "tech-c" | "mnt-by" | "orgid" | "organisation"
        );
        if handle
            || matches!(
                key.as_str(),
                "inetnum"
                    | "inet6num"
                    | "cidr"
                    | "netrange"
                    | "origin"
                    | "originas"
                    | "aut-num"
                    | "nserver"
            )
        {
            links.extend(
                value
                    .split_whitespace()
                    .filter_map(|word| record_link(word, source, handle))
                    .take(16),
            );
        }
    }
    links.truncate(16);
    links
}

fn display(hop: &Hop, encoding: Encoding) -> (String, bool, &'static str) {
    let loading = matches!(hop.state, HopState::Connecting | HopState::Receiving);
    let (decoded, name) = encoding.decode(&hop.body, loading);
    let (text, clipped) = text_reply::display_text(decoded.as_bytes(), loading);
    (text, clipped, name)
}

pub fn render(url: &OneShotUrl, page: Page, width: usize) -> Doc {
    render_with_columns(url, page, width, width >= 52)
}

pub fn render_with_columns(url: &OneShotUrl, mut page: Page, width: usize, columns: bool) -> Doc {
    use crate::registration::{self, Section};
    if page.section == Section::Raw {
        return render_raw(url, page, width);
    }
    let summary = record::summarize(&page.reply, page.encoding, &url.query);
    let discovery = page
        .reply
        .hops
        .iter()
        .any(|hop| record::discovery(hop, &display(hop, page.encoding).0));
    // Unknown services retain a readable transcript. Only known discovery
    // records are suppressed; a query for the TLD itself still displays it.
    if summary.is_none() && !discovery {
        return render_raw(url, page, width);
    }
    page.view.loading = !page.reply.finished;
    page.view.notice = page.reply.notice.clone();
    let width = width.max(2);
    let mut lines = Vec::new();
    registration::line(
        &mut lines,
        Kind::Heading(1),
        &format!("WHOIS · {}", registration::clean(&url.query)),
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
    registration::line(
        &mut lines,
        Kind::Info,
        if !page.reply.finished {
            "Looking up registration…"
        } else if page.reply.successful() {
            "Registration lookup complete"
        } else {
            "Partial registration lookup"
        },
        None,
        width,
    );
    registration::line(&mut lines, Kind::Text, "", None, width);
    if let Some(summary) = summary {
        if page.view.changes && page.reply.finished && page.previous.is_some() {
            let old = page
                .previous
                .as_ref()
                .and_then(|reply| record::summarize(reply, page.encoding, &url.query));
            let current = summary.comparison();
            let old = old.map(|record| record.comparison()).unwrap_or_default();
            let mut changed = false;
            for (kind, text) in text_reply::changes(&old, &current) {
                if !matches!(kind, Kind::Added | Kind::Removed) {
                    continue;
                }
                changed = true;
                registration::line(
                    &mut lines,
                    kind,
                    &format!("{} {text}", if kind == Kind::Added { "+" } else { "-" }),
                    None,
                    width,
                );
            }
            if !changed {
                registration::line(
                    &mut lines,
                    Kind::Info,
                    "No registration field changes since the previous refresh.",
                    None,
                    width,
                );
            }
        } else {
            summary.render_with_columns(page.section, width, columns, &mut lines);
        }
    } else {
        registration::line(
            &mut lines,
            Kind::Info,
            if page.reply.finished {
                "No matching registration record was returned."
            } else {
                "Waiting for the domain or network record…"
            },
            None,
            width,
        );
        for hop in &page.reply.hops {
            let (text, _, _) = display(hop, page.encoding);
            if record::discovery(hop, &text) {
                continue;
            }
            for line in text.lines() {
                registration::line(&mut lines, Kind::Text, line, None, width);
            }
        }
    }
    for hop in &page.reply.hops {
        if let Some(notice) = &hop.notice {
            registration::line(
                &mut lines,
                Kind::Error,
                &format!("{}: {notice}", authority(&hop.target)),
                None,
                width,
            );
        }
    }
    if let Some(notice) = &page.reply.notice {
        registration::line(&mut lines, Kind::Error, notice, None, width);
    }
    if page.section == Section::Details {
        registration::line(&mut lines, Kind::Heading(2), "Lookup sources", None, width);
        for (index, hop) in page.reply.hops.iter().enumerate() {
            registration::line(
                &mut lines,
                Kind::Text,
                &format!(
                    "{}. {} · {} · {} bytes · {} ms",
                    index + 1,
                    authority(&hop.target),
                    hop.state.label(),
                    hop.body.len(),
                    hop.elapsed_ms
                ),
                Some(Link::OneShot(hop.target.clone())),
                width,
            );
        }
        registration::line(
            &mut lines,
            Kind::Info,
            &format!(
                "Encoding {} · E change encoding · S save · save 2 saves server 2",
                page.encoding.label()
            ),
            None,
            width,
        );
    }
    registration::navigation(&mut lines, page.section, false, width);
    if let Some(action) = crate::rdap::action(&url.query) {
        registration::line(
            &mut lines,
            Kind::OtherLink,
            "Look up with RDAP",
            Some(action),
            width,
        );
    }
    registration::line(
        &mut lines,
        Kind::Quote,
        "D changes · S save received replies",
        None,
        width,
    );
    let mut doc = Doc::from_lines(
        Link::OneShot(url.clone()),
        lines,
        page.reply.transcript(),
        width,
        false,
        None,
    );
    doc.whois = Some(page);
    doc
}

fn render_raw(url: &OneShotUrl, mut page: Page, width: usize) -> Doc {
    page.view.loading = !page.reply.finished;
    page.view.notice = page.reply.notice.clone();
    if page.view.wrap {
        page.view.horizontal = 0;
    }
    let width = width.max(2);
    let body_width = if page.view.wrap { width } else { usize::MAX };
    let mut lines = Vec::new();
    let mut query: String = url.query.chars().take(256).collect();
    if query.len() < url.query.len() {
        query.push('…');
    }
    let title = text_reply::display_text(format!("WHOIS · {query}").as_bytes(), false).0;
    text_reply::push_line(&mut lines, Kind::Heading(1), title, None, width);
    crate::registration::navigation(&mut lines, crate::registration::Section::Raw, false, width);
    let count = page.reply.hops.len();
    text_reply::push_line(
        &mut lines,
        Kind::Info,
        format!(
            "{} · {} bytes · {} server{} · encoding {}",
            if page.reply.finished {
                if page.reply.successful() {
                    "Complete"
                } else {
                    "Partial"
                }
            } else {
                "Loading…"
            },
            page.reply.bytes(),
            count,
            if count == 1 { "" } else { "s" },
            page.encoding.label()
        ),
        None,
        width,
    );
    text_reply::push_line(
        &mut lines,
        Kind::Info,
        "W wrap · D changes · E encoding · S save · Shift+←/→ pan".into(),
        None,
        width,
    );
    if let Some(link) = crate::rdap::action(&url.query) {
        text_reply::push_line(
            &mut lines,
            Kind::OtherLink,
            "Look up with RDAP".into(),
            Some(link),
            width,
        );
    }

    let compare = page.view.changes && page.reply.finished && page.previous.is_some();
    if compare {
        text_reply::push_line(
            &mut lines,
            Kind::Info,
            "Changes since the previous successful refresh: + added, - removed".into(),
            None,
            width,
        );
    }
    let mut sections: Vec<_> = page.reply.hops.iter().map(|hop| (hop, false)).collect();
    if compare && let Some(previous) = &page.previous {
        sections.extend(
            previous
                .hops
                .iter()
                .filter(|old| !page.reply.hops.iter().any(|hop| hop.target == old.target))
                .map(|hop| (hop, true)),
        );
    }
    // Reserve room for every server, including errors and the last referral.
    let per_section =
        (MAX_ROWS.saturating_sub(lines.len() + 128) / sections.len().max(1)).saturating_sub(128);
    let mut extras = Vec::new();
    let mut seen = HashSet::new();
    let mut changed = false;
    for (index, (hop, removed)) in sections.into_iter().enumerate() {
        let (text, clipped, encoding) = display(hop, page.encoding);
        lines.push(DocLine {
            kind: Kind::Text,
            text: String::new(),
            link: None,
        });
        text_reply::push_line(
            &mut lines,
            if removed {
                Kind::Removed
            } else {
                Kind::Heading(2)
            },
            format!(
                "{}. {} · {} · {} bytes · {} ms · {encoding}",
                index + 1,
                authority(&hop.target),
                if removed {
                    "absent from this lookup"
                } else {
                    hop.state.label()
                },
                hop.body.len(),
                hop.elapsed_ms
            ),
            Some(Link::OneShot(hop.target.clone())),
            width,
        );
        if hop.state == HopState::Incomplete {
            text_reply::push_line(
                &mut lines,
                Kind::OtherLink,
                format!("↻ Retry {}", authority(&hop.target)),
                Some(Link::OneShot(hop.target.clone())),
                width,
            );
        }
        let old = page
            .previous
            .as_ref()
            .and_then(|previous| previous.hops.iter().find(|old| old.target == hop.target));
        let old_text = old.map(|old| display(old, page.encoding).0);
        let source = if removed {
            text.lines()
                .map(|line| (Kind::Removed, line))
                .collect::<Vec<_>>()
        } else if compare {
            text_reply::changes(old_text.as_deref().unwrap_or(""), &text)
        } else {
            text.lines().map(|line| (Kind::Pre, line)).collect()
        };
        // RFC 3912 §2: EOF completes this server's final line even while a
        // later referral is still loading. Its links are already usable.
        let incomplete_tail = matches!(hop.state, HopState::Connecting | HopState::Receiving)
            && !text.ends_with('\n');
        let source_len = source.len();
        let start = lines.len();
        let mut truncated = clipped;
        for (row, (mut kind, line)) in source.into_iter().enumerate() {
            if lines.len() - start >= per_section {
                truncated = true;
                break;
            }
            changed |= matches!(kind, Kind::Added | Kind::Removed);
            let found = if kind == Kind::Removed || (incomplete_tail && row + 1 == source_len) {
                Vec::new()
            } else {
                line_links(line, &hop.target)
            };
            // The first target is selectable on the record itself; only the
            // other targets need extra rows in our one-link-per-line model.
            for link in found.iter().skip(1) {
                if extras.len() < MAX_LINKS && seen.insert(link.to_string()) {
                    extras.push(link.clone());
                }
            }
            let painted = match kind {
                Kind::Added => format!("+ {line}"),
                Kind::Removed => format!("- {line}"),
                _ => {
                    if line.trim_start().starts_with(['%', '#']) {
                        kind = Kind::Quote;
                    } else if field_prefix(line).is_some() {
                        kind = Kind::Field;
                    }
                    line.to_string()
                }
            };
            text_reply::push_line(
                &mut lines,
                kind,
                painted,
                found.into_iter().next(),
                body_width,
            );
            if lines.len() - start > per_section {
                lines.truncate(start + per_section);
                truncated = true;
                break;
            }
        }
        if text.is_empty() {
            text_reply::push_line(
                &mut lines,
                Kind::Info,
                match hop.state {
                    HopState::Complete => "Empty reply.",
                    HopState::Incomplete => "No reply received.",
                    _ => "Waiting for reply…",
                }
                .into(),
                None,
                width,
            );
        }
        if truncated {
            text_reply::push_line(&mut lines, Kind::Error, "Display truncated to keep this reply responsive; received bytes remain available to save.".into(), None, width);
        }
        if let Some(notice) = &hop.notice {
            text_reply::push_line(&mut lines, Kind::Error, notice.clone(), None, width);
        }
    }
    if compare && !changed {
        text_reply::push_line(
            &mut lines,
            Kind::Info,
            "No changes since the previous refresh.".into(),
            None,
            width,
        );
    }
    if let Some(notice) = &page.reply.notice {
        text_reply::push_line(&mut lines, Kind::Error, notice.clone(), None, width);
    }
    if !extras.is_empty() {
        text_reply::push_line(
            &mut lines,
            Kind::Heading(2),
            "Links and related records".into(),
            None,
            width,
        );
        for link in extras {
            if lines.len() >= MAX_ROWS - 2 {
                break;
            }
            text_reply::push_line(
                &mut lines,
                Kind::OtherLink,
                format!("↗ {link}"),
                Some(link),
                width,
            );
        }
    }
    // Keep horizontal panning meaningful on both frontends without a fetch.
    page.view.horizontal = page.view.horizontal.min(
        lines
            .iter()
            .map(|line| unicode_width::UnicodeWidthStr::width(line.text.as_str()))
            .max()
            .unwrap_or(0)
            .saturating_sub(width),
    );
    let mut doc = Doc::from_lines(
        Link::OneShot(url.clone()),
        lines,
        page.reply.transcript(),
        width,
        false,
        None,
    );
    doc.whois = Some(page);
    doc
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpListener;

    fn target() -> OneShotUrl {
        parse_url("whois://example.test/example.com").unwrap()
    }
    fn hop(body: &[u8]) -> Hop {
        Hop {
            target: target(),
            body: Arc::from(body),
            state: HopState::Complete,
            elapsed_ms: 1,
            notice: None,
        }
    }
    fn reply(body: &[u8]) -> Reply {
        Reply {
            hops: vec![hop(body)],
            finished: true,
            notice: None,
        }
    }

    #[test]
    fn whois_url_components_roundtrip_and_queries_decode_once() {
        let url = parse_url("WHOIS://[::1]:4343/%65xample.com%2520#local/note").unwrap();
        assert_eq!(
            (&*url.host, url.port, &*url.query),
            ("::1", 4343, "example.com%20")
        );
        assert_eq!(parse_url(&url.to_string()), Some(url));
        assert_eq!(
            parse_url("whois://example.test/../literal").unwrap().query,
            "../literal"
        );
        assert_eq!(
            parse_url("whois://example.test/192.0.2.0/24")
                .unwrap()
                .query,
            "192.0.2.0/24"
        );
        for address in [
            "whois://host/%0D%0Asecond",
            "whois://host/%00",
            "whois://host/%xy",
            "whois://host:bad/query",
            "whois://host:99999/query",
            "whois://user@host/query",
            "whois://[broken]/query",
            "whois://host/query?ambiguous",
        ] {
            assert!(parse_url(address).is_none(), "{address}");
        }
        assert!(server_target("whois://example.test/another-query", "q").is_none());
        assert!(server_target("example.test#fragment", "q").is_none());
        assert!(server_target("example.test:0", "q").is_none());
    }

    #[test]
    fn whois_commands_preserve_quotes_spaces_and_ipv6() {
        let url = command_target("\"-r -T inetnum 192.0.2.1\" [::1]:4343").unwrap();
        assert_eq!(url.query, "-r -T inetnum 192.0.2.1");
        assert_eq!((&*url.host, url.port), ("::1", 4343));
        assert_eq!(
            command_target("-h whois.ripe.net -- '-r -T inetnum 192.0.2.1'")
                .unwrap()
                .query,
            url.query
        );
        assert_eq!(
            command_target("'literal%20text' example.test")
                .unwrap()
                .query,
            "literal%20text"
        );
        assert_eq!(
            command_target("example.com").unwrap().host,
            crate::oneshot::WHOIS_DEFAULT
        );
        assert!(command_target("\"unclosed query").is_err());
        assert!(command_target("one two ignored").is_err());
        assert!(command_target("\"first\nsecond\" host").is_err());
    }

    #[test]
    fn whois_referrals_distinguish_service_metadata_and_validated_destinations() {
        assert_eq!(
            referral(&hop(b"domain: COM\r\nwhois: whois.example.test\r\n")).unwrap(),
            None
        );
        for field in [
            "refer",
            "Refer",
            "Whois Server",
            "Registrar WHOIS Server",
            "ReferralServer",
        ] {
            let response = format!("{field}: whois://other.test:4343/\r\n");
            let result = referral(&hop(response.as_bytes())).unwrap().unwrap();
            assert_eq!(
                (&*result.host, result.port, &*result.query),
                ("other.test", 4343, "example.com")
            );
        }
        assert_eq!(
            referral(&hop(b"Registrar WHOIS Server: EXAMPLE.TEST.:43\n")).unwrap(),
            None
        );
        let result = referral(&hop(b"Whois Server: later.test\nrefer: first.test\n"))
            .unwrap()
            .unwrap();
        assert_eq!(result.host, "first.test");
        for value in [
            "bad host.test",
            "host.test:99999",
            "whois://host.test/changed",
            "rwhois://host.test:4321/",
        ] {
            assert!(
                referral(&hop(format!("refer: {value}\n").as_bytes())).is_err(),
                "{value}"
            );
        }
    }

    #[test]
    fn whois_display_preserves_text_and_raw_bytes_and_supports_encodings() {
        let raw = b"% Comment\r\nDomain:\tEXAMPLE.COM\r\naddress: M\xfcnchen\r\nremark: \x1b[31mred\x1b[0m\r\n\r\nunknown: value\r\n";
        let mut page = Page::new(reply(raw));
        page.section = crate::registration::Section::Raw;
        let doc = render(&target(), page.clone(), 80);
        assert!(
            doc.lines
                .iter()
                .any(|line| line.text == "Domain: EXAMPLE.COM" && line.kind == Kind::Field)
        );
        assert!(doc.lines.iter().any(|line| line.text == "address: München"));
        assert!(doc.lines.iter().any(|line| line.text == "remark: red"));
        assert!(doc.lines.iter().any(|line| line.text == "unknown: value"));
        assert_eq!(&*doc.whois.as_ref().unwrap().reply.hops[0].body, raw);
        page.encoding = Encoding::Utf8;
        let doc = render(&target(), page, 80);
        assert!(doc.lines.iter().any(|line| line.text.contains("M�nchen")));
        assert_eq!(Encoding::Auto.decode(b"ok \xc3", true).0, "ok ");
        assert_eq!(Encoding::Auto.decode(b"M\xfc", false).0, "Mü");
        assert_eq!(Encoding::Latin1.decode(b"\xc3\xbc", false).0, "Ã¼");
    }

    #[test]
    fn whois_links_use_the_responder_and_keep_multiple_targets_selectable() {
        let doc = render(&target(), Page::new(reply(b"origin: AS64496\nadmin-c: TEST-RIPE\nnserver: ns.example 192.0.2.1 2001:db8::1\nurl: https://example.test/one https://example.test/two\nwhois: other.test\n")), 80);
        let links: HashSet<_> = doc
            .lines
            .iter()
            .filter_map(|line| line.link.as_ref().map(ToString::to_string))
            .collect();
        for value in [
            "whois://example.test/AS64496",
            "whois://example.test/TEST-RIPE",
            "whois://example.test/192.0.2.1",
            "whois://example.test/2001%3Adb8%3A%3A1",
            "https://example.test/one",
            "https://example.test/two",
            "whois://other.test/example.com",
        ] {
            assert!(links.contains(value), "{value}: {links:?}");
        }
    }

    #[test]
    fn whois_comparison_retains_one_success_and_ignores_timing_noise() {
        let old = Page::new(reply(b"status: old\nunchanged\n"));
        let mut page = Page::refreshed(reply(b"status: new\nunchanged\n"), &old);
        page.view_action("changes", Some(true)).unwrap();
        let mut doc = render(&target(), page, 80);
        assert!(
            doc.lines
                .iter()
                .any(|line| line.kind == Kind::Removed && line.text == "- status: old")
        );
        assert!(
            doc.lines
                .iter()
                .any(|line| line.kind == Kind::Added && line.text == "+ status: new")
        );
        doc.whois
            .as_mut()
            .unwrap()
            .view_action("changes", Some(false))
            .unwrap();
        doc.rerender_reply(80);
        assert!(doc.lines.iter().any(|line| line.text == "status: new"));
        let current = doc.whois.take().unwrap();
        let mut failed = Page::refreshed(reply(b"interrupted"), &current);
        failed.reply.notice = Some("timeout".into());
        let next = Page::refreshed(reply(b"third"), &failed);
        assert_eq!(
            &*next.previous.unwrap().hops[0].body,
            b"status: new\nunchanged\n"
        );
    }

    #[test]
    fn whois_display_bounds_pathological_rows_columns_and_combining_text() {
        for raw in [
            vec![b'\n'; MAX_RESPONSE],
            vec![b'x'; MAX_RESPONSE],
            vec![b'\t'; MAX_RESPONSE],
            "\u{301}".repeat(MAX_RESPONSE / 2).into_bytes(),
        ] {
            let mut page = Page::new(reply(&raw));
            page.view.wrap = true;
            let doc = render(&target(), page, 10);
            assert!(doc.lines.len() <= MAX_ROWS);
            assert!(
                doc.lines
                    .iter()
                    .map(|line| line.text.as_str())
                    .collect::<String>()
                    .contains("truncated")
            );
            assert!(
                doc.lines.iter().map(|line| line.text.len()).sum::<usize>() < MAX_RESPONSE + 8192
            );
            assert_eq!(&*doc.whois.unwrap().reply.hops[0].body, raw);
        }
    }

    async fn listener() -> (TcpListener, OneShotUrl) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target = server_target(
            &format!("127.0.0.1:{}", listener.local_addr().unwrap().port()),
            "example.com",
        )
        .unwrap();
        (listener, target)
    }
    async fn serve(listener: TcpListener, response: Vec<u8>, hold: Duration) -> Vec<u8> {
        let (socket, _) = listener.accept().await.unwrap();
        let mut socket = BufReader::new(socket);
        let mut query = Vec::new();
        socket.read_until(b'\n', &mut query).await.unwrap();
        let _ = socket.get_mut().write_all(&response).await;
        tokio::time::sleep(hold).await;
        query
    }

    #[tokio::test]
    async fn whois_follows_multiple_hops_and_retains_every_answer() {
        let (first, a) = listener().await;
        let (second, b) = listener().await;
        let (third, c) = listener().await;
        let root = tokio::spawn(serve(
            first,
            format!("ROOT\r\nrefer: {}\r\n", authority(&b)).into_bytes(),
            Duration::ZERO,
        ));
        let middle = tokio::spawn(serve(
            second,
            format!("MIDDLE\r\nRegistrar WHOIS Server: {}\r\n", authority(&c)).into_bytes(),
            Duration::ZERO,
        ));
        let leaf = tokio::spawn(serve(third, b"FINAL\r\n".to_vec(), Duration::ZERO));
        let reply = fetch(&a).await.unwrap();
        assert!(reply.successful());
        assert_eq!(reply.hops.len(), 3);
        for (hop, text) in reply.hops.iter().zip(["ROOT", "MIDDLE", "FINAL"]) {
            assert!(String::from_utf8_lossy(&hop.body).contains(text));
        }
        for task in [root, middle, leaf] {
            assert_eq!(task.await.unwrap(), b"example.com\r\n");
        }
    }

    #[tokio::test]
    async fn whois_detects_cycles_and_reports_dead_referrals_with_initial_data() {
        let (root, a) = listener().await;
        let task = tokio::spawn(serve(
            root,
            format!("ROOT\nrefer: {}\n", authority(&a)).into_bytes(),
            Duration::ZERO,
        ));
        let reply = fetch(&a).await.unwrap();
        assert!(reply.notice.unwrap().contains("cycle"));
        assert_eq!(reply.hops.len(), 1);
        task.await.unwrap();
        let (root, a) = listener().await;
        let (dead, b) = listener().await;
        drop(dead);
        let task = tokio::spawn(serve(
            root,
            format!("ROOT\nrefer: {}\n", authority(&b)).into_bytes(),
            Duration::ZERO,
        ));
        let reply = fetch(&a).await.unwrap();
        assert_eq!(reply.hops.len(), 2);
        assert!(reply.notice.is_some());
        assert!(String::from_utf8_lossy(&reply.hops[0].body).contains("ROOT"));
        assert_eq!(reply.hops[1].state, HopState::Incomplete);
        task.await.unwrap();
    }

    #[tokio::test]
    async fn whois_publishes_partial_data_and_preserves_it_at_deadline() {
        let (server, target) = listener().await;
        let task = tokio::spawn(serve(server, b"EARLY\r\n".to_vec(), Duration::from_secs(2)));
        let mut updates = Vec::new();
        let reply = exchange(&target, Duration::from_millis(180), |update| {
            updates.push(update);
            async { true }
        })
        .await
        .unwrap();
        assert!(updates.iter().any(|update| {
            !update.finished
                && update
                    .hops
                    .last()
                    .is_some_and(|hop| &*hop.body == b"EARLY\r\n")
        }));
        assert!(reply.finished && reply.notice.is_some());
        assert_eq!(&*reply.hops[0].body, b"EARLY\r\n");
        task.abort();
    }

    #[tokio::test]
    async fn whois_caps_received_bytes_without_discarding_them() {
        let (server, target) = listener().await;
        let task = tokio::spawn(serve(server, vec![b'x'; MAX_RESPONSE + 1], Duration::ZERO));
        let reply = fetch(&target).await.unwrap();
        assert_eq!(reply.bytes(), MAX_RESPONSE);
        assert!(reply.notice.unwrap().contains("truncated"));
        task.await.unwrap();
        let mut invalid = target;
        invalid.query = "one\r\ntwo".into();
        assert!(fetch(&invalid).await.unwrap_err().contains("control"));
    }

    #[test]
    fn whois_exports_exact_server_bytes_and_labels_combined_transcripts() {
        let raw = b"name:\tAndr\xe9\r\n\x1b[31mred\x1b[0m";
        let mut page = Page::new(reply(raw));
        assert_eq!(page.export(&target(), None).unwrap().body, raw);
        let mut second = hop(b"");
        second.target.host = "second.test".into();
        page.reply.hops.push(second);
        assert_eq!(page.export(&target(), Some(1)).unwrap().body, raw);
        let empty = page.export(&target(), Some(2)).unwrap();
        assert!(empty.body.is_empty() && !empty.fetch_body);
        assert!(page.export(&target(), Some(0)).is_err());
        assert!(page.export(&target(), Some(3)).is_err());
        let transcript = page.export(&target(), None).unwrap().body;
        assert!(transcript.windows(raw.len()).any(|bytes| bytes == raw));
        assert!(String::from_utf8_lossy(&transcript).contains("% WHOIS whois://second.test/"));
        page.reply.finished = false;
        assert!(
            page.export(&target(), None)
                .unwrap()
                .suggested_filename
                .ends_with("-partial.txt")
        );
    }

    #[test]
    fn whois_completed_server_links_are_usable_while_a_referral_loads() {
        let mut reply = reply(b"URL: https://example.test/complete");
        let mut pending = hop(b"URL: https://example.test/not-yet-complete");
        pending.target.host = "next.test".into();
        pending.state = HopState::Receiving;
        reply.hops.push(pending);
        reply.finished = false;
        let doc = render(&target(), Page::new(reply), 80);
        let links: Vec<_> = doc
            .lines
            .iter()
            .filter_map(|line| line.link.as_ref())
            .map(ToString::to_string)
            .collect();
        assert!(
            links
                .iter()
                .any(|link| link == "https://example.test/complete")
        );
        assert!(
            !links
                .iter()
                .any(|link| link == "https://example.test/not-yet-complete")
        );
    }

    #[test]
    fn whois_long_queries_and_records_leave_room_for_each_server() {
        let mut target = target();
        target.query = "q".repeat(8192);
        let mut reply = Reply::from_bytes(target.clone(), b"record\n".repeat(MAX_ROWS));
        for index in 1..MAX_HOPS {
            let mut next = reply.hops[0].clone();
            next.target.host = format!("server-{index}.test");
            reply.hops.push(next);
        }
        let mut page = Page::new(reply);
        page.view.wrap = true;
        let doc = render(&target, page.clone(), 2);
        assert!(doc.lines.len() <= MAX_ROWS);
        for hop in &page.reply.hops {
            assert!(
                doc.lines
                    .iter()
                    .any(|line| line.link.as_ref() == Some(&Link::OneShot(hop.target.clone())))
            );
        }
    }

    #[tokio::test]
    async fn whois_limits_the_whole_chain_in_bytes_and_hops() {
        let (first, a) = listener().await;
        let (second, b) = listener().await;
        let mut body = format!("refer: {}\r\n", authority(&b)).into_bytes();
        body.resize(600 * 1024, b'x');
        let root = tokio::spawn(serve(first, body, Duration::ZERO));
        let leaf = tokio::spawn(serve(second, vec![b'y'; 600 * 1024], Duration::ZERO));
        let reply = fetch(&a).await.unwrap();
        assert_eq!(reply.bytes(), MAX_RESPONSE);
        assert_eq!(reply.hops[0].body.len(), 600 * 1024);
        assert_eq!(reply.hops[1].state, HopState::Incomplete);
        assert!(reply.notice.unwrap().contains("truncated"));
        root.await.unwrap();
        leaf.await.unwrap();

        let mut chain = Vec::new();
        for _ in 0..=MAX_HOPS {
            chain.push(listener().await);
        }
        let start = chain[0].1.clone();
        let targets: Vec<_> = chain.iter().map(|(_, target)| target.clone()).collect();
        let mut tasks = Vec::new();
        for (index, (server, _)) in chain.into_iter().enumerate() {
            if index == MAX_HOPS {
                break;
            }
            tasks.push(tokio::spawn(serve(
                server,
                format!(
                    "answer {index}\r\nrefer: {}\r\n",
                    authority(&targets[index + 1])
                )
                .into_bytes(),
                Duration::ZERO,
            )));
        }
        let reply = fetch(&start).await.unwrap();
        assert_eq!(reply.hops.len(), MAX_HOPS);
        assert!(reply.notice.unwrap().contains("4 servers"));
        for task in tasks {
            task.await.unwrap();
        }
    }

    #[tokio::test]
    async fn whois_deadline_covers_referrals_and_slow_consumers() {
        let (first, a) = listener().await;
        let (second, b) = listener().await;
        let root = tokio::spawn(serve(
            first,
            format!("ROOT\nrefer: {}\n", authority(&b)).into_bytes(),
            Duration::from_millis(100),
        ));
        let leaf = tokio::spawn(serve(second, b"LEAF\n".to_vec(), Duration::from_secs(2)));
        let reply = exchange(&a, Duration::from_millis(300), |_| async { true })
            .await
            .unwrap();
        assert_eq!(reply.hops.len(), 2);
        assert_eq!(reply.hops[0].state, HopState::Complete);
        assert_eq!(&*reply.hops[1].body, b"LEAF\n");
        assert!(reply.notice.unwrap().contains("timed out"));
        root.await.unwrap();
        leaf.abort();

        let (server, target) = listener().await;
        let task = tokio::spawn(serve(server, b"RETAIN\n".to_vec(), Duration::from_secs(2)));
        let reply = exchange(&target, Duration::from_millis(250), |update| async move {
            if update.bytes() > 0 {
                std::future::pending::<()>().await;
            }
            true
        })
        .await
        .unwrap();
        assert_eq!(&*reply.hops[0].body, b"RETAIN\n");
        assert!(reply.notice.unwrap().contains("timed out"));
        task.abort();
    }
}
