# TRust — handoff notes for future sessions

GNU-telnet-parity client + gopher/gemini/text-web browser + image
viewer + finger/whois/dict, in a cyberpunk ratatui TUI. The README
describes features and architecture (read it); this file is how to
work, what not to break, and the traps that already cost debugging
time. She commits to git herself — don't commit unless asked, and
don't nag about it.

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
  HTML-only LLM chat) — the canonical live/non-JS form submission test.
  DDG lite is the search demo. Formatting checks currently use
  https://www.safebooru.org/index.php?page=post&s=list as a reference.
  Her test pages (jstest/expandtest at rubymaelstrom.com) are regression
  fixtures. Her terminal is foot (sixel, no kitty).

## Quality bar

- `cargo fmt`, `cargo clippy` (zero warnings), `cargo test` — all
  clean before calling anything done.
- Every feature gets BOTH a unit/integration test and a live tmux
  smoke test against a throwaway local server. The tmux run proves the
  UX and has caught real bugs the tests missed.
- Release builds are `cargo build --release` (LTO profile in
  Cargo.toml); plain `cargo build` does not refresh it. SHE TESTS THE
  RELEASE BINARY — a feature isn't delivered until release is rebuilt
  (a stale release made working JS look broken once).

## BINDING: maximize web compatibility, never fix to a single site

When refining against a particular site (safebooru, danbooru, archive.org,
SL marketplace, …), the site is a TEST CASE, not the TARGET. Every fix MUST
aim for the broadest correct behavior across the web — fix the platform
primitive (the DOM/CSS/JS/layout rule), not the symptom on that one page.
Her standing rule: "always aim for maximum compatibility across the web
rather than setting too narrow of a target with our fixes." Concretely:
- Diagnose to the GENERAL root cause (a missing platform method, a spec
  behavior we approximate wrong, an engine bug), then fix THAT. The
  danbooru/archive.org wins were all general platform gains, never
  per-host special-casing — keep it that way.
- NO site sniffing, NO `if host == …` branches, NO hard-coded selectors
  or shapes that only make one page look right. If a fix would only help
  the current site, it's the wrong fix — widen it or flag the trade-off.
- Prefer correctness that other sites silently benefit from; verify a fix
  didn't regress the canaries (jQuery/D3/Vue/Lit) and the other reference
  sites before calling it done.
- A site that "renders" via a narrow hack is NOT done; a platform fix that
  makes that site AND its neighbors render is.

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
- **The startup line field runs commands only ONCE**: fresh sessions
  show "Enter run" and execute the first line as a command, then flip
  to "Enter send" — a second chained command silently goes nowhere.
  For multi-command tmux scripts: first command via the line field,
  the rest via Ctrl-] (verify `trust>` appeared before typing, verify
  effects in the status line before proceeding). Burned us twice; a
  demo "bug" turned out to be the demo driver clicking reset.

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
  asking her. (JS storage is RAM-only, session-lifetime — see below.)
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
tests in app.rs ARE the spec — don't "simplify" them. They outrank the
living-page logic; keep them UNTOUCHED.

## Binding decisions

- **User-Agent: `TRust/0.1` exactly.** Hand-rolled HTTP/1.1, no
  reqwest/hyper; no gzip unless the wild forces it.
- `set webcolors` sidelined unless she re-raises it. File uploads/multipart
  deliberately unsupported. `parse_port` has deliberately no "ssh".
- Inline images not ruled out forever, but the line-based doc model
  stays. Animated GIF (someday webm) is her "impress me" stretch goal
  — webm needs a video decoder, don't promise it.
- Identity minting prompts for a name (CN matters — astrobotany uses
  it as the username), prefilled with $USER.
