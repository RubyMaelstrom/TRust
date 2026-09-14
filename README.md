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

All TRust binaries use Lumen, a pure-Rust JavaScript engine maintained
in a sibling checkout. The current integration checkout is laid out as:

```text
Code/
├── Lumen/   # tested engine revision: 8b456e1c3d544111af3dae058becef11f8ce6bb9
└── TRust/
```

The path dependency is intentional: TRust and Lumen are being developed
together while the host-boundary work is upstreamed. Keep the sibling Lumen
checkout at the integration revision recorded above before building.

From `TRust`, `cargo build --release` builds the `trust`, `trust-desktop`,
`trust-headless`, and developer replay binaries. Lumen is an unconditional
dependency; no backend-selection feature is needed. Use `--no-default-features`
to build with the system allocator instead of mimalloc.

Before handing off browser changes, run both `cargo test` and
`cargo test --release`, then `cargo clippy --all-targets` and
`cargo build --release`. Test page rendering with the release binaries, including
the sites affected by the change. A debug test pass and release startup check
do not verify release rendering or performance.

`python3 tools/check_css_wpt.py` runs a pinned official WPT subset for CSS
variables, registered properties, and background shorthands in the release
headless browser. It caches upstream sources, supports `--offline` on subsequent
runs, writes JSON results and browser logs under `target/css-wpt-results`, and
exits unsuccessfully for failed tests or incomplete harness runs. It is a
targeted conformance check; its result files retain every failure.

The native desktop binary, `trust-desktop`, uses
winit and the same CSS-pixel layout engine as the terminal browser. HTML boxes,
author colors, borders, gradients, images and Parley-shaped text paint through
a renderer-neutral TRust display list. Vello CPU is the correctness/reference
renderer and permanent software fallback; Vello Hybrid can present that same
list directly through wgpu. The established `trust` terminal frontend remains
fully supported through a CSS-pixel-to-cell adapter. Tests and developer tools
can render the identical page pipeline without a window through
`trust::render::headless`.

Starting `trust-desktop` without an address opens COMMAND; providing an address
goes straight to the page. COMMAND floats over the page without changing HTML
layout or the viewport reported to JavaScript. Its console shows response and
viewport details, with additional document and image-cache readouts when space
permits. Image-cache RAM uses the existing decoded-pixel byte counter; it
excludes browser chrome, GPU copies, and other page allocations. Reading it
adds no background reporting, heap measurement, or refresh timer.
Ctrl+L opens COMMAND with the current address selected.

## JavaScript engine

Lumen is the sole JavaScript engine for the terminal, desktop, and headless
frontends. The browser-facing contract lives in TRust; engine-specific host
bindings and the resident page actor live in `src/lumen_backend.rs` and the
sibling Lumen checkout. This keeps DOM, networking, storage, workers, and
rendering behavior shared by both frontends.

Number inputs provide desktop spin buttons and Up/Down keyboard stepping.
The shared DOM implements `stepUp()`, `stepDown()`, and `valueAsNumber` for
number, range, date, month, week, time, and datetime-local inputs, including
step/bounds validation and independent current/default values. A manual
acceptance page is available at `src/fixtures/number_input.html`.

The normal build advertises US English: `Accept-Language: en-US,en;q=0.9`,
`navigator.language === "en-US"`, and an `en-US` default for native Intl.
This preference does not depend on the OS language, region, or geographic
location and needs no build flag. Explicit locale requests remain supported;
websites may still choose content using their own account or IP-based settings.

The opt-in `trust-lumen-spike` binary is a synthetic Lumen benchmark harness;
it is not a separate browser backend and is omitted from ordinary builds.

## Launching it

```
trust <host> [port]          # telnet (port may be a name: smtp, nntp, ...)
trust gemini://gem.sdf.org   # or gopher://, gophers://, http(s)://, finger://, ...
trust                        # start at the command prompt
trust-desktop https://example.com  # native graphical browser
trust-desktop /path/to/image.png   # local file path
trust-desktop file:///path/to/page.html  # explicit file URL
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

Developer diagnostics, benchmark inputs, ignored live-site gates, and their
environment-variable reference are collected in
[`DIAGNOSTICS.md`](DIAGNOSTICS.md).

### Local files

All frontends accept absolute paths, explicit `./` or `../` paths, existing
relative files, and `file:` URLs. For example:

```sh
target/release/trust-desktop ~/Pictures/IdleHeart.png
target/release/trust-desktop "./Pictures/Idle Heart.png"
target/release/trust file:///home/ruby/Documents/page.html
```

Paths are converted to URLs with proper escaping; a filename's literal `#`,
`?`, or `%` is not mistaken for URL syntax. In an explicit file URL, use
percent-encoding for those filename characters; queries and fragments are
not part of the filesystem path. The command prompt also accepts whole
paths containing spaces, with or without `open`.

