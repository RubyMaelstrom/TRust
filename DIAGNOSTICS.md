# TRust diagnostics and diagnostic inputs

This is the central index for TRust's developer diagnostics. It consolidates
the environment variables and command-line inputs that are otherwise described
in module comments and ignored tests. It is an engineering aid, not part of the
normal browser UI.

The source remains authoritative when a diagnostic's output or defaults change.
When adding or changing an input, update this file and the nearby source
comment/example in the same change.

## Conventions

* Presence-only flags are enabled by setting the variable to any value. `=1` is
  used in examples. They are disabled when unset.
* Value inputs are normally parsed once at process or page-thread startup. Set
  them before launching TRust or the test process; changing the environment
  while it is running is not reliable.
* Most site and bundle diagnostics are `#[ignore]` tests. They are intentionally
  opt-in because they use a network, a local capture, or a large external
  input. Run them from the repository root with `cargo test --release ...
  -- --ignored --nocapture`.
* Release binaries use Lumen by default. The `src/js.rs` diagnostics belong to
  the explicit legacy Boa backend (`trust-boa`); they are not linked into the
  normal Lumen binaries. HTTP, layout, terminal, and most frontend diagnostics
  are shared unless noted otherwise.
* Diagnostics print to stderr unless a variable explicitly names an output
  file/directory. Use a local or authorized test endpoint; avoid repeatedly
  probing public sites.

## Quick-start commands

Trace a normal Lumen page load and its network requests:

```sh
TRUST_NET_TRACE=1 TRUST_LUMEN_TRACE=1 target/release/trust https://example.test/
```

Trace terminal redraws, page events, layout, and image work:

```sh
TRUST_DIAG_FRAME=1 target/release/trust https://example.test/
```

Run the live browser-workload gate against a controlled page:

```sh
TRUST_BROWSER_GATE=https://example.test/ \
  TRUST_BROWSER_GATE_EXPECT_HTML_CONTAINS='ready' \
  cargo test --release browser_workload_gate -- --ignored --nocapture
```

Run the network diagnostic against a URL:

```sh
TRUST_NET_DIAG=https://example.test/ \
  TRUST_DIAG_SETTLE=1 \
  cargo test --release net_diag -- --ignored --nocapture
```

## Command-line inputs

### `trust`

The terminal browser accepts `trust [URL-or-host] [port]`. Its interactive
prompt commands (`open`, `post`, `reload`, `mode`, `send`, `set`, `toggle`, and
`status`) are documented in the [Driving it section of `README.md`](README.md).

### `trust-headless`

`trust-headless --help` is generated from the usage string in
[`src/bin/trust-headless.rs`](src/bin/trust-headless.rs):

| Input | Meaning | Default |
|---|---|---:|
| `URL` | Required initial navigation | — |
| `--width N` | CSS viewport width in CSS pixels | `1024` |
| `--height N` | CSS viewport height in CSS pixels | `768` |
| `--timeout SECS` | Hard wall-clock limit for navigation/settling | `10` |
| `--settle SECS` | Quiet-period fallback, used only on a page that never goes inert (see below) | `0.75` |
| `--max-chars N` | Character budget for page text (`unlimited` for the whole page) | `8000` |
| `--format text\|semantic` | Display-list text or accessibility tree output | `text` |
| `--links` | Include link targets in text output | off |
| `--js-diagnostics` | Print the last page-script outcome to stderr: JS errors, captured console lines, panic flag, skipped modules, and page fetch count (`[js-errors]`/`[js-console]`/`[js-outcome]` blocks) | off |
| `-h`, `--help` | Print usage | — |

#### How `trust-headless` decides it is finished

Waiting is the interesting part of a one-shot dump, and there are two very
 different kinds of wait. The driver prefers the engine's own verdict, reached
through `BrowserController::page_render_is_final`, and only falls back to a
clock when the engine declines to answer:

| Verdict | Meaning | Reached when |
|---|---|---|
| engine final | No further render can arrive for this document | The fetch committed with no resident actor (a script-free page, Gopher, Gemini, internal gemtext), **or** the actor classified the document inert and sent `PageEvt::Static`, **or** the fetch failed and no document committed |
| quiet | The page stopped changing; not the same as finished | A live actor never went final and `--settle` elapsed since the last revision |
| timeout | The dump is incomplete | `--timeout` expired before either of the above |

Every run prints which one it got in its closing stderr line, so the
difference between a verdict and a guess is never lost:

