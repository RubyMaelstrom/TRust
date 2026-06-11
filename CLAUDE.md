# TRust — handoff notes for future sessions

GNU-telnet-parity client + gopher/gemini/text-web browser + image
viewer + finger/whois/dict, in a cyberpunk ratatui TUI. The README
describes features and architecture (read it); this file is how to
work, what not to break, and the traps that already cost debugging
time. As of 2026-06-11 everything in the README's "Status" section is
shipped and live-verified; 70 tests, clippy zero. She commits to git
herself — don't commit unless asked, and don't nag about it.

## Working with her

- She's "sister", not "brother".
- Planning discussion *before* implementation on big features; small
  fixes can just be done and verified. She decides; give a clear
  recommendation and honest trade-offs.
- gopherus is the UX reference for browsing, GNU telnet for terminal
  behavior. When in doubt, match those.
- She values honest caveats (what's stubbed, what's deferred) and live
  demos over claims.
- Her POST/forms application is https://rubymaelstrom.com/chat (an
  HTML-only LLM chat) — the canonical live form test. DDG lite is the
  search demo. Her terminal is foot (sixel, no kitty protocol).

## Quality bar

- `cargo fmt`, `cargo clippy` (zero warnings), `cargo test` — all
  clean before calling anything done.
- Every feature gets BOTH a unit/integration test and a live tmux
  smoke test against a throwaway local server. The tmux run proves the
  UX and has caught real bugs the tests missed.
- Release builds are `cargo build --release` (LTO profile in
  Cargo.toml); plain `cargo build` does not refresh it.

## Live-testing workflow (and its gotchas)

- Fake servers are small Python scripts in `$CLAUDE_JOB_DIR/tmp/`
  (throwaway; recreate as needed). Run the TUI inside tmux:
  `tmux new-session -d -x 90 -y 24 -s name './target/debug/trust ...'`
  then `send-keys` / `capture-pane -p` (`-e` for styling escapes).
- **`cargo test`/`clippy` do NOT rebuild `target/debug/trust`** —
  always `cargo build` before a tmux smoke test (burned us once).
- When the binary may touch ~/.config/trust (pins, identities), launch
  it with `TRUST_KNOWN_HOSTS=... TRUST_IDENTITIES=...` pointed at temp
  paths. Tests must keep setting both env vars too.
- **Never `pkill -f <pattern>` where the pattern appears in your own
  shell command** — it kills your own shell (exit 144). Save PIDs at
  spawn or filter `ps` output. Her machine runs other python3
  (open-webui on :8080, sometimes games) — never kill python3 broadly.
- tmux: multi-word text needs `send-keys -l '...'`; Ctrl-] is
  `send-keys C-]`; shift-keys are `S-Left`/`S-End`; wheel events can
  be injected as SGR sequences (`$'\x1b[<64;10;10M'`, 65 = down). The
  first key sent to a fresh session is sometimes swallowed — send a
  throwaway key or re-send before concluding something is broken.