- **`v` in the browser opens the selected link in mpv** (`open_in_mpv`
  in app.rs): handed the link's http(s) URL — mpv plays direct video
  and, via yt-dlp, YouTube/Vimeo/etc. Spawned detached (null stdio so
  it can't fight ratatui; its own window); mpv-not-on-PATH degrades to a
  notice. This is TRust's FIRST external-process delegation; still
  RAM-only/zero-persist. `selected_web_url` extracts the URL (Http
  direct; JsClick/External hrefs resolved against the page, http(s)
  only — foreign schemes like mailto: return None).
- **YouTube links AUTO-LAUNCH in mpv on follow (Enter), in EVERY view.**
  `browser_follow` checks `is_youtube_video_url` first and routes to
  `launch_mpv` instead of navigating — gopher/gemini included (their
  `h`/`URL:` and `=>`-to-http links both become `Link::Http`). Always
  on, no toggle yet (a `set` flag is an easy add if she wants to
  disable). Recognizer matches youtu.be/<id>, youtube.com/watch,
  /shorts/ /embed/ /live/ /v/, on www/m/music/-nocookie hosts. `v` is
  the manual form for ANY web link. The real "render YouTube" target is
  a privacy front-end (Invidious/Piped), not youtube.com — simpler
  markup, links resolve to IDs mpv plays.

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

## The JS engine (the big one — default ON)

Goal: best terminal browser, with real JS for the big web. JS is a
*parse-time DOM transformation*: fetch → html5ever → arena DOM → Boa
runs scripts under budget → serialize post-JS HTML into `Doc.raw` →
existing html2text pipeline. Scripts run ONCE per fetch; resize/re-wrap
never re-runs JS. Default-ON since 2026-06-13 (`set js off` disables
per session); status shows `· JS` / `· JS:n!`. archive.org renders +
SPA-routes in the TUI; jQuery 3.7 / D3 7.9 / Vue 3.5 (render-function)
/ Lit 3 all run.

### Module roles

- **dom.rs** — the arena (`Vec<Node>` + `NodeId`; html5ever TreeSink
  builds straight into it; serializer back to HTML drops
  script/noscript/template/style). NOT rcdom in this path. Also owns
  the selector engine, template-content cloning, and the **CSS cascade**:
  one `PROPS` registry (name/inherited/baked — the single source of truth),
  `cascaded` (author-only winner, for `is_hidden`/baking) and
  `computed_value` (the single inheritance authority — UA defaults +
  registry-driven inheritance, memoized per epoch), plus `text_decoration`
  (propagation/accumulation). See "CSS cascade" below.
- **js.rs** — the syscall boundary and the **ONLY module allowed to
  import `boa_engine`** (keep it that way — it localizes the Boa surface
  to one file). **Boa IS the engine, not a placeholder.** It's the only
  viable pure-Rust JS engine (rquickjs/v8/SpiderMonkey are C/C++ and
  would betray the pure-Rust ethos), so the answer to a Boa showstopper
  is **fork/patch Boa**, not swap it. **WE NOW OWN THAT FORK
  (2026-06-15):** Boa is vendored at `vendor/boa_engine-0.21.1/` and
  pinned via `[patch.crates-io] boa_engine = { path = ... }` (same as the
  crossterm vendor). Her standing call: "Boa is halfway done — improve
  the engine itself instead of working around its edges." Fix Boa bugs in
  the fork; don't add js.rs workarounds. First fix shipped: the
  cyclic-module `Debug` recursion (below). Don't architect around a
  hypothetical engine swap. `page_context()` sets RuntimeLimits
  (10M loop iterations). Two budgets: `COMPUTE_BUDGET` (2s) caps
  cumulative *execution* time (gates launching more scripts —
  measures compute, not wall, so a slow server can't starve a fast
  page) and `WALL_BUDGET` (20s, NETWORK-INCLUSIVE) is the hard load
  deadline enforced as a `tokio::time::timeout` on the async job loop;
  on expiry the future drops at an await boundary and we render
  whatever the DOM holds. `Outcome` tolerates per-script errors,
  counting them for the badge + app.notice. The whole web
  platform is PRELUDE — plain JS over the integer syscalls:
  Node/Element/Document classes, events, virtual-time timers,
  classList/dataset/style, URL/URLSearchParams, atob/btoa, ES modules,
  shadow DOM, `<template>` content, customElements,
  CSSStyleSheet/adoptedStyleSheets, TextEncoder/Decoder, the Intl shim,
  etc. `__dom_*` / `__http_fetch` syscalls take and return integer ids.
  **Why PRELUDE is JS, not Rust** (the real reasons — NOT portability):
  (1) GC — nodes are bare ints in JS-land, so we never wrap the arena as
  Boa GC objects (no `Trace`/lifetime tangle); (2) conciseness — the
  platform written in the language it's specced in is ~1.3K lines vs
  several KB of Boa native-object glue; (3) single source of truth — the
  DOM stays canonical in the Rust arena (what layout reads). **Cost
  (measured, release, `cargo test --release prelude_cost -- --ignored`):
  ~8ms/page (5.9ms parse+compile, 1.9ms run), 65KB.** Negligible — a real
  SPA's own JS is ~5-6s (archive.org); a tiny page pays ~8ms once, only
  if it has a `<script>` at all. NOT worth optimizing: the only lever
  (compile-once cache) is blocked — Boa's `Script` binds to the realm it
  parsed in, and every page needs its own realm.
- **http.rs `execute_js`** — runs `transform` on a dedicated 64MB-stack
  thread (`trust-js`). Fetches external scripts/sheets/module graph,
  then serializes post-JS HTML into `Doc.raw`. **NEVER call transform
  with net set from a runtime thread — `block_on` panics; the dedicated
  thread is the only sanctioned path.**

### Maintainability note (suggestion, not plan)

`app.rs` and `js.rs` are large because they own real orchestration
boundaries. They are still coherent, so don't split them just to make
smaller files. But if future work keeps adding behavior there, consider
compartmentalizing around stable seams: browser/form command handling,
live-page actor protocol/dispatch helpers, and the JS prelude/platform
surface are likely candidates. Treat this as refactoring pressure to
watch, not a roadmap item.

### Caps, storage, limits

- `MAX_PAGE_FETCHES` 96 (PROVISIONAL, her call — archive.org's module
  graph is ~32, full boot 62; 24 cut it off mid-graph),
  `MAX_PAGE_PRELOADS` 96, `MAX_PAGE_SHEETS` 16, `PREFETCH_CONCURRENCY`
  8. `subresource_allowed` / `script_source_allowed` block
  private-address pivots. No CORS theater (cookies are exact-host only; no
  Domain= cross-site/sibling access). **Page fetches run CONCURRENTLY** (see "Parallel fetch"
  below): `fetch()` and async XHR fire `__http_fetch_async` jobs that
  don't block the JS thread, so `Promise.all([...])` overlaps. Async
  XHR still defers `__finish` via `setTimeout(0)` (callbacks are
  macrotasks — promise reactions must run first). Sync XHR
  (`open(...,false)`) keeps the blocking `__http_fetch` syscall.
- Storage: RAM-only, session-lifetime, origin-bucketed local/session
  (`WebStorage` Arc in App). Deviation from "no storage", accepted as
  zero-I/O.
- **Cookies: RAM-only, default ON, exact-host only** (`COOKIE_JAR` in
  http.rs, process-global like `POOL`; `set cookies off` disables capture,
  sends, and `document.cookie` exposure without deleting the in-memory jar).
  We CAPTURE `Set-Cookie` from responses (collected in `read_response`
  before the dedup'ing header HashMap loses the multi-valued ones; stored in
  `finish_response`), expose non-HttpOnly matches to page JS via
  `document.cookie` (`__cookie_get`/`__cookie_set` → `cookies_for_js`
  /`set_cookie_from_js`; `navigator.cookieEnabled` true), and send matching
  cookies back on requests. Privacy invariant (STRICT NO-CROSS-SITE, her
  call — this is the policy FOR NOW and may change going forward): cookies
  are stored in RAM and returned ONLY to the exact host that created them —
  `Domain=` is ignored, so no parent/sibling subdomain access ever. Subset
  of RFC 6265: name=value + Path/Secure/
  HttpOnly/Max-Age(=0 deletes); Domain/Expires/SameSite ignored; HttpOnly
  hidden from JS but still sent, Secure only over https, path matched. This
  is what fixed
  old.reddit (its `getLoIdData` reads the loid cookie unguarded).
  Process-global ⇒ tests must use unique hostnames (like the TOFU pins).
- **CSS cascade (generalized 2026-06-16)**: a real cascade, no longer
  display/visibility-only. Per-property winner = lexicographic max of
  (!important, inline, specificity, source order); `hidden` attr wins
  outright. Sources in cascade order: tree sheets (`<style>` + fetched
  `<link>`) in composed document order, then adoptedStyleSheets — SCOPED by
  tree root (document vs shadow fragment). Selector engine: `:not(compound)`,
  attr operators (`~= |= ^= $= *=`), per-Complex specificity,
  paren/quote-aware comma splitting; querySelector shares it.
  `:hover`/`:focus`/unknown pseudos parse but NEVER match (fail-open); an
  unparseable member kills its whole rule (also fail-open).
  - **The property surface is the `PROPS` registry** (one table; `is_tracked`
    + the serializer bake-list derive from it). Adding a property = one row.
  - **Inheritance is general**: `computed_value(id,prop)` is the SINGLE
    authority — author cascade, else the UA default for the tag
    (`ua_default`: `<b>/<strong>`→bold, `<i>/<em>`→italic, `<pre>`→pre,
    `<ul>`→disc/circle/square by depth, `<ol>`→decimal or its `type` attr),
    else (for `inherited` props) the parent's computed value. Memoized per
    epoch (`computed_cache`). The layout reads font-weight/font-style/
    white-space/text-align/text-transform/list-style-type through it; the old
    `layout.rs` emphasis/ws THREADING IS GONE. `text_decoration(id)` is
    separate (it PROPAGATES and accumulates `<u>`+`<s>`, `none` resets — not
    inheritance). UA tag defaults that aren't inherited CSS (block/inline
    display, `<a>`, heading sizing) stay in the layout/serializer.
  - **`list-style-type` (Phase 2)**: inherited + baked; `list-style`
    shorthand expands to it. The layout's `next_list_marker(li)` reads
    `computed_value` and renders the marker — `none` (no marker), disc/circle/
    square glyphs, or a formatted counter (`format_list_marker` +
    `alpha_marker`/`roman_marker`: decimal/decimal-leading-zero/lower|upper-
    alpha/lower|upper-roman). `list_stack` is now `Vec<u32>` (every level
    counts); `<ol start>` seeds it, `<li value>` resets it.
    `list-style-position`/`-image` not done.
  - **`gap`/`column-gap`/`row-gap` + `justify-content` (Phase 2)**: tracked
    (baked, not inherited). `flex_gap(id, avail, row_axis)` resolves the
    longhand or the `gap` shorthand component (defaults: 1 cell between
    columns for readability, 0 between rows/shelves) — used by `flow_flex_row`
    and `flow_flex_wrap` (row-gap spaces shelves). `justify_offsets` does
    `justify-content` main-axis distribution in `flow_flex_row` (flex-end/
    center/space-between/around/evenly) when grow didn't eat the free space;
    grid `justify-content` and `align-items` (~N/A in our 1-row item model)
    not done.
  - **getComputedStyle is cascade-backed**: `__dom_computed` → `computed_value`
    (read-only proxy; inline fallback). NOT inline-only anymore.
  - **Lengths**: `css_length_em` is the one context-free unit parser
    (em/rem/px/pt + `ch`=1 cell); `resolve_cells`/`resolve_calc` the one
    contextual resolver (`%` vs containing block, `vw` vs `viewport_w`,
    `calc()`-lite = `+`/`-` chain; `*`/`/`→ignored). `vh`/`vmin`/`vmax`
    DEFERRED (layout carries no viewport height).
  - **`@media` (Phase 1.5, shipped)**: `parse_sheet` evaluates `@media`
    against `Dom.viewport_px` (set by `execute_js` from `PageEnv` =
    `cols*cell_px`) and splices matching rule bodies into the cascade.
    Supports min/max-width, width, min/max-height, height, orientation,
    `screen`/`all`/`print` type, `and`/comma/`not`/`only`; px + em(16px)
    lengths. Conservative: an unknown feature, or a width/height test with an
    unknown viewport (`0`), DOESN'T match (drops the body, == the old skip).
    **JS-pipeline only** (the serializer drops `<style>`, so it's evaluated
    once at load + baked; a resize crossing a breakpoint needs a reload — her
    call). `@media` in JS-off pages / `min-resolution` / `prefers-*` not
    evaluated.
  - LIMITS: visibility treated like display (subtree hides; no
    visible-child-of-hidden-parent); sibling combinators `+`/`~` unparsed;
    `:host` ignored; other @-rules skipped; NO color (her call — cyberpunk
    consistency stays); rides the JS pipeline (`set js on` + script-bearing
    pages only).