```console
$ trust-headless http://127.0.0.1:8199/plain.html 2>&1 | tail -1
trust-headless: http://127.0.0.1:8199/plain.html · … · 1024x768 CSS px · the engine reports this render is final in 27ms (ok)
$ trust-headless --settle 0 --timeout 10 http://127.0.0.1:8199/timers.html 2>&1 | tail -1
trust-headless: http://127.0.0.1:8199/timers.html · … · 1024x768 CSS px · timed out after 10s without the engine calling it final in 10018ms (INCOMPLETE)
```

`--settle 0` disables the clock entirely and trusts the engine verdict alone; a
page with a timer loop then waits for `--timeout`. The default `--timeout` is 10 s
because a dozen real pages settled between 0.4 s and 4.4 s measured here, and a
page that never goes quiet should fail in 10 s rather than in half a minute.

The middle field of that line is what the protocol answered — `HTTP 404
(text/html)`, `Gemini 200 text/gemini`, `1024 bytes` — taken from the fetched
document, not from the controller's status. The two are different things: the
controller's status becomes a resident script actor's own note the moment it
repaints, so a server's verdict disappears behind `Page updated · JS` in any
page that runs script. Measured after that change: a wrong-cased GitHub repo
reports `HTTP 404 (text/html)` in 3.0 s, plain text reports `HTTP 200
(text/plain)`, and Python's own error page reports `HTTP 404 (text/plain)`.

The closing line is prose for a human; the **exit status** is the machine-readable
answer, and it separates "the fetch worked" from "the dump is the whole story":

| Exit | Meaning |
|---:|---|
| `0` | Complete dump. A page answering 404 or 500 is a complete dump: the fetch succeeded, and what the server said is in the line above for the caller to judge |
| `1` | No page loaded — DNS failure, refused connection, unparsable address |
| `2` | Bad command line |
| `3` | Text was dumped but the page never went quiet or final before `--timeout`, so what was printed is as-of-timeout, not the whole page |

Page text is bounded: `--max-chars` defaults to 8000 characters, because an
8000-character budget is the size a fetch tool is expected to hand back and
`text/plain` documents have no natural limit — RFC 2616 arrives as 422,102
characters. A budget never ends the text mid-line when a line broke in the last
quarter of it, and the closing line says `text capped at 8000 of 422102
characters` when the budget bit. `--links` output is not counted against the
budget; ask for it explicitly and it arrives in full.

The engine verdict is worth roughly an order of magnitude on documents that can
reach it — a script-free page used to pay the whole settle window for no reason,
and measured about 0.05 s instead of about 1.3 s. A failed or unparsable
address likewise now returns in tens of milliseconds instead of idling out the
timeout.

One caveat deserves prominence because it decides most real-world behavior:
`Static` requires *no* hover or scroll work, and its hover clause asks whether
the page merely **styles** `a:hover` in a way that can change its render. So a
page that only wants to recolor a link on hover keeps its actor resident
forever, and `page_render_is_final` answers false even though nothing further is
pending. Those clauses state why the engine must stay *reachable*, not why it
must keep *painting*; a driver that wants to stop waiting is asking the painting
question. Until the actor answers the two separately, hover-styled pages fall
back to the quiet period — measured: script-free settles in tens of
milliseconds, a page with a dead script and no hover styling in about 0.16 s,
and one dead script plus `a:hover { color: red }` never.

Residency is not the same thing as cost, and the two are worth measuring
separately before anyone tries to fix the first. Sampling `/proc` on the GB10
workstation while the window-free driver held pages open:

| question | measurement |
|---|---|
| does a resident page burn cycles? | no: `trust-page-lume` recorded **0.000 s** of CPU over 30 s idle. The ~0.6% that shows at all is shared Tokio pool housekeeping, present whether or not a page is resident |
| does residency accumulate across navigation? | no: five chained `location.href` navigations, every page hover-styled so no actor retires, left **one** `trust-page-lume` thread |
| does a resident page's memory grow? | no: flat at **200.4 MiB** at 5 s and at 45 s |
| what does the live DOM plus realm actually cost? | the same page with its one `:hover` rule takes **133.7 MiB**, the same page without it **84.2 MiB**, and a 20 000-object author heap adds **6 MiB** on top |

