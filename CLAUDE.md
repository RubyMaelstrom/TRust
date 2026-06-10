# TRust — notes for future sessions

A telnet client (GNU telnet parity) + gopher browser + TLS, cyberpunk
ratatui TUI. The README is the authoritative feature/architecture doc;
this file is workflow, gotchas, and the up-next plan.

## Quality bar

- `cargo fmt`, `cargo clippy` (zero warnings), `cargo test` — all clean
  before calling anything done. Currently 28 tests.
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
- RAM-only ethos: entry histories, scrollback, gopher history, TOFU
  pins — nothing persists to disk yet. First persistent state should be
  the TOFU known-hosts file (see Gemini plan) and is a deliberate,
  user-approved decision, not a default.
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

## Next up: Gemini (agreed plan)

Everything it needs already exists; estimated shape:

1. `gemini.rs` modeled on `gopher.rs`: URL parse (default port 1965,
   default path `/`), one-shot fetch over `tls::connector()` (SNI
   required), request is `gemini://host/path\r\n`, response is
   `<status><space><meta>\r\n` + body.
2. Status codes: 2x render body; 3x follow redirect (cap ~5, re-fetch);
   1x prompt for input via the existing Mode::Search flow (append
   `?query` URL-encoded); 4x/5x show error in status; 6x (client cert)
   out of scope initially — report politely.
3. Gemtext → DocLine: `=>` link lines (link + label), `#`/`##`/`###`
   headings (style by level), `* ` lists, `> ` quotes, ``` toggles
   preformatted (NO wrapping inside pre blocks; everything else wraps
   via the existing push_wrapped pattern — first row carries the link).
4. Generalize the browser: GopherView/GopherDoc/DocLine currently
   carry `GopherUrl` links. Introduce a link enum or trait (gopher item
   vs gemini URL) so one BrowserView serves both; the nav/wrap/history
   code should not fork.
5. Persistent TOFU known-hosts file (first on-disk state — confirm
   location/format with the user; suggest ~/.local/share/trust/).
6. Dispatch: `gemini://` scheme + port 1965; CLI arg already flows
   through `dispatch_open`.

Further out (user-endorsed direction): finger (79), whois (43), DICT
(2628); HTTP(S)+html2text later; SSH is an explicit non-goal.

## User preferences observed

- Wants planning discussion *before* implementation on big features;
  small fixes can just be done and verified.
- gopherus is the UX reference for browsing; GNU telnet for terminal
  behavior. When in doubt, match those.
- Values honest caveats (what's stubbed, what's deferred) and live
  demos over claims.
