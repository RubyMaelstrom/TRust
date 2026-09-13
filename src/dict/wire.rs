//! RFC 2229 §§2.2–2.4, 3.1–3.6, 4: incremental CRLF framing and per-command
//! response states. A successful DEFINE ends at 250, not at TCP EOF.
use super::{Operation, Target, atom};
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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Greeting,
    Client,
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
        }
    }

    fn stop(&mut self, notice: impl Into<String>) {
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

    fn line(&mut self, line: &str) -> Result<(), String> {
        if matches!(
            self.state,
            State::Definition | State::Entries | State::Information
        ) {
            if line == "." {
                self.state = match self.state {
                    State::Definition => {
                        self.reply.definitions.last_mut().unwrap().complete = true;
                        self.received += 1;
                        State::Between
                    }
                    _ => State::Completion,
                };
                return Ok(());
            }
            let line = if line.starts_with("..") {
                &line[1..]
            } else {
                line
            };
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
        if line.len() < 3
            || !line.as_bytes()[..3].iter().all(u8::is_ascii_digit)
            || (line.len() > 3 && line.as_bytes()[3] != b' ')
        {
            return Err("Malformed DICT status line.".into());
        }
        let code = line[..3].parse::<u16>().unwrap();
        let text = line.get(4..).unwrap_or("");
        if self.state == State::Greeting {
            if code == 220 {
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
            if matches!(code, 250 | 500 | 502) {
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
                });
                self.state = State::Definition;
            }
            (State::Start, 152) if self.operation == Operation::Match => {
                self.expected = count(text)?;
                self.state = State::Entries;
            }
            (State::Start, 110) if self.operation == Operation::Databases => {
                self.expected = count(text)?;
                self.state = State::Entries;
            }
            (State::Start, 111) if self.operation == Operation::Strategies => {
                self.expected = count(text)?;
                self.state = State::Entries;
            }
            (State::Start, 112) if self.operation == Operation::Info => {
                self.state = State::Information
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
                    match std::str::from_utf8(&line[..line.len() - 2]) {
                        Ok(text) if text.chars().count() + 2 <= 1024 => self.line(text),
                        Ok(_) => Err("Incomplete reply: DICT line exceeds 1024 characters.".into()),
                        Err(_) => {
                            Err("Incomplete reply: invalid UTF-8 from dictionary server.".into())
                        }
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
    // RFC §4 permits sending CLIENT and the query alongside the greeting.
    let payload = format!("CLIENT TRust\r\n{command}");
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
        let raw = b"220 ready\r\n250 client\r\n150 1 definition\r\n151 \"say \\\"hi\\\"\" db \"A \\\"quoted\\\" source\" : text follows\r\n..dot\r\n250 is definition text\r\n.\r\n250 ok\r\n";
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
        let prefix = "220 ready\r\n250 client\r\n150 2 definitions\r\n151 word db Source\r\n";
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
        assert!(!parsed(b"220 ready\r\n250 client\r\n250 ok\r\n", Operation::Define).complete);
    }
    #[test]
    fn dict_match_and_catalog_entries_are_decoded() {
        for (operation, code) in [
            (Operation::Match, 152),
            (Operation::Databases, 110),
            (Operation::Strategies, 111),
        ] {
            let reply = parsed(format!("220 ready\r\n250 client\r\n{code} 1 result\r\nwn \"ice cream\"\r\n.\r\n250 ok\r\n").as_bytes(), operation);
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
                format!("220 ready\r\n250 client\r\n150 1 result\r\n{header}\r\n").as_bytes(),
                Operation::Define,
            );
            assert!(reply.notice.is_some(), "{header}");
        }
        let prefix = "220 ready\r\n250 client\r\n150 1 result\r\n151 word db Source\r\n";
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
            b"220 ready\r\n500 CLIENT unsupported\r\n552 no match\r\n",
            Operation::Define,
        );
        assert!(reply.complete && reply.no_match);
    }

    #[test]
    fn dict_aggregate_cap_keeps_received_data() {
        let mut parser = Parser::new(Operation::Define);
        parser.feed(b"220 ready\r\n250 client\r\n150 1 result\r\n151 word db Source\r\n");
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
            assert_eq!(command.chars().count(), 1024);
            io.get_mut()
                .write_all(b"220 ready\r\n250 client\r\n552 no match\r\n")
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
            assert_eq!(command, "DEFINE wn \"ice cream\"\r\n");
            io.get_mut().write_all(b"220 ready\r\n250 client\r\n150 2 results\r\n151 \"ice cream\" wn Source\r\nFirst definition\r\n.\r\n").await.unwrap();
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
            io.get_mut()
                .write_all(b"220 ready\r\n250 client\r\n552 no match\r\n")
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
        let body = b"220 ready\r\n250 client\r\n150 1 result\r\n151 word db Source\r\npartial without CRLF";
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut io = BufReader::new(socket);
            let mut command = String::new();
            io.read_line(&mut command).await.unwrap();
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