So residency is one parked thread and roughly 50 MiB, charged to the single
page on screen — the terminal and `trust-desktop` each hold one
`BrowserController`, and `drop_live_page()` reclaims it on the next fetch —
which is a bill, not a leak. The interesting number in that table is the last
one: a document with **no scripts at all** pays a script realm because the live
page is spawned whenever `hover_css_affects_rendering()` is true, so one
stylesheet rule is enough to buy a realm. Collapsing that would mean
separating live selector state from the realm, and the risk surface of that
change is hover itself; the reward is one page's worth of RAM.

#### How `trust-headless` reads a page

Text output is built from the display list, so it reports what was painted and
nothing else: text hidden behind a collapsed section, or behind `display:none`,
never appears. Reading it needs two decisions, and getting either wrong mangles
prose.

Runs are grouped into blocks in the order the page drew them, which for in-flow
content is document order, and each visual line inside a block is read left to
right. Sorting runs by `y` alone does not work. docs.python.org paints the
classifier of its heading 3 px above the word it classifies (`— Data Classes` at
`y=149.3` against `dataclasses` at `152.3`), so the classifier sorts first,
every run to its left is measured as though it continued rightward from there,
the gap comes out negative, and the heading arrives as
`— Data Classesdataclasses`. Reordering *within* a line and never across lines
fixes it without inventing a reading order for the page as a whole.

Two ratios decide where a visual line ends, both measured against the line
height rather than fixed pixels so they hold at any font size or device pixel
ratio. Word spaces on real pages come out around **0.2** line-heights; gaps
between unrelated boxes on one line around **3** or more.

| gap between boxes | read as |
|---|---|
| within half a pixel | the same text, no separator |
| up to `COLUMN_GAP_LINES` (1.5 line-heights) | a word space |
| wider than that | a separate line: a second column or an unconnected widget |
| boxes overlapping by more than a fifth of a line | separate lines, never one sentence |

The last row exists because keras.rstudio.com paints a screen-reader-only label
on the logo at the same baseline, which previously read as one word.

`--links` reports every link target the page painted, resolved to an absolute
address against the address that actually answered — `/comments/` on
`example.com/blog/post.html` is reported as `https://example.com/comments/`, and
a fragment as `https://docs.python.org/3/library/dataclasses.html#mutable-default-values`,
because a caller that has to guess where it was reading from cannot use either.
Buttons and form controls report nothing, since they have no address.

Two limits are worth stating. Relative references resolve against the document
URL, not a `<base href>` override: the engine keeps that resolution to itself, and
inventing a second implementation in a driver is how the two drift apart. And
`--format semantic` reports no link targets at all, because the accessibility tree
records a `value` only for form fields; links in that format would need the
engine to carry `href` into `SemanticNode`, not a driver-side guess from geometry.

### `trust-desktop`

`trust-desktop --help` accepts `[--renderer=auto|cpu|hybrid] [URL]`.
`TRUST_DESKTOP_TRACE` and the desktop benchmark inputs are listed below.

### Developer replay and spike binaries

These binaries are opt-in targets and do not open the normal browser UI:

| Binary and input | Meaning | Default |
|---|---|---:|
| `trust-browser-replay [--warmups N] [--samples N] [--external NAME=PATH] [--sheet NAME=PATH] FIXTURE.html [...]` | Deterministically replays one or more local HTML fixtures through the shared Lumen browser pipeline. `--external` and `--sheet` may be repeated to provide named local script/style resources. | warmups `1`, samples `5`; at least one fixture required |
| `trust-lumen-spike [--tier interp\|bytecode\|jit] [--threshold N] [--benchmark PATH]` | Runs the synthetic Lumen/`js-engine-benchmark` harness and prints timing, GC, event-loop, and score data. Requires the `lumen-spike` Cargo feature. | tier `jit`, threshold `0`, benchmark `/usr/share/cry/benchmarks/js-engine-benchmark/run.js` |

Both binaries support `-h`/`--help`. `trust-boa` and
`trust-desktop-boa` are the same terminal/desktop inputs built with the
explicit legacy Boa backend.

## Live runtime diagnostics

### Cross-frontend and terminal diagnostics