Local HTML can load relative images, stylesheets (including imports), classic
scripts, and frames. File documents have opaque origins: Fetch/XHR, modules,
web fonts, and other CORS-dependent resources need an HTTP(S) origin instead.
Standalone and inline SVG work; external `file:` SVG sprite references are
restricted to keep local data out of the shared sprite cache.
Web pages cannot read or navigate into local files. Local bookmarks and direct
user navigation remain available.

Only an empty file-URL host or `localhost` is accepted; other authorities are
not resolved or mounted. Reading uses ordinary filesystem permissions and is
limited to regular files of at most 512 MiB. Directories, special files, and
methods other than GET return an error; opening a file never writes to it.

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
| `finger [user]@<host>[:port]` | who's there / their .plan (RFC 1288); IPv6 accepts `[address]:port` |
| `wrap [on\|off]` | toggle wrapping of a Gemini, Gopher, Finger, WHOIS, RDAP or DICT reply |
| `gopher-info [page]` | show Gopher+ item information and available formats; `page` selects the current page |
| `changes [on\|off]` | compare a Finger or WHOIS reply with its previous successful refresh |
| `whois <query> [server[:port]]` | WHOIS lookup via IANA or a chosen server; quote queries containing spaces |
| `encoding [auto\|utf8\|latin1]` | select WHOIS display encoding without refetching |
| `save [server-number]` | save received Gemini/Gopher source, original DICT text, RDAP JSON, the WHOIS transcript, or one WHOIS server's exact bytes |
| `rdap [domain\|IP\|CIDR\|AS-number]` | show the authoritative RDAP record; omitted query uses the current WHOIS lookup |
| `dict [--database name] <word or "phrase"> [server[:port]]` | definitions from dict.org or a chosen server (RFC 2229) |
| `dict --match strategy <word> [server]` | matching words; `prefix`, `exact`, or a server-advertised strategy |
| `dict --databases` / `dict --strategies` | browse dictionaries or search modes; `--server` chooses the server |
| `dict-filter <text>` | filter the current dictionary/source list locally; omit text to clear |
| `bookmark` / `bookmark add [title]` | bookmark the current destination, for any protocol |
| `bookmark link [title]` | bookmark the selected link without fetching it |
| `bookmarks [filter]` | open the local bookmark list; filter titles and addresses |
| `bookmark rename <id> <title>` | rename an entry using its displayed ID |
| `bookmark remove <id>` / `bookmark undo` | remove an entry / undo the last removal in this session |
| `reload` | re-fetch what's on screen, history untouched |
| `close` / `quit` | drop the connection / exit |
| `mode character\|line\|auto` | force input mode or follow ECHO |
| `send brk\|ip\|ao\|ayt\|ec\|el\|ga\|nop\|escape` | transmit IAC commands (or a literal Ctrl-]) |
| `set encoding cp437\|utf8` | CP437 for BBS ANSI art |
| `set image sixel\|halfblocks\|kitty\|iterm2\|auto` | force the image protocol |
| `set js on\|off` | run web-page JavaScript against a real DOM (on by default; `off` opts out) |
| `toggle crlf` | Enter sends CR LF instead of CR NUL |
| `status` | connection/options report |

**Ctrl+B** bookmarks the current destination; **Alt+B** opens bookmarks.
During Telnet sessions both shortcuts retain their remote meanings (Ctrl+B is
STX; Alt+B sends Escape then B). Use `bookmark` and `bookmarks` from the command
prompt there. Opening bookmarks keeps the Telnet session connected; Back returns
to it. Elsewhere the shortcuts also work in input fields and the image viewer.

Bookmarks are shared by the terminal and desktop browsers, stored as versioned
JSON in `$XDG_DATA_HOME/trust/bookmarks.json` (default
`~/.local/share/trust/bookmarks.json`). They load only when needed. Saves are
atomic and coordinate between running instances. Saving an existing destination
reuses its entry; only an explicit title replaces its name. `status` shows the
storage directories. Existing TLS certificates, pins and DICT preferences keep
their existing locations. No disk cache or persistent cookies are introduced.