- **Intl**: prelude shim (Boa's `intl_bundled` measured and rejected —
  +11MB ICU, ~2x binary, and HALF-BUILT: `DateTimeFormat().format`
  throws and `DisplayNames` isn't a constructor, both used by
  archive.org). Shim covers NumberFormat, DateTimeFormat (ISO-ish
  English), Collator, DisplayNames (returns the code), PluralRules,
  RelativeTimeFormat, getCanonicalLocales. en-only;
  `resolvedOptions`/`supportedLocalesOf` everywhere so detection
  passes; Number/Date `toLocaleString` route through it.
- **crypto** (2026-06-14): `getRandomValues`/`randomUUID` (Math.random
  — no CSPRNG, fine for ids/keys not real crypto) + `crypto.subtle.digest`
  (real pure-JS SHA-1 + SHA-256; rest of SubtleCrypto still absent).
  Libraries that hash before they fetch now work.
- **Layout-gated SPAs / virtualization** (2026-06-14): three measurement
  changes so infinite-scrollers and responsive layouts render. (a) The
  reported CSS-pixel viewport is the REAL terminal size: `viewport (cols,
  rows) * cell_px` where `cell_px` = the picker's font size, threaded
  app→`execute_js`→`PageEnv.cell_px` (8x16 nominal in tests). Her call:
  cells-to-px so a wide terminal looks like a wide browser, for ALL sites
  not just one. (b) `getBoundingClientRect`/`client|offsetWidth|Height`
  return the viewport box (were 0) — a heuristic (we have no JS-side
  layout), enough for scrollers that gate cell rendering on a non-zero
  measurement. (c) `ResizeObserver` fires once with the viewport rect
  (was a no-op), like `IntersectionObserver`. Regression-clean: canaries
  (jQuery/D3/Vue/Lit), old.reddit, safebooru all still render; 202 tests.
