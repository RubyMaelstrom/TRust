//! RFC 2229 §§2.2–2.4, 3.1–3.6, 3.10–3.11, 4: incremental CRLF framing and
//! per-command response states. A successful DEFINE ends at 250, not at TCP
//! EOF. CLIENT and OPTION MIME are pipelined with the lookup (§4); when the
//! user saved AUTH credentials for the server, the lookup waits for the
//! banner's msg-id so the AUTH digest (§3.11) can precede it. With MIME on,
//! every text response starts with a (possibly empty) MIME header and a blank
//! line (§3.10.1), decoded by `mime.rs`.
use super::{Operation, Target, atom, mime};
use std::{future::Future, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    time::{Instant, timeout_at},
};

const MAX_RESPONSE: usize = crate::text_reply::MAX_RESPONSE;
const MAX_ITEMS: usize = 4096;
const MAX_DEFINITIONS: usize = 256;
const TIMEOUT: Duration = Duration::from_secs(15);
const UPDATE_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Definition {
    pub word: String,
    pub database: String,
    pub description: String,
    pub body: Arc<String>,
    pub complete: bool,
    /// A MIME content type other than plain text (§3.10.1).
    pub content_type: Option<String>,
    /// Non-text MIME parts the user can save.
    pub attachments: Vec<mime::Attachment>,
    /// MIME parts that could not be decoded and were skipped.
    pub skipped: Vec<String>,
}