| Input | Value | Effect and output |
|---|---|---|
| `TRUST_NET_TRACE` | presence flag | Adds timestamped request/subresource timing to stderr. The `net:` lines use one shared millisecond origin; DOM mutation and JavaScript phase markers may use the same timeline. |
| `TRUST_DIAG_FRAME` | presence flag | Enables the terminal frame report (`DIAGFRAME`) and related page/layout reports (`DIAGGEOM`, `DIAGROUTE`, `DIAGRELAY`). It includes redraws, draw time, page-event work, full replacements, image relayout/render counts, load phases, scroll state, and cascade/layout costs. |
| `TRUST_DIAG_PATCH` | presence flag | Adds timing output for incremental region/subtree layout patches. |
| `TRUST_DIAG_SCROLL_BOXES` | presence flag | Makes the HTTP path use the one-shot transform for inner-scroll-box investigation instead of retaining a resident actor. Diagnostic-only; pair it with the current scroll/geometry output when investigating nested scrollers. |
| `TRUST_NO_FRAME_SKIP` | presence flag | Disables the terminal's identical-frame suppression and restores the always-draw path. Useful for A/B measurements of redraw behavior. |
| `TRUST_DUMP_RAW` | directory path | Writes each live serialized HTML render as `render_<timestamp-or-sequence>.html` for offline replay/diffing. The directory must already exist. Used by both the terminal app and shared browser controller. |
| `TRUST_LAYOUT_TRACE` | presence flag | Prints graphical layout stage timing from `layout2::lay_out_graphical`. |
| `TRUST_FRAG_DIAG` | presence flag | Dumps the resolved graphical fragment tree (tag, position, size, and clip). Used with `layout_dump` for layout/paint discrepancies. |
| `TRUST_PANIC_LOG` | file path | Appends every panic, including background-thread panics, with thread name, terminal-owner status, message, and forced backtrace. The normal terminal panic hook remains separate. |
| `TRUST_TRACE_PAGE_EVENTS` | presence flag | Prints a `[trace-event] <variant>` line to stderr for every page event the shared controller handles (`Updated`, `Static`, `Patched`, `Trouble`, navigation/settle events). Useful for proving which render path a page reached and in what order. |

### Lumen diagnostics

| Input | Value | Effect and output |
|---|---|---|
| `TRUST_LUMEN_TRACE` | presence flag | Logs Lumen script start/completion, JavaScript errors, console messages, and unhandled rejection details to stderr. |
| `TRUST_LUMEN_TASK_TRACE` | presence flag | Emits a once-per-second resident page-actor task census: turns, commands, interactions, host/platform/timer/lifecycle work, render passes, updates, finishes, and queue state. |
| `TRUST_LUMEN_PROBE` | JavaScript source | Evaluates the supplied expression/source in the resident page after a task and prints its value, throw, interruption, or parse error. This is a diagnostic probe, not page content. |
| `TRUST_PRELUDE_FILE` | file path | Replaces the embedded platform prelude (`js_platform.js`) with the given file's contents for the process, read once on first use. Intended for rapid iteration on prelude code: edit `src/js_platform.js`, set this to that path, and the *next page load* picks the change up — no rebuild or relink is needed. The override is read in the Lumen backend at prelude evaluation time, independently of `__trust_cfg`. Release/shipped builds keep the embedded prelude; leave this unset. An unreadable path falls back to the embedded prelude. |
| `TRUST_TRACE_FRAMES` | presence flag | Requests the prelude's gated frame-flow trace, emitted as `FT ...` console lines (`log: FT …` under `--js-diagnostics`): frame hydration sweeps (root + frame count), `processIframeAttributes` src and fetch outcome, queue-task dispatch, `loadFrameMarkup` entry (url + markup size), `finishParsedFrameLoad`, and `runFrameScripts` found-script counts. The trace flag travels through `__trust_cfg.frameTrace`, so it also works for worker/standalone contexts where the prelude reads the cfg. |
| `TRUST_TRACE_FETCH` | presence flag | Logs every page fetch to stderr as `[fetch-trace] sync|async <METHOD> <url>`, followed by `[fetch-trace] result status=<N> type=<TYPE> len=<N>` when the response resolves through the unbuffered result path. Pair with `TRUST_LUMEN_TRACE` and `--js-diagnostics` to reconstruct a page's script/fetch timeline. |

### WebSocket diagnostics

| Input | Value | Effect and output |
|---|---|---|
| `TRUST_WS_DIAG` | file path | Appends WebSocket handshake, open/close, error, and frame diagnostics to this file. A file is used because stderr can race task/process shutdown. |
| `TRUST_WS_DIAG_CAP` | non-negative integer | Maximum payload bytes echoed per frame in the WebSocket diagnostic. Default: `300`. |

## Network and browser-workload test inputs

All tests in this section live in [`src/http.rs`](src/http.rs). The test name is
the final argument in each example.

### Common inputs