- archive.org collection pages now RENDER their collection UI + tile grid
  (2026-06-14): bugs fixed — (1) shadow-DOM upgrade gap below, (2) missing
  `crypto.subtle.digest` (collection search builds a SHA-1 request `uid`
  before fetching tiles; without digest the fetch threw and the grid
  stayed empty), (3) the measurement changes above (the infinite-scroller
  rendered ~0 cells against a 24px viewport). net_diag LIVE: 200
  `item-tile`s + 106 imgs in the post-JS body; in-app the filters sidebar
  + search/sort UI + the ~98-cell grid render. REMAINING (budget, not a
  bug): tile CONTENT (titles/thumbs) population RACES WALL_BUDGET (20s) —
  the bigger real-terminal viewport makes the scroller request more tiles,
  so it sometimes serializes mid-"Searching…" with placeholder cells.
  Consistent tile content needs a budget bump or a faster search-render
  path — her call (archive.org is a known landmine; don't grind it).

### JS traps (learned the hard way)

1. **Boa's parser recurses on the native stack** — big bundles
   (archive.org) overflow the 2MB tokio blocking thread (= process
   abort, uncatchable). Hence the dedicated 64MB `trust-js` thread.
2. **`with(this)` USED to panic Boa's VM** ("must be declarative
   environment") — Vue's in-browser template compiler emits it. FULLY
   FIXED IN THE FORK now (2026-06-15): simple interpolation first (Codex,
   the runtime band-aids), then `v-for` via the parser fix below — and
   the band-aids were DELETED once the parser fix subsumed them.
   ROOT CAUSE of the `v-for` failure: `boa_ast`'s scope analyzer numbers
   scopes by *lexical nesting depth*, then `optimize_scope_indicies`
   (`scope_analyzer.rs` `ScopeIndexVisitor`) RE-INDEXES them to match the
   envs the VM actually pushes (collapsing elided/all-local scopes).
   `Script`/`Module::analyze_scope` ran that pass; `FunctionExpression::
   analyze_scope` (the `new Function` path) did NOT — so a dynamically
   compiled function kept the naive indices, a `with` inside it left every
   nested binding's locator one env too high, and a `v-for` callback
   pushing its own env on top landed the stale index in the wrong env
   (`{{ x }}` happened to be top-of-stack so the old clamp masked it).
   THE FIX (`vendor/boa_ast-0.21.1`): `FunctionExpression::analyze_scope`
   now runs `optimize_function_scope_indicies`, a function-aware variant
   that FORCES the root function's scope slot (matching the engine's
   `force_function_scope`; a naive `optimize_scope_indicies` would instead
   elide that slot and OOB-define the function's own captured `const`).
   Verified: the now-un-ignored Boa test
   `closure_in_with_captures_block_binding_through_parameter`, the always-run
   `vue_v_for_style_render_closure_runs` (js.rs), the
   `vue_v_for_template_compiler_canary` (real Vue 3.5 bundle → `<li>`s), and
   a live tmux smoke (release) rendering a `v-for` list with a clean `· JS`
   badge. The trap-#2 runtime band-aids — the `environment_expect` clamp,
   `declarative_ref_at_or_below`, and the `CreateMappedArgumentsObject`
   fallback — are GONE; `mapped_arguments_inside_with_environment` and
   `vue_template_render_function_with_body` still pass without them. NOTE:
   real-world Vue usually PRECOMPILES templates (ship render fns, which
   always worked — the canaries); runtime template compilation was the
   affected path. `run_script` still catch_unwinds as a safety net for any
   OTHER VM panic.
3. **Class getters without setters throw in strict mode** — every
   commonly-assigned Element property needs a setter (jQuery does
   `input.type = "radio"`).
4. jQuery requires `document.implementation.createHTMLDocument`
   OUTSIDE try/catch.
5. Raw strings holding JS need `r##` — selectors contain `"#`.
6. **Boa 0.21 VM panics (define opcode, OOB binding slot) when a
   closure capturing a block-scoped (`const`/`let`) class-constructor
   local is invoked from a NATIVE callback** (Array#sort found it;
   direct calls and `var` capture are fine). Prelude rule: constructor
   locals captured by callback-bound closures use `var` (see
   Intl.Collator). 2026-06-15: an `Element.attributes` getter built with
   `names.map(n => ({ get value(){ return …el…n… } }))` re-tripped this —
   `.map` is the native callback, the getters captured the `const el`.
   ABORTED archive.org's live page (release is panic=unwind so it's
   "caught", but a corrupted VM still tanks the page → looks like a crash).
   Rewrote with a plain `for` loop, snapshot values, and `this`-based
   `item`/`getNamedItem` (no captured-local closures). RULE OF THUMB: in
   the prelude, don't create closures that capture an outer block-scoped
   local INSIDE a native callback (`.map`/`.forEach`/`.sort`/getters).
   SAFETY NET (main.rs, 2026-06-15): a panic hook SWALLOWS panics on the
   `trust-*` worker threads (they're already `catch_unwind`-sandboxed) so a
   Boa VM panic from a REAL PAGE's own code degrades the page instead of
   dumping a backtrace over the ratatui screen. (Trap #6 itself is now
   FIXED in the fork — see "What's next"; this safety net still guards any
   OTHER engine panic.) A genuine main-thread panic still restores the terminal +
   prints. The ·JS:n! badge/notice is how the user learns JS degraded.
   2026-06-15 archive.org HOME crash — REAL FIX SHIPPED via the Boa fork
   (replacing Codex's blunt import-count cap, now DELETED). Root cause
   (gdb-traced): a dynamic `import()` of a statically-imported specifier
   makes Boa's `assert_eq!(entry, loaded)` (`module/source.rs:420`) fire;
   building that panic message `Debug`-formats the `Module`, whose Debug
   recursed through the CYCLIC `loaded_modules` graph → stack overflow
   *during panic formatting* = an UNCATCHABLE abort. Fixed in the fork:
   `Module` (`module/mod.rs:68`) and `SourceTextModule` (`source.rs:230`)
   `Debug` impls now print non-recursive identity only (addr/path/requested
   specifiers) and never borrow `loaded_modules` (the assert holds it
   `borrow_mut`). The assert is now an ORDINARY panic → caught by
   `run_module`'s `catch_unwind` + the trust-* panic hook → graceful
   degrade, on ANY cyclic-module site. BOTH crude caps removed from js.rs
   (`MAX_SAFE_MODULE_IMPORTS` and `MAX_SAFE_MODULE_PRELOADS` +
   `note_unsafe_module_graph`/`module_static_import_count`); regression
   `wide_module_fanout_runs_instead_of_falling_back` (30 imports / 31
   preloads → runs) replaces `oversized_module_fanout_falls_back_to_original`.
   Live: `https://archive.org/` no longer aborts. (Then **trap #6** — the
   define-opcode binding-slot abort — was ALSO fixed, see "What's next";
   archive.org HOME now boots fully and renders its homepage UI.)
7. **Template cloning must propagate `template_contents`**
   (`clone_subtree`/`transplant`) — webcomponents-loader.js probes
   exactly this; failing it makes the loader declare us IE-grade.
8. **A missing platform method = ONE "not a callable" rejection.** This
   USED to be stackless (hence the mirror-harness/console-probe bisection
   below); the Boa fork now attaches a real `.stack` (2026-06-15), so
   `app.notice`/`outcome.errors` name the offending script + line:col —
   READ THE STACK FIRST, the mirror harness is the fallback. Silent-killers
   since fixed:
   `Element.toggleAttribute`, the ChildNode mixin
   (`replaceWith`/`before`/`after`/`replaceChildren`), anchor URL
   components (`a.pathname` via `__urlPart`), `<base href>` resolution
   (`baseHref()`), `new MyElement()` constructing registered custom
   elements, `attachInternals`, `Event.composedPath`,
   TextEncoder/Decoder, both serializers dropping `<style>`,
   `Element.attributes` (Alpine morphs via `Array.from(el.attributes)`).
9. **`customElements.define()`'s catch-up upgrade must PIERCE SHADOW
   ROOTS** (2026-06-14). It used `document.querySelectorAll(name)`
   (light-DOM only). A custom element rendered into a shadow tree
   BEFORE its definition — archive.org's router does this for the
   late-loaded page component — was then never upgraded: constructed
   never, rendered never, the element sat empty
   (`<collection-page></collection-page>`). ceScan already crosses
   `__sr`; define() now uses a shadow-piercing `ceUpgradeName` walk.
   This single bug was why the whole archive.org collection page was an
   empty shell.
10. **Host objects MUST carry a `Symbol.toStringTag`** (2026-06-14).
    `window` is `g[Symbol.toStringTag]="Window"`, Document gets
    `"HTMLDocument"`. Without them `Object.prototype.toString.call(window)`
    is `"[object Object]"`, so a deep-merge/clone that uses an
    isPlainObject gate (jQuery UI's `widget.extend`) treats `window` as a
    plain object and follows `window.window` / `document.defaultView`
    forever → Boa recursion-limit (512) trip. This was danbooru bug #2 of
    a chain (see below). Add tags to any future host object that
    self-references.

### danbooru.donmai.us (FIXED, 2026-06-14/15)
Its post grid is SERVER-rendered (`<div class="posts-container">` with ~20
`<article class="post-preview">`), but danbooru re-renders it client-side
(Alpine.js morph) and was dying on a CHAIN of three missing-platform errors,
each masking the next — leaving the grid empty. All three fixed (general
wins): (1) missing `DOMException`; (2) host-object `Symbol.toStringTag`
(toString-call cycle, trap #10); (3) **missing `Element.attributes`** — Alpine
does `Array.from(el.attributes)`, which was `Array.from(undefined)` → ToObject
throw inside a jQuery.Deferred (stack-swallowed). Added a NamedNodeMap-like
`attributes` (array-like via `__dom_attr_names`, live `.value`). Now net_diag
LIVE /posts: errors:[] and the grid renders (~18 `<article>` + 22 imgs, was
3/0/4). Diagnosis trick for the stackless ToObject error: Boa doesn't fill
`.stack` even on throw-catch, so wrap the ToObject builtins (Object.keys/
entries/getPrototypeOf/…, **Array.from**) to log the offender on null/
undefined → caught `Array.from(undefined)` instantly. (Aside: timer errors now
append `e.stack` like the rejection tracker.)
LAYOUT (2026-06-15): danbooru's `.posts-container` is `display:grid`, and
`layout::flex_mode` only matched `flex`/`inline-flex` → grid fell to block =
ONE column. Now `grid`/`inline-grid` map to `FlexMode::Grid` (shelf-packed
flex-wrap; template tracks ignored — the documented approximation). The grid
now wraps into columns sized to the terminal.

### Diagnostics

- **Mirror harness** (`$CLAUDE_JOB_DIR/tmp/mirror.py` pattern): cache
  bundles, STUB env to stub modules, python text-patch to wrap suspect
  methods with `console.log` probes. console → `Outcome.console` —
  that's the diagnostic channel (Boa gives rejections no `.stack`,
  so this bisection IS the tool; budget an hour, not a re-architecture).
- `net_diag` / `js_diag` ignored tests fetch + diagnose any URL/file;
  `TRUST_NET_DIAG_OUT=<path>` dumps full post-JS HTML;
  `TRUST_JS_DIAG=<file>`; `TRUST_NET_TRACE=1` prints per-request timing.
- Canary bundles in `target/canary/` (see js.rs doc comment):
  `cargo test --release canary -- --ignored --nocapture`; `lit_canary`
  / net_diag / js_diag likewise.
- Regression tests for the platform surface:
  `webcomponents_loader_feature_probes_pass`,
  `spa_router_platform_surface_works`,
  `intl_shim_formats_honestly_and_passes_feature_detection`,
  `keep_alive_reuses_connections`, the dom CSS tests.

### Page-load speed (it's the wire, not Boa)

Diagnosed with `TRUST_NET_TRACE=1`: archive.org's ~62 subresource
fetches were strictly SERIAL on FRESH `Connection: close` sockets
(~500ms DNS+TCP+TLS even for a 91-byte file); Boa compute is only
~5-6s. The fix (both in http.rs) took archive.org ~44s → ~15s:

- **Keep-alive connection pool** (`POOL`, RAM-only, keyed
  scheme/host/port; `Conn` enum wraps plain or TLS behind one
  AsyncRead/Write face). Idle TTL 30s, ≤8 idle/key, newest-first
  reuse. **REUSE IS GET-ONLY** with up to 2 silent retries — a pooled
  socket can be server-closed while idle, and the recovering re-dial
  must never double-submit, so POST always dials fresh. `exchange`
  returns a "safe to pool" bool; truncation/missing close_notify =
  keep the bytes, don't reuse.
- **Concurrent prefetch** in `execute_js`: external scripts +
  stylesheets + the announced module graph (`<link rel=modulepreload>`
  + module entry srcs) fetched `buffered(8)`, which KEEPS LIST ORDER
  so execution/cascade order is preserved regardless of arrival.
  Sum-of-latencies → max-of-latencies for the announced graph.
### Parallel fetch (the JS-driven fetches now overlap too)

The remaining slow tail was the JS-driven runtime fetches a router
issues as it mounts: `__http_fetch` did `handle.block_on(http::fetch)`
AT CALL TIME, so every `fetch()`/XHR blocked the single JS thread —
`Promise.all([fetch,fetch])` couldn't overlap (no core pegged, just
serial network waits). Fixed (all in js.rs):

- **`fetch()`/async XHR → `__http_fetch_async`**: synchronous
  precondition checks (`page_net_prepare`), then a pending `JsPromise`
  + a `Job::AsyncJob` (`NativeAsyncJob`) that `await`s `http::fetch`
  WITHOUT borrowing the context (borrow only after, to settle the
  promise). They fire immediately and overlap. Sync XHR keeps the
  blocking `__http_fetch`/`page_net_fetch`.
- **Custom `PageJobExecutor`** (the ONLY change from the bundled
  `SimpleJobExecutor`): it **parks** on `group.next().await`
  (`FuturesUnordered`) instead of busy-polling (`poll_once`+`yield_now`).
  The stock executor spins a core the whole time a request is
  outstanding — unshippable. `run_jobs_into` drives it via
  `handle.block_on(timeout(budget.remaining(), exec.run_jobs_async))`
  so tokio reactors the concurrent sockets; no-net pages take the
  plain `ctx.run_jobs()` path.
- The module loader (`load_imported_module`, already `async fn`) now
  `await`s its fetch instead of `block_on` (block_on nested inside the
  job-loop's block_on = "runtime within a runtime" panic). Sibling
  module fetches overlap too.
- TRAPS (regression-tested): (1) the busy-poll executor — never go
  back to `SimpleJobExecutor` for net pages. (2) Don't hold a `RefCell`
  borrow across `.await` (clippy catches it; the preloaded-cache take
  must be its own statement). (3) Async XHR delivers `__finish` via
  `setTimeout(0)` — its callbacks are macrotasks, so promise reactions
  (e.g. a fetch `.catch`) must run first; making XHR a microtask
  reorders them (`without_a_net_grant_fetch_rejects_and_xhr_errors`
  pins this). (4) Scripts still run sequentially with a job-drain
  between each; overlap is WITHIN a script (Promise.all) and during
  settle — not across classic `<script>` tags (matches the platform).
- Proof: `page_fetches_run_concurrently` (12 fetches @120ms: 1.56s
  serial → ~216ms). Live (release, tmux): 16 page fetches all hit a
  local server within ~20ms of each other (NB: a test server's listen
  backlog — Python `socketserver.request_queue_size` defaults to 5 —
  will fake a ~7-concurrency ceiling; raise it or you'll chase a
  phantom cap). jQuery/D3/Vue/Lit canaries unchanged.
- STILL TRUE: ~5-6s of archive.org's time is irreducible Boa compute;
  fetches a router issues strictly sequentially (await chains) still
  can't be parallelized by anyone — only the pool helps those. But
  any concurrency the page expresses now actually happens.

## The living document (JS Phase 2b — in flight)

Goal: a displayed page keeps its engine + DOM alive; user actions
dispatch real DOM events; the page re-renders from the mutated arena.
Design agreed in full 2026-06-12 — **deviations need a conversation,
not a commit.**

### The page actor (SHIPPED)

A living page is a **dedicated resident thread owning the Context +
arena** for the page's lifetime (the 64MB `trust-js` thread, promoted
from one-shot). The app talks to it over channels exactly like a
telnet connection task: `PageCmd` in (Click, SetValue, Submit),
`PageEvt` out (Updated{html, outcome-delta}, Static, Navigate(url),
Trouble, Settled, SubmitDefault). The app NEVER touches engine
internals — same invariant as "the app never sees protocol bytes".
`blocking_recv` = zero idle CPU; auto-Static exit when a page has no
clickables or forms (articles never hold an engine). ONE live engine
ever (the foreground page); navigating away
**freezes** it (final serialized HTML → the Doc.raw history stores; the
history entry remembers it was live via `ViewPos.was_live`); Esc kills
it. **REVIVE-ON-BACK (2026-06-15):** going back to a was-live http(s)
page (JS on) RELOADS it in place (`browser_back` → `replace_nav` +
`start_fetch`) so links/forms work again, instead of a dead snapshot;
the frozen doc shows while it reloads. Pays a full JS reload — her call
(load times are usually short / improving). Non-live or js-off entries
restore static instantly as before. Budgets are
**per-dispatch** (~1s + wire-time extension); a Boa panic degrades the
page to last-good static + the `· JS:n!` badge.

### Invariants from step 1 (binding, her requirements)

- **Dirty bit**: every mutating syscall sets it; idempotent
  set_text/set_attr (same value) stay dirty-free (counters rewriting
  the same value cost nothing). Serialize once per dispatch/timer
  batch, never per mutation. No mutation → no Updated; the actor sends
  `Settled` only to clear the app's busy state.
- **Coalesce Updated**: parse only the newest snapshot per event-loop
  turn (parse is the cost, not the draw); drop intermediates unparsed.
- **Selection/scroll stability**: selection is re-found by its *target*
  (x-trust-js node id / href / form field id) in the re-extracted doc,
  fallback nearest line; scroll kept and clamped.
- **Click markers**: the serializer wraps clickables in
  `<a href="x-trust-js:<id>:<orig-href>">` → `Link::JsClick{node, href}`
  (Display shows the href so previews stay readable). Clickable =
  `<button>`, input[type=button|submit], summary, [role=button],
  [onclick], or any node in the prelude's listener registry. Delegation
  containers are detected (a subtree-holding-interactives is a listener
  host, not a button); html/body are never wrapped. `onclick` (and
  friends) compile to listeners LAZILY at first dispatch, cached by
  source text, `return false` = preventDefault. Live anchors
  (self-or-ancestor listens, delegation included): un-prevented click
  → `PageEvt::Navigate(resolved URL)`; `javascript:`/`#` never navigate.
- **`el.style` is backed by the real style attribute** (writes are DOM
  mutations — dirty-bit, serialized, idempotent-write-free); both
  serializers honor the DOM's visibility primitives (`hidden` attr,
  inline display:none / visibility:hidden via `Dom::is_hidden`).
  getComputedStyle is now CASCADE-backed (`__dom_computed` →
  `computed_value`: inheritance + UA defaults for tracked props, inline
  fallback), read-only — see "CSS cascade".

### Shipped in this session (2026-06-14)

- **Step 2 forms shipped**: editing a live form field now sends
  `PageCmd::SetValue`, mutates the resident DOM, fires `input` and
  `change`, and re-renders from the actor snapshot. Submit sends
  `PageCmd::Submit` and dispatches a real `submit` event; if page JS
  calls preventDefault, the page owns the update, otherwise
  `PageEvt::SubmitDefault` tells the app to run the existing GET/POST
  submit path. Live forms stay resident even without click listeners.
- **Step 3 script navigation shipped**: `location.href = ...`,
  `window.location = ...`, `location.assign(...)`, `replace(...)`, and
  `reload()` now queue a script navigation that the page actor emits as
  `PageEvt::Navigate` after click/form dispatch and timer settling.
  Same-document hash changes stay in the live page, update `location`,
  and fire `hashchange` with `oldURL`/`newURL` so hash-router SPAs can
  update in place.
- **Resize-at-rest shipped**: the interactive run loop no longer
  re-wraps browser documents on every resize event. It arms a 200ms
  one-shot sleep only while a browser wrap target is pending, resets it
  as the target changes, and calls the existing immediate
  `sync_browser_wrap()` only once the terminal rests. Telnet NAWS still
  renegotiates immediately via `sync_vt_size()`; direct reparse callers
  (`refresh_forms`, tests, image relayout) still use the immediate
  primitive.
- **HTTP mouse interactions shipped**: mouse hover over an HTTP laid-out
  clickable item updates the highlighted target; a single left click
  activates the target through the same path as Enter (links follow,
  form text fields enter the edit prompt, buttons/forms dispatch their
  existing action). Scope is deliberately **HTTP laid-out docs only**;
  gopher/gemini keep the gopherus keyboard/scroll selection model
  untouched. Mouse wheel behavior is unchanged. Linked decoded images
  are clickable across their full reserved cell box, not only the top
  row; hit-testing scans upward for tall interactive items. 2026-06-15:
  Mouse4 (browser-back thumb button) now triggers browser history back,
  same as Backspace, for all browser docs. Crossterm 0.29 did not expose
  side buttons, so the project patches it via `vendor/crossterm-0.29.0`
  to surface Mouse4/Mouse5 from SGR button codes 8/9. (THREE vendored
  forks as of 2026-06-15: `vendor/crossterm-0.29.0`,
  `vendor/boa_engine-0.21.1`, and `vendor/boa_ast-0.21.1` — the last two
  are the JS front+back of Boa, see the JS engine section. All pinned in
  `[patch.crates-io]`. NOTE: a `[patch]` only applies in the root TRust
  manifest, so building/testing `boa_engine` standalone uses the REGISTRY
  `boa_ast`, not our vendored one — boa_ast changes must be driven through
  TRust.)
- **TRAP pinned**: live serializer writes `data-trust-node` on
  form/control elements because the app re-parses snapshots into a
  fresh layout DOM; never use layout parse node ids as actor node ids.
  On live updates, `replace_live_doc` parses with **no form seed** —
  the actor DOM is truth. Static resize/edit reparses still seed from
  `Doc.forms` as before.
- **Regression coverage added**: actor tests for input/change,
  prevented submit, default submit fallback, script navigation via
  click/form dispatch, and same-document hash routing; app tests for
  prompt edits through the page actor, prevented submits avoiding HTTP
  fetch, HTTP mouse hover, single-click link/form activation,
  full-height linked-image click activation, and a guard that mouse
  hover leaves gopherus selection alone.
  `a_mutationless_click_emits_nothing` now expects `Settled` rather
  than silence.
- **Live smoke**: debug tmux runs against throwaway local HTTP servers
  proved a hash-router link updates in place (`route:#route`), an
  `onclick` calling `location.assign('/next.html')` navigates to the
  destination page, HTTP mouse click on a text input opens `input>` with
  the current value, HTTP mouse click on a link navigates to
  `next.html`, and a lower-row click inside a multi-row linked image
  follows that image link. 2026-06-15 Mouse4 smoke: debug tmux
  against a two-page local HTTP server, Enter to page two, injected
  `\x1b[<128;10;10M`, returned to page one.

### Still to build

- **Timers frozen at rest** (requirement): after the load settle,
  timers advance ONLY during dispatches (each drains due timers under
  its budget). Click→fetch→update works; idle clocks/polling
  deliberately don't (no idle CPU — the redraw-only-on-events invariant
  holds). Background ticking is a possible later opt-in — don't add it
  unprompted.
