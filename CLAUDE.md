# TRust — notes for future sessions

A telnet client (GNU telnet parity) + gopher browser + TLS, cyberpunk
ratatui TUI. The README is the authoritative feature/architecture doc;
this file is workflow, gotchas, and the up-next plan.

## Quality bar

- `cargo fmt`, `cargo clippy` (zero warnings), `cargo test` — all clean
  before calling anything done. Currently 49 tests.
- Every feature gets BOTH a unit/integration test and a live smoke test
  in tmux against a throwaway local server. Unit tests prove the logic;
  the tmux run proves the UX (and has caught real bugs the tests missed).

## Live-testing workflow (and its gotchas)

- Fake servers are small Python scripts in `$CLAUDE_JOB_DIR/tmp/`
  (fake_telnetd.py, fake_bbs.py, fake_gopherd.py, ... — recreate as
  needed; they're throwaway). Run the TUI inside tmux:
  `tmux new-session -d -x 90 -y 24 -s name './target/debug/trust ...; sleep 5'`
  then `tmux send-keys` / `tmux capture-pane -p` (`-e` to check styling
  escapes, e.g. `7m` for the reverse-video link highlight).
- **`cargo test`/`clippy` do NOT rebuild `target/debug/trust`.** Always
  `cargo build` before a tmux smoke test or you'll test a stale binary
  (this burned us once).
- **Never `pkill -f <pattern>` where the pattern appears in your own
  shell command** — it kills your own shell (exit 144). Capture the PID
  at spawn time (`SRV=$!`) or `pgrep` first, inspect, then `kill <pid>`.
  The user's machine runs other python3 processes (open-webui on :8080,
  sometimes Steam/Proton games) — never kill python3 indiscriminately.
- Wheel/mouse events can be synthesized through tmux:
  `tmux send-keys -l $'\x1b[<64;10;10M'` (SGR wheel-up; 65 = down).
- Ctrl-] in tmux: `tmux send-keys C-]` works (arrives as 0x1D).

## Architecture invariants (don't break these)

- The app layer never sees telnet protocol bytes; `telnet.rs` owns the
  libmudtelnet parser and speaks to `app.rs` only via mpsc
  Command/Event channels. `run_session` is generic over the transport
  (TCP or TLS) — keep it that way.
- The remote stream renders through the embedded vt100 emulator
  (tui-term widget). Anything a real terminal would *answer* (DSR/CPR
  probes, DA, NAWS sizes, TTYPE) we must answer ourselves — the remote
  cannot see the user's actual terminal. NAWS/CPR report the widget's
  inner size, not the real terminal's.
- One emulator per connection: `open()` calls `reset_screen()`; stale
  scroll regions/alt-screen from a dead session caused a nasty
  bottom-line-overwrite bug (regression test exists).
- All TLS goes through `tls::connector()` (single ClientConfig, TOFU
  verifier, installs the process-wide crypto provider). Don't build a
  second ClientConfig.
- RAM-only ethos for session state: entry histories, scrollback,
  browser history. The ONE deliberate exception (user-approved
  2026-06): TOFU pins persist to ~/.config/trust/known_hosts
  (TRUST_KNOWN_HOSTS overrides; TLS tests point it at a temp file and
  must keep doing so). Pins are keyed host:port — the verifier can't
  see the port, so tls::connector(host, port) bakes the key in per
  connection. Don't add other persistent state without asking her.
- Crossterm quirk: legacy terminals deliver Ctrl-] as `Ctrl-5` (0x1D
  maps to `Char('5')+CONTROL`); both encodings are matched in app.rs.
- Documents (gopher, future gemini) wrap at *parse* time into real
  lines — never use ratatui render-time wrapping, it breaks the
  scroll/selection index math. Raw bytes are kept per doc for re-wrap
  on resize and re-decode on encoding change.

## The gopherus navigation model (user-specified, tested)

