//! DICT requests, discovery and readable dictionary pages.
//!
//! RFC 2229 §§2–5 (RFC Editor snapshot 2026-09-06): commands and responses
//! have distinct framing, DEFINE/MATCH preserve database order, and URL
//! selectors are client-side. RFC 3986 §§2.4, 3.1, 3.2.2, 3.5 governs URL
//! components: split before decoding once; never send fragments.

use crate::doc::Link;
use std::fmt;

mod view;
mod wire;
pub use view::hanging_indent;
pub use view::{Action, Page, SavedView, Section, render};
pub use wire::{Definition, Entry, Reply, fetch, fetch_updates};

pub const DEFAULT_SERVER: &str = "dict.org";
pub const USAGE: &str =
    "dict [--database name] [--match strategy] <word or \"phrase\"> [server[:port]]";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    Define,
    Match,
    Databases,
    Strategies,
    Info,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub host: String,
    pub port: u16,
    pub word: String,
    pub database: String,
    pub strategy: String,
    pub number: Option<usize>,
    pub operation: Operation,
}

impl Target {
    pub fn parse(input: &str) -> Result<Self, String> {
        if input.len() > 24576 || input.chars().any(char::is_control) {
            return Err("Invalid DICT address: control character or excessive length.".into());
        }
        let url = url::Url::parse(input).map_err(|_| "Invalid DICT address.")?;
        if url.scheme() == "about" && url.path() == "dict" {
            let values: Vec<_> = url.query_pairs().collect();
            if values.len() != 2 {
                return Err("Invalid dictionary catalog address.".into());
            }
            let base = values
                .iter()
                .find(|(k, _)| k == "url")
                .ok_or("Missing dictionary address.")?;
            if !base
                .1
                .get(..7)
                .is_some_and(|s| s.eq_ignore_ascii_case("dict://"))
            {
                return Err("Invalid dictionary catalog address.".into());
            }
            let mut target = Self::parse(&base.1)?;
            target.operation = match values
                .iter()
                .find(|(k, _)| k == "show")
                .map(|(_, v)| v.as_ref())
            {
                Some("databases") => Operation::Databases,
                Some("strategies") => Operation::Strategies,
                Some("info") => Operation::Info,
                _ => return Err("Unknown dictionary catalog.".into()),
            };
            target.number = None;
            target.command()?;
            return Ok(target);
        }
        if url.scheme() != "dict" || url.query().is_some() {
            return Err("Invalid DICT address.".into());
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err("Authenticated DICT addresses are not supported.".into());
        }
        let host = match url.host().ok_or("Missing dictionary server.")? {
            url::Host::Ipv6(ip) => ip.to_string(),
            url::Host::Ipv4(ip) => ip.to_string(),
            url::Host::Domain(name) => url::Host::parse(name)
                .map_err(|_| "Invalid dictionary server.")?
                .to_string(),
        };
        if host.is_empty() {
            return Err("Missing dictionary server.".into());
        }
        // Preserve query words such as '.' and '..'; URL path normalization
        // must not interpret them as filesystem navigation.
        let path = input
            .split('#')
            .next()
            .unwrap()
            .split_once("://")
            .ok_or("Invalid DICT address.")?
            .1
            .split_once('/')
            .map_or("", |(_, p)| p);
        let (operation, path) = if let Some(p) = path.strip_prefix("d:") {
            (Operation::Define, p)
        } else if let Some(p) = path.strip_prefix("m:") {
            (Operation::Match, p)
        } else if path.contains(':') {
            return Err("DICT addresses use d: for definitions or m: for matches.".into());
        } else {
            (Operation::Define, path)
        };
        let parts: Vec<_> = path.split(':').collect();
        let limit = if operation == Operation::Match { 4 } else { 3 };
        if parts.len() > limit {
            return Err("Too many DICT URL fields.".into());
        }
        let field = |n| decode(parts.get(n).copied().unwrap_or(""));
        let word = field(0)?;
        let database = field(1)?;
        let strategy = if operation == Operation::Match {
            field(2)?
        } else {
            String::new()
        };
        let selector = field(limit - 1)?;
        let number = if selector.is_empty() {
            None
        } else {
            if !selector.bytes().all(|b| b.is_ascii_digit()) {
                return Err("DICT result numbers must be decimal digits.".into());
            }
            Some(
                selector
                    .parse::<usize>()
                    .ok()
                    .filter(|n| *n > 0)
                    .ok_or("DICT result numbers start at 1.")?,
            )
        };
        let target = Self {
            host,
            port: url.port().unwrap_or(2628),
            word,
            database: if database.is_empty() {
                "!".into()
            } else {
                database
            },
            strategy: if strategy.is_empty() {
                ".".into()
            } else {
                strategy
            },
            number,
            operation,
        };
        target.command()?;
        Ok(target)
    }