| Input | Value | Effect and default |
|---|---|---|
| `TRUST_NET_DIAG` | absolute URL | Primary URL for `net_diag`, `diag_all_errors`, `form_fill_submit_diag`, `img_box_diag`, and `wpt_diag`. Required by those tests. |
| `TRUST_NET_DIAG_OUT` | file path | Writes the newest or final post-JavaScript HTML snapshot to this file when supported by the diagnostic. |
| `TRUST_DIAG_VP` | `WIDTHxHEIGHT` | Synthetic viewport in terminal cells for network diagnostics. Defaults vary by test: `80x24` (`net_diag`), `120x40` (`click_diag`), and `200x50` (`diag_all_errors`, `img_box_diag`, browser gate, and WPT). |
| `TRUST_DIAG_COOKIE` | `name=value[; name2=value2]` | Seeds the process cookie jar before the cold fetch. This is useful for controlled authenticated/cookie-gated pages. |
| `TRUST_DIAG_INJECT` | JavaScript file path | Inserts the file's source into a `<script>` at the beginning of `<head>` before page scripts run. |
| `TRUST_DIAG_SETTLE` | presence flag | Drains post-shell live-page `Updated` events so SPA mounts and later errors are included. |
| `TRUST_DIAG_DRAIN` | integer | Maximum number of settle events to drain when `TRUST_DIAG_SETTLE` is enabled. Default: `6`. |
| `TRUST_DIAG_DRAIN_TO` | seconds | Timeout for each settle-event receive. Default: `20`. |

### `net_diag`

```sh
TRUST_NET_DIAG=https://example.test/ \
  cargo test --release net_diag -- --ignored --nocapture
```

The basic fetch/JavaScript diagnostic reports the response, JavaScript outcome,
live/static classification, and a bounded initial or settled HTML snapshot.
Add `TRUST_DIAG_SETTLE=1` for a live SPA's post-shell state. It also accepts
`TRUST_DIAG_CLICK=<DOM-node-id>` to dispatch a click and
`TRUST_DIAG_SCROLL=<count>` to send repeated bottom-directed scroll commands.

| Input | Value | Effect and default |
|---|---|---|
| `TRUST_DIAG_CLICK` | numeric DOM node id | Dispatches a click through the resident actor, then reports navigation, updates, and errors. |
| `TRUST_DIAG_SCROLL` | integer count | Sends that many scroll commands toward the document bottom. Invalid values fall back to `5`. |
| `TRUST_DIAG_SCROLL_STEP` | floating-point CSS-pixel distance | Multiplier for each scroll step. Default: `100000`. |

### `click_diag`

```sh
TRUST_CLICK_DIAG=https://example.test/ \
  TRUST_CLICK_TEXT='Open' \
  TRUST_CLICK_PROBE='id="dialog"' \
  cargo test --release click_diag -- --ignored --nocapture
```

`TRUST_CLICK_DIAG` is the URL (distinct from `TRUST_DIAG_CLICK`, which is a
node id). The harness finds an element by link text, clicks it through the live
actor, and checks whether a probe substring disappears.

| Input | Value | Effect and default |
|---|---|---|
| `TRUST_CLICK_DIAG` | absolute URL | Required target URL. |
| `TRUST_CLICK_TEXT` | text substring | Link/click target text. Empty means no primary click. |
| `TRUST_CLICK_PROBE` | HTML substring | Probe checked before/after the click. Default: `id="disclaimer"`. |
| `TRUST_CLICK_WAIT` | seconds | Click/post-click wait. Default: `8`. |
| `TRUST_CLICK_RETAIN` | HTML substring | Optional assertion that the post-click snapshot retains this marker. |
| `TRUST_CLICK_FIND2` | HTML/tag substring | Optional second click target, resolved from the newest snapshot. |
| `TRUST_SET_FIND` | HTML/tag substring | Optional editable element to locate before clicking. |
| `TRUST_SET_VALUE` | text | Value sent to the element found by `TRUST_SET_FIND`. |
| `TRUST_EVT2_DUMP` | directory path | Writes progressive second-click snapshots as `evt2-<n>.html`. |

### `form_fill_submit_diag`

```sh
TRUST_NET_DIAG=https://example.test/login/ \
  TRUST_FORM_SUBMIT_TEXT='Sign in' \
  cargo test --release form_fill_submit_diag -- --ignored --nocapture
```

Loads a login-style page, fills the first text and password inputs, clicks the
matching submit button, and reports enablement, events, errors, and final HTML.
`TRUST_FORM_SUBMIT_TEXT` defaults to `ログイン`. `TRUST_NET_DIAG_OUT` may save
the final snapshot.