- Later: Phase 3+: MutationObserver (stub class exists, records nothing).

## Telnet negotiation parity

TSPEED/NEW-ENVIRON/STATUS/LFLOW shipped (`suboption_reply` in
telnet.rs; live-verified against vert.synchro.net, which really asks
for TSPEED and NEW-ENVIRON). Three decisions baked in: NEW-ENVIRON
answers an empty IS — nothing from the local environment ever goes on
the wire; TSPEED claims 38400,38400; STATUS replies come from the
task's own `OptionStates`, NOT `parser.options` — libmudtelnet marks an
option "remotely enabled" whenever we merely accept a DO, so its table
lies.

## What's next

### Shipped 2026-06-16 (CSS engine generalization — Phase 1, 4 steps)
Audit-first plan (she approved): generalize the cascade so adding CSS "just
works" instead of nibbling edges. **NO COLOR** (her call — cyberpunk
consistency stays; color/background OFF). **Ratatui is the renderer** for any
future visible prop (Style/Modifier/box-drawing, not bare terminal). All
UNCOMMITTED (she commits), fmt/clippy-0, 237 tests, release-smoked in tmux.
1. **One property registry** (`PROPS`, dom.rs): `TRACKED`/`BAKE_PROPS`/
   `is_tracked` collapsed into one `name`/`inherited`/`baked` table.
