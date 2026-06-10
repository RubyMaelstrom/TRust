# TRust — Telnet in Rust

A telnet client aiming for 1:1 functionality with GNU telnet — plus a
gopher browser — wrapped in a cyberpunk-themed terminal UI.

```
cargo run -- <host> [port]         # telnet, connect on startup
cargo run -- gopher://host[/sel]   # open the gopher browser
cargo run                          # start at the command prompt
```

Input mode follows the ECHO negotiation, the way GNU telnet picks its
mode:

- **Line mode** (server does not echo): text is typed into the entry
  field at the bottom (local echo, cursor editing); **Enter** sends the
  line. **Up/Down** recall previously entered lines (in-memory only,
  cleared on exit; an unfinished line is stashed and restored when you
  arrow back past the newest entry). Control chords (Ctrl-C, Ctrl-D, ...)
  bypass the field.
- **Character mode** (server sends `WILL ECHO` — BBSes, login prompts,
  full-screen apps): every keystroke goes straight to the wire and the
  server echoes. The entry field dims to a `CHAR` strip so the layout
  stays put when servers toggle ECHO around password prompts.

The **mouse wheel** scrolls the session feed through 10k lines of
in-memory scrollback (an amber `SCROLL` badge shows the offset; sending
anything snaps back to live). The mouse is captured for this, so use
Shift+drag for terminal-native text selection.

**Ctrl-]** toggles command mode (GNU telnet's `telnet>` prompt) from
either input mode; **Esc** returns to the session. Commands:

| Command | Effect |
|---|---|
| `open <host> [port]` | connect (default port 23; `gopher://`/port 70 open the browser, `telnets://`/port 992 use TLS, `telnet://` forces plain telnet) |
| `close` | drop the connection |
| `mode character\|line\|auto` | force input mode or follow ECHO |
| `send brk\|ip\|ao\|ayt\|ec\|el\|ga\|nop\|escape` | transmit an IAC command (or a literal Ctrl-]) |
| `set encoding cp437\|utf8` | CP437 translation for BBS ANSI art (dim badge when active) |
| `toggle crlf` | Enter sends CR LF instead of the default CR NUL |
| `status` | print connection/options report into the feed |
| `quit` | exit |

BEL from the remote rings the real terminal's bell.

Note: crossterm reports the raw `0x1D` byte that terminals send for
Ctrl-] as `Ctrl-5`, so both encodings are matched in `app.rs`.

## TLS

`open telnets://host [port]` (default 992) or any port-992 connection
wraps the telnet session in TLS via rustls. Certificate validation is
**trust-on-first-use**, matching small-net practice (self-signed certs
everywhere): the first certificate a host presents is accepted and its
SHA-256 fingerprint pinned; a different certificate later is refused.
Pins are in-memory only for now — a persistent known-hosts store lands
with Gemini, which will reuse this connector. A green `TLS` badge shows
in the status bar while a TLS session is live.

## Gopher browser

`open gopher://host[:port][/Xselector]` (or any port-70 connection)
replaces the terminal panel with a gopherus-style browser:

- **Up/Down** navigate gopherus-style. When the adjacent line is also a
  link, the highlight steps onto it, with the page scrolling along so the
  selection tends to ride the center of the screen (except near the
  document's ends, where the page pins and the highlight walks between
  the visible links). Across link-free stretches the page scrolls under
  the sticky selection; the highlight hands off to the next link once it
  comes closer to the center, and disappears while no link is on screen.
  The status bar shows the selected link's URL.
- **Right** (or Enter) follows the selected link; **Left** goes back
  through the in-RAM history, restoring your position. **Esc** returns
  to the terminal view.
- Type-7 search items open an amber `search>` prompt; the query is sent
  tab-separated per RFC 1436.
- Menus and text files render with type-colored links (menus cyan, text
  green, search amber); errors show in pink, info lines in plain text.
  `set encoding cp437` applies to gopherspace too.
- Long lines word-wrap to the panel width and re-flow live on terminal
  resize (the raw bytes are kept per document, so encoding switches
  re-render too). Only the first row of a wrapped menu item is
  selectable; continuations belong to the same item visually.