Up/Down in the browser: adjacent link → highlight steps onto it (page
scrolls along so the selection rides the screen center); non-link next
line → page scrolls under the sticky selection; handoff goes to the
*next link in document order* (never skipping) when that link is
strictly closer to the center row; no links visible → no highlight;
page pinned at either end → highlight walks between visible links.
Right/Enter follows, Left pops history (position restored), Esc returns
to the terminal view. The 7 `gopherus_*`/rewrap tests in app.rs encode
this — they are the spec; don't "simplify" them.

## Gemini (done 2026-06) — notes

Implemented per plan: `gemini.rs` (URL parse/resolve, header parse,
fetch with capped redirects, gemtext → DocLine), generalized document
model in `doc.rs` (Link enum: Gopher/Gemini/External; Kind enum for
styling), BrowserView (renamed from GopherView) shared by both
protocols. 1x input reuses Mode::Search with percent-encoded queries.

Hard-won detail: many gemini servers close without TLS close_notify;
`fetch_once` treats UnexpectedEof during body read as EOF. Keep that.

Testing note: TOFU pins are global per server name within the process,
so TLS tests must not share a hostname across different certs — the
telnet TLS test owns "localhost", the gemini test owns "127.0.0.1"
(one cert for all its phases). A live capsule for tmux demos:
fake_gemini.py + openssl-generated cert in $CLAUDE_JOB_DIR/tmp.

## HTTP — Phase A: DONE (2026-06-10). Decisions remain binding:

- **Hand-rolled HTTP/1.1 in `http.rs`** (no reqwest/hyper). GET *and
  POST* — she has a specific application needing POST. Before
  finalizing the POST UX, ask what it needs (content-type? auth
  headers?); meanwhile build the plumbing: `fetch(method, url, body,
  content_type)` plus a `post <url> <body>` command defaulting to
  application/x-www-form-urlencoded.
- **User-Agent: `TRust/0.1`** exactly — "leave them scratching their
  heads".
- **`set webcolors` (html2text css feature): SIDELINED.** Don't build
  it unless she re-raises it.
- **Images: placeholder-only for now** (`[img: alt]` line carrying the
  image URL). A dedicated planning discussion happens before any image
  viewer work — she wants to review SOTA ratatui image options
  (ratatui-image, sixel/kitty protocols, half-blocks) at that point.
- **No JS, ever** — design position like no-SSH, written in README.

Implementation notes (as built):

1. New crates: `url` (HTTP URL parse/join only — gemini/gopher keep
   their hand-rolled resolution), `webpki-roots`, `html2text`. NOT
   flate2/encoding_rs unless the wild forces it (start UTF-8 + manual
   Latin-1).
