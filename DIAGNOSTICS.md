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

Compare native WebAssembly execution against the interpreter with the same
release artifact: `TRUST_WASM_NATIVE_JIT=0` (or `false`) disables the optional
native compiler. It is enabled by default on little-endian AArch64 and x86-64;
unsupported instructions and fuel-metered engines use the interpreter.
`WASMI_JIT_TRACE=1` reports compiled instruction counts, native code sizes, and
compilation times. The per-engine cache accepts at most 128 native regions of
256 Wasmi instruction words each and 2,048 candidate addresses. Native code
allocations are released when the engine and its active calls are dropped.

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

These binaries are opt-in targets and do not open the normal browser UI:

| Binary and input | Meaning | Default |
|---|---|---:|
| `trust-browser-replay [--warmups N] [--samples N] [--external NAME=PATH] [--sheet NAME=PATH] FIXTURE.html [...]` | Deterministically replays one or more local HTML fixtures through the shared Lumen browser pipeline. `--external` and `--sheet` may be repeated to provide named local script/style resources. | warmups `1`, samples `5`; at least one fixture required |
| `trust-lumen-spike [--tier interp\|bytecode\|jit] [--threshold N] [--benchmark PATH]` | Runs the synthetic Lumen/`js-engine-benchmark` harness and prints timing, GC, event-loop, and score data. Requires the `lumen-spike` Cargo feature. | tier `jit`, threshold `0`, benchmark `/usr/share/cry/benchmarks/js-engine-benchmark/run.js` |

Both binaries support `-h`/`--help`.

## Live runtime diagnostics

### Cross-frontend and terminal diagnostics