    pub fn server(&self) -> String {
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        if self.port == 2628 {
            host
        } else {
            format!("{host}:{}", self.port)
        }
    }

    pub fn lookup(&self, word: &str, database: &str) -> Self {
        Self {
            word: word.into(),
            database: database.into(),
            number: None,
            operation: Operation::Define,
            ..self.clone()
        }
    }

    pub fn catalog(&self, operation: Operation) -> Self {
        Self {
            operation,
            number: None,
            ..self.clone()
        }
    }

    pub(crate) fn command(&self) -> Result<String, String> {
        if self.number == Some(0) {
            return Err("DICT result numbers start at 1.".into());
        }
        if self.host.is_empty() || self.host.chars().any(char::is_control) || self.port == 0 {
            return Err("Invalid dictionary server or port.".into());
        }
        if !atom(&self.database) || !atom(&self.strategy) || self.word.chars().any(char::is_control)
        {
            return Err("Invalid DICT word, database or strategy.".into());
        }
        let command = match self.operation {
            Operation::Define => format!("DEFINE {} {}\r\n", self.database, quote(&self.word)),
            Operation::Match => format!(
                "MATCH {} {} {}\r\n",
                self.database,
                self.strategy,
                quote(&self.word)
            ),
            Operation::Databases => "SHOW DB\r\n".into(),
            Operation::Strategies => "SHOW STRAT\r\n".into(),
            Operation::Info => format!("SHOW INFO {}\r\n", self.database),
        };
        if command.chars().count() > 1024 || command.len() > 6144 {
            return Err(
                "DICT commands are limited to 1024 characters, including quoting and CRLF.".into(),
            );
        }
        Ok(command)
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if !matches!(self.operation, Operation::Define | Operation::Match) {
            let base = Self {
                operation: Operation::Define,
                ..self.clone()
            };
            let mut url = url::Url::parse("about:dict").expect("internal URL");
            url.query_pairs_mut()
                .append_pair("url", &base.to_string())
                .append_pair(
                    "show",
                    match self.operation {
                        Operation::Databases => "databases",
                        Operation::Strategies => "strategies",
                        _ => "info",
                    },
                );
            return url.fmt(f);
        }
        write!(
            f,
            "dict://{}/{}:{}:{}",
            self.server(),
            if self.operation == Operation::Match {
                "m"
            } else {
                "d"
            },
            encode(&self.word),
            encode(&self.database)
        )?;
        if self.operation == Operation::Match {
            write!(f, ":{}", encode(&self.strategy))?;
        }
        if let Some(n) = self.number {
            write!(f, ":{n}")?;
        }
        Ok(())
    }
}

pub fn is_address(input: &str) -> bool {
    input
        .split_once(':')
        .is_some_and(|(scheme, _)| scheme.eq_ignore_ascii_case("dict"))
        || input.starts_with("about:dict?")
}

pub(crate) fn atom(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|c| c != ' ' && !c.is_ascii_control() && !matches!(c, '\'' | '"' | '\\'))
}
pub(crate) fn quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}
pub(crate) fn encode(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~!*".contains(&byte) {
            out.push(byte as char);
        } else {
            use std::fmt::Write;
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}
pub(crate) fn decode(value: &str) -> Result<String, String> {
    let mut out = Vec::new();
    let mut bytes = value.bytes();
    while let Some(b) = bytes.next() {
        out.push(if b == b'%' {
            let a = bytes.next().and_then(|c| (c as char).to_digit(16));
            let b = bytes.next().and_then(|c| (c as char).to_digit(16));
            match (a, b) {
                (Some(a), Some(b)) => (a * 16 + b) as u8,
                _ => return Err("Invalid percent escape in DICT address.".into()),
            }
        } else {
            b
        });
    }
    String::from_utf8(out).map_err(|_| "DICT addresses must encode UTF-8.".into())
}