2. **Full inheritance unification** (she chose this over foundation-only): UA
   defaults moved INTO the cascade (`ua_default`); `computed_value` is the
   single inheritance authority (memoized `computed_cache`); `text_decoration`
   models propagation/accumulation. The `layout.rs` emphasis/white-space
   THREADING IS DELETED — layout reads the cascade. 7 gopherus + all layout
   tests stayed green.
3. **getComputedStyle cascade-backed**: `__dom_computed` syscall →
   `computed_value` (read-only proxy, inline fallback). Was inline-only.
4. **Unified length resolver** (layout.rs): `css_length_em` (one unit parser,
   +`ch`) + `resolve_cells`/`resolve_calc` (one contextual resolver: `%`,
   `vw` vs `viewport_w`, `calc()`-lite). `vh`/`vmin`/`vmax` deferred.
See the "CSS cascade" section above and the trust-project-status memory.
**Phase 1.5 = `@media` ALSO SHIPPED** (same day): `parse_sheet` evaluates
`@media` vs `Dom.viewport_px` (JS-pipeline only, baked at load) — details in
the "CSS cascade" section. **Phase 2 IN PROGRESS**: `list-style-type` SHIPPED
(de-bullets nav `list-style:none`, alpha/roman/decimal, depth-varying nested
disc/circle/square, `<ol type|start>`/`<li value>`); **`gap`/`column-gap`/
`row-gap` + `justify-content` SHIPPED** (flex/grid spacing + main-axis
distribution) — both in "CSS cascade". NEXT Phase 2: border→box-drawing
(ratatui — the showpiece, its own design beat), text-indent. SKIP bucket (visual-only): gradients, box-shadow,
border-radius, filters, transforms, z-index.

