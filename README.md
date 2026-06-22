# TRust — Telnet in Rust

A GNU-telnet-compatible client that grew a browser. Telnet (plain or
TLS) for your BBSes and MUDs, plus gopher, gemini, and the text-only
web — forms, search engines, even images — all in one cyberpunk
terminal UI.

```
trust <host> [port]          # telnet (port may be a name: smtp, nntp, ...)
trust gemini://gem.sdf.org   # or gopher://, http(s)://, finger://, ...
trust                        # start at the command prompt
```

## Driving it

The bottom of the screen is the entry field. In **line mode** you type
locally and Enter sends the line — with cursor editing, Shift+arrow
selection, Up/Down history recall, and horizontal scrolling when a
line outgrows the field. When a server negotiates ECHO (BBS login
prompts, full-screen apps), TRust switches to **character mode** and
every keystroke goes straight to the wire, just like GNU telnet.

**Ctrl-]** opens the `trust>` command prompt from anywhere; in line
mode plain **Esc** works too. You can skip `open` entirely — typing
`gemini://gem.sdf.org` at the prompt just goes there.

| Command | Effect |
|---|---|
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

The mouse wheel scrolls 10k lines of scrollback (Shift+drag for
terminal-native text selection, since the mouse is captured). BEL
rings your real bell. BBS "ANSI detection" works — TRust answers
cursor-position and device-attribute probes itself, because the remote
talks to an embedded vt100 emulator, not your actual terminal.

## The browser

Any gopher, gemini, or http(s) URL opens a shared browser panel with
gopherus-style navigation: **Up/Down** move through the page with the
link highlight riding the center of the screen, **Right/Enter**
follows, **Left** goes back (position restored), **Esc** returns to
the terminal. **Ctrl-F** finds in the page — type to highlight matches
as you go, **Enter/↓** and **Shift-Enter/↑** step through them (with a
match counter), **Esc** closes. Pages re-wrap live when you resize.
Fetches run in the background with timeouts and size caps — a little
heart beats at the right end of the entry bar while one is in flight.

Things it handles along the way:

- **Search**: gopher type-7 items and gemini input prompts open an
  amber `search>` field. On the web, search engines work because...
- **HTML forms work.** Text fields, checkboxes, radios, selects, and
  buttons render as selectable widget rows — Enter edits, toggles,
  cycles, or submits (GET and POST). Hidden fields ride along, typed
  values survive resizes. No file uploads, no multipart.
- **Images** open in a full-panel viewer, scaled to fit: sixel, kitty,
  or iTerm2 graphics when your terminal speaks them, unicode
  half-blocks anywhere else. Works for web images, gopher `I`/`g`/`p`
  items, and gemini `image/*`. PNG/JPEG/GIF/WebP. Left/Esc puts you
  back on the page exactly where you were.
- **One-shot lookups** (`finger`, `whois`, `dict`) render in the same
  panel, and their URL schemes are followable links everywhere —
  gophermaps, gemtext, HTML.
- **JavaScript is on by default** (`set js off` opts out): pages run through a
  real JS engine (Boa, pure Rust) against a real DOM before rendering —
  scripts build the page, the text pipeline draws what they built.
  jQuery manipulates pages, D3 appends elements, Vue mounts components,
  and pages may `fetch()`/XHR through TRust's own HTTP stack (capped
  per page, never into private address space), run ES modules, see a
  RAM-only cookie jar, and keep RAM-only `localStorage` for the
  session. **Cookies are strictly no-cross-site for now**: captured in
  RAM, never written to disk, and sent back *only* to the exact host
  that set them — `Domain=` is ignored, so no parent or sibling
  subdomain ever sees another host's cookie (`set cookies off` disables
  them entirely). This policy may change going forward, but that is the
  rule today. **Pages with things to click or edit stay alive**: buttons,
  script-handled links, and forms join the normal navigation — Enter
  dispatches real click/input/change/submit events into the live DOM
  and the page re-renders with whatever the handlers built, your place
  preserved. SPA-style links or forms that call preventDefault update
  in place; real navigations and unhandled form submits proceed through
  TRust's normal fetch path. Strictly budgeted (per-dispatch wall clock,
  hard loop limits); idle pages cost nothing (timers advance only when
  you interact) and pages with nothing interactive never hold an engine
  at all. Script errors show as a `JS:n!` badge; the page still renders.