### `diag_all_errors`

```sh
TRUST_NET_DIAG=https://example.test/ \
  TRUST_DIAG_VP=200x50 \
  cargo test --release diag_all_errors -- --ignored --nocapture
```

Drains the full live settle and reports the accumulated unique JavaScript
errors/stacks rather than only load-time errors. It accepts the common
`TRUST_NET_DIAG`, `TRUST_DIAG_VP`, `TRUST_DIAG_SETTLE`, and
`TRUST_NET_DIAG_OUT` inputs.

### `browser_workload_gate`

This is the strongest live-site acceptance harness. It checks actor
responsiveness, optional named-control activation, fatal errors, HTML
milestones, semantic-control disappearance, DOM size, and site-specific
completion checks for YouTube, Twitch, Steam, and Speedometer.

```sh
TRUST_BROWSER_GATE=https://example.test/ \
  TRUST_BROWSER_GATE_CLICK='Start' \
  TRUST_BROWSER_GATE_EXPECT_HTML_CONTAINS='summary' \
  TRUST_BROWSER_GATE_EXPECT_CONTROL_GONE='Start' \
  cargo test --release browser_workload_gate -- --ignored --nocapture
```

| Input | Value | Default/effect |
|---|---|---|
| `TRUST_BROWSER_GATE` | absolute URL | Required page under test. |
| `TRUST_BROWSER_GATE_SECONDS` | integer seconds | Initial/final milestone deadline. Default: `45`. |
| `TRUST_BROWSER_GATE_CLICK` | accessible-name substring | Activates the first exposed activatable control whose accessible name contains this text. |
| `TRUST_BROWSER_GATE_CLICK_SECONDS` | integer seconds | Deadline after the named click. Default: `20`. |
| `TRUST_BROWSER_GATE_EXPECT_HTML_CONTAINS` | HTML substring | Required final HTML milestone; for interactive pages it is also the default initial milestone. |
| `TRUST_BROWSER_GATE_EXPECT_INITIAL_HTML_CONTAINS` | HTML substring | Optional separate pre-click milestone. |
| `TRUST_BROWSER_GATE_EXPECT_HTML_NOT_CONTAINS` | HTML substring | Forbidden final HTML milestone. |
| `TRUST_BROWSER_GATE_EXPECT_CONTROL_GONE` | accessible-name substring | Asserts no matching activatable control remains after the click. |
| `TRUST_BROWSER_GATE_EXPECT_NO_ERRORS` | presence flag | Requires zero collected JavaScript errors (otherwise errors are printed but do not automatically fail). |
| `TRUST_BROWSER_GATE_MIN_NODES` | integer | Minimum DOM node count for non-special hosts; default `1`. YouTube/Twitch/Steam use built-in empty-shell floors. |
| `TRUST_BROWSER_GATE_OUT` | file path | Saves the final serialized HTML snapshot. |
| `TRUST_DIAG_VP` | `WIDTHxHEIGHT` | Gate viewport in terminal cells. Default: `200x50`. |

### `img_box_diag`

```sh
TRUST_NET_DIAG=https://example.test/ \
  TRUST_DIAG_VP=200x50 \
  cargo test --release img_box_diag -- --ignored --nocapture
```

Drains the page, decodes real image resources, and reports rendered image boxes,
CSS sizes, classes, and source tails. It accepts `TRUST_NET_DIAG`,
`TRUST_DIAG_VP`, and `TRUST_NET_DIAG_OUT`.

### `layout_dump`

```sh
TRUST_LAYOUT_FILE=/tmp/post-js.html \
  TRUST_DIAG_VP=120x40 \
  TRUST_LAYOUT_GREP=header \
  cargo test --release layout_dump -- --ignored --nocapture
```

This is the offline layout/paint diagnostic. It requires a post-JavaScript HTML
file and prints rows, regions, carousels, a DOM legend, and optional geometry.