/// Both command consoles use literal quote handling and exactly the same defaults.
pub fn command_target(arguments: &str) -> Result<Target, String> {
    let args = crate::command::word_arguments(arguments)?;
    let mut database = None;
    let mut strategy = None;
    let mut server = None;
    let mut operation = Operation::Define;
    let mut positional = Vec::new();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--database" | "-d" => database = Some(args.next().ok_or(USAGE)?.clone()),
            "--server" | "-h" => server = Some(args.next().ok_or(USAGE)?.clone()),
            "--match" | "-m" => {
                strategy = Some(args.next().ok_or(USAGE)?.clone());
                operation = Operation::Match;
            }
            "--databases" => operation = Operation::Databases,
            "--strategies" => operation = Operation::Strategies,
            "--" => {
                positional.extend(args.cloned());
                break;
            }
            _ if arg.starts_with('-') => return Err(USAGE.into()),
            _ => positional.push(arg.clone()),
        }
    }
    if positional.len() > 2 || (server.is_some() && positional.len() > 1) {
        return Err(USAGE.into());
    }
    if positional.is_empty() && matches!(operation, Operation::Define | Operation::Match) {
        return Err(USAGE.into());
    }
    let server = server
        .or_else(|| positional.get(1).cloned())
        .unwrap_or_else(|| DEFAULT_SERVER.into());
    if server.contains(['/', '?', '#', '@']) {
        return Err("Use a dictionary server name or [IPv6]:port.".into());
    }
    let mut target = Target::parse(&format!("dict://{server}/d:"))?;
    target.word = positional.first().cloned().unwrap_or_default();
    target.database = database.unwrap_or_else(|| preferred(&target).unwrap_or_else(|| "*".into()));
    target.strategy = strategy.unwrap_or_else(|| ".".into());
    target.operation = operation;
    target.command()?;
    Ok(target)
}

fn preferences_path() -> Option<std::path::PathBuf> {
    crate::storage::config_directory()
        .ok()
        .map(|p| p.join("dict.json"))
}
fn preferences() -> serde_json::Map<String, serde_json::Value> {
    let data = preferences_path().and_then(|p| {
        (std::fs::metadata(&p).ok()?.len() <= 8192)
            .then(|| std::fs::read(p).ok())
            .flatten()
    });
    data.and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}
pub fn preferred(target: &Target) -> Option<String> {
    preferences()
        .get(&target.server())
        .and_then(|v| v.as_str())
        .filter(|s| s.len() <= 256 && atom(s))
        .map(str::to_owned)
}
pub(crate) fn remember(target: &Target, database: &str) -> Result<(), String> {
    if !atom(database) || database.len() > 256 {
        return Err("Invalid preferred dictionary.".into());
    }
    let path = preferences_path().ok_or("No configuration directory is available.")?;
    let mut prefs = preferences();
    if prefs.len() >= 16
        && !prefs.contains_key(&target.server())
        && let Some(key) = prefs.keys().next().cloned()
    {
        prefs.remove(&key);
    }
    prefs.insert(target.server(), database.into());
    std::fs::create_dir_all(path.parent().unwrap()).map_err(|e| e.to_string())?;
    let temp = path.with_extension(format!("{}.tmp", std::process::id()));
    std::fs::write(&temp, serde_json::to_vec(&prefs).unwrap())
        .and_then(|_| std::fs::rename(&temp, &path))
        .map_err(|e| e.to_string())
}

pub fn command_seed(target: &Target) -> String {
    format!(
        "dict --server {} --database {} ",
        quote(&target.server()),
        quote(&target.database)
    )
}

