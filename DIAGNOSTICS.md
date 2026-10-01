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
* Every build uses Lumen. HTTP, layout, and JavaScript diagnostics are shared
  across frontends unless noted otherwise.
* Diagnostics print to stderr unless a variable explicitly names an output
  file/directory. Use a local or authorized test endpoint; avoid repeatedly
  probing public sites.

## Quick-start commands

### Native input and worker fetch

`src/fixtures/native_input.html` records actual mouse/keyboard modifier state,
screen/client/target-relative coordinates, user activation, capture transitions,
and click counts, including a same-origin iframe. It does not synthesize input:

```sh
TRUST_LUMEN_TRACE=1 target/release/trust-desktop src/fixtures/native_input.html
cargo test --release --lib pointer_
cargo test --release --lib user_activation
cargo test --release --lib worker_fetch
```

The pointer and activation fixtures run in interpreter, bytecode, and JIT tiers.
The worker fetch regression holds a loopback response until worker timers,
messages, and microtasks make progress; it also checks blob-worker origin
inheritance, HTTP error responses, and rejection of unresolved relative URLs.
These are general conformance checks, not a claim that a challenge service will
accept the browser. They follow the local 2026-09-06 snapshots of HTML's
[user activation](https://html.spec.whatwg.org/multipage/interaction.html#tracking-user-activation)
and [worker settings](https://html.spec.whatwg.org/multipage/workers.html),
[Pointer Events](https://www.w3.org/TR/pointerevents4/),
[CSSOM View mouse coordinates](https://drafts.csswg.org/cssom-view-1/#extensions-to-the-mouseevent-interface),
and [Fetch](https://fetch.spec.whatwg.org/#fetch-method).

### Worker performance timelines

Workers use the shared User Timing, Resource Timing, and PerformanceObserver
implementation. Their entry buffers and time origins belong to the worker,
not its owner Window; navigation timing remains Window-only. Observer delivery
and resource-buffer-full events run as worker tasks after microtask checkpoints,
without an extra thread or polling timer.

```sh
cargo test --release --lib worker_performance
cargo test --release --lib worker_event_targets
cargo test --release --lib user_timing_buffers_and_private_slots
```

The network fixture exercises real classic, module, and blob workers against two
loopback origins. It checks successful fetches, observer delivery, per-worker
entry ownership, and Timing-Allow-Origin filtering. Interface/event regressions
also run in interpreter, bytecode, and JIT tiers. These implement the local
2026-09-06 snapshots of [HR-Time](https://w3c.github.io/hr-time/#sec-performance),
[User Timing](https://w3c.github.io/user-timing/),
[Performance Timeline](https://w3c.github.io/performance-timeline/#queue-the-performanceobserver-task),
[Resource Timing](https://w3c.github.io/resource-timing/#marking-resource-timing),
and [DOM event dispatch](https://dom.spec.whatwg.org/#concept-event-dispatch).
Successful local checks do not establish why a remote challenge accepts or
rejects a browser.

### WebAssembly

Compare native WebAssembly execution against the interpreter with the same
release artifact: `TRUST_WASM_NATIVE_JIT=0` (or `false`) disables the optional
native compiler. It is enabled by default on little-endian AArch64 and x86-64;
unsupported instructions and fuel-metered engines use the interpreter.
`WASMI_JIT_TRACE=1` reports compiled instruction counts, native code sizes, and
compilation times. The per-engine cache accepts at most 128 native regions of
256 Wasmi instruction words each and 2,048 candidate addresses. Native code
allocations are released when the engine and its active calls are dropped.
Compilation counters persist across host/JavaScript calls. The bounded cache
recycles cold counters so one-shot startup code cannot exclude later hot functions;
compiled regions and checked fallbacks retain their cache entries.
`TRUST_WASM_TRACE=1` logs page WebAssembly imports and exported calls (the first 64,
then every 1,000th) as `wasm:` lines on stderr.

The deterministic `src/fixtures/wasm_native_benchmark.html` fixture reports
execution time and checks its result. Compare both modes without competing
builds; repeat measurements because process RSS includes allocator and browser
variation. On Linux, `/usr/bin/time -v` reports peak RSS:

```sh
cargo build --release
TRUST_WASM_NATIVE_JIT=0 /usr/bin/time -v target/release/trust-headless --settle 3 "file://$PWD/src/fixtures/wasm_native_benchmark.html"
TRUST_WASM_NATIVE_JIT=1 /usr/bin/time -v target/release/trust-headless --settle 3 "file://$PWD/src/fixtures/wasm_native_benchmark.html"
stat -c '%s bytes' target/release/trust
```

Run the owned Wasmi compiler regressions, including interpreter comparisons,
with its local dependency patches:

```sh
cargo test --manifest-path vendor/wasmi-1.1.0/Cargo.toml \
  --target-dir target/wasmi-tests --features simd,native-jit \
  --config "patch.crates-io.wasmi_core.path='$PWD/vendor/wasmi_core-1.1.0'" \
  --config "patch.crates-io.wasmi_ir.path='$PWD/vendor/wasmi_ir-1.1.0'" \
  --config "patch.crates-io.wasmi_collections.path='$PWD/vendor/wasmi_collections-1.1.0'"
```

Trace a normal Lumen page load and its network requests:

```sh
TRUST_NET_TRACE=1 TRUST_LUMEN_TRACE=1 target/release/trust https://example.test/
```

Network trace lines include `protocol=h3` / `h2` / `http/1.1` and
`reused=true/false`. HTTPS discovers HTTP/3 through authenticated `Alt-Svc`
response headers and otherwise negotiates HTTP/2 through ALPN; no User-Agent
changes or site-specific transport rules are involved. An initial HTTP/2
response can advertise QUIC for later requests. HTTP/1.1-only and no-ALPN servers
retain the existing transport, and WebSocket Upgrade uses a separate
HTTP/1.1-only connector.

Run the bounded, transport-only live HTTP/2 check (no JavaScript or clicks):

```sh
TRUST_NET_DIAG=https://civitai.red/ \
  cargo test --release --lib http2_live_transport_probe -- --ignored --nocapture
```

This makes two GETs through the real browser networking layer, reports status,
negotiated protocol, connection reuse, body size and challenge presence, and
expects HTTP/2 200 responses without challenge headers (this diagnostic clears
Alt-Svc between requests to isolate TCP). It is a live acceptance
check, not a stable offline test or a guarantee that every protected site works.
The ordinary `cargo test --lib http2` suite uses local TLS servers for ALPN
fallback, multiplexing, flow control, uploads, disk streaming, cancellation,
GOAWAY/reset retry safety, response validation, and timing. HTTP/2 receive
credits are 1 MiB per stream / 8 MiB per connection, not eager allocations;
uploads stage at most 64 KiB at a time. Header blocks are bounded to 256 KiB
and 256 fields, with at most 128 interim responses per request. Page-body and
download limits remain separate. At most 32 recently used origin entries are
retained for reuse; active streams keep their own session alive when evicted.

Check HTTP/3 discovery and actual QUIC responses:

```sh
TRUST_NET_DIAG=https://civitai.red/ \
  cargo test --release --lib http3_live_transport_probe -- --ignored --nocapture
```

This makes three GETs and requires at least one actual HTTP/3 response. It
reports status/challenge presence without requiring a challenge-free response:
HTTP/3 support does not itself guarantee acceptance by a challenge service.
Failure can also indicate missing Alt-Svc or blocked UDP. Normal browser
requests fall back to TCP; they do not require HTTP/3 to succeed. The first
QUIC probe waits at most 250 ms in the foreground and runs at most three
seconds in the background. Concurrent requests use TCP until QUIC is ready;
failed alternatives cool down for 60 seconds. Only handshakes race, not HTTP
requests; at most 16 speculative handshakes run at once. With `TRUST_NET_TRACE`,
`h3-ready` and `h3-unavailable` lines report setup outcomes using origin hosts
and ports, without cookie values or URL paths/queries. Possibly processed
non-idempotent requests are never blindly retried.

`cargo test --lib http3` exercises local TLS/QUIC endpoints, including origin
authentication at a different advertised host/port, multiplexing, uploads,
early responses, stream-local cancellation, downloads, cookies, compression,
malformed messages, encoded-header bounds, 421, and fallback. Receive credits
are 1 MiB per stream / 8 MiB per connection; uploads stage 64 KiB. Metadata
is limited to 256 KiB decoded / 512 KiB encoded and 256 fields. QPACK uses
static entries/literals and advertises zero dynamic-table capacity. No 0-RTT,
server push, extended CONNECT, or QUIC datagrams are enabled. Unused sessions
close after 30 seconds; 128 origin/partition advertisements are retained only
in memory, for at most seven days or their shorter advertised freshness.
DNS HTTPS records and HTTP/2 ALTSVC-frame discovery are not implemented.

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

The terminal browser accepts `trust [URL-or-host] [port]`. Press Ctrl+] and type
`help` for interactive prompt commands; the help text lives in
[`src/command.rs`](src/command.rs).

### `trust-headless`

`trust-headless --help` is generated from the usage string in
[`src/bin/trust-headless.rs`](src/bin/trust-headless.rs):

| Input | Meaning | Default |
|---|---|---:|
| `URL` or `FILE` | Required initial navigation; a local path loads as a `file://` page | — |
| `--width N` | CSS viewport width in CSS pixels | `1024` |
| `--height N` | CSS viewport height in CSS pixels | `768` |
| `--timeout SECS` | Hard wall-clock limit for navigation/settling | `10` |
| `--settle SECS` | Quiet-period fallback, used only on a page that never goes inert (see below) | `0.75` |
| `--max-chars N` | Character budget for page text (`unlimited` for the whole page) | `8000` |
| `--format text\|semantic` | Display-list text or accessibility tree output | `text` |
| `--links` | Include link targets in text output | off |
| `--js-diagnostics` | Print the last page-script outcome to stderr: JS errors, captured console lines, panic flag, skipped modules, and page fetch count (`[js-errors]`/`[js-console]`/`[js-outcome]` blocks) | off |
| `--site-data` | Load the saved profile's bookmarked-site cookies and storage (resolved through `XDG_DATA_HOME`) and write changes back to it. For profiling with a user's site permissions, point `XDG_DATA_HOME` at a copy of the profile. | off |
| `-h`, `--help` | Print usage | — |

`TRUST_HEADLESS_PNG=PATH` also writes the final display list as a PNG. Like the
desktop frontend, it first fetches and decodes the page's eager, painted and
near-viewport images (at most 200 per round, 20 s in all) and lays the page out
again with their sizes; the stderr `[snapshot]` line counts loaded and failed
images. Animated images show their first frame.

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

Image-driven initialization is not an acceptance gate covered by this driver.
Unlike the terminal and desktop frontends, it currently has no image
fetch/decode scheduler and does not send decoded intrinsic sizes back to the
resident actor. An image's `load` handler can therefore remain pending even
when the text dump exits successfully. Use the appropriate frontend to validate
decoded images and image-dependent page behavior. The
`decoded_image_completion_updates_complete_and_delivers_load` regression test
covers the actor's completion/event boundary, not the headless decoder pipeline.

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

These tools are opt-in targets and do not open the normal browser UI.
Run the replay tool with `cargo run --release --example trust-browser-replay -- FIXTURE.html`.
It lives under `target/release/examples/` and is not part of the distributed browsers.

| Binary and input | Meaning | Default |
|---|---|---:|
| `trust-browser-replay [--warmups N] [--samples N] [--external NAME=PATH] [--sheet NAME=PATH] FIXTURE.html [...]` | Deterministically replays one or more local HTML fixtures through the shared Lumen browser pipeline. `--external` and `--sheet` may be repeated to provide named local script/style resources. | warmups `1`, samples `5`; at least one fixture required |
| `trust-lumen-spike [--tier interp\|bytecode\|jit] [--threshold N] [--benchmark PATH]` | Runs the synthetic Lumen/`js-engine-benchmark` harness and prints timing, GC, event-loop, and score data. Requires the `lumen-spike` Cargo feature. | tier `jit`, threshold `0`, benchmark `/usr/share/cry/benchmarks/js-engine-benchmark/run.js` |

Both binaries support `-h`/`--help`.

`trust-browser-replay` runs with no network and virtual time, and prints a JSON report
(per-sample timings, errors, console, fetch counts, SHA-256 of inputs and output). A
fixture passes when it sets `data-replay-state="complete"` and matching nonempty
`data-replay-checksum`/`data-replay-expected` attributes without a JavaScript error
or panic; any failure exits nonzero. `--external` names must equal a script's `src`
attribute exactly, and only parser-inserted classic scripts are served: module
scripts, dynamically inserted scripts and `fetch`/XHR requests fail offline. Lumen's
`benchmarks/browser-replays/` holds the current fixtures and their run command.

## Live runtime diagnostics

### Cross-frontend and terminal diagnostics

| Input | Value | Effect and output |
|---|---|---|
| `TRUST_NET_TRACE` | presence flag | Adds timestamped request/subresource timing to stderr. The `net:` lines use one shared millisecond origin; DOM mutation and JavaScript phase markers may use the same timeline. |
| `TRUST_COOKIE_TRACE` | presence flag | Traces response receipt, jar accept/reject/delete, script access and selected request-cookie counts. Reports `cf_clearance` counts separately, with numeric status/time and a hashed origin identifier, never cookie values or arbitrary names/URLs. `[cookie-attributes]` reports only Secure/HttpOnly/Partitioned booleans and a normalized SameSite category for clearance cookies. `stored-challenge-partition` and `request-challenge-partition` identify the narrow challenge-cookie exception; the latter can have count zero. `[cookie-integrity]` compares outgoing clearance values byte-for-byte against bounded in-process receipt copies (64 records, at most 8192 value bytes each); `unknown` includes altered, oversized or evicted receipts, not just mismatches. The flag itself does not change cookie policy. It does **not** redact URLs or other sensitive data emitted by other enabled diagnostics. |
| `TRUST_DIAG_FRAME` | presence flag | Enables the terminal frame report (`DIAGFRAME`) and related page/layout reports (`DIAGGEOM`, `DIAGROUTE`, `DIAGRELAY`). It includes redraws, draw time, page-event work, full replacements, image relayout/render counts, load phases, scroll state, and cascade/layout costs. |
| `TRUST_DIAG_PATCH` | presence flag | Adds timing output for incremental region/subtree layout patches. |
| `TRUST_DIAG_SCROLL_BOXES` | presence flag | Makes the HTTP path use the one-shot transform for inner-scroll-box investigation instead of retaining a resident actor. Diagnostic-only; pair it with the current scroll/geometry output when investigating nested scrollers. |
| `TRUST_NO_FRAME_SKIP` | presence flag | Disables the terminal's identical-frame suppression and restores the always-draw path. Useful for A/B measurements of redraw behavior. |
| `TRUST_DUMP_RAW` | directory path | Writes each live serialized HTML render as `render_<timestamp-or-sequence>.html` for offline replay/diffing. The directory must already exist. Used by both the terminal app and shared browser controller. |
| `TRUST_LAYOUT_TRACE` | presence flag | Prints graphical layout stage timing from `layout2::lay_out_graphical`. |
| `TRUST_FRAG_DIAG` | presence flag | Dumps the resolved graphical fragment tree (tag, position, size, and clip). Used with `layout_dump` for layout/paint discrepancies. |
| `TRUST_PANIC_LOG` | file path | Appends every panic, including background-thread panics, with thread name, terminal-owner status, message, and forced backtrace. The normal terminal panic hook remains separate. |
| `TRUST_UA_FIREFOX` | affirmative value (`1`, `true`, `yes`, `on`) | Replaces TRust's `TRust/0.1` User-Agent with Firefox's current one (`Mozilla/5.0 (X11; Linux x86_64; rv:153.0) Gecko/20100101 Firefox/153.0`) for diagnostics. Selection happens once per process, so the header on every HTTP/1.1, HTTP/2 and HTTP/3 request, fetch/XHR, WebSocket handshake and download matches `navigator.userAgent` in the page, every frame and every worker; HTML's `navigator.appVersion` then derives as `5.0 (X11)`. Prints one stderr line naming the active string. Nothing else about the request changes: `Accept`, `Accept-Language`, Fetch Metadata `Sec-*` and `Sec-GPC` keep TRust's own values. Unset, empty or `0` keeps TRust's User-Agent. |
| `TRUST_WEBGL_TRACE` | presence flag | Prints why a page's WebGL context could not be created. |
| `TRUST_TRACE_PAGE_EVENTS` | presence flag | Prints a `[trace-event] <variant>` line to stderr for every page event the shared controller handles (`Updated`, `Static`, `Patched`, `Trouble`, navigation/settle events). Useful for proving which render path a page reached and in what order. |

#### A/B-testing a site's User-Agent gate

Sites sometimes gate features on the `User-Agent` string. Compare TRust's own
string with Firefox's on the same navigation:

```sh
target/release/trust https://example.org/                      # TRust/0.1
TRUST_UA_FIREFOX=1 target/release/trust https://example.org/   # Firefox/153.0
```

The enabled form prints `TRUST_UA_FIREFOX: diagnostics report Firefox's
User-Agent: ...` once to stderr, and `navigator.userAgent` in the page agrees
with the header. The conformance tests for both halves are:

```sh
cargo test --lib -- firefox_user_agent_diagnostic_is_opt_in one_user_agent_header_describes_both_wire_and_scripts
cargo test --lib -- navigator_user_agent_and_appversion_follow_the_environment_value
```

The replacement is the reduced User-Agent Firefox sends by default (Firefox 100
onward), read from the locally installed Firefox 153 build: its platform
fragment, `rv:` and product tokens. To imitate a newer Firefox, update the
`FIREFOX_USER_AGENT` constant in `src/http.rs`; do not extend the diagnostic to
Firefox's `Accept`, `Sec-*`, or plugin-flavoured metadata, which stays TRust's
own by design.

### Lumen diagnostics

| Input | Value | Effect and output |
|---|---|---|
| `TRUST_LUMEN_TRACE` | presence flag | Logs Lumen script start/completion, JavaScript errors, console messages, and unhandled rejection details to stderr. |
| `TRUST_LUMEN_TASK_TRACE` | presence flag | Emits a once-per-second resident page-actor task census: turns, commands, interactions, host/platform/timer/lifecycle work, render passes, updates, finishes, and queue state. |
| `TRUST_LUMEN_PROBE` | JavaScript source | Evaluates the supplied expression/source in the resident page after a task and prints its value, throw, interruption, or parse error. This is a diagnostic probe, not page content. |
| `TRUST_LUMEN_DIAGNOSTIC_INIT` | JavaScript function body | Opt-in instrumentation called once after the resident main-page platform bootstrap, before author scripts. It receives the lexical parameter `__trustDiagnosticReport`, never a Window property. This callback accepts exactly one valid JSON object string, at most 4 MiB, and prints one `lumen: diagnostic:` line; malformed, oversized or repeated submissions throw/report diagnostic errors. It is not installed in child frames, workers, or replay-only engines, and is never rerun by diagnostic drains. Initializer errors do not skip author scripts. A one-shot `load` listener can capture the callback, install a completion hook and start a benchmark after setup. For controlled timing, disable both `TRUST_LUMEN_PROBE` and `TRUST_LUMEN_TRACE`; retain `--js-diagnostics` for errors at exit. Leave this flag unset for ordinary acceptance browsing; hooks can alter page behavior and may emit private data. |
| `TRUST_PRELUDE_FILE` | file path | Replaces the embedded platform prelude (`js_platform.js`) with the given file's contents for the process, read once on first use. Intended for rapid iteration on prelude code: edit `src/js_platform.js`, set this to that path, and the *next page load* picks the change up — no rebuild or relink is needed. The override is read in the Lumen backend at prelude evaluation time, independently of `__trust_cfg`. Release/shipped builds keep the embedded prelude; leave this unset. An unreadable path falls back to the embedded prelude. |
| `TRUST_WORKER_PRELUDE_FILE` | file path | Replaces the complete assembled worker prelude, including shared platform blocks, for diagnostic iteration. Read once on first worker creation; an unreadable path uses the embedded prelude. Leave unset for acceptance testing and normal browsing. The assembly order is defined by `js::worker_prelude()`. |
| `TRUST_TRACE_FRAMES` | presence flag | Requests the prelude's gated frame-flow trace, emitted as `FT ...` console lines (`log: FT …` under `--js-diagnostics`): frame hydration, navigation, resource completion, native click target paths, and script discovery. Includes Window message origins/target origins, serialized lengths, frame IDs, transfer counts, and MessagePort queue enablement/delivery (not message contents). The trace flag travels through `__trust_cfg.frameTrace`, including child Window Realms. |
| `TRUST_TRACE_CHALLENGE_MESSAGES` | presence flag | Emits `[challenge-message]` records for Window messages to/from the exact `https://challenges.cloudflare.com` origin. Records send, target-origin drop, deserialize failure, dispatch start and dispatch return, with network-correlated timestamps and frame IDs. Reads the already-serialized wire, not author objects: no additional getters, serialization, or message listeners. Logs only allowlisted event/retry/reason names, bounded numeric error codes/intervals, and presence flags for opaque result/token fields; no token/cookie values, arbitrary field names, URLs or widget IDs. Unknown strings are redacted. Parsing is limited to 64 KiB per wire; output to 4,096 records per process. The flag is inherited by child Window Realms and is off by default. `dispatched` means event dispatch returned, not that the challenge succeeded. |
| `LUMEN_TRACE_ERRORS` | `1` or diagnostic substring | Opt-in engine diagnostics for native errors, including caught exceptions. Bounded to 1024 matching errors per process. Feature probes often intentionally catch errors; this is not an uncaught-error counter. |
| `LUMEN_TRACE_ERROR_SOURCE` | presence flag or character limit | With `LUMEN_TRACE_ERRORS`, include up to four interpreted stack frames' source prefixes (default 800 characters each; numeric limits capped at 16,384). Non-callable primitive diagnostics also include up to 32 bounded callee previews and argument types. May contain private page/script data; keep logs local and inspect before sharing. No author getters or `toString` methods are invoked. |
| `LUMEN_TRACE_UNDEFINED_PROPERTIES` | presence flag | Records the last 32 slow-path property reads returning `undefined` on each execution thread. Matching `LUMEN_TRACE_ERRORS` diagnostics print their bounded property/function names and receiver types; named reads include the native object kind and up to eight own descriptor names. This is a partial history, not proof of absent properties: getters and proxies can return `undefined`. It never repeats a lookup or invokes author diagnostic hooks. Logs may contain private names; keep them local. |
| `LUMEN_TRACE_BOUND_CALLS` | presence flag | Records the last 256 bound-function call/return events per execution thread; matching `LUMEN_TRACE_ERRORS` diagnostics print parent call IDs and bounded native argument/result previews. Does not replace browser functions, invoke author accessors/traps, or retain returned objects. A diagnostic run still incurs logging overhead and may expose private script data; keep its logs local. |
| `LUMEN_TRACE_BOUND_ARRAYS` | presence flag | With bound-call tracing, records native descriptor changes in small arrays owned by the first saved argument, before and after calls. Scans at most 16 own fields, 4 arrays of 16–512 entries, and retains at most 16 preview snapshots without retaining JS values. Useful for tracking corrupted working state without replacing JavaScript functions or arrays; keep the potentially private output local. |
| `LUMEN_FN_PROFILE` | presence flag | Opt-in interpreted function timing, including compiled execution entered through the interpreter; reports inclusive/self time and callers. Each reported entry also includes its internal compilation, activation, scope and same-realm state sampled on the first entry in that batch; this is not a count of fast-path misses. Adds overhead and is not an uninstrumented wall-time benchmark. `LUMEN_FN_PROFILE_FLUSH_SECONDS` sets the minimum elapsed time before a completed call can flush a batch. |
| `LUMEN_FN_PROFILE_SOURCE` | presence flag | With function profiling, include up to 16,384 source characters for each reported hot function (at most 32 per batch). This can expose private page code; keep captures local. |
| `LUMEN_JIT_CODE_BUDGET_MB` | integer, 1–1024 MiB | Process-wide requested executable-byte cap shared by JavaScript and native RegExp code. Read once; invalid values use the default (128 MiB). It does not reserve that much memory up front. The cap excludes page rounding and non-executable metadata. Function profiles and `LUMEN_PERF_METRICS` report live requested bytes, limit and rejected reservations. JavaScript bodies deferred for budget pressure retry only when their requested capacity becomes available; unsupported bodies remain on the checked VM. No live code is evicted or patched. |
| `LUMEN_JIT_NO_JSCVT` | presence flag | ARM64 generated code uses the portable guarded ToInt32 sequence (with its checked-helper fallback) for bitwise operators even when the CPU implements FEAT_JSCVT (`fjcvtzs`). Read once per process. Use it to compare the two code paths or to rule out the instruction when diagnosing a numeric difference; results must be identical. |
| `LUMEN_JIT_INLINE_STUBS` | presence flag | ARM64 generated code emits name-cache validation, generic property-cache probes, secondary call-cache probes and direct-call sequences at every site instead of in per-chunk shared stubs (by default only loop bodies keep name validation and direct calls in line). Read once per process. A code-size and performance comparison only; results must be identical. |
| `LUMEN_GC_LOG` | presence flag | Logs post-collection object counts at allocation and task-boundary collections, with retained Realm, template, pin and host-settings counts. A task-boundary reclaimed count excludes objects already released by reference counting while the task unwound. |
| `LUMEN_GC_DUMP` | presence flag | Dumps bounded root-property/scope-name samples, object-kind and initial-root counts, root-array size buckets, kept/pool counts, largest array length and the current stack. Reports the six most frequent function bodies among at most 1,024 sampled distinct bodies, with omitted-instance counts and names capped at 80 characters. Counts describe the pre-sweep snapshot, not the final live graph. Uses internal metadata without invoking author hooks; may expose private names, so keep captures local. Adds diagnostic traversal overhead. |
| `LUMEN_GC_DUMP_MIN_OBJECTS` | non-negative integer | With `LUMEN_GC_DUMP`, skip snapshots below this object count. Missing/invalid values use zero. Useful for restricting output to large-heap pressure events. |
| `LUMEN_GC_DUMP_ROOT_LIMIT` | positive integer | Maximum object-root records per dump, default 60, clamped to 1–16,384. Includes internal object identity and external-reference counts (excluding diagnostic temporaries). `LUMEN_GC_DUMP` also reports native DOM roots, allocation leases and resource leases grouped by owner document on each full host trace, independently of the engine object-count floor. |
| `LUMEN_GC_RETAINING_PATHS` | presence flag | Independently traverses full-GC snapshots of 15,000–500,000 objects and logs root-to-Realm retaining witnesses (at most 128 links each). Includes environments, native graph, explicit captures, internal slots and ephemerons; an ephemeron label requires both its inputs live but displays only one path. Diagnostic only: adds substantial traversal/storage work, exposes private property/binding names, and never changes collector marks or executes author hooks. |
| `LUMEN_TRACE_CODE_POINT_ERRORS` | presence flag | Logs the already-converted invalid number, argument index and argument count for at most 32 `String.fromCodePoint` errors per process. Does not repeat coercion or alter exception behavior. Diagnostic numeric inputs may be private; keep captures local. |
| `TRUST_TRACE_FETCH` | presence flag | Logs every page fetch to stderr as `[fetch-trace] sync|async <METHOD> <url>`, followed by `[fetch-trace] result status=<N> type=<TYPE> len=<N>` when the response resolves through the unbuffered result path. Pair with `TRUST_LUMEN_TRACE` and `--js-diagnostics` to reconstruct a page's script/fetch timeline. |

### Dedicated worker resources

Dedicated workers have no fixed count quota. Each has its own native thread and
Lumen realm, using Rust's default thread stack instead of a 64 MiB reservation;
Lumen grows execution-stack segments on demand. Idle workers block on their inbox
without polling. Creation is constrained by available memory and OS thread
resources; an OS thread-creation failure queues a Worker `error` event with the
underlying error message.

Worker messages use a dynamically allocated FIFO, with redundant MessagePort
wakeups coalesced. Termination cancels that worker's script/module/fetch work
without cancelling its owner or siblings; document cancellation reaches every
worker. A completed worker releases its native resources and the host's reference
to its JavaScript wrapper after queued replies have been dispatched.

Run the resource/lifecycle regressions with `cargo test --lib worker` and
`cargo test --release --lib worker`. These include 32 simultaneously live workers,
burst-message ordering, deep execution on the default stack, simulated OS thread
exhaustion, and cancellation during a stalled worker-script fetch. These checks
verify browser behavior, not acceptance by any particular challenge provider.

### Archive SVG / Hybrid image-lifetime regression gate

Run the pixel gates on a machine with a working Hybrid GPU adapter:

```sh
cargo test --lib -- --test-threads=1 --nocapture \
  archive_svg_pixels_survive_hover_and_resource_key_changes \
  hybrid_recycles_image_slots_without_erasing_replacement_pixels \
  hybrid_atlas_growth_preserves_upload_order_and_row_padding \
  hybrid_reuploads_reinserted_stable_image_handle_without_erasing_it
```

The Archive fixture contains the actual book/Wayback SVG geometry and compares
GPU readback with the CPU renderer, including each image's own rectangle,
after repeated hover and resource-key changes. The other gates cover recycled
atlas slots, same-handle updates, atlas growth, and aligned/padded upload rows.
An unavailable adapter means **not exercised**, not a visual pass; the new gates
print that explicitly. SVG decoding and `trust-headless` text dumps alone do
not cover the Hybrid GPU upload path.

For release acceptance, load Archive.org in the actual release desktop using
the normal Hybrid renderer, both with diagnostics unset and with
`TRUST_DESKTOP_TRACE=1`. Check the Wayback logo and every top-bar icon after
load and hover. Compare with installed production using matching conditions;
trace overhead can expose timing-sensitive defects in production too. In the
2026-09-05 failure, an encoded atlas clear/copy executed after a queue texture
write and erased the new image. The fork now encodes Pixmap transfers with
those clears/copies, following WebGPU's queue/command ordering. No SVG, JS,
network or memory-limit workaround is involved.

### Repeated SVG sizes / scrolling crash

An SVG used at different concrete object sizes in one frame needs independent
raster registrations. Both desktop backends cache SVG variants by source,
revision, CSS viewport, and device pixel dimensions. Every variant counts toward
the existing 256-entry limit; entries referenced by the current frame cannot be
evicted. Scrolling translations reuse registrations.

```sh
cargo test --release --lib repeated_svg_sizes_survive_scrolling -- --test-threads=1 --nocapture
cargo test --release --lib svg_cache_variants_obey_budget -- --test-threads=1 --nocapture
cargo test --release --bin trust-desktop inline_svg_animation_is_available -- --nocapture
RUST_BACKTRACE=1 target/release/trust-desktop https://mouse.dev/blog/muse-runtime-export/
```

The pixel tests cover A/B/A size ordering, fractional CSS sizes with identical
integer raster dimensions, device scale changes, scrolling, and cache pressure.
For native acceptance, rapidly scroll between the article's top and footer,
including frames showing the shared header/footer SVG logo at different sizes.
Repeat with `--renderer=cpu`; the normal Hybrid run must stay on Hybrid without
an image-registry panic or CPU fallback. GPU tests print an explicit notice if
no adapter is available, which is not a GPU pass.

The animated header also changes its inline SVG path on each animation frame.
Desktop presentation prepares those self-contained SVG data snapshots before
submitting the frame; an asynchronous decode would insert a loading rectangle
between successive shapes. External images keep their asynchronous loader.
The native scheduler regression checks current path pixels before any worker
completion, failed SVG handling, and ordinary network request scheduling.
For animation diagnosis, `TRUST_DUMP_RAW` records the actor's ordered DOM
snapshots; compare the header's `--p` values with the native paint trace.
Ordinary page wheel units move 120 CSS pixels; touchpad pixel deltas retain
their exact device-scaled distances. The wheel amount is a UA preference,
not an animation-timing requirement. Terminal scrollback keeps its own scale.

Standards basis: CSS Images 3 §4.2 object negotiation and SVG 2 §8.3 initial
viewport, plus SVG 2's rendering tree and HTML's update-the-rendering order,
local 2026-09-06 snapshots cited beside `ImageCacheKey` and `prepare_svg_data`.

### Covered-window presentation / bounded controller handoff

The native controller's actor bridge must not drain bounded actor output into
an unbounded UI queue. `core::events` keeps at most 16 queued events and merges
only adjacent, complete, diagnostic-free `Updated` snapshots with the same
document generation and cumulative fetch count. A quiet paint stream therefore
retains only its latest queued snapshot. Navigation, history, input results,
patches, diagnostics, incomplete renders, and final/static events remain FIFO
barriers; a full barrier queue applies asynchronous backpressure. Retired layouts
are released outside the queue lock, and native wakes are coalesced at the
empty-to-nonempty edge.

Both CPU and Hybrid desktop presenters call winit's `pre_present_notify` just
before an actual surface presentation. On Wayland this arms the compositor frame
callback that paces redraws. Do not arm it for a skipped frame with no commit,
and do not use keyboard focus as a substitute for window visibility.

Focused gates:

```sh
cargo test --release --lib core:: -- --test-threads=1
cargo test --release --bin trust-desktop -- --test-threads=1
cargo test --release --lib render:: -- --test-threads=1 --nocapture
```

The queue tests include 10,000 updates with a paused consumer, snapshot release,
generation/barrier preservation, cumulative nonzero fetch counts, shutdown,
multiple waiting producers, and cancellation. GPU readback tests are necessary
for pixels but do not exercise native swapchain presentation. For that gate,
use an **optimized release desktop**, verify the log selected the real Hybrid
adapter, open ShowBuzz, and completely cover the window for at least 13 minutes.
Record RSS with a process-specific memory guard; then uncover and check the
current clock and working hit testing. Also test repeated cover/uncover,
minimize/restore, resize, and a still-visible but unfocused window, plus a CPU
control. The original failure reached multiple GiB while covered and immediately
released most of it on exposure; a visually hidden window alone is not a pass.

Standards basis: WHATWG HTML [update the rendering](https://html.spec.whatwg.org/multipage/webappapis.html#update-the-rendering)
and [event-loop processing model](https://html.spec.whatwg.org/multipage/webappapis.html#event-loop-processing-model),
local revision `e5071a20c8569d8a3ec02ed27dd01b948773f850` (2026-09-06 snapshot),
and the installed Wayland `wl_surface.frame` contract. Coalescing presentation
snapshots is not permission to discard JavaScript tasks or semantic events.

### English-preference regression gate

The normal build keeps HTTP language preferences, NavigatorLanguage, and native
Intl defaults in US English regardless of the host environment. Exercise both
the language contract and preservation of Lumen's native Intl/GC APIs with:

```sh
LANG=de_DE.UTF-8 LC_ALL=de_DE.UTF-8 LANGUAGE=de_DE:de TZ=Europe/Berlin \
  cargo test --lib -- --test-threads=1 locale::tests:: \
    platform_prelude_preserves_native_language_builtins \
    default_accept_language_reaches_the_wire
```

Repeat with `cargo test --no-default-features --lib` to check the system
allocator build. These tests use a local fixture and loopback HTTP server;
they do not contact public sites. Explicit locale requests remain supported.

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
| `TRUST_BROWSER_GATE_FIXTURE` | absolute HTML file path | Replays a captured main response at the requested URL when navigation is rate-limited; referenced subresources still use the requested origin. |
| `TRUST_BROWSER_GATE_TYPE` | text | Types into a named live text control before the optional click and requires the text to appear in the DOM. |
| `TRUST_BROWSER_GATE_TYPE_INTO` | accessible-name substring | Selects the text control for `TRUST_BROWSER_GATE_TYPE`. |
| `TRUST_BROWSER_GATE_DESKTOP` | presence flag | Uses the desktop actor presentation path, omitting terminal adaptation. |
| `TRUST_BROWSER_GATE_KEY_TIMING` | presence flag | Waits for each typed key acknowledgement and reports its latency and intervening render updates; pair with `TRUST_BROWSER_GATE_DESKTOP` for desktop input diagnosis. |
| `TRUST_BROWSER_GATE_CLICK` | accessible-name substring | Activates the first exposed activatable control whose accessible name contains this text. |
| `TRUST_BROWSER_GATE_CLICK_SECONDS` | integer seconds | Deadline after the named click. Default: `20`. |
| `TRUST_BROWSER_GATE_EXPECT_HTML_CONTAINS` | HTML substring | Required final HTML milestone; for interactive pages it is also the default initial milestone. |
| `TRUST_BROWSER_GATE_EXPECT_INITIAL_HTML_CONTAINS` | HTML substring | Optional separate pre-click milestone. |
| `TRUST_BROWSER_GATE_EXPECT_HTML_NOT_CONTAINS` | HTML substring | Forbidden final HTML milestone. |
| `TRUST_BROWSER_GATE_EXPECT_CONTROL_GONE` | accessible-name substring | Asserts no matching activatable control remains after the click. |
| `TRUST_BROWSER_GATE_EXPECT_NO_ERRORS` | presence flag | Requires zero collected JavaScript errors (otherwise errors are printed but do not automatically fail). |
| `TRUST_BROWSER_GATE_MIN_NODES` | integer | Minimum DOM node count for non-special hosts; default `1`. YouTube/Twitch/Steam use built-in empty-shell floors. |
| `TRUST_BROWSER_GATE_OUT` | file path | Saves the final serialized HTML snapshot. |
| `TRUST_BROWSER_GATE_PNG` | PNG file path | Renders the final release paint list with Vello CPU for visual inspection. |
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

### Captured-document diagnostics

These ignored `--lib` tests read saved files and use no network.

| Test | Inputs | Output |
|---|---|---|
| `captured_classic_script_error_diagnostic` | `TRUST_CAPTURE_SCRIPT_DOCUMENT`: saved HTML | Runs each inline classic script in a fresh engine (external scripts are skipped) and prints its completion, error and stack. |
| `captured_page_computed_style_diagnostic` | `TRUST_CAPTURE_DIR` containing `reference-dom.html` and `reference-raw-css.json` | Prints boxes and computed styles for selected nodes. Hard-coded to an openai.com capture. |
| `captured_vega_svg_resource_diagnostic` | `TRUST_CAPTURE_DOM` (HTML) and `TRUST_CAPTURE_DIR` (output) | Writes each Vega chart's SVG resource and decoded PNG. Hard-coded to an openai.com capture. |

```sh
TRUST_CAPTURE_SCRIPT_DOCUMENT=/tmp/page.html \
  cargo test --release --lib captured_classic_script_error_diagnostic -- --ignored --nocapture
```

## Desktop and layout benchmarks

| Input | Value | Effect and default |
|---|---|---|
| `TRUST_DESKTOP_TRACE` | presence flag | Prints desktop frame-stage timings, including the live presentation path. |
| `TRUST_DESKTOP_BENCH` | presence flag | Enables the ignored `desktop_pipeline_bench`; without it the test exits with a usage message. |
| `TRUST_DESKTOP_BENCH_ITERATIONS` | positive integer | Number of iterations per desktop fixture. Default: `5`; values are clamped to at least `1`. |
| `TRUST_DESKTOP_BENCH_HTML` | local HTML path | Replace the desktop benchmark's HTML fixtures with a captured document. Measures composition, pointer hit testing, raster, and layout separately; no network requests. |
| `TRUST_LAYOUT_PROFILE` | presence flag | Reports layout operation counts and exclusive time per transaction. Includes cache copying/storage, actual box calculations, intrinsic sizing, inline layout, translations and positioning. Clock reads add overhead; use for attribution, then disable for latency measurements. |
| `TRUST_SELECTOR_PROFILE_FILE` | HTML file path | Optional input for the ignored `selector_workload_profile` test. Without it, uses a general 2,000-rule/220-section workload. Compares subject indexing, ancestor rejection, and bounded matching reuse; reports candidates and median times, asserts identical results, and lists costly universal rules. |
| `TRUST_SELECTOR_PROFILE_DEEP` | presence flag | Uses 2,000 rules sharing a required ancestor across 100 levels. Ignored when `TRUST_SELECTOR_PROFILE_FILE` is set. Measures repeated ancestor searches that survive rejection. |
| `TRUST_LAYOUT_BENCH_ITERATIONS` | positive integer | Samples per warm/edit/resize/resource phase of `layout_engine_workload_matrix`. Default: `7`. Cold layout is one sample after process font initialization. |
| `TRUST_LAYOUT_BENCH_BOXES` | integer | Repeated sibling count in the engine matrix. Default: `220`. Nested cases use depths 4, 8 and 12. |
| `TRUST_LAYOUT_BENCH_FILTER` | text | Runs only matrix cases whose name contains this text. Default: all cases. |
| `TRUST_LAYOUT_BENCH_PROFILE` | presence flag | Adds operation accounting to the engine matrix and prints named `ENGINE_OPS` fields. This also adds measurement overhead to its wall times. |
| `TRUST_LAYOUT2_BENCH` | — | Appears in the `p8_layout_bench` source example but is not read by the test. The Cargo test selector is the actual switch. |

Run the fixture benchmarks with:

```sh
TRUST_DESKTOP_BENCH=1 TRUST_DESKTOP_BENCH_ITERATIONS=5 \
  cargo test --release desktop_pipeline_bench -- --ignored --nocapture

cargo test --release p8_layout_bench -- --ignored --nocapture

cargo test --release --lib layout_engine_workload_matrix -- --ignored --nocapture
TRUST_LAYOUT_BENCH_PROFILE=1 TRUST_LAYOUT_BENCH_FILTER=nested-grid \
  cargo test --release --lib layout_engine_workload_matrix -- --ignored --nocapture

cargo test --release --lib selector_workload_profile -- --ignored --nocapture
TRUST_SELECTOR_PROFILE_DEEP=1 \
  cargo test --release --lib selector_workload_profile -- --ignored --nocapture
```

The engine matrix isolates style/box construction, flow, and retained CSSOM
geometry from JavaScript and rasterization. It covers block/inline flow,
flex/grid, tables, floats, positioned descendants, container queries, shadow
slots, images, vertical text and line clamping. Compare both cold and changing
inputs; an unchanged-page result alone does not measure editing performance.
Run timing samples without competing builds. Ordinary tests enforce bounded
work growth and compare cached geometry, graphical paint/hit testing, and
terminal rows with a fresh layout; they do not enforce machine-dependent
wall-time thresholds.

See [Layout performance and reuse](docs/layout-performance.md) for the reuse
contracts, measurements, standards references, and remaining limits.

## JavaScript engine profiling

Two scripts in `tools/` measure Lumen from the outside, through either the Lumen
CLI (`../Lumen`, `lumen FILE.js`) or a TRust binary such as `trust-headless`. They
add no engine input of their own and change no behavior. Both require Linux; the
sampler needs `kernel.perf_event_paranoid` of 2 or lower (it samples only user
space). Run timing work on an otherwise idle machine and keep losses in the record.

### `tools/psample.py`: sampling profiler

```sh
# Frame pointers make call chains usable; keep this build in its own target directory.
(cd ../Lumen && RUSTFLAGS="-C force-frame-pointers=yes" \
  cargo build --release -p lumen --features embed --bin lumen --target-dir target/fp)
tools/psample.py /tmp/profile.txt --jitmap -- taskset -c 5 ../Lumen/target/fp/release/lumen bench.js
tools/psample.py /tmp/page.txt -- taskset -c 5 target/release/trust-headless --settle 3 "file://$PWD/page.html"
```

The sampler starts the command stopped, attaches a `perf_event_open` task-clock
event (default 4,000 Hz, `--freq`), resumes it, and records each sample's user
instruction pointer and frame-pointer call chain (default 24 frames, `--depth`)
until the process exits. It then symbolizes the samples against the mapped
executables' ELF symbol tables (`nm`/`readelf`, demangled with `rustfilt` or
`c++filt`) and writes one report file:

* self and inclusive percentages, the leading callers of the hottest functions,
  and the most frequent five-frame stacks;
* an approximate category rollup (JIT code, JIT helper bridges, calls/frames,
  property access, elements, allocation, reference counting, side tables,
  strings, GC). Categories are name heuristics, useful for direction only.

Without frame pointers only self times are reliable. The command's stdout is
left alone. Mapping snapshots are taken every 0.5 s, so very short runs may
misattribute late JIT code.

`--jitmap` runs the command with `LUMEN_JIT_MAP=1` and redirects its stderr to
`<report>.jitmap`. Samples in anonymous executable memory are then charged to
`[jit <first local names>] pc<N> <op>`: the Lumen JIT function (named by its first
four local slots) and the bytecode operation whose machine code contains the
sample. Code between two operation offsets is charged to the earlier operation, so
fused templates and the shared call-finish stub appear under a neighboring op.
`LUMEN_JIT_MAP` is presence-only and ARM64-only: at each native compilation it
prints `[jit-map-range] <base> <len> <names>` and one
`[jit-map-pc] <base> <offset> <pc> <op> <names>` line per operation to stderr.
Without `--jitmap`, generated code is reported as `[jit]`.

Two environment inputs to the script add instruction-level detail:

| Input | Value | Output |
|---|---|---|
| `PSAMPLE_HOT` | substring of a symbol name | Per-instruction histogram for matching functions, as executable virtual addresses for `objdump -d --start-address=...`. Entries below 0.1% of samples are omitted. |
| `PSAMPLE_JITHOT` | substring of a JIT function's local names (needs `--jitmap`) | Per-offset histogram inside matching generated functions. Entries below 0.05% of samples are omitted. |
| `PSAMPLE_FOCUS` | substring of a symbol name | Callers of the outermost matching frame, and the direct callees and inclusive costs beneath it, as percentages of all samples. |

To read generated code at those offsets, run the same script with
`LUMEN_JIT_CODEDUMP=<substring>` (ARM64 only; also printed by `lumen --help`). It
prints `[jit-codedump] fn(<names>) <N> words` followed by one hex instruction word
per line for every chunk whose first four local names contain the substring. Any
value also prints a `[jit-map] fn(<names>) base=<address> len=<bytes>` line per
chunk. Write the words little-endian to a file and disassemble it with
`objdump -D -b binary -m aarch64 FILE`. Base addresses change from run to run;
take a dump and its map from the same run.

### `tools/js_ab.py`: interleaved A/B timing

```sh
tools/js_ab.py --base ../Lumen/target/base/release/lumen \
  --cand ../Lumen/target/release/lumen -n 5 --node bench.js other.js
```

Runs every script once per engine per round (default 3, `-n`), alternating the
engine order between rounds and pinning with `taskset -c 5` (`--cpu`). Every
stdout line of the form `name: <number> ...` is a measurement; the report gives
the per-engine median and the candidate/baseline ratio (below 1 is faster for
times, above 1 is better for scores). `--node` adds a Node.js reference column.
Build the baseline and candidate with the same Cargo profile. Benchmarks should
print through `typeof print === "function" ? print : console.log`.

The Lumen CLI installs Lumen's own size-class allocator, while TRust installs
mimalloc (the default `mimalloc` feature). For allocation-heavy comparisons,
measure the browser build as well: the CLI can overstate allocation costs.

### Lumen allocation measurements

From `../Lumen`, two ignored Lumen tests print object-allocation costs and type
sizes without JavaScript dispatch:

```sh
taskset -c 5 cargo test --release -p lumen --lib object_alloc_bench -- --ignored --nocapture
cargo test --release -p lumen --lib object_layout_sizes -- --ignored --nocapture
```

`object_alloc_bench` reports nanoseconds per operation for object creation and
destruction, templated creation, retained allocation and release, a raw `Rc`
baseline, weak-pointer creation, a native stack probe, collector-registry churn,
heap-handle cloning, `Props` creation, and active-heap lookup. Its filter also
matches the second test, which lives in the same module. `object_layout_sizes`
prints the sizes of `Object`, `Props`, `Callable`, `Exotic`, `Property`, and
`RefCell<Object>`. Neither affects ordinary test runs.

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

The ordinary `terminal_input_raster_preview` test accepts
`TRUST_TERMINAL_INPUT_SNAPSHOT=PREFIX` to save `PREFIX-<scale>.png` renders of the
input field, and `TRUST_TERMINAL_INPUT_HYBRID=1` to also compare them with a
headless Hybrid GPU render (saved as `PREFIX-hybrid-<scale>.png`).

## TLS and client-identity inputs

These are operational inputs rather than performance diagnostics, but they are
included here because they are part of TRust's `TRUST_*` environment surface.

| Input | Value | Effect and default |
|---|---|---|
| `TRUST_KNOWN_HOSTS` | file path | Overrides the default `~/.config/trust/known_hosts` file used for TLS trust-on-first-use pins. |
| `TRUST_IDENTITIES` | directory path | Overrides the default `~/.config/trust/identities` directory containing per-host client identity PEM files. |

## Inputs that are not active controls

`TRUST_LAYOUT2_BENCH` is shown in the `p8_layout_bench` doc comment for
discoverability, but the test does not inspect it. Selecting the ignored test
by name is sufficient.

## Adding a diagnostic

When adding an input:

1. Give presence-only flags and value inputs distinct names and define their
   accepted syntax/default in the source comment.
2. State whether the input is shared, engine-only, terminal-only,
   desktop-only, or test-only.
3. Include one copy-paste command for the test or executable path.
4. Specify output destination and whether the input is read once or per use.
5. Update this document and run `git diff --check`.
# Architecture reassessment probes

The optional `architecture-diagnostics` Cargo feature records cumulative DOM
tag-lookup callers, computed-style cache hits and copied bytes, style invalidation
fanout, and Lumen allocation/call-cache observations. It is off in ordinary builds.
DOM reports are sampled at existing page diagnostic drains (at most once per two
seconds); no reporting timer or background work is added. Allocation reports occur
at completed collections. Counters and bounded site tables retain no DOM/JS owners.
These builds are for causal diagnosis, never controlled acceptance timing.

With `lumen/optimizing-jit`, `LUMEN_OPT_JIT_DIAGNOSTICS=1` additionally records
optimizer compiler phases, code/IR sizes, native entries, helper calls, heap guards,
ownership transfers and frame publication. Heap guards count the explicit property
and ownership guard routines, not every conditional branch. Transfers count logical
retains/releases, including alias-cancelled groups; they are not allocator calls.
Existing artifacts stay under the architecture evidence directories; use new output
names and retain failed attempts as well as completed runs.
`LUMEN_OPT_JIT_DIAGNOSTICS=compile` records compiler/static observations without
emitting native execution counters; use that mode to attribute compilation cost.

### Representation and browser-work comparisons

The following controls are enabled by default in ordinary builds. Each reads its
environment variable once on first use; exactly `0` selects the comparison path.
They produce no diagnostic output by themselves. Lumen controls affect every
frontend using the engine; TRust controls affect the shared browser actor or DOM.
Use a fresh process for each setting and retain complete results, including losses.

| Control | Comparison selected by `0` |
| --- | --- |
| `LUMEN_DENSE_ELEMENTS` | Disable extended contiguous array descriptors; earlier tiny-array storage remains available. |
| `LUMEN_ITERATOR_RESULTS` | Allocate iterator result objects in closed native consumers. Public `next()` results remain distinct in either mode. |
| `LUMEN_ACTIVATION_PLANS` | Reconstruct compiled activation bindings for each invocation. |
| `LUMEN_INDEXED_ACTIVATIONS` | Use linear planned-binding lookup for wide compiled environments. |
| `LUMEN_LAYOUT_NAME_IC` | Disable fixed-layout name-cache proofs. |
| `LUMEN_COLD_ENV_LAYOUTS` | Disable shared cold-environment name layouts. |
| `LUMEN_SHARED_NATIVE_OPERATIONS` | Retain the older ARM64 region operation vocabulary. |
| `TRUST_NATIVE_WRAPPERS` | Use the earlier JavaScript wrapper-identity management path. |
| `TRUST_NATIVE_DOM_TRAVERSAL` | Use the earlier traversal path instead of single-edge native queries. |
| `TRUST_NATIVE_LIVE_COLLECTIONS` | Use the earlier child-collection representation. |
| `TRUST_DEFER_STYLE_INVALIDATION` | Walk invalidated style dependencies eagerly. |
| `TRUST_STYLE_SHARING` | Disable shared cascade and common computed-property tables. |
| `TRUST_VARIABLE_STYLE_CONTEXTS` | Exclude variable-dependent inputs from shared computation. |
| `TRUST_PERSISTENT_STYLE_CONTEXTS` | Keep transient row ownership instead of bounded retention across invalidation. |
| `TRUST_STYLE_RECORDS` | Recompute typed style records from canonical computed values. |

For example, from the TRust root:

```sh
TRUST_PERSISTENT_STYLE_CONTEXTS=0 target/release/trust ./src/fixtures/number_input.html
```

Leave these variables unset to exercise the ordinary default. Each control changes
the implementation path while preserving the same required web semantics; turning
one off does not recreate an entire historical engine or browser revision.
Cranelift remains a separate optional build feature with opt-in runtime policies.
`LUMEN_OPT_JIT_OSR=1` is a restricted loop-continuation experiment, independent of
whole-function `LUMEN_OPT_JIT` admission. Neither is enabled in an ordinary release.
See the sibling Lumen `AGENTS.md` and `docs/optimizing-tier.txt` for its limits and
the retained negative application results.