2. `tls.rs`: second connector `webpki_connector()` with standard cert
   validation against webpki-roots. TOFU is WRONG for the web (90-day
   Let's Encrypt rotation = constant false alarms); keep TOFU strictly
   for gemini/telnets.
3. Request: `GET|POST <path> HTTP/1.1`, `Host`, `Connection: close`,
   `Accept-Encoding: identity`, `User-Agent: TRust/0.1`. Response:
   status line + headers, **chunked transfer decoder** (servers chunk
   regardless of Connection: close), body cap 5 MB for http (small-net
   stays 2 MB). Redirects ≤10: 301/302/303 become GET, 307/308 keep
   method+body; `Location` may be relative (Url::join). http→https
   upgrades fine.
4. `Link::Http(url::Url)` variant (Url is Clone+Eq+Display).
   `gemini::absolute_link` and gopher `h`/`URL:` items return it for
   http(s); External stays for mailto and the rest. Follow → fetch
   task → `Payload::Http(http::Response)`.
5. Render: text/html → html2text *rich* mode → DocLine (headings →
   Kind::Heading, links resolved via base Url::join → Link, pre/code →
   Kind::Pre, images → `[img: alt]` + Link to image URL); other text/*
   → plain lines; else "unsupported media type". Store Content-Type in
   Doc.meta for the resize re-parse (same pattern as gemini).
   CHECK html2text's current API in ~/.cargo/registry source after
   adding — the rich/TaggedLine API has changed across versions.
6. Dispatch: `http://`/`https://` schemes (ports 80/443 imply nothing —
   schemes only, since bare-port heuristics would misfire on dev
   servers). Status codes land in the status bar; 4xx/5xx still render
   their body if HTML (error pages are content).

Phase A landed: http.rs (Request/Response, exchange generic over
TCP/TLS, parse_response + dechunk are pure & unit-tested), Link::Http
(url::Url), html_to_lines maps RichAnnotation→DocLine (single-link
lines selectable directly; multi-link lines emit `→ label` rows;
heading/quote detection via RichDecorator's #/> prefixes; consecutive
wrapped link rows dedup so only the first is selectable). `post <url>
[body]` command (form-urlencoded). Latin-1 done. Verified live against
https://example.com (WebPKI) and a local POST echo.

## HTTP Phase B: HTML forms — DONE (2026-06-11)

Her POST application turned out to be https://rubymaelstrom.com/chat —
an HTML-only LLM chat (hidden session field + text input + Send
button). Forms (GET and POST) are implemented and verified live
against it (Talkie replied through TRust) and against
lite.duckduckgo.com (POST search → results). That closes the standing
"ask her POST app's needs" question.

How it works (and the traps, hard-won):

- `doc.rs`: `Form`/`Field`/`FieldKind` + `Link::Form{form,field}` +
  `Kind::Input`/`Kind::Button`. `Doc.forms` is LIVE state — field
  values/checked are mutated in place; any re-parse must seed from it
  (`http::parse_seeded`). `Form::encode(pressed)` serializes the
  successful fields (only the pressed submit; unchecked boxes stay
  home; nameless fields skipped; value-less checked boxes send "on").
- `http.rs::extract_forms` walks the DOM *between* html2text's
  `parse_html` and `dom_to_render_tree` (the split public API is why no
  extra crate is needed). Each rendering control is replaced by
  `<div><img src="x-trust-form:F.I" alt="[widget row]"></div>` — img
  because its label renders from the alt *attribute*: html2text
  re-exports rcdom's `Element` variant but NOT `Text`, so text nodes
  cannot be fabricated or matched from outside. Element text (button
  labels, options, textarea defaults) is read via `node.serialize` +
  tag-strip (`node_as_dom_string` is a debug dump — don't).
- **rcdom's `Node::drop` force-clears every descendant's `children`,
  live Rc holders or not.** A node spliced out of a snippet DOM must be
  *detached from its parent* before that DOM drops, or it arrives
  empty. This was the silent killer; `marker_node` does the detach.
- Interaction: Enter on a field → the amber prompt (titled INPUT,
  `input>`, prefilled) via `search_target = Link::Form`; checkboxes
  toggle, radios pick within their name group, selects cycle; submit →
  GET builds `action?query`, POST reuses `start_post`. After any value
  change `refresh_forms()` sets `wrapped_to = 0` and calls
  `sync_browser_wrap()` inline (the run loop draws *before* syncing, so
  waiting for the next frame would show stale rows).
- `app.notice`: fetch errors/empty responses set it so the status bar
  shows the message instead of the selected-link hint (cleared on next
  browser key). Without it a dead search engine looks like "nothing
  happened".
- Not supported (deliberately): file uploads, multipart encoding,
  `<button type=button>`/reset (dropped — they're JS hooks).
- FrogFind's search backend was broken on 2026-06-11 (200 + empty body
  for us, 503 for browser UAs) — not a TRust bug; DDG lite is the
  working search demo.

Still later: gzip if the wild forces it. Phase C: image viewer panel
(planning talk first — SOTA ratatui image options). Gemini client
certs if she hits a capsule needing them. Also still on the list:
finger (79), WHOIS (43), DICT (2628) one-shots.

Done since: .gmi files over gopher render as gemtext
(gemini::parse_gemtext takes a resolver closure; gopher::resolve_gmi
maps relative targets to selectors on the same host).

SSH remains an explicit non-goal. JS is now also an explicit non-goal.

## User preferences observed

- She's "sister", not "brother".

- Wants planning discussion *before* implementation on big features;
  small fixes can just be done and verified.
- gopherus is the UX reference for browsing; GNU telnet for terminal
  behavior. When in doubt, match those.
- Values honest caveats (what's stubbed, what's deferred) and live
  demos over claims.
