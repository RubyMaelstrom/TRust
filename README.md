# TRust — Terminal browser in Rust

A terminal-based browser written in Rust, that's why it's called TRust.
Oh wait, did you know it also supports telnet, gopher, gemini, finger, and whois?
Oh, and HTTP. With a full rusty JS engine forked from Boa and customized.
Image support? We got it. Live JS rendering? Yup. Full CSS? Yeah.

Browse the web, connect to MUDs, check out your favorite gopher holes and
gemini capsules, all in one place. Do you like YouTube or any other
audio/video content? If you have mpv installed, it will automatically
open the target in mpv for your viewing and listening pleasure.

## Installation

Just `git clone` this repo and then `cargo build --release`.

## Launching it

```
trust <host> [port]          # telnet (port may be a name: smtp, nntp, ...)
trust gemini://gem.sdf.org   # or gopher://, http(s)://, finger://, ...
trust                        # start at the command prompt
```

## Driving it

**TAB** or **Ctrl+]** opens the `trust>` command prompt from anywhere; in line
mode plain **Esc** works too. You can skip `open` entirely — typing
`gemini://gem.sdf.org` at the prompt just goes there.

| Command | Effect |
|---|---|
| `website.com` | with no port defaults to opening using http. If you include a port that isn't one of the standard protocol ports, it assumes telnet. http://website.com:2323 for http w/port, gemini://website.com for gemini sites, etc |
| `open <host> [port]` | connect — URLs pick their protocol, `host:port` works, ports can be service names; `telnets://` (or port 992) is telnet over TLS |
| `post <url> [body]` | HTTP POST, form-urlencoded |
| `finger [user]@<host>` | who's there / their .plan (RFC 1288) |
| `whois <domain> [server]` | domain lookup via IANA, referral followed (RFC 3912) |
| `dict <word> [server]` | definitions from dict.org (RFC 2229) |
| `reload` | re-fetch what's on screen, history untouched |
| `close` / `quit` | drop the connection / exit |
| `mode character\|line\|auto` | force input mode or follow ECHO |
| `send brk\|ip\|ao\|ayt\|ec\|el\|ga\|nop\|escape` | transmit IAC commands (or a literal Ctrl-]) |
| `set encoding cp437\|utf8` | CP437 for BBS ANSI art |
| `set image sixel\|halfblocks\|kitty\|iterm2\|auto` | force the image protocol |
| `set js on\|off` | run web-page JavaScript against a real DOM (on by default; `off` opts out) |
| `toggle crlf` | Enter sends CR LF instead of CR NUL |
| `status` | connection/options report |