### Shipped 2026-06-15 (later — the Boa fork: 4 engine fixes)
We FORKED Boa (her call: "no half-steps — improve the engine itself, not
its edges"). Vendored `vendor/boa_engine-0.21.1` + `[patch.crates-io]`.
**js.rs stays the only crate-importer; fix engine bugs IN the fork.**
Four landed, all release-verified, fmt/clippy-0, 211 tests:
1. **Cyclic-module `Debug` recursion → catchable panic** (`module/mod.rs`,
   `source.rs` Debug impls print non-recursive identity). Killed the
   uncatchable archive.org abort. Both crude module caps
   (`MAX_SAFE_MODULE_IMPORTS`, `MAX_SAFE_MODULE_PRELOADS`) DELETED.
2. **Module rejections report the real reason** (`js.rs` `run_module` +
   shared `describe_rejection`) — was a generic "module rejected" while the
   Debug recursion made formatting dangerous; now safe.
3. **Errors carry a JS-readable `.stack`** (`error.rs` `JsError::to_opaque`).
   Boa already captured a 50-frame backtrace on throw (`Display` rendered
   it) but DROPPED it when materializing the JS object — the root of the
   "opaque, stackless rejection" pain. Now attached V8-style at the single
   boundary `catch` and promise rejections share. **The mirror-harness /
   console-probe bisection is largely OBSOLETE — read the stack.**
4. **TRAP #6 FIXED** (`bytecompiler/class.rs`): the define-opcode
   binding-slot abort. A class constructor pushed its function scope
   UNCONDITIONALLY, but `boa_parser` assigns binding scope-indices assuming
   an all-local, non-required function scope is ELIDED (as regular
   functions do) — so a `const`/`let` in the ctor body captured by a
   closure resolved to the empty function env (0 slots) and the define
   opcode wrote OOB. Fix: the ctor now honors the SAME conditional-push
   rule `FunctionCompiler::compile` uses. **archive.org HOME went from an
   empty 1872B shell (`js:None`) to a full JS boot (panicked:false, 62
   fetches) that RENDERS the homepage UI in-app.** Verified across base/
   derived(super)/new.target/arguments/nested-block constructors with
   correct values. Regression: `class_constructor_const_captured_by_closure_does_not_panic`.
   The prelude `var`-not-`const`-in-constructors rule (trap #6 workaround)
   is RETIRED — const/let in prelude constructors is safe now (existing
   `var` usages left as-is; harmless).

Tests added: `wide_module_fanout_runs_instead_of_falling_back`,
`thrown_errors_carry_a_js_readable_stack`,
`class_constructor_const_captured_by_closure_does_not_panic` (cap test
dropped). See the boa-cyclic-module-debug-crash memory for the running
fork log.

### Shipped 2026-06-15 (the Boa fork: trap #2 `with` — NOW COMPLETE, v-for too)
**Trap #2 (`with(this)` VM panic) is FULLY fixed.** Two stages: (1) Codex
killed the uncatchable abort + got simple interpolation running via runtime
band-aids (`declarative_ref_at_or_below` + an `environment_expect` clamp +
a `CreateMappedArgumentsObject` fallback). (2) Sister fixed `v-for` at the
PARSER (the real root cause) and DELETED those band-aids.
ROOT CAUSE (recap): `boa_ast`'s scope analyzer numbers scopes by lexical
nesting depth, then `optimize_scope_indicies` (`scope_analyzer.rs`
`ScopeIndexVisitor`) RE-INDEXES to match the envs the VM actually pushes.
`Script`/`Module::analyze_scope` ran it; `FunctionExpression::analyze_scope`
(the `new Function` path) did not, so dynamic functions kept naive indices
and a `with` inside one left every nested binding one env too high — masked
for top-of-stack `{{ x }}`, fatal for a `v-for` callback that pushes its own
env (`get/name.rs` OOB read).
THE FIX (`vendor/boa_ast-0.21.1`, ~25 lines): a new
`optimize_function_scope_indicies` (sibling of `optimize_scope_indicies`)
that sets a one-shot `force_function_scope` flag on the visitor;
`ScopeIndexVisitor::visit_function_like` consumes it (`std::mem::take`, so
only the ROOT function is forced — nested fns keep escape-analysis) and
reserves the function's stack slot to match the engine's
`force_function_scope`. `FunctionExpression::analyze_scope` calls it after
the escape pass. A naive `optimize_scope_indicies(self, scope)` would NOT
work — it elides the forced slot and OOB-defines the function's own captured
`const` ("len 0 index 0"); forcing is the whole trick.
BAND-AIDS DELETED: the `environment_expect` clamp, `declarative_ref_at_or_below`,
and the mapped-arguments fallback are reverted to plain `expect`; the parser
fix subsumes them.
VERIFIED: full boa suite **861** green (the repro
`closure_in_with_captures_block_binding_through_parameter` un-ignored), boa_ast
6, `mapped_arguments_inside_with_environment` +
`vue_template_render_function_with_body` still pass band-aid-free; TRust **213**
+ new `vue_v_for_style_render_closure_runs` (always-run) and
`vue_v_for_template_compiler_canary` (real Vue 3.5 bundle → `<li>alpha…`);
jQuery/D3/Vue/Lit canaries `errors:[]`; live tmux (fresh release binary)
renders a `v-for` list with a clean `· JS` badge. fmt/clippy-0.
GATING NOTE: a `[patch]` only applies in the root TRust build, so the boa
suite was run via a TEMPORARY `[patch.crates-io] boa_ast = { path = .. }` in
the boa_engine vendor manifest (REMOVED after — don't leave it).
NEXT fork candidate: efficiency (prelude compile-once, modest payoff — measure
first).

### Shipped 2026-06-15 (handoff summary for Codex)
Big-web rendering push, all uncommitted (she commits), fmt/clippy clean,
207 tests, live-verified in tmux. Five general JS-platform gains + two
site fixes + revive-on-back:
- **archive.org collections render** (was an empty shell): shadow-DOM
  custom-element upgrade gap (define() pierces shadow now), real
  `crypto.subtle.digest` (SHA-1/256; the search uid), and the
  measurement changes below. net_diag LIVE: 200 tiles + 106 imgs in the
  DOM; in-app the filters/search/grid render.
- **CSS-pixel viewport = real terminal size** (`viewport × cell_px`,
  cell_px = picker font size, `PageEnv.cell_px`); non-zero
  getBoundingClientRect/client*; ResizeObserver fires once. Lets
  layout-gated SPAs/virtualization render. Her cells-to-px call.
- **danbooru post grid renders** (was 1 col of nothing): `DOMException`,
  host-object `Symbol.toStringTag` (deep-merge cycle), `Element.attributes`
  (Alpine morph), and `display:grid`→flex-wrap (was block = one column).
- **Panic-hook safety net** (main.rs): swallows caught `trust-*` worker
  panics so a real page's own Boa-VM bug degrades the page instead of
  dumping a backtrace over the TUI (archive.org's bundle trips trap #6).
- **Revive-on-back**: back to a was-live page reloads it live.

OPEN / next candidates:
- **Boa fork SHIPPED 2026-06-15** (`vendor/boa_engine-0.21.1` +
  `vendor/boa_ast-0.21.1`, `[patch.crates-io]`): 6 engine fixes — cyclic-module
  Debug, module rejection text, error `.stack`, **trap #6**, and **trap #2
  `with(this)` NOW COMPLETE incl. `v-for`** (the boa_ast
  `optimize_function_scope_indicies` parser fix; runtime band-aids deleted) —
  see the "Shipped (the Boa fork)" sections above and the
  boa-cyclic-module-debug-crash memory.
- **Efficiency (prelude compile-once)**: modest payoff — measure first.
- **archive.org tile CONTENT vs structure**: the grid scaffolds but tile
  titles/thumbs race WALL_BUDGET (20s) at the bigger real viewport —
  budget/perf, not a bug.
- JS Phase 2b follow-up: timer semantics remain deliberately frozen at
  rest; future work is platform depth such as MutationObserver and any
  opt-in background ticking decision. Live form integration, script
  navigation, hash routing, resize-at-rest, and HTTP mouse interactions
  shipped 2026-06-14.
- LINEMODE (RFC 1184) — the last negotiation-parity gap. Test against
  *real* servers/BBSes, not only fakes.
- Remaining GNU command-mode parity: full `set`/`unset`, `display`,
  `logout`, `z`, `!`, `.telnetrc`.
- Aspirational: inline images (planning talk first), animated GIF.