/// RFC 2229 §3.11 AUTH, as this session went.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Auth {
    #[default]
    None,
    /// 230 Authentication successful.
    Accepted(String),
    /// 531 Access denied.
    Denied { user: String, message: String },
    /// 5xx: AUTH is not implemented by this server.
    Unsupported(String),
    /// AUTH could not be attempted (no msg-id in the banner, or no secret).
    Unavailable(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub description: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Reply {
    pub raw: Arc<Vec<u8>>,
    pub definitions: Vec<Definition>,
    pub entries: Vec<Entry>,
    pub information: Arc<String>,
    pub finished: bool,
    pub complete: bool,
    pub no_match: bool,
    pub suggesting: bool,
    pub notice: Option<String>,
    /// §3.1 banner capabilities, e.g. `auth`, `mime`.
    pub capabilities: Vec<String>,
    /// OPTION MIME was accepted for this session (§3.10.1).
    pub mime: bool,
    pub auth: Auth,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Greeting,
    Client,
    Option,
    Auth,
    Start,
    Definition,
    Between,
    Entries,
    Information,
    Completion,
    Done,
}

struct Parser {
    reply: Reply,
    state: State,
    operation: Operation,
    pending: Vec<u8>,
    expected: usize,
    received: usize,
    /// An AUTH command will follow the banner (§3.11).
    auth_user: Option<String>,
    /// The banner's msg-id, handed once to the exchange to compute AUTH.
    challenge: Option<String>,
    /// The banner arrived: commands deferred for AUTH may be sent.
    greeted: bool,
    /// MIME header lines of the current text response, until its blank line.
    header: Option<Vec<Vec<u8>>>,
    /// The parsed header of the current text response.
    entity: Option<mime::Header>,
    /// The current text response's body, when it cannot be shown as it
    /// arrives (another charset or a transfer encoding).
    encoded: Vec<u8>,
}

/// RFC 2229 §2.4.1 response parameters are atoms or double-quoted strings.
/// Backslash escapes the following character; a quote may be escaped data.
fn fields(mut input: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    while !input.is_empty() {
        if out.len() == 8 {
            return Err("Too many DICT response parameters.".into());
        }
        let mut value = String::new();
        if let Some(rest) = input.strip_prefix('"') {
            let mut chars = rest.char_indices();
            let mut end = None;
            while let Some((i, c)) = chars.next() {
                match c {
                    '"' => {
                        end = Some(i + 1);
                        break;
                    }
                    '\\' => value.push(chars.next().ok_or("Unfinished DICT escape.")?.1),
                    c if c.is_control() => {
                        return Err("Control character in DICT parameter.".into());
                    }
                    c => value.push(c),
                }
            }
            input = &rest[end.ok_or("Unclosed DICT response quote.")?..];
        } else {
            let end = input.find(' ').unwrap_or(input.len());
            value.push_str(&input[..end]);
            input = &input[end..];
            if !atom(&value) {
                return Err("Invalid DICT response parameter.".into());
            }
        }
        out.push(value);
        if input.is_empty() {
            break;
        }
        input = input
            .strip_prefix(' ')
            .ok_or("Missing DICT parameter separator.")?;
        // Some servers append a human-readable annotation after the defined
        // parameters. Callers pass only the parameters they need below.
        input = input.trim_start_matches(' ');
    }
    Ok(out)
}

fn header_fields(input: &str, count: usize) -> Result<Vec<String>, String> {
    // Stop after the protocol-defined fields, permitting the RFC's sample
    // `151 ... : definition text follows` annotation.
    let mut end = 0;
    let mut quoted = false;
    let mut escaped = false;
    let mut fields_seen = 1;
    for (i, c) in input.char_indices() {
        if escaped {
            escaped = false;
        } else if quoted && c == '\\' {
            escaped = true;
        } else if c == '"' {
            quoted = !quoted;
        } else if !quoted && c == ' ' {
            if fields_seen == count {
                let values = fields(&input[..i])?;
                return if values.len() == count {
                    Ok(values)
                } else {
                    Err("Missing DICT response parameters.".into())
                };
            }
            fields_seen += 1;
        }
        end = i + c.len_utf8();
    }
    let values = fields(&input[..end])?;
    if values.len() != count {
        return Err("Missing DICT response parameters.".into());
    }
    Ok(values)
}

impl Parser {
    fn new(operation: Operation) -> Self {
        Self {
            reply: Reply::default(),
            state: State::Greeting,
            operation,
            pending: Vec::new(),
            expected: 0,
            received: 0,
            auth_user: None,
            challenge: None,
            greeted: false,
            header: None,
            entity: None,
            encoded: Vec::new(),
        }
    }

    /// §3.10.1: a text response begins with its MIME header when OPTION MIME
    /// is on; without it, text is UTF-8 (§2.4.3).
    fn begin_text(&mut self) {
        self.header = self.reply.mime.then(Vec::new);
        self.entity = None;
        self.encoded.clear();
    }

    fn streams(&self) -> bool {
        self.entity
            .as_ref()
            .is_none_or(mime::Header::streams_as_utf8)
    }

    /// The decoded text (and attachments) of a text response that could not
    /// be streamed.
    fn decoded(&mut self) -> Option<mime::Decoded> {
        if self.streams() {
            return None;
        }
        let header = self
            .entity
            .clone()
            .unwrap_or_else(mime::Header::text_default);
        Some(mime::decode(&header, &std::mem::take(&mut self.encoded)))
    }

    fn finish_definition(&mut self) {
        let decoded = self.decoded();
        let content_type = self
            .entity
            .as_ref()
            .filter(|h| h.content_type != "text/plain")
            .map(|h| h.content_type.clone());
        let definition = self.reply.definitions.last_mut().unwrap();
        if let Some(decoded) = decoded {
            definition.body = Arc::new(decoded.text);
            definition.attachments = decoded.attachments;
            definition.skipped = decoded.skipped;
        }
        definition.content_type = content_type;
        definition.complete = true;
    }

    /// §3.1: `220 text capabilities msg-id`.
    fn banner(&mut self, text: &str) {
        let tokens: Vec<&str> = text.split_whitespace().collect();
        let bracketed = |t: &&str| t.starts_with('<') && t.ends_with('>') && t.len() > 2;
        let msg_id = tokens
            .last()
            .filter(|t| bracketed(t) && t.contains('@'))
            .map(|t| t.to_string());
        let capabilities_at = tokens
            .len()
            .saturating_sub(if msg_id.is_some() { 2 } else { 1 });
        if let Some(capabilities) = tokens
            .get(capabilities_at)
            .filter(|t| bracketed(t) && !t.contains('@'))
        {
            self.reply.capabilities = capabilities[1..capabilities.len() - 1]
                .split('.')
                .filter(|c| !c.is_empty())
                .take(32)
                .map(|c| c.to_ascii_lowercase())
                .collect();
        }
        self.greeted = true;
        if self.auth_user.is_some() {
            match msg_id {
                Some(id) => self.challenge = Some(id),
                None => {
                    self.auth_user = None;
                    self.reply.auth = Auth::Unavailable(
                        "the server's greeting carries no msg-id for AUTH".into(),
                    );
                }
            }
        }
    }

    fn stop(&mut self, notice: impl Into<String>) {
        // A body that could not stream is decoded as far as it arrived.
        if matches!(self.state, State::Definition | State::Information) && !self.streams() {
            let mut partial = std::mem::take(&mut self.encoded);
            partial.extend_from_slice(&self.pending);
            self.pending.clear();
            let header = self
                .entity
                .clone()
                .unwrap_or_else(mime::Header::text_default);
            let decoded = mime::decode(&header, &partial);
            if self.state == State::Definition {
                if let Some(definition) = self.reply.definitions.last_mut() {
                    definition.body = Arc::new(decoded.text);
                    definition.attachments = decoded.attachments;
                }
            } else {
                self.reply.information = Arc::new(decoded.text);
            }
        }
        // Keep an unfinished last text line, even if EOF split a UTF-8 codepoint.
        if !self.pending.is_empty() {
            let text = String::from_utf8_lossy(&self.pending).into_owned();
            if self.state == State::Definition {
                if let Some(definition) = self.reply.definitions.last_mut() {
                    Arc::make_mut(&mut definition.body).push_str(&text);
                }
            } else if self.state == State::Information {
                Arc::make_mut(&mut self.reply.information).push_str(&text);
            }
            self.pending.clear();
        }
        self.reply.finished = true;
        self.reply.complete = false;
        self.reply.notice = Some(notice.into());
        self.state = State::Done;
    }

    fn server_error(&mut self, code: u16, text: &str) {
        let label = match code {
            420 => "Dictionary server temporarily unavailable",
            421 => "Dictionary server shutting down",
            530 | 531 => "Dictionary access denied",
            550 => "Unknown dictionary; choose one from Dictionaries",
            551 => "Unknown search mode; choose one from Search modes",
            554 => "No dictionaries available",
            555 => "No search modes available",
            _ => "Dictionary request failed",
        };
        self.reply.notice = Some(format!("{label} ({code}): {text}"));
        self.reply.complete = true;
        self.state = State::Done;
    }

    fn line(&mut self, raw: &[u8]) -> Result<(), String> {
        if matches!(
            self.state,
            State::Definition | State::Entries | State::Information
        ) {
            if raw == b"." {
                self.header = None;
                self.state = match self.state {
                    State::Definition => {
                        self.finish_definition();
                        self.received += 1;
                        State::Between
                    }
                    State::Information => {
                        if let Some(decoded) = self.decoded() {
                            self.reply.information = Arc::new(decoded.text);
                        }
                        State::Completion
                    }
                    _ => State::Completion,
                };
                return Ok(());
            }
            // §2.4.3: a doubled leading period stands for one.
            let raw = if raw.starts_with(b"..") {
                &raw[1..]
            } else {
                raw
            };
            if let Some(header) = &mut self.header {
                if raw.is_empty() {
                    self.entity = Some(mime::parse_header(header));
                    self.header = None;
                } else if header.len() < 64 {
                    header.push(raw.to_vec());
                }
                return Ok(());
            }
            if matches!(self.state, State::Definition | State::Information) && !self.streams() {
                self.encoded.extend_from_slice(raw);
                self.encoded.extend_from_slice(b"\r\n");
                return Ok(());
            }
            let line = std::str::from_utf8(raw).map_err(|_| {
                "Incomplete reply: invalid UTF-8 from dictionary server.".to_string()
            })?;
            match self.state {
                State::Definition => {
                    let body = Arc::make_mut(&mut self.reply.definitions.last_mut().unwrap().body);
                    body.push_str(line);
                    body.push('\n');
                }
                State::Information => {
                    let body = Arc::make_mut(&mut self.reply.information);
                    body.push_str(line);
                    body.push('\n');
                }
                State::Entries => {
                    if self.reply.entries.len() == MAX_ITEMS {
                        return Err("Reply truncated at 4096 dictionary entries.".into());
                    }
                    let values = header_fields(line, 2)?;
                    if !atom(&values[0]) {
                        return Err("Invalid dictionary name in reply.".into());
                    }
                    self.reply.entries.push(Entry {
                        name: values[0].clone(),
                        description: values[1].clone(),
                    });
                    self.received += 1;
                }
                _ => unreachable!(),
            }
            return Ok(());
        }
        let line = std::str::from_utf8(raw)
            .map_err(|_| "Incomplete reply: invalid UTF-8 from dictionary server.".to_string())?;
        if line.len() < 3
            || !line.as_bytes()[..3].iter().all(u8::is_ascii_digit)
            || (line.len() > 3 && line.as_bytes()[3] != b' ')
        {
            return Err("Malformed DICT status line.".into());
        }
        // §2.4.1: the first digit is 1–5; nothing else is a reply code.
        if !(b'1'..=b'5').contains(&line.as_bytes()[0]) {
            return Err(format!(
                "Malformed DICT status line: {} is not a DICT reply code.",
                &line[..3]
            ));
        }
        let code = line[..3].parse::<u16>().unwrap();
        let text = line.get(4..).unwrap_or("");
        if self.state == State::Greeting {
            if code == 220 {
                self.banner(text);
                self.state = State::Client;
                return Ok(());
            }
            if code >= 400 {
                self.server_error(code, text);
                return Ok(());
            }
            return Err("Missing DICT greeting.".into());
        }
        if self.state == State::Client {
            // Identification is advisory; unsupported CLIENT must not discard
            // the already-pipelined lookup's independent response (RFC §4).
            if matches!(code, 250 | 500 | 501 | 502) {
                self.state = State::Option;
                return Ok(());
            }
        }
        if self.state == State::Option {
            // §3.10.1.2: older servers answer 500; then text has no header.
            if code == 250 || (500..=503).contains(&code) {
                self.reply.mime = code == 250;
                self.state = if self.auth_user.is_some() {
                    State::Auth
                } else {
                    State::Start
                };
                return Ok(());
            }
        }
        if self.state == State::Auth {
            // §3.11.2; the lookup that follows proceeds either way.
            let user = self.auth_user.take().unwrap_or_default();
            let outcome = match code {
                230 => Some(Auth::Accepted(user)),
                531 | 530 => Some(Auth::Denied {
                    user,
                    message: format!("{code} {text}"),
                }),
                500..=503 => Some(Auth::Unsupported(format!("{code} {text}"))),
                _ => None,
            };
            if let Some(outcome) = outcome {
                self.reply.auth = outcome;
                self.state = State::Start;
                return Ok(());
            }
        }
        if code == 552
            && self.state == State::Start
            && matches!(self.operation, Operation::Define | Operation::Match)
        {
            self.reply.no_match = true;
            self.reply.complete = true;
            self.state = State::Done;
            return Ok(());
        }
        if code >= 400 {
            self.server_error(code, text);
            return Ok(());
        }
        match (self.state, code) {
            (State::Start, 150) if self.operation == Operation::Define => {
                self.expected = count(text)?;
                self.state = State::Between;
            }
            (State::Between, 151) => {
                if self.reply.definitions.len() == MAX_DEFINITIONS {
                    return Err("Reply truncated at 256 definitions.".into());
                }
                let values = header_fields(text, 3)?;
                if !atom(&values[1]) {
                    return Err("Invalid dictionary name in definition.".into());
                }
                self.reply.definitions.push(Definition {
                    word: values[0].clone(),
                    database: values[1].clone(),
                    description: values[2].clone(),
                    body: Arc::new(String::new()),
                    complete: false,
                    content_type: None,
                    attachments: Vec::new(),
                    skipped: Vec::new(),
                });
                self.state = State::Definition;
                self.begin_text();
            }
            (State::Start, 152) if self.operation == Operation::Match => {
                self.expected = count(text)?;
                self.state = State::Entries;
                self.begin_text();
            }
            (State::Start, 110) if self.operation == Operation::Databases => {
                self.expected = count(text)?;
                self.state = State::Entries;
                self.begin_text();
            }
            (State::Start, 111) if self.operation == Operation::Strategies => {
                self.expected = count(text)?;
                self.state = State::Entries;
                self.begin_text();
            }
            (State::Start, 112) if self.operation == Operation::Info => {
                self.state = State::Information;
                self.begin_text();
            }
            (State::Between | State::Completion, 250) => {
                if self.operation != Operation::Info && self.received != self.expected {
                    return Err(format!(
                        "Incomplete reply: server announced {} results but sent {}.",
                        self.expected, self.received
                    ));
                }
                self.reply.complete = true;
                self.state = State::Done;
            }
            _ => return Err(format!("Unexpected DICT response ({code}).")),
        }
        Ok(())
    }

    fn feed(&mut self, bytes: &[u8]) {
        let room = MAX_RESPONSE.saturating_sub(self.reply.raw.len());
        Arc::make_mut(&mut self.reply.raw).extend_from_slice(&bytes[..room.min(bytes.len())]);
        for &byte in bytes.iter().take(room) {
            if self.state == State::Done {
                break;
            }
            self.pending.push(byte);
            if self.pending.len() > 6144 {
                self.stop("Incomplete reply: DICT line exceeds 6144 bytes.");
                break;
            }
            if byte == b'\n' {
                let line = std::mem::take(&mut self.pending);
                let result = if !line.ends_with(b"\r\n") {
                    Err("Incomplete reply: DICT requires CRLF line endings.".into())
                } else {
                    let content = &line[..line.len() - 2];
                    // §2.4.3: 1024 characters; a MIME charset other than
                    // UTF-8 counts one per octet.
                    let characters = std::str::from_utf8(content)
                        .map_or(content.len(), |text| text.chars().count());
                    if characters + 2 <= 1024 {
                        self.line(content)
                    } else {
                        Err("Incomplete reply: DICT line exceeds 1024 characters.".into())
                    }
                };
                if let Err(error) = result {
                    self.stop(error);
                    break;
                }
            }
        }
        if bytes.len() > room && self.state != State::Done {
            self.stop("Incomplete reply: truncated at 1 MiB.");
        }
    }
}

fn count(text: &str) -> Result<usize, String> {
    text.split(' ')
        .next()
        .filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| "Invalid DICT result count.".into())
}

