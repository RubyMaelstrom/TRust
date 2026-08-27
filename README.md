# TRust — Terminal browser in Rust

A terminal-based browser written in Rust, that's why it's called TRust.
Oh wait, did you know it also supports telnet, gopher, gemini, finger, and whois?
Oh, and HTTP. With the pure-Rust Lumen JavaScript engine integrated directly.
Image support? We got it. Live JS rendering? Yup. Full CSS? Yeah.

Browse the web, connect to MUDs, check out your favorite gopher holes and
gemini capsules, all in one place. Do you like YouTube or any other
audio/video content? If you have mpv installed, it will automatically
open the target in mpv for your viewing and listening pleasure. Direct YouTube
playback URLs, including `youtu.be` shares, are delegated to mpv while YouTube
search, channel, and other browsing pages remain in TRust.

## Installation

The normal TRust binaries use Lumen, a pure-Rust JavaScript engine maintained
in a sibling checkout. The current integration checkout is laid out as:

```text
Code/
├── Lumen/   # TRust integration branch, currently 384e7f4
└── TRust/
```

The path dependency is intentional: TRust and Lumen are being developed
together while the host-boundary work is upstreamed. Keep the sibling Lumen
checkout at the integration revision recorded above before building.

From `TRust`, `cargo build --release` builds the Lumen-only `trust` and
`trust-desktop` release binaries. Lumen is selected at compile time, so neither
normal artifact contains the legacy Boa engine.

The native desktop binary, `trust-desktop`, uses
winit and the same CSS-pixel layout engine as the terminal browser. HTML boxes,
author colors, borders, gradients, images and Parley-shaped text paint through
a renderer-neutral TRust display list. Vello CPU is the correctness/reference
renderer and permanent software fallback; Vello Hybrid can present that same
list directly through wgpu. The established `trust` terminal frontend remains
fully supported through a CSS-pixel-to-cell adapter. Tests and developer tools
can render the identical page pipeline without a window through
`trust::render::headless`.

## JavaScript engine

Lumen is the production JavaScript engine for both the terminal and desktop
frontends. The browser-facing contract lives in TRust; engine-specific host
bindings and the resident page actor live in `src/lumen_backend.rs` and the
sibling Lumen checkout. This keeps DOM, networking, storage, workers, and
rendering behavior shared by both frontends.

Boa is retained only as an explicitly selected legacy/regression backend. It is
not linked into normal release binaries:

```sh
cargo build --release --no-default-features --features mimalloc,boa-backend \
  --bin trust-boa --bin trust-desktop-boa
```

The opt-in `trust-lumen-spike` binary is a synthetic Lumen benchmark harness;
it is not a separate browser backend and is omitted from ordinary builds.

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
`gemini://gem.sdf.org` at the prompt just goes there. Text that is neither a
command nor an address searches DuckDuckGo Lite.

| Command | Effect |
|---|---|
| `<search terms>` | search DuckDuckGo Lite (any text that is not a command or address) |
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