Gopher menus and text appear progressively in a centered, left-aligned
monospace column sized to the longest displayed source line, preserving line
breaks and spacing. Short phlogs get a narrower column; longer authored lines
expand it when space permits. The desktop keeps at least 22px of padding on
each side. Narrow views wrap to fit.
**W** toggles wrapping and
**Shift+Left/Right** pans unwrapped text; **E** cycles automatic, UTF-8, Latin-1
and CP437 display decoding without changing selectors. Automatic mode accepts
UTF-8 and otherwise uses Latin-1. **S** (or `save`) saves received source bytes.
**Esc** stops loading and preserves the displayed prefix, marked incomplete.
Wrapped menu entries remain clickable on every row, with one keyboard stop.
Recent Gopher pages and view settings remain in a bounded in-memory reading
history; `reload` explicitly requests a fresh copy.

`gophers://` uses Gopher over TLS, on port 70 unless a port is specified.
Links to the same host and port retain TLS, including searches and Gopher+
formats. Explicit `gophers://` requests require TLS and never fall back to
unencrypted Gopher. Bookmarks retain the chosen scheme.

Gemini text also arrives progressively, in a reading column of 96 characters.
Use **W** or `wrap` for ordinary text, **Shift+Left/Right** to pan preformatted
blocks, and **S** or `save` to save the received source. `gemini-width 20..240`
adjusts the column, `outline` lists numbered headings, `heading next|previous|N`
jumps to one, and `gemini-alt [on|off]` shows preformatted descriptions.
`gemini-help` (or `about:gemini`) opens the dedicated guide. Local `.gmi`,
`.gemini`, `.gemtext` and `.gmni` files open as Gemtext previews.

Both frontends support Gemini input and certificate prompts, images, and binary
Save/Open offers. Sensitive input is masked, excluded from command history,
and removed from stored page addresses. Certificate consent covers the requested
host, port and path subtree; existing PEM identities are retained and explicitly
authorized when first needed. A binary save consumes the original response
connection, avoiding a second input submission. Cross-protocol redirects show an
explicit link. Stop, timeouts and display limits preserve received text with a
notice; source saving retains its original bytes. Text decoding supports UTF-8,
ASCII and ISO-8859-1, with an explicit message for unsupported charsets.

Gophers and Gemini accept server certificates without CA, hostname, expiry or
fingerprint-pin checks, including certificate replacements. Traffic is
encrypted, but the server's identity is not verified. Existing Gemini server
pins are ignored; Gemini client identities use explicit scope authorization. HTTPS uses WebPKI and
Telnet TLS keeps its existing `known_hosts` pinning.

Search links prompt for a query in either frontend. A bookmark of the search
endpoint prompts again; a bookmark containing a query repeats that search.
Selectors retain their original bytes through copying, navigation and bookmarks,
including percent escapes, IPv6 hosts and non-default ports. Image items use the
image display and HTML items use the HTML renderer. Generic binary items are
checked for a supported format: images (including WebP), text and HTML open
directly in TRust. Unsupported files offer Save / Open / Cancel after a bounded
type check; files exceeding the page-body limit can also be downloaded.
Redundant-server menu entries
inherit the original item's type and can be selected as alternate destinations.

Desktop Gopher menus and text honor ANSI SGR foreground/background colors,
including bright colors, the 256-color palette, and truecolor RGB. Color resets
restore TRust's palette; selected links keep TRust's highlight. Wrapping, search,
and copying use the plain text. The terminal frontend keeps its fixed colors.
Cursor movement, screen clearing, and other terminal commands are discarded;
color detail is bounded so dense artwork remains responsive.

Gopher+ menus, text, item information and alternate formats are supported.
**I** (or `gopher-info`) opens information about the selected Gopher link,
falling back to the current page; `gopher-info page` requests the current page's
information explicitly. Available formats and languages appear as links;
supported formats open inside TRust and the chosen view can be bookmarked.
Information is fetched on demand and uses the existing in-memory page history.
Gopher+ multimedia items without a default format open their available formats.
ASK items display their form requirements; interactive form submission is not
implemented. Unimplemented legacy service types report an explicit error.

Gopher page responses are limited to 2 MiB, and display rows/line widths are
bounded independently. Downloads stream directly to disk with a 2 GiB ceiling.
Limits and interrupted replies are reported visibly; receiving a complete dot
terminator or the declared Gopher+ byte count finishes immediately even if the
server keeps the socket open. Gopher+ framing is removed before displaying or
saving the content.