- **tmux can't show sixel.** Image smoke tests: `set image
  halfblocks`, then assert on `38;2;R;G;Bm` cells in
  `capture-pane -e`. Sixel itself she verifies in foot.

## Architecture invariants (don't break these)

- The app never sees telnet protocol bytes: `telnet.rs` owns the
  parser, speaks to `app.rs` via mpsc channels, and `run_session` is
  generic over TCP/TLS transports.
- Remote output renders through the embedded vt100 emulator. Anything
  a real terminal would *answer* (DSR/CPR, DA, NAWS, TTYPE) we answer
  ourselves; sizes reported are the widget's inner size. One emulator
  per connection — `open()` resets it (stale scroll regions caused a
  nasty bug; regression test exists).
- Documents wrap at *parse* time into real lines. Never use ratatui
  render-time wrapping — it breaks the scroll/selection index math.
  Raw bytes stay on each Doc for re-wrap (resize) and re-decode
  (encoding switch); every protocol's arm in `sync_browser_wrap` must
  keep working.
- TLS: TOFU (`tls::connector`) for telnets + gemini, WebPKI
  (`webpki_connector`) for the web — never mix them. Pins are keyed
  host:port, baked in per connection (the verifier can't see ports).
- RAM-only ethos for session state. TWO approved persistent
  exceptions: TOFU pins (`~/.config/trust/known_hosts`,
  TRUST_KNOWN_HOSTS overrides) and gemini client identities
  (`~/.config/trust/identities/<host>.pem`, TRUST_IDENTITIES
  overrides; cert+key any order; written 0600; NEVER overwritten —
  capsules pin them; presented by `gemini_connector` only, exact-host
  match, never on telnets). Don't add other persistent state without
  asking her.
- Esc is layered: prompt → cancel, viewer/browser → close, line-mode
  session → command mode. In char-mode sessions Esc MUST go to the
  remote — full-screen BBS apps depend on it. (Crossterm quirk: legacy
  terminals deliver Ctrl-] as `Ctrl-5`; both are matched.)
- The run loop redraws only on events; the loading-heart ticker in the
  select is gated on `App::loading()` and must stay gated, or the app
  burns CPU while idle.

## The gopherus navigation model (user-specified, tested)

Up/Down: adjacent link → highlight steps onto it (page scrolls along
so the selection rides the screen center); non-link next line → page
scrolls under the sticky selection; handoff goes to the *next link in
document order* (never skipping) when it's strictly closer to the
center row; no links visible → no highlight; page pinned at either end
→ highlight walks between visible links. Right/Enter follows, Left
pops history (position restored), Esc exits. The 7 `gopherus_*`/rewrap
tests in app.rs ARE the spec — don't "simplify" them.

## Binding decisions

- **User-Agent: `TRust/0.1` exactly.** Hand-rolled HTTP/1.1, no
  reqwest/hyper; no gzip unless the wild forces it.
- **No JS, no SSH, ever** (README states both). `set webcolors`
  sidelined unless she re-raises it. File uploads/multipart
  deliberately unsupported. `parse_port` has deliberately no "ssh".
- Inline images not ruled out forever, but the line-based doc model
  stays. Animated GIF (someday webm) is her "impress me" stretch goal
  — webm needs a video decoder, don't promise it.
- Identity minting prompts for a name (CN matters — astrobotany uses
  it as the username), prefilled with $USER.

## Traps that cost real debugging time

- **html2text re-exports rcdom's `Element` variant but NOT `Text`** —
  text nodes can't be fabricated or matched from outside. Form widgets
  are marker `<img>` nodes (`x-trust-form:F.I` src, label in the alt
  attribute) spliced between `parse_html` and `dom_to_render_tree`.
  Element text is read via `node.serialize` + tag-strip
  (`node_as_dom_string` is a debug dump — don't).
- **rcdom's `Node::drop` force-clears every descendant's `children`,
  live Rc holders or not.** A node spliced out of a snippet DOM must
  be detached from its parent before that DOM drops (`marker_node`
  does this) or it arrives empty. This one was a silent killer.
- `Doc.forms` is LIVE state — re-parses must seed from it
  (`http::parse_seeded`); `refresh_forms()` calls `sync_browser_wrap`
  inline because the run loop draws before syncing.
- Many gemini servers close without TLS close_notify — UnexpectedEof
  during body reads is EOF, not an error. Same tolerance in http.
- `Picker::from_query_stdio()` must run after ratatui::init but BEFORE
  the EventStream exists (it reads stdin). Image decode sniffs magic
  bytes (servers lie) and caps dimensions (decompression bombs);
  encode is a fixed Protocol for the stateless Image widget, work on
  spawn_blocking; Fit does not upscale (deliberate). `ImageView.raw`
  is Arc — `encoded_for` + the img_rx in-flight check prevent
  re-encode storms.
- The status-60 identity prompt routes through `run_search` via
  `cert_for` — it must stay ahead of the `search_target` match, and
  Esc/Ctrl-] must clear it.
- `app.notice` keeps fetch failures visible over the selected-link
  status hint — without it a dead server looks like "nothing
  happened".
- TOFU pins are process-global in tests: don't share a hostname across
  different certs (telnet TLS test owns "localhost", gemini tests own
  "127.0.0.1" — distinct ports keep pins distinct).
- CHECK a crate's actual API in ~/.cargo/registry source after
  adding/upgrading — html2text and ratatui-image both shift.
- WHOIS referrals: `refer:`/`whois:` lines, one hop, host:port form
  honored (that's also how the test picks its port). DICT pipelines
  DEFINE+QUIT so the fetch stays read-to-EOF.

## What's next

- TSPEED (RFC 1079), LFLOW, LINEMODE, NEW-ENVIRON, STATUS-the-option —
  negotiation parity. Test against *real* servers/BBSes, not only
  fakes.
- Remaining GNU command-mode parity: full `set`/`unset`, `display`,
  `logout`, `z`, `!`, `.telnetrc`.
- Aspirational: inline images (planning talk first), animated GIF.