pub async fn fetch(target: &Target) -> Result<Reply, String> {
    fetch_updates(target, |_| async { true }).await
}

pub async fn fetch_updates<F, Fut>(target: &Target, publish: F) -> Result<Reply, String>
where
    F: FnMut(Reply) -> Fut,
    Fut: Future<Output = bool>,
{
    exchange(target, publish, TIMEOUT).await
}

async fn exchange<F, Fut>(
    target: &Target,
    mut publish: F,
    budget: Duration,
) -> Result<Reply, String>
where
    F: FnMut(Reply) -> Fut,
    Fut: Future<Output = bool>,
{
    let command = target.command()?;
    let deadline = Instant::now() + budget;
    let mut parser = Parser::new(target.operation);
    let credentials = match super::credentials::for_target(target) {
        Ok(credentials) => credentials,
        Err(error) => {
            parser.reply.auth = Auth::Unavailable(error.clone());
            parser.stop(error);
            return Ok(parser.reply);
        }
    };
    parser.auth_user = credentials.as_ref().map(|c| c.user.clone());
    if !timeout_at(deadline, publish(parser.reply.clone()))
        .await
        .unwrap_or(false)
    {
        parser.stop("Incomplete reply: stopped.");
        return Ok(parser.reply);
    }
    let connected = timeout_at(
        deadline,
        TcpStream::connect((target.host.as_str(), target.port)),
    )
    .await;
    let mut stream = match connected {
        Ok(Ok(stream)) => stream,
        Ok(Err(error)) => {
            parser.stop(format!("Dictionary connection failed: {error}"));
            return Ok(parser.reply);
        }
        Err(_) => {
            parser.stop("Dictionary connection timed out.");
            return Ok(parser.reply);
        }
    };
    // RFC §4 permits sending CLIENT, OPTION MIME and the query alongside the
    // greeting. AUTH needs the greeting's msg-id, so with credentials the
    // query waits for it.
    let mut command_sent = credentials.is_none();
    let payload = if command_sent {
        format!("CLIENT TRust\r\nOPTION MIME\r\n{command}")
    } else {
        "CLIENT TRust\r\nOPTION MIME\r\n".to_string()
    };
    if !matches!(
        timeout_at(deadline, stream.write_all(payload.as_bytes())).await,
        Ok(Ok(()))
    ) {
        parser.stop("Dictionary request could not be sent.");
        return Ok(parser.reply);
    }
    let mut last_publish = Instant::now() - UPDATE_INTERVAL;
    let mut dirty = false;
    loop {
        if parser.state == State::Done {
            if parser.reply.no_match
                && !parser.reply.suggesting
                && parser.reply.notice.is_none()
                && target.operation == Operation::Define
            {
                let suggestion = match (Target {
                    operation: Operation::Match,
                    strategy: ".".into(),
                    number: None,
                    ..target.clone()
                })
                .command()
                {
                    Ok(command) => command,
                    Err(error) => {
                        parser.reply.notice =
                            Some(format!("Spelling suggestions unavailable: {error}"));
                        break;
                    }
                };
                parser.reply.suggesting = true;
                parser.reply.complete = false;
                parser.state = State::Start;
                parser.operation = Operation::Match;
                parser.received = 0;
                parser.expected = 0;
                if !matches!(
                    timeout_at(deadline, stream.write_all(suggestion.as_bytes())).await,
                    Ok(Ok(()))
                ) {
                    parser.stop("Spelling suggestions could not be requested.");
                    break;
                }
                dirty = true;
            } else {
                break;
            }
        }
        let mut bytes = [0u8; 8192];
        let wake = if dirty {
            (last_publish + UPDATE_INTERVAL).min(deadline)
        } else {
            deadline
        };
        match timeout_at(wake, stream.read(&mut bytes)).await {
            Ok(Ok(0)) => {
                parser.stop("Incomplete reply: server closed before completing the response.");
                break;
            }
            Ok(Ok(n)) => {
                parser.feed(&bytes[..n]);
                dirty = true;
                if !command_sent && parser.greeted {
                    let mut deferred = String::new();
                    if let (Some(challenge), Some(credentials)) =
                        (parser.challenge.take(), &credentials)
                    {
                        deferred.push_str(&credentials.command(&challenge));
                    }
                    deferred.push_str(&command);
                    command_sent = true;
                    if !matches!(
                        timeout_at(deadline, stream.write_all(deferred.as_bytes())).await,
                        Ok(Ok(()))
                    ) {
                        parser.stop("Dictionary request could not be sent.");
                        break;
                    }
                }
            }
            Ok(Err(error)) => {
                parser.stop(format!("Incomplete reply: {error}"));
                break;
            }
            Err(_) if Instant::now() >= deadline => {
                parser.stop("Incomplete reply: dictionary lookup timed out.");
                break;
            }
            Err(_) => {}
        }
        if dirty && last_publish.elapsed() >= UPDATE_INTERVAL && parser.state != State::Done {
            if !timeout_at(deadline, publish(parser.reply.clone()))
                .await
                .unwrap_or(false)
            {
                parser.stop("Incomplete reply: stopped.");
                break;
            }
            last_publish = Instant::now();
            dirty = false;
        }
    }
    parser.reply.finished = true;
    // Do not wait for EOF/221 to make completed definitions available. QUIT
    // still closes the session politely; a stalled peer gets no extra deadline.
    let _ = timeout_at(
        deadline.min(Instant::now() + Duration::from_millis(100)),
        stream.write_all(b"QUIT\r\n"),
    )
    .await;
    Ok(parser.reply)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn parsed(raw: &[u8], operation: Operation) -> Reply {
        let mut parser = Parser::new(operation);
        parser.feed(raw);
        if parser.state != State::Done {
            parser.stop("Incomplete reply");
        }
        parser.reply
    }
    #[test]
    fn dict_framing_quotes_dot_transparency_and_status_text_inside_definitions() {
        let raw = b"220 ready\r\n250 client\r\n500 unknown\r\n150 1 definition\r\n151 \"say \\\"hi\\\"\" db \"A \\\"quoted\\\" source\" : text follows\r\n..dot\r\n250 is definition text\r\n.\r\n250 ok\r\n";
        let reply = parsed(raw, Operation::Define);
        assert!(reply.complete, "{:?}", reply.notice);
        assert_eq!(reply.definitions[0].word, "say \"hi\"");
        assert_eq!(reply.definitions[0].description, "A \"quoted\" source");
        assert_eq!(
            &*reply.definitions[0].body,
            ".dot\n250 is definition text\n"
        );
        for split in 0..raw.len() {
            let mut parser = Parser::new(Operation::Define);
            parser.feed(&raw[..split]);
            parser.feed(&raw[split..]);
            assert_eq!(parser.reply.definitions, reply.definitions);
        }
    }
    #[test]
    fn dict_premature_eof_counts_and_bad_states_keep_partial_text() {
        let prefix =
            "220 ready\r\n250 client\r\n500 unknown\r\n150 2 definitions\r\n151 word db Source\r\n";
        let reply = parsed(format!("{prefix}partial").as_bytes(), Operation::Define);
        assert!(!reply.complete);
        assert_eq!(&*reply.definitions[0].body, "partial");
        let reply = parsed(
            format!("{prefix}one\r\n.\r\n250 ok\r\n").as_bytes(),
            Operation::Define,
        );
        assert!(!reply.complete);
        assert!(reply.definitions[0].complete);
        assert!(reply.notice.unwrap().contains("announced 2"));
        assert!(
            !parsed(
                b"220 ready\r\n250 client\r\n500 unknown\r\n250 ok\r\n",
                Operation::Define
            )
            .complete
        );
    }
    #[test]
    fn dict_match_and_catalog_entries_are_decoded() {
        for (operation, code) in [
            (Operation::Match, 152),
            (Operation::Databases, 110),
            (Operation::Strategies, 111),
        ] {
            let reply = parsed(format!("220 ready\r\n250 client\r\n500 unknown\r\n{code} 1 result\r\nwn \"ice cream\"\r\n.\r\n250 ok\r\n").as_bytes(), operation);
            assert!(reply.complete);
            assert_eq!(reply.entries[0].description, "ice cream");
        }
    }

    #[test]
    fn dict_bad_headers_and_limits_are_contained() {
        for header in [
            "151 word  db Source",
            "151 \"word db Source",
            "151 word db",
            "151 word \"bad db\" Source",
        ] {
            let reply = parsed(
                format!("220 ready\r\n250 client\r\n500 unknown\r\n150 1 result\r\n{header}\r\n")
                    .as_bytes(),
                Operation::Define,
            );
            assert!(reply.notice.is_some(), "{header}");
        }
        let prefix =
            "220 ready\r\n250 client\r\n500 unknown\r\n150 1 result\r\n151 word db Source\r\n";
        let reply = parsed(
            format!("{prefix}{}\r\n", "a".repeat(1023)).as_bytes(),
            Operation::Define,
        );
        assert!(reply.notice.unwrap().contains("1024"));
        let reply = parsed(
            format!("{prefix}{}", "a".repeat(6145)).as_bytes(),
            Operation::Define,
        );
        assert!(reply.notice.unwrap().contains("6144"));
        let reply = parsed(b"420 unavailable\r\n", Operation::Define);
        assert!(reply.notice.unwrap().contains("temporarily unavailable"));
        let reply = parsed(
            b"220 ready\r\n500 CLIENT unsupported\r\n500 OPTION unsupported\r\n552 no match\r\n",
            Operation::Define,
        );
        assert!(reply.complete && reply.no_match);
    }

    #[test]
    fn dict_aggregate_cap_keeps_received_data() {
        let mut parser = Parser::new(Operation::Define);
        parser.feed(
            b"220 ready\r\n250 client\r\n500 unknown\r\n150 1 result\r\n151 word db Source\r\n",
        );
        let chunk = b"some definition text\r\n".repeat(256);
        while parser.state != State::Done {
            parser.feed(&chunk);
        }
        assert_eq!(parser.reply.raw.len(), MAX_RESPONSE);
        assert!(parser.reply.notice.unwrap().contains("1 MiB"));
        assert!(!parser.reply.definitions[0].body.is_empty());
    }

    #[tokio::test]
    async fn dict_maximum_define_word_does_not_lose_reply_when_match_would_be_too_long() {
        use tokio::{
            io::{AsyncBufReadExt, BufReader},
            net::TcpListener,
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target = Target::parse(&format!(
            "dict://127.0.0.1:{}/d:{}",
            listener.local_addr().unwrap().port(),
            "x".repeat(1011)
        ))
        .unwrap();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut io = BufReader::new(socket);
            let mut command = String::new();
            io.read_line(&mut command).await.unwrap();
            command.clear();
            io.read_line(&mut command).await.unwrap();
            assert_eq!(command, "OPTION MIME\r\n");
            command.clear();
            io.read_line(&mut command).await.unwrap();
            assert_eq!(command.chars().count(), 1024);
            io.get_mut()
                .write_all(b"220 ready\r\n250 client\r\n500 unknown\r\n552 no match\r\n")
                .await
                .unwrap();
            command.clear();
            io.read_line(&mut command).await.unwrap();
            assert_eq!(command, "QUIT\r\n");
        });
        let reply = fetch(&target).await.unwrap();
        server.await.unwrap();
        assert!(reply.finished && reply.complete && reply.no_match);
        assert!(!reply.raw.is_empty());
        assert!(reply.notice.unwrap().contains("suggestions unavailable"));
    }

    #[tokio::test]
    async fn dict_streams_definitions_and_finishes_at_250_before_eof() {
        use tokio::{
            io::{AsyncBufReadExt, BufReader},
            net::TcpListener,
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target = Target::parse(&format!(
            "dict://127.0.0.1:{}/d:ice%20cream:wn",
            listener.local_addr().unwrap().port()
        ))
        .unwrap();
        let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
        let (close_tx, close_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut io = BufReader::new(socket);
            let mut command = String::new();
            io.read_line(&mut command).await.unwrap();
            assert_eq!(command, "CLIENT TRust\r\n");
            command.clear();
            io.read_line(&mut command).await.unwrap();
            assert_eq!(command, "OPTION MIME\r\n");
            command.clear();
            io.read_line(&mut command).await.unwrap();
            assert_eq!(command, "DEFINE wn \"ice cream\"\r\n");
            io.get_mut().write_all(b"220 ready\r\n250 client\r\n500 unknown\r\n150 2 results\r\n151 \"ice cream\" wn Source\r\nFirst definition\r\n.\r\n").await.unwrap();
            finish_rx.await.unwrap();
            io.get_mut()
                .write_all(
                    "151 crème wn Source\r\nSecond définition\r\n.\r\n250 done\r\n".as_bytes(),
                )
                .await
                .unwrap();
            command.clear();
            io.read_line(&mut command).await.unwrap();
            assert_eq!(command, "QUIT\r\n");
            close_rx.await.unwrap();
        });
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let fetch = tokio::spawn(async move {
            fetch_updates(&target, |reply| {
                let _ = tx.send(reply);
                async { true }
            })
            .await
            .unwrap()
        });
        let early = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let reply = rx.recv().await.unwrap();
                if reply.definitions.first().is_some_and(|d| d.complete) {
                    break reply;
                }
            }
        })
        .await
        .unwrap();
        assert!(!early.finished);
        assert_eq!(&*early.definitions[0].body, "First definition\n");
        finish_tx.send(()).unwrap();
        let reply = tokio::time::timeout(Duration::from_secs(1), fetch)
            .await
            .unwrap()
            .unwrap();
        assert!(reply.finished && reply.complete);
        assert_eq!(reply.definitions.len(), 2);
        assert_eq!(reply.definitions[1].word, "crème");
        close_tx.send(()).unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn dict_suggestions_use_the_same_server_database_and_literal_word() {
        use tokio::{
            io::{AsyncBufReadExt, BufReader},
            net::TcpListener,
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target = Target::parse(&format!(
            "dict://127.0.0.1:{}/d:serendipty:wn",
            listener.local_addr().unwrap().port()
        ))
        .unwrap();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut io = BufReader::new(socket);
            let mut command = String::new();
            io.read_line(&mut command).await.unwrap();
            command.clear();
            io.read_line(&mut command).await.unwrap();
            assert_eq!(command, "OPTION MIME\r\n");
            command.clear();
            io.read_line(&mut command).await.unwrap();
            io.get_mut()
                .write_all(b"220 ready\r\n250 client\r\n500 unknown\r\n552 no match\r\n")
                .await
                .unwrap();
            command.clear();
            io.read_line(&mut command).await.unwrap();
            assert_eq!(command, "MATCH wn . \"serendipty\"\r\n");
            io.get_mut()
                .write_all(b"152 1 match\r\nwn \"serendipity\"\r\n.\r\n250 ok\r\n")
                .await
                .unwrap();
            command.clear();
            io.read_line(&mut command).await.unwrap();
            assert_eq!(command, "QUIT\r\n");
        });
        let reply = fetch(&target).await.unwrap();
        server.await.unwrap();
        assert!(reply.finished && reply.complete && reply.suggesting && reply.no_match);
        assert_eq!(reply.entries[0].description, "serendipity");
    }

    #[test]
    fn dict_reply_codes_must_start_with_one_through_five() {
        for bad in ["050 hidden", "600 nope", "000 zero"] {
            let reply = parsed(
                format!("220 ready\r\n250 client\r\n500 unknown\r\n{bad}\r\n").as_bytes(),
                Operation::Define,
            );
            let notice = reply.notice.unwrap();
            assert!(notice.contains("not a DICT reply code"), "{bad}: {notice}");
            assert!(notice.contains(&bad[..3]), "{notice}");
        }
        let reply = parsed(b"050 not a banner\r\n", Operation::Define);
        assert!(reply.notice.unwrap().contains("050"));
    }

    #[test]
    fn dict_banner_capabilities_and_mime_headers_are_parsed() {
        let raw = "220 dict.test dictd 1.13 <auth.mime.X-extra> <1.2@dict.test>\r\n250 ok\r\n250 ok\r\n150 2 definitions\r\n151 \"caf\" fr \"French\"\r\nContent-Type: text/plain; charset=iso-8859-1\r\nContent-Transfer-Encoding: quoted-printable\r\n\r\ncaf=E9 cr=E8me\r\n.\r\n151 sun en \"English\"\r\n\r\n..A plain star\r\n.\r\n250 ok\r\n";
        let reply = parsed(raw.as_bytes(), Operation::Define);
        assert!(reply.complete, "{:?}", reply.notice);
        assert!(reply.mime);
        assert_eq!(reply.capabilities, ["auth", "mime", "x-extra"]);
        assert_eq!(&*reply.definitions[0].body, "café crème\n");
        assert_eq!(&*reply.definitions[1].body, ".A plain star\n");
        // A server without OPTION MIME (§3.10.1.2: 500) sends bare text.
        let raw = b"220 ready\r\n250 ok\r\n500 unknown command\r\n150 1\r\n151 w db d\r\nContent-Type: just text\r\n.\r\n250 ok\r\n";
        let reply = parsed(raw, Operation::Define);
        assert!(!reply.mime);
        assert_eq!(&*reply.definitions[0].body, "Content-Type: just text\n");
    }

    async fn auth_server(
        banner: &'static str,
        auth_reply: &'static str,
        body: &'static [u8],
    ) -> (Target, tokio::task::JoinHandle<Vec<String>>) {
        use tokio::{
            io::{AsyncBufReadExt, BufReader},
            net::TcpListener,
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let target = Target::parse(&format!("dict://127.0.0.1:{port}/d:glyph:pics")).unwrap();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut io = BufReader::new(socket);
            let mut commands = Vec::new();
            let mut line = String::new();
            io.read_line(&mut line).await.unwrap();
            commands.push(std::mem::take(&mut line));
            io.read_line(&mut line).await.unwrap();
            commands.push(std::mem::take(&mut line));
            // Nothing else may arrive before the banner when AUTH is due.
            io.get_mut()
                .write_all(format!("{banner}\r\n250 ok\r\n250 ok\r\n").as_bytes())
                .await
                .unwrap();
            loop {
                io.read_line(&mut line).await.unwrap();
                let command = std::mem::take(&mut line);
                let lookup = command.starts_with("DEFINE");
                commands.push(command.clone());
                if command.starts_with("AUTH") {
                    io.get_mut().write_all(auth_reply.as_bytes()).await.unwrap();
                }
                if lookup {
                    io.get_mut().write_all(body).await.unwrap();
                    break;
                }
            }
            io.read_line(&mut line).await.unwrap();
            commands.push(line);
            commands
        });
        (target, server)
    }

    #[tokio::test]
    async fn dict_auth_digest_follows_the_banner_and_mime_attachments_decode() {
        let body = b"150 1 definition\r\n151 glyph pics \"Pictures\"\r\nContent-Type: multipart/mixed; boundary=XYZ\r\n\r\n--XYZ\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nA symbol.\r\n--XYZ\r\nContent-Type: image/png\r\nContent-Transfer-Encoding: base64\r\nContent-Disposition: attachment; filename=glyph.png\r\n\r\niVBORw0K\r\n--XYZ--\r\n.\r\n250 ok\r\n";
        let (target, server) = auth_server(
            "220 dict.test dictd <auth.mime> <1896.697170952@dbc.mtview.ca.us>",
            "230 Authentication successful\r\n",
            body,
        )
        .await;
        super::super::credentials::save(&target.server(), "mrose", "tanstaaf").unwrap();
        let reply = fetch(&target).await.unwrap();
        let commands = server.await.unwrap();
        super::super::credentials::forget(&target.server()).unwrap();
        assert_eq!(commands[0], "CLIENT TRust\r\n");
        assert_eq!(commands[1], "OPTION MIME\r\n");
        // RFC 1939 §7's APOP example: the digest RFC 2229 §3.11 reuses.
        assert_eq!(
            commands[2],
            "AUTH mrose c4c9334bac560ecc979e58001b3e22fb\r\n"
        );
        assert_eq!(commands[3], "DEFINE pics \"glyph\"\r\n");
        assert_eq!(commands[4], "QUIT\r\n");
        assert_eq!(reply.auth, Auth::Accepted("mrose".into()));
        assert!(reply.complete, "{:?}", reply.notice);
        let definition = &reply.definitions[0];
        assert_eq!(&*definition.body, "A symbol.");
        assert_eq!(definition.content_type.as_deref(), Some("multipart/mixed"));
        assert_eq!(
            definition.attachments[0].filename.as_deref(),
            Some("glyph.png")
        );
        assert_eq!(*definition.attachments[0].bytes, b"\x89PNG\r\n");
        // The secret never reaches the transcript or a debug dump.
        assert!(!format!("{reply:?}").contains("tanstaaf"));
    }

    #[tokio::test]
    async fn dict_auth_denial_and_missing_msg_id_still_complete_the_lookup() {
        let body = b"552 no match\r\n";
        let (target, server) = auth_server(
            "220 dict.test <auth> <1.2@dict.test>",
            "531 Access denied, use \"SHOW INFO\" for server information\r\n",
            body,
        )
        .await;
        super::super::credentials::save(&target.server(), "joe", "wrong secret").unwrap();
        let reply = exchange(&target, |_| async { true }, Duration::from_secs(3))
            .await
            .unwrap();
        server.abort();
        super::super::credentials::forget(&target.server()).unwrap();
        assert!(matches!(&reply.auth, Auth::Denied { user, .. } if user == "joe"));
        assert!(reply.no_match);
        // No msg-id: AUTH cannot be computed, so the lookup goes alone.
        let (target, server) = auth_server("220 dict.test plain banner", "", body).await;
        super::super::credentials::save(&target.server(), "joe", "secret").unwrap();
        let reply = exchange(&target, |_| async { true }, Duration::from_secs(3))
            .await
            .unwrap();
        let commands = {
            server.abort();
            Vec::<String>::new()
        };
        super::super::credentials::forget(&target.server()).unwrap();
        assert!(commands.is_empty());
        assert!(matches!(reply.auth, Auth::Unavailable(_)));
        assert!(reply.no_match);
        // A `user;AUTH@` address without its secret asks first.
        let named = Target::parse("dict://ann;AUTH@127.0.0.1:9/d:x").unwrap();
        assert!(super::super::credentials::needs_secret(&named));
        let reply = fetch(&named).await.unwrap();
        assert!(matches!(reply.auth, Auth::Unavailable(_)));
    }

    #[tokio::test]
    async fn dict_timeout_retains_received_bytes_and_partial_definition() {
        use tokio::{
            io::{AsyncBufReadExt, BufReader},
            net::TcpListener,
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target = Target::parse(&format!(
            "dict://127.0.0.1:{}/d:word",
            listener.local_addr().unwrap().port()
        ))
        .unwrap();
        let body = b"220 ready\r\n250 client\r\n500 unknown\r\n150 1 result\r\n151 word db Source\r\npartial without CRLF";
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut io = BufReader::new(socket);
            let mut command = String::new();
            io.read_line(&mut command).await.unwrap();
            command.clear();
            io.read_line(&mut command).await.unwrap();
            assert_eq!(command, "OPTION MIME\r\n");
            command.clear();
            io.read_line(&mut command).await.unwrap();
            io.get_mut().write_all(body).await.unwrap();
            tokio::time::sleep(Duration::from_secs(2)).await;
        });
        let reply = exchange(&target, |_| async { true }, Duration::from_millis(200))
            .await
            .unwrap();
        server.abort();
        assert!(!reply.complete);
        assert!(reply.notice.unwrap().contains("timed out"));
        assert_eq!(&**reply.raw, body);
        assert_eq!(&*reply.definitions[0].body, "partial without CRLF");
    }
}
