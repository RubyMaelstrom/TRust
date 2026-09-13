//! Gopher transport, opaque URL octets and bounded menu/text presentation.
//! RFC Editor snapshot 2026-09-06: RFC 1436 §§2, 3.8 and appendix;
//! RFC 4266 §§2.1–2.3; RFC 3986 §§2.1, 2.4, 3.1, 3.2.2 and 3.5.

use crate::doc::{Doc, DocLine, Kind, Link, push_wrapped};
use crate::{gemini, text_reply};
use std::{fmt, future::Future, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::{Instant, sleep_until, timeout};

pub const MAX_RESPONSE: usize = 2 * 1024 * 1024;
const MAX_REQUEST: usize = 8192;
const MAX_LINE: usize = 8192;
const MAX_ROWS: usize = 4096;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const IDLE_TIMEOUT: Duration = Duration::from_secs(15);
const TOTAL_TIMEOUT: Duration = Duration::from_secs(120);
const UPDATE_INTERVAL: Duration = Duration::from_millis(150);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GopherUrl {
    pub host: String,
    pub port: u16,
    pub item_type: char,
    /// Opaque protocol octets: display encoding must never change a selector.
    pub selector: Vec<u8>,
    pub query: Option<Vec<u8>>,
    pub gopher_plus: Option<Vec<u8>>,
}

impl GopherUrl {
    pub fn new(host: String, port: u16, item_type: char, selector: Vec<u8>) -> Self {
        Self {
            host,
            port,
            item_type,
            selector,
            query: None,
            gopher_plus: None,
        }
    }

    pub fn parse(input: &str) -> Option<Self> {
        if input.len() > MAX_REQUEST * 3 + 4096 || input.chars().any(char::is_control) {
            return None;
        }
        let (scheme, rest) = input.split_once("://")?;
        if !scheme.eq_ignore_ascii_case("gopher") {
            return None;
        }
        // Split BEFORE percent decoding. Generic URL path normalization would
        // destroy opaque selectors such as /a/../b (RFC 1436 §2).
        let rest = rest.split('#').next()?;
        let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
        let authority_url = url::Url::parse(&format!("gopher://{authority}/")).ok()?;
        if !authority_url.username().is_empty()
            || authority_url.password().is_some()
            || authority_url.query().is_some()
            || authority_url.fragment().is_some()
        {
            return None;
        }
        let host = match authority_url.host()? {
            url::Host::Ipv6(ip) => ip.to_string(),
            url::Host::Ipv4(ip) => ip.to_string(),
            url::Host::Domain(host) => url::Host::parse(host).ok()?.to_string(),
        };
        let bytes = percent_decode(path)?;
        let (item_type, tail) = bytes
            .split_first()
            .map_or(('1', &[][..]), |(&t, tail)| (t as char, tail));
        if !item_type.is_ascii_graphic() {
            return None;
        }
        let mut fields = tail.split(|&b| b == b'\t');
        let mut result = Self::new(
            host,
            authority_url.port().unwrap_or(70),
            item_type,
            fields.next()?.to_vec(),
        );
        result.query = fields.next().map(<[u8]>::to_vec);
        result.gopher_plus = fields.next().map(<[u8]>::to_vec);
        if fields.next().is_some() || result.validate().is_err() {
            return None;
        }
        Some(result)
    }

    fn validate(&self) -> Result<(), String> {
        let fields = [
            &self.selector[..],
            self.query.as_deref().unwrap_or_default(),
            self.gopher_plus.as_deref().unwrap_or_default(),
        ];
        if fields
            .iter()
            .any(|v| v.iter().any(|b| matches!(b, b'\r' | b'\n' | b'\t')))
            || fields.iter().map(|v| v.len()).sum::<usize>() > MAX_REQUEST
        {
            return Err("Gopher request contains a line separator or exceeds 8 KiB".into());
        }
        if self.host.is_empty() || !self.item_type.is_ascii_graphic() {
            return Err("Invalid Gopher destination".into());
        }
        Ok(())
    }

    pub fn request(&self) -> Result<Vec<u8>, String> {
        self.validate()?;
        // Gopher+ has different framing and ASK semantics. Never silently
        // execute a plus command as an ordinary selector.
        if self.gopher_plus.is_some() {
            return Err("Gopher+ requests are not supported yet".into());
        }
        let mut request = self.selector.clone();
        if let Some(query) = &self.query {
            request.push(b'\t');
            request.extend_from_slice(query);
        }
        request.extend_from_slice(b"\r\n");
        Ok(request)
    }

    pub fn with_query(&self, query: &str) -> Result<Self, String> {
        let mut result = self.clone();
        result.query = Some(query.as_bytes().to_vec());
        result.validate()?;
        Ok(result)
    }

    pub fn needs_query(&self) -> bool {
        self.item_type == '7' && self.query.is_none()
    }
    pub fn is_image(&self) -> bool {
        matches!(self.item_type, 'I' | 'g' | 'p')
    }
    pub fn is_download(&self) -> bool {
        matches!(self.item_type, '4' | '5' | '6' | '9' | 'd' | 's' | ';')
    }
    pub fn is_text(&self) -> bool {
        matches!(self.item_type, '0' | '1' | '7' | 'h')
    }
    pub fn filename(&self) -> String {
        String::from_utf8_lossy(
            self.selector
                .rsplit(|&b| b == b'/')
                .next()
                .unwrap_or_default(),
        )
        .into_owned()
    }
}

fn percent_decode(value: &str) -> Option<Vec<u8>> {
    let mut result = Vec::with_capacity(value.len());
    let mut bytes = value.bytes();
    while let Some(b) = bytes.next() {
        result.push(if b == b'%' {
            ((bytes.next()? as char).to_digit(16)? * 16 + (bytes.next()? as char).to_digit(16)?)
                as u8
        } else {
            b
        });
    }
    Some(result)
}

fn encode(bytes: &[u8], f: &mut fmt::Formatter<'_>) -> fmt::Result {
    for &b in bytes {
        // URI serialization is independent of the display charset. Generic
        // URL consumers still must not normalize this opaque selector path.
        if b.is_ascii_alphanumeric() || b"-._/~".contains(&b) {
            write!(f, "{}", b as char)?;
        } else {
            write!(f, "%{b:02X}")?;
        }
    }
    Ok(())
}

impl fmt::Display for GopherUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.host.contains(':') {
            write!(f, "gopher://[{}]", self.host)?;
        } else {
            write!(f, "gopher://{}", self.host)?;
        }
        if self.port != 70 {
            write!(f, ":{}", self.port)?;
        }
        write!(f, "/{}", self.item_type)?;
        encode(&self.selector, f)?;
        if let Some(query) = &self.query {
            write!(f, "%09")?;
            encode(query, f)?;
        }
        if let Some(plus) = &self.gopher_plus {
            write!(f, "%09")?;
            encode(plus, f)?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Encoding {
    #[default]
    Auto,
    Utf8,
    Latin1,
    Cp437,
}
impl Encoding {
    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Utf8 => "UTF-8",
            Self::Latin1 => "Latin-1",
            Self::Cp437 => "CP437",
        }
    }
    pub fn next(self) -> Self {
        match self {
            Self::Auto => Self::Utf8,
            Self::Utf8 => Self::Latin1,
            Self::Latin1 => Self::Cp437,
            Self::Cp437 => Self::Auto,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct View {
    pub controls: text_reply::View,
    pub encoding: Encoding,
    /// Physical source line and first display row for each wrapped row.
    pub sources: Vec<usize>,
    pub owners: Vec<usize>,
    /// Widest sanitized display line before wrapping, in terminal cells.
    /// Retain this during parsing so painting need not rescan the document.
    pub source_columns: usize,
}

/// Reading-column preference, not a protocol limit: RFC 1436 §3.9 discusses
/// 80-column screens and short menu labels, but does not limit text-file width.
pub const PREFERRED_COLUMNS: usize = 80;

impl View {
    fn measure_line(&mut self, text: &str) {
        self.source_columns = self
            .source_columns
            .max(unicode_width::UnicodeWidthStr::width(text));
    }

    pub fn reading_columns(&self, available: usize) -> usize {
        available.min(PREFERRED_COLUMNS.max(self.source_columns))
    }
}

impl Default for View {
    fn default() -> Self {
        Self {
            controls: text_reply::View {
                wrap: true,
                ..Default::default()
            },
            encoding: Encoding::Auto,
            sources: Vec::new(),
            owners: Vec::new(),
            source_columns: 0,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Page {
    pub reply: text_reply::Reply,
    pub view: View,
}
impl Page {
    pub fn new(reply: text_reply::Reply) -> Self {
        Self {
            reply,
            view: View::default(),
        }
    }
}
impl From<Vec<u8>> for Page {
    fn from(body: Vec<u8>) -> Self {
        Self::new(text_reply::Reply {
            body,
            finished: true,
            notice: None,
        })
    }
}

/// RFC 8305 §§4–5: alternate address families and stagger connection
/// attempts; the winning socket cancels the rest. The system resolver still
/// owns DNS ordering, so this is not a separate DNS implementation.
async fn connect_addresses(host: &str, port: u16) -> std::io::Result<TcpStream> {
    use futures::{StreamExt, stream::FuturesUnordered};
    let resolved: Vec<_> = tokio::net::lookup_host((host, port))
        .await?
        .take(32)
        .collect();
    let first_v6 = resolved.first().is_some_and(|a| a.is_ipv6());
    let mut v4 = std::collections::VecDeque::new();
    let mut v6 = std::collections::VecDeque::new();
    for address in resolved {
        if address.is_ipv6() {
            v6.push_back(address);
        } else {
            v4.push_back(address);
        }
    }
    let mut candidates = std::collections::VecDeque::new();
    let mut prefer_v6 = first_v6;
    while !v4.is_empty() || !v6.is_empty() {
        let address = if prefer_v6 {
            v6.pop_front().or_else(|| v4.pop_front())
        } else {
            v4.pop_front().or_else(|| v6.pop_front())
        };
        candidates.push_back(address.unwrap());
        prefer_v6 = !prefer_v6;
    }
    let mut attempts = FuturesUnordered::new();
    let mut next = Instant::now();
    let mut error = std::io::Error::new(std::io::ErrorKind::AddrNotAvailable, "No addresses found");
    loop {
        if candidates.is_empty() && attempts.is_empty() {
            return Err(error);
        }
        tokio::select! {
            _ = sleep_until(next), if !candidates.is_empty() => {
                attempts.push(TcpStream::connect(candidates.pop_front().unwrap()));
                next = Instant::now() + Duration::from_millis(250);
            }
            Some(result) = attempts.next(), if !attempts.is_empty() => {
                match result { Ok(stream) => return Ok(stream), Err(e) => { error = e; next = Instant::now(); } }
            }
        }
    }
}

pub async fn connect(url: &GopherUrl) -> Result<TcpStream, String> {
    let request = url.request()?;
    let mut stream = timeout(CONNECT_TIMEOUT, connect_addresses(&url.host, url.port))
        .await
        .map_err(|_| "Gopher connection timed out".to_string())?
        .map_err(|e| format!("Gopher connection failed: {e}"))?;
    timeout(CONNECT_TIMEOUT, stream.write_all(&request))
        .await
        .map_err(|_| "Gopher request timed out".to_string())?
        .map_err(|e| format!("Gopher request failed: {e}"))?;
    Ok(stream)
}

pub async fn fetch(url: &GopherUrl) -> Result<Page, String> {
    fetch_updates(url, |_| async { true }).await.map(Page::new)
}

pub async fn fetch_updates<F, Fut>(
    url: &GopherUrl,
    mut publish: F,
) -> Result<text_reply::Reply, String>
where
    F: FnMut(text_reply::Reply) -> Fut,
    Fut: Future<Output = bool>,
{
    if !(url.is_text() || url.is_image()) {
        return Err(format!(
            "Gopher type {} needs a download or terminal handler",
            url.item_type
        ));
    }
    let mut stream = connect(url).await?;
    let mut body = Vec::new();
    let mut buf = [0; 8192];
    let total = Instant::now() + TOTAL_TIMEOUT;
    let mut idle = Instant::now() + IDLE_TIMEOUT;
    let mut next_update = Instant::now() + UPDATE_INTERVAL;
    let mut dirty = false;
    let mut scanned = 0;
    let mut line_start = 0;
    let mut complete_prefix = 0;
    let notice = loop {
        tokio::select! {
            result = tokio::time::timeout_at(total.min(idle), stream.read(&mut buf)) => {
                match result {
                    Ok(Ok(0)) => break None,
                    Ok(Ok(n)) => {
                        let room = MAX_RESPONSE.saturating_sub(body.len());
                        body.extend_from_slice(&buf[..n.min(room)]);
                        idle = Instant::now() + IDLE_TIMEOUT;
                        let mut terminated = false;
                        if url.is_text() {
                            while scanned < body.len() {
                                if body[scanned] == b'\n' {
                                    let line = body[line_start..scanned].strip_suffix(b"\r").unwrap_or(&body[line_start..scanned]);
                                    if line == b"." { body.truncate(scanned + 1); terminated = true; break; }
                                    complete_prefix = scanned + 1;
                                    line_start = scanned + 1;
                                }
                                scanned += 1;
                            }
                        }
                        if terminated { break None; }
                        if n > room { break Some("Incomplete Gopher reply: truncated at 2 MiB".into()); }
                        dirty = url.is_text() && complete_prefix > 0;
                    }
                    Ok(Err(e)) => break Some(format!("Incomplete Gopher reply: {e}")),
                    Err(_) => break Some("Incomplete Gopher reply: server timed out".into()),
                }
            }
            _ = sleep_until(next_update), if dirty => {
                if !publish(text_reply::Reply { body: body[..complete_prefix].to_vec(), finished: false, notice: None }).await {
                    return Err("Gopher request cancelled".into());
                }
                dirty = false;
                next_update = Instant::now() + UPDATE_INTERVAL;
            }
        }
    };
    if body.is_empty()
        && let Some(error) = &notice
    {
        return Err(error.clone());
    }
    Ok(text_reply::Reply {
        body,
        finished: true,
        notice,
    })
}

pub fn status(url: &GopherUrl, reply: &text_reply::Reply) -> String {
    format!(
        "{url} · {} bytes{} · W wrap · E encoding{}",
        reply.body.len(),
        if reply.finished { "" } else { " · receiving" },
        reply
            .notice
            .as_ref()
            .map(|s| format!(" · {s}"))
            .unwrap_or_default()
    )
}

fn kind_of(t: char) -> Kind {
    match t {
        'i' => Kind::Info,
        '3' => Kind::Error,
        '1' => Kind::Dir,
        '0' => Kind::Document,
        '7' => Kind::Search,
        _ => Kind::OtherLink,
    }
}

/// Absolute protocol links share a single router (including Telnet/TLS).
pub fn absolute_link(target: &str) -> Option<Link> {
    if target.split_once(':').is_some_and(|(s, _)| {
        matches!(
            s.to_ascii_lowercase().as_str(),
            "gopher"
                | "gemini"
                | "http"
                | "https"
                | "finger"
                | "whois"
                | "dict"
                | "telnet"
                | "telnets"
                | "telnet+tls"
        )
    }) {
        crate::core::parse_navigation_target(target)
            .map(|(link, _)| link)
            .ok()
    } else {
        None
    }
}

fn resolve_gmi(base: &GopherUrl, target: &str) -> Link {
    if let Some(link) = absolute_link(target) {
        return link;
    }
    if let Some(rest) = target.strip_prefix("//") {
        return GopherUrl::parse(&format!("gopher://{rest}"))
            .map(Link::Gopher)
            .unwrap_or_else(|| Link::External(target.into()));
    }
    if target.contains(':') {
        return Link::External(target.into());
    }
    let base_selector = String::from_utf8_lossy(&base.selector);
    let selector = if target.starts_with('/') {
        gemini::normalize(target)
    } else {
        let dir = base_selector
            .rfind('/')
            .map(|i| &base_selector[..=i])
            .unwrap_or("/");
        gemini::normalize(&format!("{dir}{target}"))
    };
    let t = if selector.ends_with('/') { '1' } else { '0' };
    Link::Gopher(GopherUrl::new(
        base.host.clone(),
        base.port,
        t,
        percent_decode(&selector).unwrap_or_else(|| selector.into_bytes()),
    ))
}

fn decode(bytes: &[u8], encoding: Encoding) -> String {
    match encoding {
        Encoding::Cp437 => String::from_utf8_lossy(&crate::cp437::decode(bytes)).into_owned(),
        Encoding::Latin1 => bytes.iter().map(|&b| char::from(b)).collect(),
        Encoding::Auto if std::str::from_utf8(bytes).is_err() => {
            bytes.iter().map(|&b| char::from(b)).collect()
        }
        _ => String::from_utf8_lossy(bytes).into_owned(),
    }
}
fn split_lines(raw: &[u8]) -> impl Iterator<Item = &[u8]> {
    raw.split_inclusive(|&b| b == b'\n')
        .map(|l| l.strip_suffix(b"\n").unwrap_or(l))
        .map(|l| l.strip_suffix(b"\r").unwrap_or(l))
}
/// RFC 1436 text framing also applies before the optional Gemtext/HTML parser.
pub fn text_body(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len().min(MAX_RESPONSE));
    for line in split_lines(&raw[..raw.len().min(MAX_RESPONSE)]) {
        if line == b"." {
            break;
        }
        let line = if line.starts_with(b"..") {
            &line[1..]
        } else {
            line
        };
        out.extend_from_slice(line);
        out.push(b'\n');
    }
    out
}

pub fn parse(url: &GopherUrl, raw: Vec<u8>, cp437: bool, width: usize) -> Doc {
    let mut page = Page::from(raw);
    if cp437 {
        page.view.encoding = Encoding::Cp437;
    }
    render(url, page, width)
}

pub fn render(url: &GopherUrl, mut page: Page, width: usize) -> Doc {
    let width = width.max(10);
    let wrap = if page.view.controls.wrap {
        width
    } else {
        usize::MAX / 4
    };
    page.reply.body.truncate(MAX_RESPONSE);
    page.view.controls.loading = !page.reply.finished;
    page.view.controls.notice = page.reply.notice.clone();
    page.view.owners.clear();
    page.view.sources.clear();
    page.view.source_columns = 0;
    let mut lines = Vec::new();
    let mut truncated = false;
    let mut previous_type = None;
    let menu = matches!(url.item_type, '1' | '7');
    let gemtext = !menu && url.filename().to_ascii_lowercase().ends_with(".gmi");
    if gemtext {
        let body = text_body(&page.reply.body);
        // Bound input BEFORE handing it to the richer parser.
        let bounded: Vec<u8> = split_lines(&body)
            .take(MAX_ROWS)
            .flat_map(|l| l[..l.len().min(MAX_LINE)].iter().copied().chain(*b"\n"))
            .collect();
        let logical =
            gemini::parse_gemtext(&bounded, usize::MAX / 4, &|target| resolve_gmi(url, target));
        truncated = bounded.len() < body.len();
        for (source, line) in logical.into_iter().enumerate() {
            if lines.len() >= MAX_ROWS - 1 {
                truncated = true;
                break;
            }
            let start = lines.len();
            let (display, clipped) = text_reply::display_text(line.text.as_bytes(), false);
            truncated |= clipped;
            page.view.measure_line(&display);
            push_wrapped(
                &mut lines,
                line.kind,
                display,
                line.link,
                if line.kind == Kind::Pre {
                    usize::MAX / 4
                } else {
                    wrap
                },
            );
            if lines.len() > MAX_ROWS - 1 {
                lines.truncate(MAX_ROWS - 1);
                truncated = true;
            }
            page.view
                .owners
                .extend(std::iter::repeat_n(start, lines.len() - start));
            page.view
                .sources
                .extend(std::iter::repeat_n(source, lines.len() - start));
        }
    } else {
        for (source, line) in split_lines(&page.reply.body).enumerate() {
            if line == b"." {
                break;
            }
            if lines.len() >= MAX_ROWS - 1 {
                truncated = true;
                break;
            }
            let mut t = 'i';
            let mut link = None;
            let mut label = line;
            if menu {
                let mut fields = line.splitn(5, |&b| b == b'\t');
                let first = fields.next().unwrap_or_default();
                if let (Some(&item_type), Some(selector), Some(host), Some(port)) =
                    (first.first(), fields.next(), fields.next(), fields.next())
                {
                    t = item_type as char;
                    if t == '+' {
                        t = previous_type.unwrap_or('i');
                    } else if !matches!(t, 'i' | '3') {
                        previous_type = Some(t);
                    }
                    label = &first[1..];
                    let host = std::str::from_utf8(host)
                        .ok()
                        .filter(|h| h.len() <= 1024)
                        .and_then(|h| url::Host::parse(h).ok())
                        .map(|h| h.to_string().trim_matches(['[', ']']).to_string());
                    let port = std::str::from_utf8(port)
                        .ok()
                        .and_then(|p| p.parse::<u16>().ok());
                    if t == 'h' && selector.starts_with(b"URL:") && selector.len() <= MAX_REQUEST {
                        link = std::str::from_utf8(&selector[4..]).ok().map(|target| {
                            absolute_link(target).unwrap_or_else(|| Link::External(target.into()))
                        });
                    } else if !matches!(t, 'i' | '3')
                        && selector.len() <= MAX_REQUEST
                        && let (Some(host), Some(port)) = (host, port)
                    {
                        let target = GopherUrl::new(host.clone(), port, t, selector.to_vec());
                        if target.validate().is_ok() {
                            link = Some(if t == '8' {
                                Link::Telnet {
                                    host,
                                    port,
                                    tls: false,
                                }
                            } else {
                                Link::Gopher(target)
                            });
                        }
                    }
                }
            } else {
                t = '0';
                if line.starts_with(b"..") {
                    label = &line[1..];
                }
            }
            if label.len() > MAX_LINE {
                truncated = true;
            }
            let decoded = decode(&label[..label.len().min(MAX_LINE)], page.view.encoding);
            let (display, clipped) = text_reply::display_text(decoded.as_bytes(), false);
            truncated |= clipped;
            page.view.measure_line(&display);
            let start = lines.len();
            push_wrapped(
                &mut lines,
                if menu { kind_of(t) } else { Kind::Text },
                display,
                link,
                wrap,
            );
            if lines.len() > MAX_ROWS - 1 {
                lines.truncate(MAX_ROWS - 1);
                truncated = true;
            }
            page.view
                .owners
                .extend(std::iter::repeat_n(start, lines.len() - start));
            page.view
                .sources
                .extend(std::iter::repeat_n(source, lines.len() - start));
        }
    }
    if truncated {
        lines.push(DocLine {
            kind: Kind::Error,
            text:
                "Display truncated at the Gopher line/row limit; use save for the received source."
                    .into(),
            link: None,
        });
        page.view.sources.push(usize::MAX);
        page.view.owners.push(lines.len() - 1);
        page.view.measure_line(&lines.last().unwrap().text);
    }
    if let Some(notice) = &page.reply.notice {
        lines.push(DocLine {
            kind: Kind::Error,
            text: notice.clone(),
            link: None,
        });
        page.view.sources.push(usize::MAX);
        page.view.owners.push(lines.len() - 1);
        page.view.measure_line(notice);
    }
    // A wider viewport can reveal the entire line after horizontal panning.
    // Discard the stale offset before the now-fitting page is centered.
    page.view.controls.horizontal = if page.view.controls.wrap {
        0
    } else {
        page.view
            .controls
            .horizontal
            .min(page.view.source_columns.saturating_sub(width))
    };
    let mut doc = Doc::from_lines(
        Link::Gopher(url.clone()),
        lines,
        page.reply.body,
        width,
        page.view.encoding == Encoding::Cp437,
        None,
    );
    doc.gopher = Some(page.view);
    doc
}

/// Feed Gopher HTML/images into the existing document renderer, retaining the
/// Gopher base URL (WHATWG HTML #document-base-url / #read-html). This is a
/// representation adapter: no HTTP request, headers, cookies or timing exist.
pub fn representation(url: &GopherUrl, page: Page) -> Result<crate::http::Response, String> {
    if let Some(notice) = page.reply.notice {
        return Err(notice);
    }
    let mime = if url.is_image() {
        crate::img::sniff(&page.reply.body).ok_or("Gopher server returned an unrecognized image")?
    } else {
        "text/html"
    };
    let response = crate::http::Response {
        url: url::Url::parse(&url.to_string()).map_err(|e| e.to_string())?,
        status: 200,
        content_type: mime.into(),
        headers: Vec::new(),
        body: if url.is_image() {
            page.reply.body
        } else {
            text_body(&page.reply.body)
        },
        rendered: None,
        js: None,
        blobs: None,
        live: None,
        declarative_refresh: None,
        challenge: None,
        from_post: false,
        timing: None,
    };
    Ok(if url.is_image() {
        crate::http::image_navigation_response(response, mime)
    } else {
        response
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    #[test]
    fn urls_round_trip_opaque_octets_and_searches() {
        for input in [
            "gopher://example.test",
            "GOPHER://[::1]/1",
            "gopher://[::1]:7070/0/a%20b%25%FF",
            "gopher://e/7/search%09rust%20lang",
            "gopher://e/7%09",
            "gopher://e/00abc",
            "gopher://e/1/a/../b",
        ] {
            let url = GopherUrl::parse(input).unwrap();
            assert_eq!(
                GopherUrl::parse(&url.to_string()),
                Some(url.clone()),
                "{input}"
            );
        }
        let url = GopherUrl::parse("gopher://[::1]/7/search%09rust%20lang#ignored").unwrap();
        assert_eq!(url.host, "::1");
        assert_eq!(url.request().unwrap(), b"/search\trust lang\r\n");
        assert_eq!(
            GopherUrl::parse("gopher://e/1/a/../b").unwrap().selector,
            b"/a/../b"
        );
    }
    #[test]
    fn invalid_requests_never_become_extra_protocol_lines() {
        for bad in [
            "gopher://e/0/a\r\nb",
            "gopher://e/0/a%0Db",
            "gopher://e/7/s%09q%0A",
            "gopher://e/0/%GG",
            "gopher://e/0/%",
            "gopher://e:bad/1",
            "gopher://u:p@e/1",
            "gopher:///1",
        ] {
            assert!(GopherUrl::parse(bad).is_none(), "{bad:?}");
        }
        let mut url = GopherUrl::parse("gopher://e/0/file").unwrap();
        url.selector.push(b'\n');
        assert!(url.request().is_err());
        let url = GopherUrl::parse("gopher://e/7/s%09q%09+").unwrap();
        assert!(url.request().unwrap_err().contains("Gopher+"));
    }
    #[test]
    fn labels_and_selector_bytes_are_independent_and_mirrors_inherit_type() {
        let base = GopherUrl::parse("gopher://e").unwrap();
        let raw = b"0caf\xe9\t/\x82\te\t70\r\n+mirror\t/file\tmirror\t70\r\n.\r\n";
        for cp437 in [false, true] {
            let doc = parse(&base, raw.to_vec(), cp437, 80);
            let Some(Link::Gopher(target)) = &doc.lines[0].link else {
                panic!()
            };
            assert_eq!(target.selector, b"/\x82");
            assert_eq!(GopherUrl::parse(&target.to_string()), Some(target.clone()));
            assert!(matches!(&doc.lines[1].link, Some(Link::Gopher(u)) if u.item_type == '0'));
        }
        assert_eq!(parse(&base, raw.to_vec(), false, 80).lines[0].text, "café");
    }
    #[test]
    fn wrapped_menu_rows_remain_one_clickable_item() {
        let url = GopherUrl::parse("gopher://e").unwrap();
        let doc = parse(
            &url,
            format!("1{}\t/sel\te\t70\r\n.\r\n", "words ".repeat(20)).into_bytes(),
            false,
            20,
        );
        assert!(doc.lines.len() > 1);
        assert_eq!(doc.lines.iter().filter(|l| l.link.is_some()).count(), 1);
        for row in 0..doc.lines.len() {
            assert_eq!(doc.line_link(row), doc.lines[0].link.as_ref());
            assert_eq!(doc.link_owner(row), 0);
        }
    }
    #[test]
    fn text_and_gemtext_deframe_before_rendering() {
        let url = GopherUrl::parse("gopher://e/0/file.txt").unwrap();
        let doc = parse(
            &url,
            b"hello\r\n..dot\r\n\r\n.\r\nignored".to_vec(),
            false,
            80,
        );
        assert_eq!(
            doc.lines
                .iter()
                .map(|l| l.text.as_str())
                .collect::<Vec<_>>(),
            ["hello", ".dot", ""]
        );
        let url = GopherUrl::parse("gopher://e/0/phlog/file.gmi").unwrap();
        let doc = parse(
            &url,
            b"# Hello\r\n..dot\r\n=> next.gmi Next\r\n.\r\nignored".to_vec(),
            false,
            80,
        );
        assert_eq!(doc.lines[1].text, ".dot");
        assert!(
            matches!(&doc.lines[2].link, Some(Link::Gopher(u)) if u.selector == b"/phlog/next.gmi")
        );
        assert!(!doc.lines.iter().any(|l| l.text.contains("ignored")));
    }
    #[test]
    fn menu_url_items_route_all_protocols() {
        let base = GopherUrl::parse("gopher://e").unwrap();
        for target in [
            "https://example.test/x",
            "gopher://e/7/s%09q",
            "gemini://e/x",
            "finger://e/alice",
            "whois://e/example.test",
            "dict://e/d:hello",
            "telnet://[::1]:23",
            "telnets://e:992",
        ] {
            let doc = parse(
                &base,
                format!("hLabel\tURL:{target}\te\t70\r\n.\r\n").into_bytes(),
                false,
                80,
            );
            assert!(doc.lines[0].link.is_some(), "{target}");
            assert!(
                !matches!(doc.lines[0].link, Some(Link::External(_))),
                "{target}"
            );
        }
    }
    #[test]
    fn parser_bounds_rows_columns_and_untrusted_controls() {
        let base = GopherUrl::parse("gopher://e").unwrap();
        let doc = parse(&base, vec![b'\n'; 65536], false, 80);
        assert!(doc.lines.len() <= MAX_ROWS);
        assert!(doc.lines.last().unwrap().text.contains("truncated"));
        let raw = format!("i{}\x1b[31m\x00\t\te\t70\r\n.\r\n", "a".repeat(100000));
        let doc = parse(&base, raw.into_bytes(), false, 10);
        assert!(doc.lines.len() <= MAX_ROWS);
        assert!(doc.lines.iter().all(|l| !l.text.contains(['\x1b', '\0'])));
    }
    #[tokio::test]
    async fn completed_reply_does_not_wait_for_eof_and_request_is_decoded() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = GopherUrl::parse(&format!(
            "gopher://127.0.0.1:{}/7/search%09rust%20lang",
            listener.local_addr().unwrap().port()
        ))
        .unwrap();
        let (sent, received) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut byte = [0];
            while s.read_exact(&mut byte).await.is_ok() {
                request.push(byte[0]);
                if byte[0] == b'\n' {
                    break;
                }
            }
            sent.send(request).unwrap();
            for chunk in [b"iHello\t\te\t70\r\n.".as_slice(), b"\r", b"\ntrailer"] {
                s.write_all(chunk).await.unwrap();
                tokio::task::yield_now().await;
            }
            std::future::pending::<()>().await;
        });
        let page = timeout(Duration::from_secs(2), fetch(&url))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(received.await.unwrap(), b"/search\trust lang\r\n");
        assert!(page.reply.body.ends_with(b".\r\n"));
        assert!(!page.reply.body.ends_with(b"trailer"));
        server.abort();
    }
    #[tokio::test]
    async fn slow_reply_publishes_complete_lines_and_can_cancel() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = GopherUrl::parse(&format!(
            "gopher://127.0.0.1:{}/0/file",
            listener.local_addr().unwrap().port()
        ))
        .unwrap();
        let server = tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut request = [0; 64];
            let _ = s.read(&mut request).await;
            s.write_all(b"readable\r\nincomplete").await.unwrap();
            std::future::pending::<()>().await;
        });
        let result = timeout(
            Duration::from_secs(2),
            fetch_updates(&url, |reply| async move {
                assert_eq!(reply.body, b"readable\r\n");
                assert!(!reply.finished);
                false
            }),
        )
        .await
        .unwrap();
        assert!(result.unwrap_err().contains("cancelled"));
        server.abort();
    }
}

#[cfg(test)]
mod regression_tests {
    use super::*;
    use crate::doc::{Kind, Link};

    /// Unwrap a gopher link in tests.
    fn gopher_link(line: &crate::doc::DocLine) -> &GopherUrl {
        match line.link.as_ref().unwrap() {
            Link::Gopher(url) => url,
            other => panic!("expected gopher link, got {other:?}"),
        }
    }

    #[test]
    fn parses_urls() {
        let url = GopherUrl::parse("gopher://gopher.floodgap.com").unwrap();
        assert_eq!(url.host, "gopher.floodgap.com");
        assert_eq!((url.port, url.item_type), (70, '1'));
        assert_eq!(url.selector, b"");

        let url = GopherUrl::parse("gopher://host:7070/0/docs/readme.txt").unwrap();
        assert_eq!((url.port, url.item_type), (7070, '0'));
        assert_eq!(url.selector, b"/docs/readme.txt");

        assert!(GopherUrl::parse("https://example.com").is_none());
        assert!(GopherUrl::parse("gopher://").is_none());
    }

    #[test]
    fn parses_menus() {
        let url = GopherUrl::parse("gopher://example.org").unwrap();
        let raw = b"iWelcome to the hole\t\terror.host\t1\r\n\
                    1Deep Tunnels\t/tunnels\texample.org\t70\r\n\
                    0README\t/readme\tmirror.example\t7070\r\n\
                    7Search the void\t/search\texample.org\t70\r\n\
                    3Something broke\t\terror.host\t1\r\n\
                    stray text without tabs\r\n\
                    \tno type byte at all\t\t\r\n\
                    .\r\nignored after terminator";
        let doc = parse(&url, raw.to_vec(), false, 80);

        assert_eq!(doc.lines.len(), 7);
        assert_eq!(doc.lines[0].kind, Kind::Info);
        assert!(doc.lines[0].link.is_none());
        let dir = gopher_link(&doc.lines[1]);
        assert_eq!(
            (dir.item_type, dir.selector.as_slice()),
            ('1', b"/tunnels".as_slice())
        );
        assert_eq!(doc.lines[1].kind, Kind::Dir);
        let txt = gopher_link(&doc.lines[2]);
        assert_eq!((txt.host.as_str(), txt.port), ("mirror.example", 7070));
        assert_eq!(doc.lines[2].kind, Kind::Document);
        assert_eq!(gopher_link(&doc.lines[3]).item_type, '7');
        assert_eq!(doc.lines[3].kind, Kind::Search);
        assert!(doc.lines[4].link.is_none(), "errors are not links");
        assert_eq!(doc.lines[4].kind, Kind::Error);
        assert_eq!(doc.lines[5].text, "stray text without tabs");
        // A tab-leading line (empty type field) must not panic.
        assert_eq!(doc.lines[6].kind, Kind::Info);
    }

    #[test]
    fn item_type_eight_is_a_telnet_session_target() {
        // RFC 1436 §3.8 defines type 8 as a Telnet session and puts the
        // user/display string in the selector field.
        let url = GopherUrl::parse("gopher://example.org").unwrap();
        let doc = parse(
            &url,
            b"8Play the MUD\tguest\tmud.example\t2323\r\n.\r\n".to_vec(),
            false,
            80,
        );
        assert!(matches!(
            &doc.lines[0].link,
            Some(Link::Telnet { host, port: 2323, tls: false }) if host == "mud.example"
        ));
    }

    #[test]
    fn wraps_long_lines_at_word_boundaries() {
        // Text document: a 100-char line wraps to the given width.
        let url = GopherUrl::parse("gopher://e.org/0/x").unwrap();
        let long = "word ".repeat(20);
        let doc = parse(&url, format!("{long}\r\n.\r\n").into_bytes(), false, 20);
        assert!(doc.lines.len() >= 5, "got {} lines", doc.lines.len());
        assert!(doc.lines.iter().all(|l| l.text.chars().count() <= 20));
        assert!(doc.lines.iter().all(|l| !l.text.contains("wor d")));

        // Menu link: only the first wrapped row keeps the link.
        let url = GopherUrl::parse("gopher://e.org").unwrap();
        let menu = format!("1{}\t/sel\te.org\t70\r\n.\r\n", "Linkword ".repeat(8));
        let doc = parse(&url, menu.into_bytes(), false, 20);
        assert!(doc.lines.len() >= 2);
        assert!(doc.lines[0].link.is_some());
        assert!(doc.lines[1..].iter().all(|l| l.link.is_none()));
        // Continuations keep the kind so they style like their item.
        assert!(doc.lines[1..].iter().all(|l| l.kind == Kind::Dir));
    }

    #[test]
    fn reading_column_uses_authored_display_cells_before_wrapping() {
        let url = GopherUrl::parse("gopher://example.test").unwrap();
        let label = "界".repeat(44);
        let raw = format!("0{label}\t/{}\texample.test\t70\r\n.\r\n", "s".repeat(200));
        let mut doc = parse(&url, raw.as_bytes().to_vec(), false, 60);
        let view = doc.gopher.as_ref().unwrap();
        assert_eq!(
            view.source_columns, 88,
            "measure cells, not bytes or selectors"
        );
        assert_eq!(view.reading_columns(120), 88);
        assert_eq!(view.reading_columns(60), 60);
        assert!(doc.lines.len() > 1);
        doc.rerender_reply(120);
        assert_eq!(doc.lines.len(), 1);
        assert_eq!(doc.lines[0].text, label);
        assert_eq!(doc.raw, raw.as_bytes());

        let url = GopherUrl::parse("gopher://example.test/0/phlog").unwrap();
        let doc = parse(
            &url,
            b"  one\ttwo  \r\n\r\n..three\r\n.\r\n".to_vec(),
            false,
            120,
        );
        assert_eq!(
            doc.lines
                .iter()
                .map(|l| l.text.as_str())
                .collect::<Vec<_>>(),
            ["  one   two  ", "", ".three"]
        );
        let view = doc.gopher.as_ref().unwrap();
        assert_eq!(view.source_columns, 13);
        assert_eq!(view.reading_columns(120), 80);
    }

    #[test]
    fn renders_gmi_files_as_gemtext_with_gopher_links() {
        let url = GopherUrl::parse("gopher://e.org/0/phlog/post.gmi").unwrap();
        let body = b"# Hello from gopherspace\n\
                     => other.gmi Next post\n\
                     => ../top.gmi Up top\n\
                     => sub/ A menu\n\
                     => gemini://capsule.example/x Crossover\n\
                     => https://example.com/ Web\n";
        let doc = parse(&url, body.to_vec(), false, 80);

        assert_eq!(doc.lines[0].kind, Kind::Heading(1));
        assert_eq!(doc.lines[1].kind, Kind::GemLink);
        // Relative links resolve to gopher selectors on the same host.
        let next = gopher_link(&doc.lines[1]);
        assert_eq!(
            (next.item_type, next.selector.as_slice()),
            ('0', b"/phlog/other.gmi".as_slice())
        );
        let up = gopher_link(&doc.lines[2]);
        assert_eq!(
            (up.item_type, up.selector.as_slice()),
            ('0', b"/top.gmi".as_slice())
        );
        let menu = gopher_link(&doc.lines[3]);
        assert_eq!(
            (menu.item_type, menu.selector.as_slice()),
            ('1', b"/phlog/sub/".as_slice())
        );
        // Absolute URLs keep their scheme.
        assert!(matches!(doc.lines[4].link, Some(Link::Gemini(_))));
        assert!(matches!(doc.lines[5].link, Some(Link::Http(_))));

        // Plain .txt files still render as plain gopher text.
        let url = GopherUrl::parse("gopher://e.org/0/notes.txt").unwrap();
        let doc = parse(&url, b"# not a heading\n".to_vec(), false, 80);
        assert_eq!(doc.lines[0].kind, Kind::Text);
        assert_eq!(doc.lines[0].text, "# not a heading");
    }

    #[test]
    fn parses_text_with_byte_stuffing() {
        let url = GopherUrl::parse("gopher://example.org/0/readme").unwrap();
        let doc = parse(&url, b"hello\r\n..dotted line\r\n.\r\n".to_vec(), false, 80);
        let texts: Vec<&str> = doc.lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts, ["hello", ".dotted line"]);
    }
}