- Fetches are one-shot TCP with a 15 s timeout and a 2 MB cap, run in
  the background so the UI never blocks. Binary/image item types are
  reported as unsupported (yet); `h` items show their `URL:` target for
  use in a web browser.

## Architecture

```
src/
  main.rs     CLI args, terminal setup/teardown
  app.rs      App state + event loop (crossterm EventStream ⨯ telnet events)
  telnet.rs   Connection task: socket I/O + libmudtelnet protocol state
              (generic over plain TCP and TLS transports)
  tls.rs      rustls connector with trust-on-first-use fingerprint pinning
  gopher.rs   Gopher (RFC 1436): one-shot fetches, menu/text parsing
  cp437.rs    CP437→Unicode translation for BBS ANSI art
  ui.rs       Ratatui rendering: cyberpunk chrome, tui-term session widget,
              gopher document panel
```

Design notes:

- **The app never sees protocol bytes.** The connection task
  (`telnet.rs`) owns the `libmudtelnet::Parser`; IAC sequences, option
  negotiation, and NAWS subnegotiation happen there. The app exchanges
  plain `Send`/`Resize`/`Close` commands and `Connected`/`Data`/`Closed`
  events over mpsc channels.
- **Remote output is emulated, not echoed.** Because ratatui owns the
  screen, the remote byte stream is fed into a `vt100` parser and rendered
  with tui-term's `PseudoTerminal` widget. This keeps full-screen remote
  applications working inside the styled frame.
- **NAWS reports the widget's inner size**, not the real terminal size,
  since that is the area the remote application actually gets to draw in.
- `vt100` is used via tui-term's re-export (`tui_term::vt100`) so the two
  can never drift apart.

## GNU telnet parity roadmap

- [x] Connect/close/quit, Ctrl-] command mode
- [x] Option negotiation framework (ECHO, SGA accepted; rest refused)
- [x] NAWS (RFC 1073) with renegotiation on resize
- [x] TERMINAL-TYPE (RFC 1091) — offers ANSI, XTERM, VT100 in order
- [x] Terminal probe replies (BBS "ANSI detection"): `ESC[6n` cursor
      position report, `ESC[5n` status report, `ESC[c` device attributes —
      answered by us because the remote talks to our embedded emulator,
      not the real terminal
- [x] BINARY (RFC 856) accepted when the server asks
- [x] `send brk/ip/ao/ayt/ec/el/ga/nop/escape` (IAC commands, BREAK for
      console servers, literal Ctrl-])
- [x] `status` command, `toggle crlf` (Enter sends CR NUL by default per
      RFC 854, CR LF when toggled)
- [x] BEL passthrough to the host terminal
- [x] CP437 encoding for BBS ANSI art (`set encoding cp437` — not a GNU
      feature, but required because we emulate rather than pass through)
- [ ] TSPEED (RFC 1079), LFLOW (RFC 1372)
- [ ] LINEMODE (RFC 1184) and the localchars machinery
- [ ] NEW-ENVIRON (RFC 1572), STATUS-the-option (RFC 859)
- [ ] Remaining command-mode parity: full `set`/`unset`, `display`,
      `logout`, `z`, `!`, `.telnetrc`, service-name ports
- [x] Character-at-a-time vs line mode, driven by ECHO negotiation,
      with a manual `mode` override

## Small-net roadmap

- [x] Gopher browser (RFC 1436) with gopherus-style navigation
- [x] TLS foundation: rustls with TOFU fingerprint pinning; `telnets://`
- [ ] **Gemini — next up.** The TLS connector, document view,
      word-wrap, and fetch-task pattern are all in place; see CLAUDE.md
      for the agreed implementation plan (gemtext parsing, status
      codes, link-type generalization, persistent TOFU known-hosts).
- [ ] Finger (79), WHOIS (43), DICT (2628) — trivial one-shot
      personalities
- [ ] HTTP(S) GET + html2text rendering (after Gemini)
- SSH is an explicit non-goal.