| Input | Value | Effect and output |
|---|---|---|
| `TRUST_NET_TRACE` | presence flag | Adds timestamped request/subresource timing to stderr. The `net:` lines use one shared millisecond origin; DOM mutation and JavaScript phase markers may use the same timeline. |
| `TRUST_COOKIE_TRACE` | presence flag | Traces response receipt, jar accept/reject/delete, script access and selected request-cookie counts. Reports `cf_clearance` counts separately, with numeric status/time and a hashed origin identifier, never cookie values or arbitrary names/URLs. `[cookie-integrity]` compares outgoing clearance values byte-for-byte against bounded in-process receipt copies (64 records, at most 8192 value bytes each); `unknown` includes altered, oversized or evicted receipts, not just mismatches. Cookie policy and normal jar representation are unchanged. This flag does **not** redact URLs or other sensitive data emitted by other enabled diagnostics. |
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
| `TRUST_TRACE_FRAMES` | presence flag | Requests the prelude's gated frame-flow trace, emitted as `FT ...` console lines (`log: FT …` under `--js-diagnostics`): frame hydration, navigation, resource completion, native click target paths, and script discovery. Includes Window message origins/target origins, serialized lengths, frame IDs, transfer counts, and MessagePort queue enablement/delivery (not message contents). The trace flag travels through `__trust_cfg.frameTrace`, including child Window Realms. |
| `LUMEN_TRACE_ERRORS` | `1` or diagnostic substring | Opt-in engine diagnostics for native errors, including caught exceptions. Bounded to 1024 matching errors per process. Feature probes often intentionally catch errors; this is not an uncaught-error counter. |
| `LUMEN_TRACE_ERROR_SOURCE` | presence flag or character limit | With `LUMEN_TRACE_ERRORS`, include up to four interpreted stack frames' source prefixes (default 800 characters each; numeric limits capped at 16,384). Non-callable primitive diagnostics also include up to 32 bounded callee previews and argument types. May contain private page/script data; keep logs local and inspect before sharing. No author getters or `toString` methods are invoked. |
| `LUMEN_TRACE_UNDEFINED_PROPERTIES` | presence flag | Records the last 32 slow-path property reads returning `undefined` on each execution thread. Matching `LUMEN_TRACE_ERRORS` diagnostics print their bounded property/function names and receiver types; named reads include the native object kind and up to eight own descriptor names. This is a partial history, not proof of absent properties: getters and proxies can return `undefined`. It never repeats a lookup or invokes author diagnostic hooks. Logs may contain private names; keep them local. |
| `LUMEN_TRACE_BOUND_CALLS` | presence flag | Records the last 256 bound-function call/return events per execution thread; matching `LUMEN_TRACE_ERRORS` diagnostics print parent call IDs and bounded native argument/result previews. Does not replace browser functions, invoke author accessors/traps, or retain returned objects. A diagnostic run still incurs logging overhead and may expose private script data; keep its logs local. |
| `LUMEN_TRACE_BOUND_ARRAYS` | presence flag | With bound-call tracing, records native descriptor changes in small arrays owned by the first saved argument, before and after calls. Scans at most 16 own fields, 4 arrays of 16–512 entries, and retains at most 16 preview snapshots without retaining JS values. Useful for tracking corrupted working state without replacing JavaScript functions or arrays; keep the potentially private output local. |
| `LUMEN_FN_PROFILE` | presence flag | Opt-in interpreted function timing, including compiled execution entered through the interpreter; reports inclusive/self time and callers. Each reported entry also includes its internal compilation, activation, scope and same-realm state sampled on the first entry in that batch; this is not a count of fast-path misses. Adds overhead and is not an uninstrumented wall-time benchmark. `LUMEN_FN_PROFILE_FLUSH_SECONDS` sets the minimum elapsed time before a completed call can flush a batch. |
| `LUMEN_FN_PROFILE_SOURCE` | presence flag | With function profiling, include up to 16,384 source characters for each reported hot function (at most 32 per batch). This can expose private page code; keep captures local. |
| `LUMEN_JIT_CODE_BUDGET_MB` | integer, 1–1024 MiB | Process-wide requested executable-byte cap shared by JavaScript and native RegExp code. Read once; invalid values use the default (128 MiB). It does not reserve that much memory up front. The cap excludes page rounding and non-executable metadata. Function profiles and `LUMEN_PERF_METRICS` report live requested bytes, limit and rejected reservations. JavaScript bodies deferred for budget pressure retry only when their requested capacity becomes available; unsupported bodies remain on the checked VM. No live code is evicted or patched. |
| `LUMEN_GC_LOG` | presence flag | Logs post-collection object counts at allocation and task-boundary collections, with retained Realm, template, pin and host-settings counts. A task-boundary reclaimed count excludes objects already released by reference counting while the task unwound. |
| `LUMEN_GC_DUMP` | presence flag | Dumps bounded root-property/scope-name samples, object-kind and initial-root counts, root-array size buckets, kept/pool counts, largest array length and the current stack. Reports the six most frequent function bodies among at most 1,024 sampled distinct bodies, with omitted-instance counts and names capped at 80 characters. Counts describe the pre-sweep snapshot, not the final live graph. Uses internal metadata without invoking author hooks; may expose private names, so keep captures local. Adds diagnostic traversal overhead. |
| `LUMEN_GC_DUMP_MIN_OBJECTS` | non-negative integer | With `LUMEN_GC_DUMP`, skip snapshots below this object count. Missing/invalid values use zero. Useful for restricting output to large-heap pressure events. |
| `LUMEN_TRACE_CODE_POINT_ERRORS` | presence flag | Logs the already-converted invalid number, argument index and argument count for at most 32 `String.fromCodePoint` errors per process. Does not repeat coercion or alter exception behavior. Diagnostic numeric inputs may be private; keep captures local. |
| `TRUST_TRACE_FETCH` | presence flag | Logs every page fetch to stderr as `[fetch-trace] sync|async <METHOD> <url>`, followed by `[fetch-trace] result status=<N> type=<TYPE> len=<N>` when the response resolves through the unbuffered result path. Pair with `TRUST_LUMEN_TRACE` and `--js-diagnostics` to reconstruct a page's script/fetch timeline. |

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
cargo test --profile browser-check --lib core:: -- --test-threads=1
cargo test --profile browser-check --bin trust-desktop -- --test-threads=1
cargo test --profile browser-check --lib render:: -- --test-threads=1 --nocapture
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
