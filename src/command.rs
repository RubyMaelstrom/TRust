//! Frontend-neutral pieces of TRust's `trust>` command surface.
//!
//! Command execution remains with each frontend because terminal sessions and
//! graphical documents own different resources. History behavior, address
//! recognition, service-name parsing, and the help source are shared so the
//! graphical COMMAND panel is a presentation of the established interface,
//! not a second command language.

pub(crate) const HISTORY_CAP: usize = 500;

const SEARCH_ENDPOINT: &str = "https://lite.duckduckgo.com/lite";

/// In-memory entry history for a TRust input surface. Never persisted.
#[derive(Default)]
pub struct History {
    pub(crate) entries: Vec<String>,
    /// Index into `entries` while browsing; `None` while editing a fresh line.
    nav: Option<usize>,
    /// The unfinished line stashed when history browsing starts.
    draft: String,
}

impl History {
    pub fn push(&mut self, line: &str) {
        self.nav = None;
        if line.is_empty() || self.entries.last().is_some_and(|last| last == line) {
            return;
        }
        if self.entries.len() == HISTORY_CAP {
            self.entries.remove(0);
        }
        self.entries.push(line.to_string());
    }

    /// Step to an older entry, stashing the in-progress line first.
    pub fn up(&mut self, current: &str) -> Option<String> {
        let index = match self.nav {
            None if !self.entries.is_empty() => {
                self.draft = current.to_string();
                self.entries.len() - 1
            }
            Some(index) if index > 0 => index - 1,
            _ => return None,
        };
        self.nav = Some(index);
        Some(self.entries[index].clone())
    }

    /// Step to a newer entry, or restore the stashed draft past the end.
    pub fn down(&mut self) -> Option<String> {
        match self.nav {
            Some(index) if index + 1 < self.entries.len() => {
                self.nav = Some(index + 1);
                Some(self.entries[index + 1].clone())
            }
            Some(_) => {
                self.nav = None;
                Some(std::mem::take(&mut self.draft))
            }
            None => None,
        }
    }

    /// Editing a recalled entry detaches it from the browse position.
    pub fn detach(&mut self) {
        self.nav = None;
    }
}

/// Resolve a port argument: a number, or a compact set of well-known service
/// names, matching TRust's established GNU-telnet-style command behavior.
pub fn parse_port(value: &str) -> Option<u16> {
    if let Ok(port) = value.parse() {
        return Some(port);
    }
    Some(match value {
        "echo" => 7,
        "daytime" => 13,
        "chargen" => 19,
        "ftp" => 21,
        "telnet" => 23,
        "smtp" | "mail" => 25,
        "whois" | "nicname" => 43,
        "domain" => 53,
        "gopher" => 70,
        "finger" => 79,
        "http" | "www" => 80,
        "pop3" => 110,
        "nntp" => 119,
        "imap" => 143,
        "https" => 443,
        "telnets" => 992,
        "gemini" => 1965,
        "dict" => 2628,
        "irc" => 6667,
        _ => return None,
    })
}

/// Whether a bare command token looks like a web host/address.
pub fn looks_like_host(value: &str) -> bool {
    let host = value.split(':').next().unwrap_or(value);
    host == "localhost" || (host.contains('.') && !host.starts_with('.') && !host.ends_with('.'))
}

/// Whether a token starts with a syntactically valid URL scheme.
///
/// This mirrors the WHATWG URL Standard's scheme start/scheme states: an
/// ASCII letter, followed by ASCII alphanumerics or `+`, `-`, `.`, then `:`.
fn has_url_scheme(value: &str) -> bool {
    let Some((scheme, _)) = value.split_once(':') else {
        return false;
    };
    let mut chars = scheme.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

/// Whether a bare COMMAND token should be handled as an address instead of a
/// search query.
pub fn looks_like_address(value: &str) -> bool {
    has_url_scheme(value) || looks_like_host(value)
}

/// Build the DuckDuckGo Lite URL used when a COMMAND line is neither a
/// recognized command nor an address.
///
/// `Url::query_pairs_mut` implements the WHATWG URL Standard's
/// `application/x-www-form-urlencoded` serializer (URL Standard §5.2), so
/// spaces become `+` and reserved/non-ASCII input is UTF-8 percent-encoded.
pub fn search_url(query: &str) -> String {
    let mut url = url::Url::parse(SEARCH_ENDPOINT).expect("static search endpoint is a valid URL");
    url.query_pairs_mut()
        .append_pair("q", query)
        // DuckDuckGo documents `kd=-1` as its Redirect: Off setting. Asking
        // the provider for direct result URLs avoids an unnecessary tracking-
        // protection hop while retaining normal URL navigation semantics.
        // https://duckduckgo.com/duckduckgo-help-pages/settings/params
        .append_pair("kd", "-1");
    url.into()
}

/// Split a trailing numeric `:port` from a non-IPv6 host.
pub fn split_host_port(value: &str) -> (&str, Option<u16>) {
    if let Some((host, port)) = value.rsplit_once(':')
        && !host.is_empty()
        && !host.contains(':')
        && let Ok(port) = port.parse::<u16>()
    {
        return (host, Some(port));
    }
    (value, None)
}

/// Small literal command tokenizer shared by both frontends. Quotes group
/// arguments; backslash quotes a quote, backslash, or whitespace. There is no
/// interpolation or shell execution. Unclosed quotes are reported, not sent.
pub fn quoted_arguments(input: &str) -> Result<Vec<String>, String> {
    if input.len() > 32768 || input.contains(['\r', '\n', '\0']) {
        return Err("Command is too long or contains a line break/control character.".into());
    }
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut started = false;
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\'
            && chars
                .peek()
                .is_some_and(|next| matches!(next, '\\' | '\'' | '"') || next.is_whitespace())
        {
            word.push(chars.next().expect("escaped character"));
            started = true;
        } else if quote == Some(ch) {
            quote = None;
        } else if quote.is_none() && matches!(ch, '\'' | '"') {
            quote = Some(ch);
            started = true;
        } else if quote.is_none() && ch.is_whitespace() {
            if started {
                words.push(std::mem::take(&mut word));
                started = false;
            }
        } else {
            word.push(ch);
            started = true;
        }
        if words.len() > 16 {
            return Err("Too many command arguments.".into());
        }
    }
    if quote.is_some() {
        return Err("Unclosed quote in command.".into());
    }
    if started {
        words.push(word);
    }
    Ok(words)
}