pub fn link(target: Target) -> Link {
    Link::Dict(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dict_url_fields_roundtrip_and_decode_exactly_once() {
        for (input, operation, word, database, strategy, number) in [
            (
                "DICT://dict.org/d:ice%20cream:wn:2#local",
                Operation::Define,
                "ice cream",
                "wn",
                ".",
                Some(2),
            ),
            (
                "dict://dict.org/m:serendipty::.",
                Operation::Match,
                "serendipty",
                "!",
                ".",
                None,
            ),
            (
                "dict://dict.org/m:caf%C3%A9:db:prefix:1",
                Operation::Match,
                "café",
                "db",
                "prefix",
                Some(1),
            ),
            (
                "dict://dict.org/d:C%23%3A%2520",
                Operation::Define,
                "C#:%20",
                "!",
                ".",
                None,
            ),
            (
                "dict://[::1]:42628/d:..",
                Operation::Define,
                "..",
                "!",
                ".",
                None,
            ),
            (
                "dict://dict.org/word",
                Operation::Define,
                "word",
                "!",
                ".",
                None,
            ),
        ] {
            let target = Target::parse(input).unwrap();
            assert_eq!(
                (
                    target.operation,
                    target.word.as_str(),
                    target.database.as_str(),
                    target.strategy.as_str(),
                    target.number
                ),
                (operation, word, database, strategy, number)
            );
            assert_eq!(Target::parse(&target.to_string()).unwrap(), target);
        }
        assert_eq!(Target::parse("dict://[::1]/d:x").unwrap().host, "::1");
        let target = Target::parse("dict://bücher.example/d:x").unwrap();
        assert_eq!(target.host, "xn--bcher-kva.example");
        for operation in [Operation::Databases, Operation::Strategies, Operation::Info] {
            let target = Target::parse("dict://dict.org/d:ice%20cream:wn")
                .unwrap()
                .catalog(operation);
            assert_eq!(Target::parse(&target.to_string()).unwrap(), target);
        }
    }

    #[test]
    fn dict_invalid_urls_and_commands_never_become_wire_commands() {
        for input in [
            "dict://dict.org/d:%0D%0AQUIT",
            "dict://dict.org/d:%00",
            "dict://dict.org/d:%ZZ",
            "dict://dict.org/d:%FF",
            "dict://dict.org/d:word:bad%20db",
            "dict://dict.org/d:x:wn:0",
            "dict://dict.org/d:x:wn:1:ignored",
            "dict://dict.org/m:x:wn:prefix:-1",
            "dict://dict.org/d:x?query",
            "dict://user;AUTH@dict.org/d:x",
            "dict://dict.org:0/d:x",
        ] {
            assert!(Target::parse(input).is_err(), "{input}");
        }
        let mut target = Target::parse("dict://dict.org/d:x").unwrap();
        target.word = "x\r\nQUIT".into();
        assert!(target.command().is_err());
        target.word = "x".repeat(1011);
        assert_eq!(target.command().unwrap().chars().count(), 1024);
        target.word.push('x');
        assert!(target.command().is_err());
        target.word = "é".repeat(1011);
        assert!(target.command().is_ok());
        target.word = "\\".repeat(1011);
        assert!(target.command().is_err(), "count after escaping");
    }

    #[test]
    fn dict_commands_preserve_phrases_apostrophes_and_explicit_source() {
        for (arguments, expected) in [
            (
                "--database wn \"ice cream\" dict.org",
                "DEFINE wn \"ice cream\"\r\n",
            ),
            ("--database wn can't", "DEFINE wn \"can't\"\r\n"),
            (
                r#"--database wn "say \"hello\"""#,
                "DEFINE wn \"say \\\"hello\\\"\"\r\n",
            ),
            (r"--database wn a\b", "DEFINE wn \"a\\\\b\"\r\n"),
            (
                "--database wn --match prefix neo",
                "MATCH wn prefix \"neo\"\r\n",
            ),
        ] {
            assert_eq!(
                command_target(arguments).unwrap().command().unwrap(),
                expected
            );
        }
        assert!(command_target("\"unclosed").is_err());
        assert!(command_target("--database wn word server ignored").is_err());
    }
}