| Input | Value | Effect and default |
|---|---|---|
| `TRUST_LAYOUT_FILE` | HTML file path | Required input document. |
| `TRUST_DIAG_VP` | `WIDTHxHEIGHT` | Terminal-cell viewport. Default: `80x0`. |
| `TRUST_DIAG_URL` | absolute URL | Base URL for relative resources and the legend. Default: `https://store.steampowered.com/`. |
| `TRUST_LAYOUT_GREP` | text | Restricts printed row/region output to lines containing this substring. |
| `TRUST_LAYOUT_SPRITE` | SVG/text file path | Primes every external SVG sprite sheet referenced by `<use>` for offline replay. |
| `TRUST_LAYOUT_IMG_CELL` | `WIDTHxHEIGHT` | Seeds every resolved `<img>` with a decoded size expressed in terminal cells. |
| `TRUST_LAYOUT_IMG_ALPHA` | presence flag | Marks seeded images transparent for overlap-compositing replay. |
| `TRUST_LAYOUT_MEASURE` | id/class substring | Prints CSSOM `getBoundingClientRect`-style geometry for matching elements. |
| `TRUST_LAYOUT_NODES` | comma-separated node ids | Prints the DOM legend for each id and its ancestor chain; an optional leading `n` is accepted. |
| `TRUST_FRAG_DIAG` | presence flag | Adds the resolved fragment tree to the diagnostic output. |

### `wpt_diag`

```sh
TRUST_NET_DIAG=https://wpt.live/... \
  cargo test --release wpt_diag -- --ignored --nocapture
```

Fetches a WPT testharness page, injects a completion callback, and reports each
subtest's PASS/FAIL/TIMEOUT/NOTRUN status. It accepts `TRUST_NET_DIAG`,
`TRUST_DIAG_VP` (default `200x50`), and `TRUST_NET_DIAG_OUT`.

## JavaScript engine and bundle diagnostics (Boa backend)

These inputs are defined in `src/js.rs`, which is compiled only when using the
explicit Boa comparison features. They are useful for engine investigations but
do not tune the normal Lumen release artifact.

| Input | Value | Effect and default |
|---|---|---|
| `TRUST_DIAG_COMPUTE_SECS` | integer seconds | Extends the manual page compute budget for an exceptionally large Boa workload. Default is the normal `50`-second budget; minimum accepted value is `1`. |
| `TRUST_JS_PHASE` | presence flag | Reports whole-load parse, compile, execute, measured JS CPU, and wall-time decomposition. |
| `TRUST_FN_CENSUS` | presence flag | Reports compiled versus executed/never-called page functions for lazy-parse sizing. |
| `TRUST_JS_PROFILE` | presence flag | Dumps and resets sampled VM hot leaf frames during page phases/interactions. |
| `TRUST_JS_BENCH` | JavaScript file path | Required input for `engine_profile`; runs one classic bundle in a faithful page context. |
| `TRUST_JS_BENCH_RUNS` | integer | Independent benchmark samples. First sample is discarded as cold. Default: `5`; minimum: `1`. |
| `TRUST_JS_BENCH_SETTLE_SECS` | integer seconds | Manual benchmark settle timeout. Default: `300`; minimum: `1`. |
| `TRUST_NO_OPT` | presence flag | For `engine_profile` only, disables Boa optimizer options and prints the A/B state. |
| `TRUST_GC_FLOOR` | integer MiB | Boa GC threshold floor. Default: `1`. Values are clamped to at least one byte after conversion. |
| `TRUST_GC_GROWTH` | integer percent | Normal GC threshold growth percentage. Default: `143`. |
| `TRUST_GC_BIG_LIVE` | integer MiB | Live-size threshold at which the large-live-set growth policy applies. Default: `16`. |
| `TRUST_GC_BIG_GROWTH` | integer percent | Threshold growth percentage above `TRUST_GC_BIG_LIVE`. Default: `400`. |
| `TRUST_NO_GC_PERMGEN` | presence flag | Disables the Boa permanent-generation tenure optimization when set (`gc_permgen` is on by default). |
| `TRUST_GC_PERM` | presence flag | `engine_profile` A/B knob that tenures the immortal platform graph for that benchmark run. |
| `TRUST_NO_LAZY_PARSE` | presence flag | Disables Boa lazy parsing and lazy compilation. Lazy parsing is on by default. |
| `TRUST_LAZY_MIN` | integer code points | Overrides the minimum function source size eligible for Boa lazy parsing/compilation. |
| `TRUST_NO_PARALLEL_PARSE` | presence flag | Forces sequential external-script parsing instead of the parallel parse pool. |
| `TRUST_NO_PRELUDE_CACHE` | presence flag | Forces the cold Boa prelude path instead of the cached prelude image. |
| `TRUST_NO_CDN_CACHE` | presence flag | Disables the cross-page compiled external-script cache. |
| `TRUST_NO_INCREMENTAL_LAYOUT` | presence flag | Test-only A/B switch that forces the full layout path. It is not a normal release runtime control. |
| `TRUST_JS_DIAG` | HTML file path | Required input for `js_diag`; transforms a local HTML file and prints post-JavaScript HTML and the outcome. |
| `TRUST_SCAN_AUDIT` | classic-script JavaScript file path | Required input for `scan_ident_audit`; compares lazy scanner identifier capture with the real parser over a bundle. |
| `TRUST_REACT_DEV` | presence flag | `react_canary` loads development React/ReactDOM bundles instead of production minified bundles. |

