# TRust — notes for future sessions

A telnet client (GNU telnet parity) + gopher browser + TLS, cyberpunk
ratatui TUI. The README is the authoritative feature/architecture doc;
this file is workflow, gotchas, and the up-next plan.

## Quality bar

- `cargo fmt`, `cargo clippy` (zero warnings), `cargo test` — all clean
  before calling anything done. Currently 58 tests.
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
  Multi-word text needs `send-keys -l '...'` — without `-l` it
  arrives mangled or not at all. Shift-modified keys: `S-Left`,
  `S-End`, etc.

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
- The run loop redraws only on events. Animations (the loading heart)
  are driven by an interval in the select gated on `App::loading()` —
  it must stay gated, or the app burns CPU redrawing while idle.

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

## HTTP (Phases A+B+C all DONE) — binding decisions

- Hand-rolled HTTP/1.1 in `http.rs` (no reqwest/hyper, no gzip unless
  the wild forces it). **User-Agent: `TRust/0.1` exactly.**
- WebPKI (`tls::webpki_connector`) for the web; TOFU strictly for
  gemini/telnets (cert rotation would make web TOFU cry wolf).
- **No JS, ever** (README design position). `set webcolors` sidelined
  unless she re-raises it. File uploads / multipart deliberately
  unsupported. Inline images not ruled out forever, but the doc model
  keeps its line-based invariant. Animated GIF (someday webm) is the
  "impress me" stretch goal — webm needs a video decoder, don't
  promise it.
- Her POST application is https://rubymaelstrom.com/chat (HTML-only
  LLM chat) — the canonical live form test. DDG lite is the search
  demo (FrogFind's backend is unreliable).

Traps that cost real debugging time (don't rediscover these):

- **html2text re-exports rcdom's `Element` variant but NOT `Text`** —
  text nodes can't be fabricated or matched from outside. Form widgets
  are therefore marker `<img>` nodes (label rendered from the alt
  *attribute*): `<div><img src="x-trust-form:F.I" alt="[row]"></div>`,
  spliced in between `parse_html` and `dom_to_render_tree`. Element
  text is read via `node.serialize` + tag-strip (`node_as_dom_string`
  is a debug dump — don't).
- **rcdom's `Node::drop` force-clears every descendant's `children`,
  live Rc holders or not.** A node spliced out of a snippet DOM must
  be detached from its parent before that DOM drops (`marker_node`
  does this) or it arrives empty.
- `Doc.forms` is LIVE state — re-parses must seed from it
  (`http::parse_seeded`); `refresh_forms()` must call
  `sync_browser_wrap()` inline because the run loop draws before
  syncing.
- `app.notice`: fetch failures show in the status bar over the
  selected-link hint until the next key — without it a dead server
  looks like "nothing happened".
- CHECK a crate's current API in ~/.cargo/registry source after
  adding/upgrading — html2text and ratatui-image both shift APIs.

Image viewer (`src/img.rs` + `App.viewer`):

- `ratatui-image` default-features OFF (avoids the `chafa-dyn` C
  dependency) + `image` with png/jpeg/gif/webp only. All pure Rust.
- `Picker::from_query_stdio()` runs in main.rs after ratatui::init but
  BEFORE the crossterm EventStream exists — the stream would eat the
  query replies. Fallback/tests: `Picker::halfblocks()`; main installs
  the queried picker via `set_picker` (records `auto_protocol` for
  `set image auto`).
- `decode` sniffs magic bytes (servers lie about content types) and
  caps dimensions 12k×12k (`image::Limits` decompression-bomb guard).
  `encode` → fixed `Protocol` for the *stateless* `Image` widget (not
  StatefulImage/ThreadProtocol — our channels already offload); work
  runs on `spawn_blocking` → `img_rx`. **Fit does not upscale** —
  small images render 1:1, deliberately.
- Viewer sits OVER the browser (state untouched beneath);
  Esc/Left/Backspace/q close; `navigate_to` closes it. `ImageView.raw`
  (Arc<[u8]>, RAM-only) re-encodes on resize/protocol switch;
  `encoded_for` + the img_rx in-flight check prevent re-encode storms.
- Routing: http `image/*` (+ octet-stream that sniffs as image),
  gopher I/g/p items, gemini `image/*` meta.
- **tmux can't show sixel.** Smoke tests force halfblocks and assert
  on `38;2;R;G;Bm` cells in `capture-pane -e`; sixel verified by her
  in foot (confirmed 2026-06-11).

Done since: .gmi files over gopher render as gemtext
(gemini::parse_gemtext takes a resolver closure; gopher::resolve_gmi
maps relative targets to selectors on the same host).

Still on the list: finger (79), WHOIS (43), DICT (2628) one-shots;
gemini client certs if she hits a capsule needing them. SSH and JS
remain explicit non-goals.

## User preferences observed

- She's "sister", not "brother".

- Wants planning discussion *before* implementation on big features;
  small fixes can just be done and verified.
- gopherus is the UX reference for browsing; GNU telnet for terminal
  behavior. When in doubt, match those.
- Values honest caveats (what's stubbed, what's deferred) and live
  demos over claims.