- **`.gmi` files served over gopher render as gemtext**, relative
  links and all — a nod to a common small-net habit.
- Cross-scheme links interconnect: gemtext can point at gopherspace,
  gophermaps at the web, and back.

## Trust (the name is not an accident)

Small-net TLS is **trust-on-first-use**: the first certificate a
host:port shows is pinned (SHA-256) in `~/.config/trust/known_hosts`,
and a different one later is refused with instructions for re-trusting
deliberately. That covers `telnets://` and gemini. The web instead
validates against the bundled Mozilla roots — TOFU would cry wolf
every cert rotation.

**Gemini identities** (client certificates): drop a PEM with your cert
and key at `~/.config/trust/identities/<host>.pem` — block order
doesn't matter, `cat my.crt my.key` is fine — and it's presented to
that host, and only that host. The status bar shows `· ID` when one
was sent. If a capsule asks for a certificate you don't have (status
60), TRust offers to mint one on the spot: a `name>` prompt prefilled
with your username, one Enter, and you're in (the name becomes the CN,
which capsules like astrobotany use as your username). Existing
identity files are never overwritten — capsules pin them, and losing
one means losing the account.

## Architecture

```
src/
  main.rs     CLI args, terminal setup, graphics-protocol query
  app.rs      App state + event loop (crossterm events ⨯ network channels)
  telnet.rs   Connection task: socket I/O + telnet protocol state
  tls.rs      rustls: TOFU pinning, WebPKI, client identities
  doc.rs      protocol-agnostic document model (Link, Kind, DocLine, Doc)
  gopher.rs   Gopher (RFC 1436)
  gemini.rs   Gemini: fetches, redirects, gemtext
  http.rs     HTTP/1.1 GET/POST, chunked transfer, html2text, forms
  dom.rs      arena DOM: html5ever in, page JS mutates, HTML out
  js.rs       Boa engine glue: DOM syscalls, web platform, budgets
  oneshot.rs  finger, WHOIS (with referrals), DICT
  img.rs      image decode + terminal-graphics encode
  cp437.rs    CP437→Unicode for BBS art
  ui.rs       ratatui rendering: chrome, session widget, browser panel
```

The shape of it: the app never touches protocol bytes (connection
tasks own them, talking over channels), remote output renders through
an embedded vt100 emulator so full-screen apps survive inside the
styled frame, and every document — gophermap, gemtext, HTML, a WHOIS
reply — parses into the same line-based model the browser renders.

## Status

Done: the telnet core with option negotiation, NAWS, TERMINAL-TYPE,
TSPEED, NEW-ENVIRON, STATUS, LFLOW, LINEMODE, BINARY, probe replies, CP437; TOFU
TLS and `telnets://`; the full browser stack across gopher, gemini
(including client identities), and the text web (HTML forms, images);
finger/whois/dict; service-name ports.

In flight: JavaScript. A real engine (Boa), run as a parse-time
document transformation under strict budgets — the line-based reader
stays, the JS-walled web opens up. Displayed pages stay alive: clicks
dispatch real DOM events and the page re-renders from the mutated
arena (archive.org boots its Lit app and routes in the TUI). A
display/visibility mini-cascade reads the page's stylesheets — inline,
`<style>`, shadow/adopted sheets, fetched `<link>` CSS — so
class-hidden menus collapse and class-toggled panels genuinely open.

Still to come: the long tail of GNU command-mode parity (`set`/`unset`,
`display`, `logout`, `z`, `!`, `.telnetrc`). Maybe someday: inline
images, animated GIFs.