Finger replies appear as they arrive, preserving columns, blank lines, and
eight-cell tab stops. **W** toggles wrapping; **Shift+Left/Right** pans an
unwrapped reply. Explicit URLs become selectable links. After `reload`, **D**
toggles a comparison with the previous successful reply; the original text
remains available. **Esc** stops loading and keeps the received text. Timeouts,
interrupted replies, and size limits leave a notice alongside any retained text.

WHOIS opens with the requested domain or network record: organization, registrar,
dates, status, nameservers and DNSSEC where available. IANA discovery records,
empty fields, duplicate values and repeated privacy placeholders do not fill the
summary. **Contacts** shows public contact information; **Record details** keeps
exact timestamps, field sources, disagreements and additional fields. **Full
server replies** retains each received answer, including comments and legal
text. Unrecognized formats fall back to a readable transcript.

These views switch locally, including while a referral is still loading. **D**
compares registration fields after a successful refresh; in the full replies it
compares the transcript. **W** wraps full replies; **Shift+Left/Right** pans;
**Esc** stops loading without discarding received data. **E** cycles automatic,
UTF-8 and Latin-1 decoding. Automatic mode prefers valid UTF-8 and otherwise uses
Latin-1, a heuristic because WHOIS has no encoding metadata. **S** saves the
lookup; `save 2` exports server 2's exact bytes, including original line endings
and encoding. A multi-server export adds headings between the original replies;
a single-server export is exact.

Use `whois "-r -T inetnum 192.0.2.1" whois.ripe.net` for a server-specific query,
or `whois -h whois.ripe.net -- "-r -T inetnum 192.0.2.1"`. Server addresses accept
ports and bracketed IPv6. WHOIS URLs decode their query path once; a `#fragment`
stays local. Automatic referrals retain every answer, detect cycles, and share
a four-server, 15-second, 1 MiB transaction budget. IANA's `refer:` is followed;
its `whois:` service metadata remains a selectable link. Failed referrals, timeouts
and truncation remain visible beside the received data. Display limits bound rows
and long lines independently of the bytes available to save.

The **Look up with RDAP** link is an explicit alternative for domains, addresses
and AS numbers. It uses IANA's RDAP bootstrap registries and prefers HTTPS.
RDAP has its own readable record view, including nested contacts with their roles,
registration events, DNSSEC, service notices and errors. Direct HTTP responses
with `application/rdap+json` use the same view. **Contacts**, **Record details**
and **Original JSON** switch locally; **S** saves the original JSON bytes.
Related RDAP links remain RDAP lookups. WHOIS never switches protocols automatically.


DICT displays definitions as they arrive, one at a time. **Definitions & sources**
selects a result locally; **Previous/Next definition** moves through the received
results in server order. **Browse dictionaries** exposes translation dictionaries,
thesauri and other sources; **Search modes** lists the server's supported matching
strategies. **Filter this list** opens a local filter. **Look up another word**
opens the command editor with the server and dictionary already filled in.

A failed definition lookup requests spelling suggestions from the same server
and database. Suggestions and common `{word or phrase}` cross-references are
selectable links with the usual keyboard, hover and click navigation. Definitions
retain their text, source attribution, numbered senses and hanging indentation.
**W** toggles wrapping; **Shift+Left/Right** pans unwrapped text. **Original text**
shows the received protocol transcript; **S** saves those exact bytes, including
original CRLF line endings. **Esc**, timeouts and failures preserve received data
and mark incomplete responses. Loading updates are throttled and both reply size
and display work are bounded.

`dict "ice cream"` accepts phrases, and apostrophes in words such as `can't` remain
literal. `dict --database wn neon` selects WordNet; `dict --match prefix neo`
searches for words beginning with `neo`. Commands default to all dictionaries,
unless **Use <dictionary> by default** has saved a preference for that server in
`$XDG_CONFIG_HOME/trust/dict.json` (or `~/.config/trust/dict.json`). Explicit URLs
always honor their database and strategy: `dict://dict.org/d:neon:wn:1` selects
the first result; `dict://dict.org/m:neo:wn:prefix` requests prefix matches.
An omitted URL database uses `!` (first matching dictionary), as RFC 2229 specifies.
URLs decode UTF-8 percent escapes once, accept bracketed IPv6 and keep fragments
local. Authentication and MIME attachments are not currently implemented.