Example bundle diagnostics:

```sh
TRUST_JS_BENCH=/tmp/bundle.js TRUST_JS_BENCH_RUNS=5 \
  cargo test --release engine_profile -- --ignored --nocapture

TRUST_JS_DIAG=/tmp/page.html \
  cargo test --release js_diag -- --ignored --nocapture

TRUST_SCAN_AUDIT=/tmp/bundle.js \
  cargo test --release scan_ident_audit -- --ignored --nocapture
```

## Desktop and layout benchmarks

| Input | Value | Effect and default |
|---|---|---|
| `TRUST_DESKTOP_TRACE` | presence flag | Prints desktop frame-stage timings, including the live presentation path. |
| `TRUST_DESKTOP_BENCH` | presence flag | Enables the ignored `desktop_pipeline_bench`; without it the test exits with a usage message. |
| `TRUST_DESKTOP_BENCH_ITERATIONS` | positive integer | Number of iterations per desktop fixture. Default: `5`; values are clamped to at least `1`. |
| `TRUST_LAYOUT2_BENCH` | — | Appears in the `p8_layout_bench` source example but is not read by the test. The Cargo test selector is the actual switch. |

Run the fixture benchmarks with:

```sh
TRUST_DESKTOP_BENCH=1 TRUST_DESKTOP_BENCH_ITERATIONS=5 \
  cargo test --release desktop_pipeline_bench -- --ignored --nocapture

cargo test --release p8_layout_bench -- --ignored --nocapture
```

## Terminal-capture diagnostics

`replay_ten_real_terminal_frames` replays a `script(1)` capture through the
same VT parser used by the app:

```sh
TRUST_TTY_CAPTURE=/tmp/session.out \
  TRUST_TTY_TIMING=/tmp/session.time \
  TRUST_TTY_SIZE=130x20 \
  cargo test --release replay_ten_real_terminal_frames -- --ignored --nocapture
```

| Input | Value | Effect and default |
|---|---|---|
| `TRUST_TTY_CAPTURE` | `script(1)` capture path | Required terminal byte stream. |
| `TRUST_TTY_TIMING` | `script(1)` timing-log path | Required timing data; only `O` output records are replayed. |
| `TRUST_TTY_SIZE` | `COLUMNSxROWS` | Parser viewport. Default: `130x20`. |

## TLS and client-identity inputs

These are operational inputs rather than performance diagnostics, but they are
included here because they are part of TRust's `TRUST_*` environment surface.

| Input | Value | Effect and default |
|---|---|---|
| `TRUST_KNOWN_HOSTS` | file path | Overrides the default `~/.config/trust/known_hosts` file used for TLS trust-on-first-use pins. |
| `TRUST_IDENTITIES` | directory path | Overrides the default `~/.config/trust/identities` directory containing per-host client identity PEM files. |

## Inputs that are not active controls

Two names appear in historical comments or command examples but are not read as
environment variables by the current source:

* `TRUST_LAZY_PARSE` is an old name used in comments around the Boa lazy-parse
  work. The active A/B control is `TRUST_NO_LAZY_PARSE`; setting
  `TRUST_LAZY_PARSE` alone has no effect.
* `TRUST_LAYOUT2_BENCH` is shown in the `p8_layout_bench` doc comment for
  discoverability, but the test does not inspect it. Selecting the ignored test
  by name is sufficient.

Likewise, `TRUST_GC_*` in prose is a family label, not a wildcard parser. The
active names are the four exact GC inputs listed above.

## Adding a diagnostic

When adding an input:

1. Give presence-only flags and value inputs distinct names and define their
   accepted syntax/default in the source comment.
2. State whether the input is shared, Lumen-only, Boa-only, terminal-only,
   desktop-only, or test-only.
3. Include one copy-paste command for the test or executable path.
4. Specify output destination and whether the input is read once or per use.
5. Update this document and run `git diff --check`.
