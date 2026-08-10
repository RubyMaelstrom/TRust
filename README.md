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

The native desktop architecture is also available as `trust-desktop`. It uses
winit and the same CSS-pixel layout engine as the terminal browser. HTML boxes,
author colors, borders, gradients, images and Parley-shaped text paint through
a renderer-neutral TRust display list. Vello CPU is the correctness/reference
renderer and permanent software fallback; Vello Hybrid can present that same
list directly through wgpu. The established `trust` terminal frontend remains
fully supported through a CSS-pixel-to-cell adapter. Tests and developer tools
can render the identical page pipeline without a window through
`trust::render::headless`.

## Launching it

```
trust <host> [port]          # telnet (port may be a name: smtp, nntp, ...)
trust gemini://gem.sdf.org   # or gopher://, http(s)://, finger://, ...
trust                        # start at the command prompt
trust-desktop https://example.com  # native graphical browser
trust-desktop --renderer=auto https://example.com    # default: Hybrid, then CPU fallback
trust-desktop --renderer=cpu https://example.com     # force reference/software rendering
trust-desktop --renderer=hybrid https://example.com  # require a present-capable GPU adapter
```

`auto` only selects Hybrid after surface, adapter, device and capability
initialization succeeds. A later recoverable Hybrid device/render failure
switches the live window to Vello CPU. `--renderer=hybrid` reports an initial
GPU failure instead of silently ignoring an explicitly requested backend.
Neither choice changes DOM, CSS, layout, hit testing, or display-list output.

For frame-stage timings, run with `TRUST_DESKTOP_TRACE=1`. The repeatable
headless fixture benchmark is:

```sh
TRUST_DESKTOP_BENCH=1 TRUST_DESKTOP_BENCH_ITERATIONS=5 \
  cargo test --release desktop_pipeline_bench -- --ignored --nocapture
```

It covers text, flex/grid, overlapping composited cards, a large scrolling
document, dynamic DOM mutation, a decoded-image grid, and graphical Telnet
redraw. See [`src/render`](src/render) for the backend boundary and comments on
current Vello-specific limitations.

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