/// The single `about:help` source used by terminal and graphical TRust.
/// Command lines are preformatted so their alignment survives any viewport.
pub const HELP_PAGE: &str = "\
# TRust help

Tab or Ctrl-] toggles the command console; Enter runs a line.
A bare URL or hostname opens directly, like an address bar.
Anything else searches DuckDuckGo Lite.

## Commands

```
open <host> [port]        web by default; ports select a known service/telnet
open <url>                gopher gemini http(s) finger telnet …
back / forward            travel browser history
close                     drop the connection
reload                    refetch the page on screen
post <url> [body]         POST a form body to a web URL
finger [user]@<host>      finger query
wrap [on|off]            wrap a Finger, WHOIS or RDAP reply
changes [on|off]         compare with the previous successful refresh
whois <query> [server]    WHOIS lookup; quote queries containing spaces
encoding [auto|utf8|latin1]  WHOIS display encoding
save [server-number]     save WHOIS replies or original RDAP JSON
rdap [domain|IP|CIDR|AS-number]  readable authoritative RDAP record
dict <word> [server]      dictionary lookup
status                    connection and options report
help                      this page
quit                      exit
```

## Settings

```
set encoding cp437|utf8   BBS art mode
set image sixel|halfblocks|kitty|iterm2|auto
set js on|off             page JavaScript (default on)
set cookies on|off        RAM-only cookies (default on)
set borders on|off        CSS borders (default off)
mode character|line|auto  telnet input mode
send escape|<iac>         Ctrl-] or an IAC (brk/ip/ayt/…)
toggle crlf               what Enter sends
```

## Browsing keys

```
Up/Down        move the selection (page scrolls along)
Enter/Right    follow the selected link
Left/Backspace back · Alt-Left/Alt-Right back/forward
PgUp/PgDn      page · Home/End top/bottom
Ctrl-F         find in page (Enter next, Shift-Enter prev)
v              play the selected link in mpv
y              copy the selected link URL (OSC 52)
Esc            stop loading and page scripts
```

Mouse: hover selects, click follows, wheel scrolls,
back/forward side buttons travel history.

## Finger replies

Replies appear as they arrive, with columns and tabs preserved.
W toggles wrapping; Shift-Left/Right pans an unwrapped reply.
After `reload`, D toggles changes since the previous successful reply.
URLs are selectable links. Esc stops loading and keeps the received text.

## WHOIS replies

The requested record appears first. Contacts and Record details show more;
Full server replies retains every answer, including discovery and legal text.
Sources and conflicting values remain available in Record details.
W, D, Shift-Left/Right and Esc work as for Finger. E cycles auto, UTF-8 and
Latin-1 decoding. S saves the lookup; `save 2` saves server 2's exact bytes.
Quote multiword queries: whois \"-r -T inetnum 192.0.2.1\" whois.ripe.net
Look up with RDAP opens a readable record for domains, IPs and AS numbers.
RDAP has Contacts, Record details and Original JSON views; S saves exact JSON.

## Telnet sessions

Line mode edits locally; character mode sends every key to
the remote (Ctrl-] still opens the console). Esc reaches the
remote in character mode — full-screen apps depend on it.

## Image viewer

Left/Backspace/q/Esc close it. `set image` picks the protocol.
";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_stashes_a_draft_and_deduplicates_adjacent_entries() {
        let mut history = History::default();
        history.push("open example.com");
        history.push("open example.com");
        history.push("status");
        assert_eq!(history.up("draft"), Some(String::from("status")));
        assert_eq!(
            history.up("ignored"),
            Some(String::from("open example.com"))
        );
        assert_eq!(history.down(), Some(String::from("status")));
        assert_eq!(history.down(), Some(String::from("draft")));
    }

    #[test]
    fn shared_address_helpers_match_command_syntax() {
        assert!(looks_like_host("example.com"));
        assert!(looks_like_host("localhost:8080"));
        assert!(!looks_like_host("reload"));
        assert!(looks_like_address("https://example.com/path"));
        assert!(looks_like_address("mailto:user@example.com"));
        assert!(looks_like_address("web+demo:value"));
        assert!(!looks_like_address("rust ownership"));
        assert!(!looks_like_address("1invalid:value"));
        assert_eq!(parse_port("gemini"), Some(1965));
        assert_eq!(
            split_host_port("example.com:2323"),
            ("example.com", Some(2323))
        );
    }

    #[test]
    fn search_query_uses_urlencoded_utf8_form_serialization() {
        assert_eq!(
            search_url("rust & web/日本"),
            "https://lite.duckduckgo.com/lite?q=rust+%26+web%2F%E6%97%A5%E6%9C%AC&kd=-1"
        );
        assert_eq!(
            search_url("100% fun + useful"),
            "https://lite.duckduckgo.com/lite?q=100%25+fun+%2B+useful&kd=-1"
        );
    }
}
