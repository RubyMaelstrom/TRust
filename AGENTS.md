# TRust Agent Guide

## README ownership

**`README.md` is user-owned and not agent-editable.** Do not change, append to,
reformat, or regenerate it unless the user explicitly asks for a README edit.
Feature work, bug fixes, and routine documentation updates do not grant that
permission. Put installation instructions in `INSTALL.md` and development
guidance in this file.

## PRIMARY BINDING RULE: OFFICIAL STANDARDS COME FIRST

**TRust is built to official standards. Before implementing a feature or fixing
a bug, you MUST consult and read the relevant authoritative web or protocol
standard through `web-standards-skill`. This is required, not optional. Use its
local specification library by default; a local official snapshot satisfies
this requirement.**

The skill's local library avoids repeatedly hitting standards organizations'
servers. Retrieve material online only when the relevant specification or
clause is missing locally, the local material cannot resolve the question, or
the user explicitly requests verification against current upstream text. A new
task does not by itself require a refresh or online freshness check.

For rendering and compatibility work, assume that a website exposing a problem
has found a standards-adherence defect in TRust. Investigate the applicable
standard and correct TRust's general behavior. Do not blame the website, dismiss
valid content as a site quirk, or add a site-specific workaround when a
standards-compliant implementation can solve the problem.

The required workflow is:

1. Identify every standard governing the behavior before changing code.
2. Use `web-standards-skill` to locate and open the local authoritative
   specification, then read the relevant algorithms, definitions, dependencies,
   and edge cases in context. Record the snapshot or revision when relevant;
   do not describe a local snapshot as verified current upstream text.
3. Prefer primary sources: WHATWG, W3C, IETF RFCs, ECMA-262/TC39, the WebAssembly
   specifications, and the official specification for the protocol involved.
4. Translate the normative algorithm into a general implementation. Preserve
   ordering, state transitions, error handling, and terminology from the
   standard where practical.
5. Add focused conformance-style regression tests, including interacting edge
   cases revealed by the normative text.
6. Record the specification and section in nearby code comments or the commit
   message when it will help future maintainers understand the behavior.
7. Run the focused test first, then the broader affected suite.

If the standard is unclear, reconcile its linked definitions and related
standards before choosing behavior. Existing TRust behavior and browser
behavior are useful evidence, but neither overrides normative requirements.

## Project Purpose

TRust is a terminal browser written in Rust. It supports HTTP(S), Telnet and
Telnet-over-TLS, Gopher, Gemini, Finger, WHOIS, DICT, WebSockets, terminal
graphics, JavaScript, Web Workers, and WebAssembly.

The project favors:

- Standards-correct, general implementations over compatibility hacks.
- Pure-Rust engines and libraries where practical.
- Hand-built, auditable protocol layers.
- Bounded CPU and memory use, with zero idle spin.
- Graceful degradation: one failed script, image, request, or background task
  should not destroy an otherwise usable page or terminal session.
- Geometry reported to page JavaScript that agrees with what the terminal
  actually paints.

## Architecture Map

- `src/main.rs`: process startup, terminal setup/restore, graphics protocol
  detection, panic handling, and application launch.
- `src/app.rs`: central event loop and application state; navigation, history,
  selection, input, scrolling, live-page messages, and image pipelines.
- `src/doc.rs`: protocol-neutral presentation model. Small-net and plain-text
  documents use styled lines; HTML uses positioned layout rows and items.
- `src/http.rs`: shared HTTP policy and hand-built HTTP/1.1 client, WebPKI
  HTTPS, redirects, connection pools, cookies, page/subresource caching, HTML
  parsing, form extraction, and full/incremental layout entry points.
  `src/http/http2.rs` integrates the Rust `h2` framing/HPACK engine with ALPN,
  multiplexed sessions, bounded streaming, cancellation, and safe retries.
  `src/http/http3.rs` adds partitioned Alt-Svc discovery and QUIC sessions;
  `http3_io.rs` bounds encoded metadata and guards stream cancellation.
  Both multiplexed transports share field semantics in `http/wire.rs`.
- `src/dom.rs`: html5ever-backed arena DOM, mutations, selectors, CSS parsing,
  cascade, computed values, serialization, and live geometry state.
- `src/js.rs`: JavaScript/browser contract and engine-neutral
  DOM/resource helpers. It delegates the resident page actor to
  `src/lumen_backend.rs`, backed by the maintained sibling Lumen checkout.
  A single resident actor owns the live DOM and JS realm and handles events,
  timers, fetch/XHR, modules, workers,
  WebSockets, storage, blobs, and WebAssembly.
- `src/layout2/`: the sole HTML layout engine.
  - `tree.rs`: styled DOM to CSS box tree.
  - `style.rs` and `value.rs`: typed style snapshots and CSS value resolution.
  - `flow.rs`, `inline.rs`, `flex.rs`, `grid.rs`, `table.rs`, `float.rs`,
    `replaced.rs`, and `intrinsic.rs`: layout in floating-point CSS pixels.
  - `inline/bidi.rs`: UAX #9 bidi paragraphs of an inline formatting
    context (CSS Writing Modes 4 `unicode-bidi` controls, HL1/P2-P3 base
    levels) and visual line reordering; `LineOut::pieces` stays logical.
  - `measure.rs`: fragment geometry for CSSOM View and observers.
  - `boundary.rs`: safe incremental-layout boundaries.
  - `paint.rs`: CSS painting order, cell compositor, clipping, fixed layers,
    regions, carousels, and the single CSS-pixel-to-cell quantization boundary.
  - `graphics.rs`: graphical (display-list) paint in CSS 2.2 Appendix E order.
    `graphics/paint_cache.rs` memoizes one `paint_fragment` call per fragment
    for the live page actor, keyed by the fragment's own data and validated
    against the Dom's paint journal (`dom/paint_journal.rs`); see its module
    comment for the complete list of reuse conditions.
  - `contract.rs`: `Row`, `Item`, regions, fixed items, geometry, terminal-cell
    metrics, and the public layout/rendering contract.
- `src/img.rs`: guarded raster/SVG decoding, sizing, tinting, alpha detection,
  compositing, and terminal graphics encoding.
- `src/media/`: inline audio/video: codec strings and MIME support
  (`codec.rs`, `container.rs`), container parsers (`isobmff.rs`, `webm.rs`,
  `demux/`), decoders (`decode/`: bundled rav1d/Symphonia, system FFmpeg),
  audio output (`audio/`), the per-element `player/`, HTTP range resources
  (`resource.rs`), Media Source Extensions (`mse/`) and the terminal's mpv
  handoff (`external.rs`). `src/media_host.rs` is the page actor's binding
  layer.
- `src/webaudio/`: the Web Audio rendering engine: render-quantum buses and
  channel mixing (`bus.rs`), AudioParam timelines (`param.rs`), the graph
  and its ordering (`graph.rs`), node processors (`nodes/`), PeriodicWave
  tables (`wave.rs`), each context's rendering thread (`engine.rs`) and
  decodeAudioData's decoding threads (`decode.rs`, `resample.rs`).
  `src/webaudio_host.rs` binds it to the prelude's Web Audio block.
- `src/ui.rs`: Ratatui rendering for browser documents, inline images, fixed
  layers, scrollbars, dialogs, and terminal sessions.
- `src/file_select.rs`: user-chosen files (accept filters, MIME types,
  directory expansion, snapshot-checked lazy reads); `src/file_picker.rs`:
  the terminal's file chooser.
- `src/core/`: shared browser controller and event delivery for desktop and
  headless clients.
- `src/bin/trust-desktop.rs`: winit window, native input, and presentation loop.
  Its modules: `tabs.rs` (tabs, their sleep and mute and the pages' requests
  for new ones), `window_chrome.rs` (the tab strip replacing the title bar
  and its partial repaints), `unload_prompt.rs` (the "leave this page?"
  question), `context_menu.rs` and `text_selection.rs`.
- `src/render/`: renderer-neutral display list, Parley-shaped text, Vello CPU
  reference renderer, Hybrid wgpu backend (Vello GPU), and windowless
  `trust::render::headless` pipeline. See module comments for backend limits.
- `src/telnet.rs`, `src/gopher.rs`, `src/gemini.rs`, `src/oneshot.rs`,
  `src/tls.rs`, and `src/ws.rs`: protocol implementations.
- `vendor/`: owned forks of wasmi and frontend/font/image dependencies.
  The Lumen engine is maintained separately at the sibling
  `../Lumen` checkout. A bug whose correct
  fix belongs in an engine should be fixed in its engine repository rather
  than hidden behind a downstream workaround.

## Important Design Invariants

- Layout geometry remains `f32` CSS pixels until `layout2::paint`; quantize
  edges to terminal cells exactly once at the paint boundary.
- Terminal layouts measure text in the terminal's cell font
  (`src/text/cells.rs`): one cell per narrow grapheme, one row per text line.
  Terminal page actors set it for their thread; terminal-side static, CSS-only
  and patch layouts scope it at their entry points. Graphical frontends and
  canvas text never enable it, so their layout stays byte-identical.
- Widths resolve top-down from containing blocks; heights resolve from content.
- JavaScript geometry comes from layout fragments, not reconstructed painted
  text.
- The DOM/CSS cascade is the single style authority. Layout consumes typed
  snapshots rather than reparsing declaration strings.
- HTML output uses positioned `Row`/`Item` data. Gopher, Gemini, one-shot
  protocols, and plain text use the simpler line model.
- One page engine (actor, Lumen Agent) runs each browsing context group: a
  tab's pages, plus the auxiliary browsing contexts they open (pop-up tabs,
  which share their opener's engine). Navigation drops the old page actor and
  its heap, unless the tab shares its group with another tab: then the next
  Document is created in the group's engine (`js::spawn_page_in`). At most
  one pre-warmed spare engine (`src/page_spare.rs`) waits beside them; it has
  run no page code, and exactly one navigation claims it and creates its
  Window there exactly as in a new engine.
- A group's top-level traversables each have their own Dom arena (their
  frames' Documents stay in it); node identities are unique across a group's
  arenas (`Dom::share_identities`), and `dom.document()` is an arena's
  Document (node 0 only in an engine's first arena). The actor and HostState
  hold the selected traversable's fields and park the others'
  (`select_traversable`); every host call selects the traversable its first
  node argument or its calling Realm belongs to (`routed_host_call`), and host
  tasks the one their content names (`host_task_traversable`). A single
  traversable never switches. The prelude's `topDocumentId` stands for the
  top-level Document; never write a literal 0 for it.
- Incremental layout is an optimization. If a patch cannot be proven safe,
  fall back to the always-correct full relayout.
- The retained graphical paint is an optimization of the same kind: a cached
  fragment is replayed only when its output provably equals a fresh build's.
  Every Dom mutation that bumps the epoch must either name the nodes it
  changed in the paint journal (`mark_dom_revision_attributed` plus
  `paint_journal.note`) or advance the global stamp (`mark_dom_revision`, the
  default). `TRUST_PAINT_VERIFY=1` and every test compare the cached paint
  with a full one; anything a leaf reads that the reuse conditions do not
  cover must be added there or taint the chunk (`paint_cache::taint`).
- Image intrinsic sizes must be shared with the live page geometry pass so DOM
  measurements match rendered boxes.
- Background panics and per-resource failures must remain contained; only the
  terminal-owner thread may tear down the TUI.
- `data-trust-*` attributes are TRust's presentation channel from the live
  arena to its serialized snapshot. The serializer writes them from arena
  state and drops authored attributes with that prefix. Read them only through
  `Dom::internal_marker` (or `StyleSource::internal_marker`), which ignores
  them in a live arena, where such an attribute is page content. Never store
  internal state as a content attribute; keep it in arena state and serialize
  a marker.
- Author script never reaches the host boundary. `js::private_bootstrap` captures
  every host binding lexically and deletes all `__` globals before author code
  runs; prelude code must use the captured bindings, not `g.__name` lookups at
  run time. Rust reaches a Realm's prelude only through `platform_control()`,
  never by evaluating `__trust` snippets. Wrapper state that carries host
  identity (node ids, child Windows, store handles) lives in internal slots,
  and DOM host operations refuse nodes of cross-origin Documents. All other
  platform state of author-reachable objects (wrappers, collections, events,
  observers, CSSOM, fetch/XHR, IndexedDB, …) also lives in internal slots
  (`internalsFor`), and internal operations are closures rather than
  prototype methods: no own or inherited `__…`/`…trust…` property is visible.
- Keep session caches bounded. Avoid copying large scripts, DOMs, images, or
  bytecode unnecessarily.

### Desktop Optimization Boundary

When optimizing `trust-desktop`, generally treat code paths already exercised by
the optimized terminal frontend—including the DOM and CSS cascade, JavaScript
page actor, protocol/resource engines, and shared box-tree/fragment layout—as
stable inputs. Begin diagnosis at the first divergent desktop stage: its browser
controller and presentation adapter, `layout2::graphics`, image scheduling and
animation, scene/damage construction, raster backends, and native event/render
loop. Do not change shared behavior merely to compensate for a desktop
bottleneck unless evidence shows that the shared path is itself responsible and
regression testing protects the terminal browser's correctness and performance.

Judge this boundary by execution path, not source-file location. Desktop-only
adapters live inside shared library modules such as `core`, `http`, and
`layout2`, while the terminal `App` and desktop `BrowserController` are distinct
orchestrators. The resident page actor owns the canonical live DOM; either
frontend may derive a non-authoritative presentation arena or snapshot for
layout and painting without creating a second source of DOM semantics.

This is a strong preference, not an absolute prohibition. Standards-correctness
fixes and actual browser features—such as new CSS behavior, JavaScript APIs, or
DOM functionality—belong in the canonical shared implementation and should
benefit both frontends.

## Development and Verification

Cargo uses the sibling Lumen checkout (`../Lumen/crates/lumen`) directly;
TRust and Lumen are developed together on their `main` branches while
host-boundary work is upstreamed. TRust supplies networking and TLS.

Build and test from the repository root:

```sh
cargo build
cargo test
cargo test --release
cargo clippy --all-targets
cargo build --release
```

Before handing off browser changes, run the debug and release tests, Clippy,
and the release build. Exercise affected sites with the release binaries;
a debug test pass or release startup check does not verify rendering or
performance. Documentation-only edits do not need a browser rebuild.

Lumen is the only JavaScript engine and is always compiled. No backend-selection
features or comparison targets exist. `--all-features` also enables the synthetic
`trust-lumen-spike` benchmark harness.

Ordinary release builds produce `trust`, `trust-desktop`, and `trust-headless`.
`trust-browser-replay` is a developer-only example target, not a distributed
executable. Run it explicitly with
`cargo run --release --example trust-browser-replay -- FIXTURE.html`.
Release uses optimized, incremental, parallel code generation without LTO so
acceptance testing and local installation stay fast. Public numbered releases
use `cargo build --profile numbered-release --locked`; that explicit profile
uses Fat-LTO and one codegen unit and is not part of the ordinary development
or local-promotion cycle. Native ARM64/x86-64 Linux and Windows cross-build
instructions are in `INSTALL.md`; use the same Lumen checkout for each. With Wine installed,
`cargo xwin test --target x86_64-pc-windows-msvc` runs the Windows tests.

Focused checks and diagnostics:

- `python3 tools/check_css_wpt.py` runs a pinned official WPT subset for CSS
  variables, registered properties, and background shorthands in the release
  headless browser. It caches upstream sources (`--offline` reuses them), writes
  JSON and browser logs to `target/css-wpt-results`, and fails for failed tests
  or incomplete harness runs. Keep the full failure results.
- `cargo test --lib webgl -- --include-ignored` runs hardware-dependent WebGL
  pixel, shader, and API tests.
- `cargo test --lib paint_cache::tests::fuzz_soak -- --ignored --nocapture`
  runs the retained-paint differential fuzz test for many seeds
  (`TRUST_PAINT_FUZZ_SEEDS=first..last`, `TRUST_PAINT_FUZZ_STEPS=n`); a failure
  prints the seed and the operation log. `TRUST_PAINT_VERIFY=1` does the same
  comparison against a live page in the release browser.
- `src/fixtures/number_input.html` is the manual number-input acceptance page.
- `DIAGNOSTICS.md` contains environment variables, ignored live-site gates,
  headless tools, and benchmark commands. `TRUST_DESKTOP_TRACE=1` enables frame
  timings; `desktop_pipeline_bench` covers text, flex/grid, compositing, long
  scrolling pages, DOM mutation, images, and graphical Telnet.

### Approved Release Promotion

The user tests TRust from the release target. Build `target/release/trust` with
`cargo build --release` for acceptance testing; do not substitute a debug build
when reproducing or validating user-visible behavior.

Do not substitute the `numbered-release` profile for routine local promotion.
Build that Fat-LTO artifact only when the user explicitly requests a public,
numbered release; its binaries land under `target/numbered-release/`.

Do not overwrite the installed executable merely because a release build
succeeds. Wait until the user explicitly says that the build is tested,
approved, and promoted. Once promotion is authorized:

1. Confirm that `cargo build --release` succeeded for the current source tree.
2. Install that exact artifact with
   `install -m 0755 target/release/trust /home/ruby/.local/bin/trust`.
3. Verify that the built and installed files are byte-identical with `cmp` or
   matching `sha256sum` output.
4. Review `git status`, `git diff`, and `git diff --check`, then commit the
   approved source changes in coherent commits.
5. Push only when the user explicitly requests it.

During development:

- Run `cargo fmt --check` and `cargo clippy` when the affected scope makes them
  useful.
- Run a focused test by name while iterating, then the relevant module or full
  suite before handoff.
- Use `git diff --check` before committing.
- Keep regression tests close to the implementation; this repository primarily
  uses inline `#[cfg(test)]` modules.
- Preserve the user's unrelated working-tree changes.
- Do not edit vendored code mechanically or update vendored dependencies unless
  the task specifically requires it.

The default features enable mimalloc. A build without that
allocator uses `--no-default-features`; Lumen remains enabled. Several
performance diagnostics and safety switches are controlled by `TRUST_*`
environment variables documented near their use.

## Runtime Contracts and Known Limits

These notes preserve development context formerly in the README. They describe
current behavior and limits; consult the governing standards before changing it.

### Desktop and web APIs

- HTTPS learns HTTP/3 endpoints from authenticated Alt-Svc response headers
  and otherwise prefers HTTP/2 via ALPN, with HTTP/1.1 fallback. QUIC uses
  TLS 1.3/WebPKI for the original origin, never the alternative hostname.
  Its 128-entry RAM-only discovery cache is partitioned like connections;
  blocked UDP incurs one short foreground wait and a failed-path cooldown.
  Discovery via DNS HTTPS records and HTTP/2 ALTSVC frames is not implemented.
  Cleartext HTTP and RFC 6455 WebSockets remain HTTP/1.1; cross-origin
  connection coalescing and WebSockets over extended CONNECT are unimplemented.
  HTTP/2 and HTTP/3 server push are disabled. HTTP/3 uses stateless QPACK
  (zero dynamic-table capacity), without 0-RTT or WebTransport/datagrams.
  Networking policy is shared by all HTTP
  transports; connections are keyed by origin, available top-level schemeful
  site, credentials, and owning runtime. Canceling multiplexed work resets
  only its stream. Never blindly replay a possibly processed POST after
  network failure. The owned h3-quinn adapter fixes pending-read cancellation
  without copying receive buffers; keep its focused regression coverage.
- CSS Fonts 4/5 properties beyond family/size/weight cascade in
  `dom::properties::fonts` and reach shaping as a typed `FontTypography`
  (`src/text/typography.rs`), resolved into Parley features and variations
  in #feature-variation-precedence order (`opsz` follows the used size by
  default). Small capitals and superscripts/subscripts a font lacks are
  synthesized in `text.rs`, which maps offsets back to the source text and
  keeps the element font's strut; the terminal cell font ignores all of it.
  Per-face `@font-face` settings (`size-adjust`, feature/variation
  descriptors, named instances) and `font-size-adjust` re-shape the runs of
  the affected fonts; metric overrides rewrite the face's `hhea`/`OS/2`.
  The vendored fontique matches with descriptor and axis ranges. Navigation
  waits for each face through its `font-display` block period (a one-shot
  CSS-only render waits through its swap period); later faces reach the live
  page through `Dom::face_font` until their failure time. Faces first used
  by dynamically added rules draw fallback text (not invisible text) during
  their block period. Not implemented: `@font-feature-values` (the
  functional `font-variant-alternates` values select nothing),
  `font-language-override`, `font-palette`, the super/subscript override
  descriptors, CSS-connected `document.fonts` faces and FontFace loading,
  `size-adjust` in SVG text, and transitions of the new properties.
- XMLHttpRequest, WebSocket and EventSource are shared Window/Worker platform
  blocks (`/*__XHR_BEGIN__*/`, `/*__WEBSOCKET_BEGIN__*/` and
  `/*__EVENTSOURCE_BEGIN__*/` in `src/js_platform.js`, assembled for workers by
  `js::worker_prelude()`); each realm supplies only an adapter. Workers may
  send synchronous XHR, which blocks only the worker thread; `responseXML` and
  the "document" response type are Window-only. A worker's WebSockets follow
  its owner document's subresource policy, arrive as host tasks of the worker
  loop and close when the worker ends. Asynchronous XHR reads its body as it
  arrives (LOADING, `readystatechange` and `progress` at most every ~50 ms);
  a synchronous XHR receives it whole, so its timeout is detected only after
  the response. Responses carry the HTTP/1.x reason phrase as `statusText`
  (empty over HTTP/2 and HTTP/3) and their header list in wire order with
  duplicates (`http::Headers`, `http::header_list_get`); scripts' Headers and
  `getAllResponseHeaders()` sort and combine it. Header values are byte
  sequences, held isomorphic decoded (one char per byte, the ByteString form
  script sees: `http::isomorphic_decode`/`isomorphic_encode`) through the
  transports and the cache, and written to the wire as those bytes; consumers
  read them per their specs (Location's bytes are percent-encoded for the URL
  parser, a Set-Cookie line and the Cookie header are UTF-8 text, a
  Content-Disposition filename is UTF-8 when its bytes are). XHR upload
  listeners set the request's use-CORS-preflight flag
  (`Request::use_cors_preflight`), which forces a CORS preflight for a
  cross-origin request. Successful preflights fill the Fetch CORS-preflight
  cache (`PREFLIGHT_CACHE`, keyed by top-level site, origin, URL and
  credentials, `Access-Control-Max-Age` capped at 2 h, 512 entries, cleared on
  a CORS failure and with site data).
- Script `fetch()` and asynchronous XHR responses resolve at the response
  header section; HTTP/1.1, HTTP/2 and HTTP/3 bodies then stream
  (`http::fetch_script_streaming`, `http/body_stream.rs`) with incremental
  content decoding (`http/content_coding.rs`: gzip, deflate, br, zstd) into
  the body's ReadableStream, which pulls from a per-response host buffer
  (`LumenHostTask::FetchHead`/`FetchBodyChunk`/`FetchBodyEnd`,
  `__http_fetch_pull`). The transfer suspends while more than 1 MiB waits
  unread and resumes below 256 KiB. Aborting the fetch, canceling or
  garbage-collecting its stream, or unloading its document cancels the
  transfer (HTTP/2 RST_STREAM CANCEL, HTTP/3 stop, HTTP/1.1 close); a body
  read to its end returns an HTTP/1.1 connection to the pool. Redirect and
  421 bodies, integrity-checked fetches, cached page resources and
  browser-owned subresources still use whole bodies. Resource Timing reports
  at the end of the body.
- As soon as an HTML document arrives, the speculative parser
  (`http::prefetch_parser_scripts`) fetches its parser-inserted classic and
  module scripts (with each module's static graph) and modulepreloads beside
  the style sheets, as the elements would request them (`<base href>`,
  `crossorigin`, `referrerpolicy`); responses only fill the page cache, so a
  script runs only when its prepared element's own request finds one. Their
  timing reports wait in the cache until the page actor takes them. A
  module's static imports start fetching when its response arrives, and an
  `import()` in any same-agent Window starts its fetch while `import()` runs.
- EventSource (HTML #server-sent-events) runs over that streaming fetch: a
  potential-CORS request (`withCredentials` selects credentials "include"),
  `Accept: text/event-stream`, cache mode "no-store", initiator type "other".
  Non-200 statuses and other MIME types fail the connection; the end of the
  body or a network error reestablishes it after the reconnection time
  (initially 3 s, set by `retry`) with `Last-Event-ID`. The last event ID
  persists across reconnections. Events are queued on the Window's remote
  event task source (the worker's platform task queue); an event stream's
  unfinished line plus data buffer is capped at 8 Mi code units, beyond
  which the connection fails. data: and same-origin blob: URLs work.
- `Worker` is the shared `/*__DEDICATED_WORKER_BEGIN__*/` block, exposed in
  Windows and in dedicated and shared worker globals (`SharedWorker` stays
  Window-only, as HTML exposes it). A nested worker's tasks reach its owning
  worker's inbox, it inherits that worker's origin and secure context, and it
  ends when its owner ends (the owner's handles drop). Its script request's
  referrer is the owning worker's URL under that worker's referrer policy (from
  its script response's `Referrer-Policy`, inherited for blob:/data: scripts);
  a worker's own fetches use its URL and policy, and blob:/data: workers send
  no referrer.
- The File API blob URL store is the host's (`src/blob_url_store.rs`), one per
  page (`PageEnv.blobs`, also what frontends present `blob:` images from) and
  shared by the page's Windows, frames and workers through the shared
  `/*__BLOB_URL_BEGIN__*/` block and `__blob_url`. Entries keep in-memory
  bytes or still-unread selected-file ranges (`file_select::FileRange`, read
  lazily by whichever agent consumes them) and their environment; resolution
  and revocation check the storage key (an opaque origin matches only
  itself). URLs are resolved when parsed (Request construction, XHR `open()`,
  Worker construction, element preparation, frame navigation), so a later
  revocation does not fail them. A frame Window's entries go when its realm
  is released and a worker's when it ends; the top-level Document's go with
  the page's store. `<img>`, `<script>`, style sheets, preloads, frames and
  module scripts (static and dynamic imports, import-map targets,
  modulepreload, module workers' imports; `ModuleFetchEnvironment`) resolve
  `blob:` URLs for their own partition, and modules need a JavaScript type.
  Top-level navigation to a `blob:` URL (links, `location`, the address bar)
  loads the entry's bytes with its type in both frontends
  (`http::blob_navigation_response`) without the partition check; the page
  actor holds the entry the navigation's URL parse resolved
  (`NavigationInitiator::blob`), the address bar resolves in the current
  page's store, and the Document's origin is its URL's (the creator's).
  Without a live entry (a reload after the creating page is gone) it fails.
- Referrer Policy: every Document has a policy container referrer policy,
  from the navigation response's `Referrer-Policy` (last valid token),
  inherited by local (about:, blob:, data:, srcdoc) Documents from their
  initiator or parent, and updated by each inserted or changed
  `<meta name=referrer>` (legacy never/default/always/
  origin-when-crossorigin), tracked by the DOM (`referrer_meta_updates`) and
  per realm in `HostState::referrer_policies`. One implementation
  (`ReferrerPolicy::determine`) serves fetch/XHR (`Request.referrer` and
  `referrerPolicy`), sendBeacon, EventSource, workers, element requests
  (`referrerpolicy` on a, area, img, iframe, link, script), CSS (imports and
  static-pipeline fonts use the sheet's location and its own
  `Referrer-Policy`; CSS images and live-DOM fonts use the Document's URL),
  frames, module imports (the importing script's URL), redirects
  (`Request::referrer_policy`), the static pipeline (`document_referrer_policy`)
  and frontend navigations (`http::NavigationInitiator`: the actor reports a
  hyperlink's or form's `noreferrer`/`referrerpolicy` with each
  `PageEvt::Navigate`/`Replace`/`SubmitForm`; static documents use
  `Doc::referrer_policy` or the response's). Elements with their own policy
  keep a page live so the actor applies it. `document.referrer` is the
  navigation request's final `Referer`.
- IndexedDB and Cache Storage are shared blocks (`/*__INDEXEDDB_BEGIN__*/`,
  `/*__CACHE_STORAGE_BEGIN__*/`) in Windows and workers over the storage shed's
  per-storage-key bottles, so every agent of an origin (within one storage
  partition) sees the same databases and caches. Commits use `__storage_cas` compare-and-set retries (a read/write
  transaction replaces only its scope's object stores). `indexed_db_host.rs` is
  the shed's coordinator for connection queues, connections, the
  versionchange/blocked handshake and cross-agent transaction scheduling; it
  queues database tasks into each realm's inbox (woken like port messages) and
  releases a realm's or agent's connections, transactions and requests when it
  ends. Each realm runs its database tasks in one FIFO. Microtasks do not run
  between IDB event listeners, so a promise continuation after an IDB request
  finds its transaction inactive.
- ReadableStream, WritableStream and TransformStream are transferable through
  `postMessage`, MessagePorts and `structuredClone` (Streams
  #transferrable-streams): the PORTS block runs the transfer and
  transfer-receiving steps over an internal entangled port whose endpoint is
  disposed when the cross-realm transform disentangles it.
- `--renderer=auto` selects Hybrid only after surface, adapter, device, and
  capability initialization succeed; recoverable later failures fall back to
  Vello CPU. `--renderer=cpu` forces the reference renderer, while
  `--renderer=hybrid` reports initial GPU failure. Backend selection must not
  change DOM, CSS, layout, hit testing, or display-list output.
- Page text selection (Selection API §2 "User Interactions", CSS UI 4
  `user-select`, CSS Pseudo 4 `::selection`): layout attributes every glyph
  run's text to the DOM Text data it lays out (`RunSelection` spans of
  `TextSourceSpan`, `layout2::inline`'s `SourceCursor`), so selections are
  anchored to `TextPoint`s (Text node, UTF-16 offset), never to display-list
  indices. trust-desktop's primary press, drag (with autoscroll), double and
  triple click, Shift+click and Ctrl+A select; a hyperlink's press stays its
  activation unless Alt is held. A live page owns its Document's selection:
  the gesture rides `PointerMetadata::selection` and runs as the default
  action of mousedown/motion/release in the prelude (`userSelectionPress`,
  one `selectstart` per gesture, user-select none/contain/all constraints);
  the actor reports what the selection selects (`dom/selection.rs`,
  `PageEvt::Selection`) after a gesture, a script change, or a DOM change
  under a selected range, and the frontend paints its own gesture until the
  report has caught up with the gesture's serial. Static pages and line
  documents (whose lines are synthetic sources) select in paint order in
  the frontend alone. `Scene::append_page_with_selection` paints the
  highlight in paint order directly below each selected run: without author
  colors a translucent scheme `Highlight` under text that keeps its color,
  with author `::selection` color/background-color (highlight inheritance,
  paired defaults) exactly those, slicing runs at cluster boundaries;
  replaced content gets a wash. `::selection` text-shadow and decorations
  are not implemented. Copying runs a live page's copy action
  (`PageCmd::Copy`, `trust.userCopy`: `copy` event, else the range's
  rendered text) and otherwise the frontend's text, where soft wraps join
  with a space (`LineStart`). The terminal frontend has no page selection.
- trust-desktop's context menu (`src/render/context_menu.rs`,
  `src/bin/trust-desktop/context_menu.rs`) is browser chrome like COMMAND:
  Open in mpv (over a media element), Open in New Tab and Copy Link (over a
  hyperlink), Copy (with a selection), Reload, Open COMMAND, Kill Page (live
  pages) and Add/Remove Bookmark (`bookmarks::Operation::Lookup`/
  `RemoveUrl`). HTML #attr-media-controls puts such media features in "the
  media element's context menu", so they do not interfere with the page's
  rendering: graphical paint names each hit-testable media element's
  external playback target (`PagePaint::media_targets`, from
  `layout2::media_target`), and the menu takes the topmost one among every
  box under the pointer (`Scene::media_target_at`), so a player's overlays
  do not hide its video. Nothing over the page opens mpv. Pointer
  Events #maybe-show-context-menu decides: a secondary release (or the menu
  key / Shift+F10, as the key's default) fires `contextmenu`, and the menu
  shows when the page's answer (`PageEvt::ContextMenu`) is uncanceled;
  Shift+secondary click bypasses the page and fires none. While it shows it
  takes the pointer and keys and is an AccessKit menu; opening, highlighting
  and closing repaint only its rectangle over the retained scene
  (`draw_partial`), never recomposing or relaying out the page. Telnet keeps
  the secondary button for the remote host.
- COMMAND overlays the page without changing layout or the JavaScript viewport.
  Its image-cache readout counts decoded pixel bytes, excluding GPU copies,
  chrome, and other allocations; do not add heap scans or a reporting timer.
- Fragment navigation handles percent-encoded IDs and legacy named anchors,
  preserves the live page/scripts, updates the address, and restores scrolling
  through Back/Forward. Desktop pointer lock releases on Escape, focus loss, or
  navigation; the terminal frontend reports it unsupported.
- The frontends own the top-level traversable's session history and report
  its index and length, with how many of the page's `PageEvt::HistoryUpdate`s
  they applied, as `PageCmd::SessionHistory`; `history.length` reads it,
  replaying the updates not yet applied as HTML's best guesses.
- `window.close()` runs HTML's checks on a top-level traversable: "is
  closing", script-closable (created by web content, or a reported session
  history of one entry; until the first report it is not), familiar with
  (an auxiliary browsing context's opener included), and allowed by
  sandboxing to navigate. Child navigables' WindowProxies do nothing. The
  definitely-close task asks the frontend to close it
  (`PageEvt::CloseTraversable`), which trust-desktop does as for its user's
  close (beforeunload first; the last tab quits). Frontends without tabs (the
  terminal, trust-headless) present the browser itself as the traversable:
  the task logs a console warning and clears "is closing".
- Tabs (trust-desktop): a strip in place of the title bar (window moves,
  minimize/maximize/close, scroll arrows on overflow), Ctrl+T, Ctrl+W,
  Ctrl+Tab/Ctrl+Shift+Tab (not for a Telnet session in character mode),
  Ctrl+Shift+T (this session's closed tabs), Ctrl+Q. Tabs fill the strip's
  height; the empty strip beside them moves the window. A tab playing sound,
  or muted, shows a speaker that mutes or unmutes it in place (HTML
  #effective-media-volume: the user overrides the volume): the controller's
  `TraversableState::audio_muted` (`PageCmd::AudioMuted`, carried to the
  tab's next Documents and through its sleep) gives every player and audio
  context of the traversable a `media::audio::Mute`, which turns its device
  output to silence while it renders and its clock runs on. Pages cannot
  observe it (their elements' `muted` and `volume` are unchanged); a moved
  media element takes its new traversable's mute. A background tab unseen
  for 10 minutes sleeps (Page Lifecycle discard: freeze, then its engine
  goes; its history wakes it), except a Telnet session, an audible page the
  user has not muted, one with unsubmitted form edits, a loading one, one
  being asked whether to leave, and one sharing its browsing context group;
  a muted page's timers are throttled while hidden as a silent one's are.
  Pages unload (pagehide, unload, keepalive beacons outliving them) when
  their tab closes or navigates; beforeunload asks the user through
  `UnloadPrompt`.
- HTML's rules for choosing a navigable in trust-desktop: `_blank`
  hyperlinks, Ctrl/middle clicks and `noopener` requests open a tab in a new
  browsing context group (`PageEvt::OpenTraversable`); `window.open` and
  named targets without noopener create an auxiliary browsing context in the
  page's own group (`__traversable_create`): an initial about:blank Document
  with the creator's origin, URL as referrer, policy container and base URL,
  a copy of its storage shed (sessionStorage is per traversable,
  `session:<shed>:<origin>`), target name and popup state. Its frontend learns
  of it from `PageEvt::OpenAuxiliary` and keeps its `TraversableLink` while it
  shares the group; a navigation result without a page actor (a download, an
  image, another protocol) leaves the group, as a browsing context group
  switch would, and the opener's view of it reports `closed`. Each Realm
  holds one WindowProxy per other traversable that follows its navigations
  (`traversableWindowProxy`, cross-origin views as for frames); a raw Window
  reached through another Realm (`popup.frames[0].parent`) is not that
  object, as a Lumen WindowProxy exotic object would make it. `opener`,
  `name` (reset on cross-origin Documents of non-auxiliary traversables),
  `BarProp.visible`, `focus()` (shows the tab when the caller's user just
  interacted) and named lookup across the group's traversables and their
  frames follow HTML. Nodes inserted into or adopted by another traversable's
  Document move between arenas with their identities and state
  (`dom/transplant.rs`), only once the insertion is known to be valid. The
  source arena then names none of them, as after a collector sweep: work
  still owed to them is handed over, their running CSS transitions and
  animations are canceled there, and the Documents of child navigables they
  contained stay behind, destroyed. A script that `document.write()` inserts
  into a Document without an active parser is appended but does not run
  (there is no script-created parser).
- SVG presentation attributes are author declarations of specificity zero
  before every author sheet (`svg_presentation_attributes` in `src/dom.rs`),
  except `transform` and its gradient/pattern forms, which only the SVG
  renderer reads.
- CSS system colors (`Canvas`, `LinkText`, …) compute to the colors the
  frontend paints: the element's light or dark scheme palette graphically,
  `crate::theme` on threads using the terminal cell font
  (`dom::color_scheme::system_color`).
- Escape on a live page is HTML's close request in both frontends
  (`PageCmd::CloseRequest`, answered by `PageEvt::CloseRequestHandled`): the
  actor exits fullscreen, else fires the Escape keydown (canceling it consumes
  the request), else processes the close watchers (modal dialogs by `closedby`,
  auto/hint popovers, `CloseWatcher`, grouped per HTML's user-activation
  rules). Only an unconsumed request makes the frontend stop loading while the
  page and its scripts keep running: the frontend cancels its navigation and
  image work, and `PageHandle::stop_loading` sends `PageCmd::StopLoading`, a
  task in which the actor runs HTML #nav-stop exactly as `window.stop()` does
  (`stop_loading_navigable`): the ongoing navigation the running task
  requested is dropped, descendant navigables' Documents are aborted by queued
  tasks, and the Document is aborted (#abort-a-document). Aborting cancels its
  fetch group: pending `fetch()` rejects with an "AbortError" DOMException,
  bodies still arriving error with it, XHR fires readystatechange/abort/loadend
  inside the stop (a body already loading ends with "error", per XHR's
  processBodyError), an EventSource whose response has not arrived fails, a
  connecting WebSocket fails (established ones stay open), keepalive fetches
  and beacons continue, and the browser's own subresource and image fetches
  and frame navigations are canceled (their elements see a failed load).
  Queued tasks of canceled script fetches are discarded. Workers are
  unaffected, including one still fetching its own script: that is a
  non-subresource request (its reserved client is the worker), which Fetch
  keeps out of the Document's fetch group. A page-initiated cross-document navigation or form submission
  runs the same abort for the old Document and its descendants before the
  navigation leaves the actor (`abort_for_navigation`), so its in-flight
  fetches abort while it is alive. While the top-level parser still runs (`window.stop()` from
  a parser script or a task during parsing) it is aborted: the tree builder's
  nodes after the parser's position are removed, readiness goes
  "interactive" then "complete", remaining and deferred parser scripts never
  run, DOMContentLoaded and load never fire, and `document.write()` is
  ignored. Other modes keep their own Escape meaning. COMMAND `stop` stops
  loading the same way; `kill` drops the live page and its scripts.
- Showing popovers, modal dialogs and fullscreen elements share the arena's
  ordered top layer (`Dom::top_layer`): each generates a root-sibling box above
  its `::backdrop` (CSS Position 4). Without `overlay` transitions, removal from
  the top layer is immediate. A modal dialog blocks its document; other nodes
  are inert to hit testing, focus and activation in both frontends.
  Presentation arenas carry this state as `data-trust-*` markers that the live
  arena never trusts. Fullscreen needs transient activation and the
  `fullscreen` policy (iframe `allow`/`allowfullscreen`, default 'self').
  trust-desktop enters borderless window fullscreen and hides its overlays; the
  terminal hides its address and status rows so the page fills it;
  trust-headless refuses. Each frontend states whether it can present
  fullscreen (`WindowEnvironment::fullscreen_supported`, `PageEnv`), which is
  the page's "fullscreen is supported": `document.fullscreenEnabled` is false
  and requests fail at once in trust-headless. A keyboard lock is recorded but
  never applied.
- `<track>` elements load WebVTT (`src/js_platform.js`'s WebVTT file parser)
  into their text tracks through HTML's track processing model; automatic
  text track selection honors only the `default` attribute (there are no
  caption preferences). The arena journals track element parent changes
  (`src/dom/media_tracks.rs`) so parser-created tracks get their addtrack
  events and selection. Cues are never rendered: no media plays inline.
- `filter: url()` (also inside `backdrop-filter`) resolves an SVG `filter`
  element into `render::FilterGraph`. Groups with such graphs, and
  `backdrop-filter` Backdrop Root Images, are rasterized offscreen by the
  CPU reference renderer and filtered by `render::filter_graph`; Hybrid
  draws those CPU pixels, so backends agree. Offscreen work is bounded (64
  renders per frame, nesting depth 4, 4 Mpx buffers at reduced resolution
  beyond); over budget, content paints unfiltered. `feImage` draws page
  images and referenced elements (as a generated document image); taint
  follows the URL's origin, so a tainted map makes `feDisplacementMap` a
  pass-through. `clip-path: url()` and `mask-image: url()` resolve
  `clipPath`/`mask` through usvg geometry and a generated mask image. The
  terminal frontend ignores filters, backdrop filters and masks; clip paths
  clip by their bounds.
- External resource documents named by `filter`, `clip-path`, `mask-image`
  and canvas `filter` (`doc.svg#id`) are fetched with CORS Anonymous
  requests by the page actor (`take_svg_resource_requests`) or the
  script-free load, into a bounded process store keyed by document origin
  (`dom::svg_external`: 16 documents, 16 MiB, no `file:`); arrival advances
  `svg_sprite_revision`. Until loaded a filter or clip-path applies nothing
  and a mask layer is transparent black. `mask-image` images and
  `crossorigin` feImages are CORS requests: their image source carries a
  secret marker (`img::cors_image_source`) that the page actor and
  `fetch_presentation_image` turn into a CORS fetch.
- WebGL 1 uses lazy-loaded EGL/OpenGL ES with Rust shader preparation and
  validation, without bundled ANGLE. A missing robust context makes
  `getContext("webgl")` return `null`. WebGL 2 and multisample antialiasing are
  unimplemented. Estimated buffer/image storage has a shared 256 MiB page budget
  and at most 16 contexts; driver overhead and CPU copies are additional.
  OffscreenCanvas WebGL contexts work in Windows and workers (EGL bindings are
  per thread); the budget and context limit are per agent.
- Canvas elements and `OffscreenCanvas` (Window and every worker) share one 2D
  implementation (`src/canvas*.rs`, the prelude's `CANVAS` block, which the
  worker prelude reuses); contexts are "2d", "bitmaprenderer" and "webgl"
  ("webgl2"/"webgpu" give `null`). An OffscreenCanvas's native bitmap belongs to
  its agent and is freed with the object; a transfer moves only its data holder
  (dimensions, inherited language/direction, weak placeholder link) through a
  bounded single-use key registry. `transferControlToOffscreen()` placeholders
  present the newest frame from a single-slot mailbox, pushed at the owning
  agent's rendering update: the page in `extract_live`, a worker after each task
  and at its `requestAnimationFrame` opportunities (dedicated workers only).
  Unallocatable 2D backing storage is a lost context: the context lost steps
  fire `contextlost`/`contextrestored` at the next rendering update.
- Canvas text honors letterSpacing/wordSpacing (resolved when text is prepared),
  fontKerning, fontStretch, fontVariantCaps (small capitals synthesized at 0.7em
  when the font lacks the feature), textRendering (`optimizeSpeed` drops kerning
  and optional ligatures), lang and the element's HTML directionality. The `font`
  setter sets its stretch/caps longhands and resets kerning, as the CSS shorthand
  does. TextMetrics baselines come from the font's BASE table when present.
- Canvas `filter` evaluates CSS filter functions natively by pulling exactly the
  region of the infinite source each step reads (blur σ is capped at 256, like
  shadows' limit). Same-document `url()` SVG filters run through resvg with the
  canvas as reference box; external documents are never loaded, so they draw
  unfiltered, and ids containing spaces, quotes or parentheses cannot be named.
- Capability APIs report only what TRust provides. There is no permission
  prompt: "notifications" and "persistent-storage" start as "prompt"
  (`Notification.permission` "default"), a request records "denied" in the
  page-scoped permission store shared with dedicated workers, and non-secure
  contexts report "denied". Notifications never show (they fire `error`;
  `maxActions` is 0). `navigator.storage` estimates the origin's localStorage,
  IndexedDB and Cache Storage usage against their summed quotas; buckets are
  best-effort and OPFS `getDirectory()` rejects. No credential type, capture
  device, display source or speech voice exists. Clipboard reads are never
  permitted; writes with transient activation go through
  `PageEvt::ClipboardWrite` to trust-desktop (arboard) or the terminal
  (OSC 52), and trust-headless refuses them. `navigator.serviceWorker` has no
  registrations and `register()` rejects with `SecurityError`. `SharedWorker`
  shares one agent per origin, URL and name within the page.
- Audio and video (HTML #media-elements) play inline in the graphical
  frontends; the terminal plays nothing inline (`canPlayType()` answers "",
  there is no `MediaSource`) and keeps opening media and YouTube players in
  mpv. Graphical layouts, static ones included (`render_html_for_environment`
  and the CSS-only render set `Dom::set_inline_media`), present media
  elements as players (a replaced box from the start, with its hit region),
  never as links to the external player; trust-desktop offers mpv only in
  the media element's context menu. The prelude runs the media element
  algorithms; `media_host` owns each element's `media::player::Player` (a
  control thread that demuxes and decodes audio, a video decode thread and a
  presenter publishing frames to a `VideoSurface` that paint reads by surface
  id), whose reports come back as `LumenHostTask::Media`. Audio goes to
  PulseAudio/PipeWire (the pure-Rust `pulseaudio` crate) or dlopen'd ALSA,
  silently in trust-headless (`media::audio::disable_device_output`); the
  clock follows the heard audio. Bundled decoders: rav1d (AV1) and Symphonia
  (MP3, FLAC, Vorbis, PCM). H.264, HEVC, AAC, VP8, VP9 and Opus come from the
  system's FFmpeg, libavcodec 59–63 dlopen'd at run time
  (`media::decode::ffmpeg`); Windows has no system decoder module yet. One
  registry (`media::decode::Registry`, bundled first) answers `canPlayType()`,
  `MediaSource.isTypeSupported()` and Media Capabilities, so they never claim
  what cannot decode. Containers are TRust's own ISO BMFF, Matroska/WebM and
  ADTS parsers plus Symphonia's Ogg/MP3/FLAC/WAV readers; HTTP media streams
  through Range requests (CORS-safelisted single ranges) into a bounded sparse
  cache. A `preload="none"` element without `autoplay` takes the resource
  fetch algorithm's optional wait for the user (#concept-media-load-resource):
  NETWORK_IDLE, `suspend`, no load-event delay, and a `Player::deferred` that
  runs no thread and fetches nothing until `play()` or a preload change
  (`resumeDeferredFetch`); a page of sixty such videos fetched and decoded
  them all at once. A top-level navigation to playable audio/video is a media
  document (HTML #read-media) on the desktop, not a download.
- Media Source Extensions (w3c/media-source): `media::mse` runs the segment
  parser loop for ISO BMFF and WebM byte streams, coded frame processing
  (segments and sequence modes, timestampOffset, append windows, overlap
  removal with decode dependencies, audio splices by trimming), removal,
  eviction and the buffered ranges; the prelude's MediaSource/SourceBuffer/
  SourceBufferList run the IDL, events and the media element's side
  (`__media_source`, `__media_attach`). An attached element's player reads
  the track buffers through `MseDemuxer`: reads return `demux::Error::
  Pending` until data is appended and wake the player when it is; a new
  initialization segment reaches the decoders as `Packet::config`. Capacity
  is 100 MiB per SourceBuffer with video, 12 MiB without; eviction frees
  whole groups of pictures before the playback position, else
  `QuotaExceededError`. MediaSource in workers and ManagedMediaSource are not
  implemented (`canConstructInDedicatedWorker` is false).
- Media Capabilities: `decodingInfo()` answers from the registry (smooth up
  to a software pixel rate of 1080p60, 1080p30 for AV1/HEVC; power
  efficient only with hardware decoding); `encodingInfo()` and the WebRTC
  types are unsupported; encrypted configurations are unsupported.
- Encrypted Media Extensions (w3c/encrypted-media) with the Clear Key key
  system only (`media::eme`): the CDM extracts key IDs from "cenc" (the
  Common SystemID pssh), "keyids" and "webm" initialization data and takes
  JWK Set licenses; sessions are "temporary". A MediaKeys object's
  `KeyRing` decrypts ISO Common Encryption `cenc`/`cbcs` and WebM samples
  in the player before decoding; a packet without a usable key waits
  (`waitingforkey`) and the ring wakes the player when keys arrive. MSE
  initialization and media segments report their pssh boxes and WebM key
  IDs as `encrypted` events. Widevine and PlayReady are not available, so
  DRM-only sites (Vimeo's protected videos, Netflix) do not play. The
  `encrypted-media` permissions policy is not checked.
- Web Audio renders in Rust (`src/webaudio/`) on one rendering thread per
  context, in both frontends. The prelude's block is the control thread: it
  validates and queues control messages through `__audio_context_binding`;
  the thread swaps the queue each render quantum (#rendering-loop) and its
  reports return as `LumenHostTask::WebAudio` tasks. An `AudioContext`
  renders ahead of the default output device (its preferred rate and
  channel count are the defaults) through a lock-free ring sized by the
  latency hint, paced by the device; suspended contexts sleep. Like Firefox,
  a context is allowed to start only once its Window has sticky activation:
  until then `resume()` waits, and `start()` on a source starts a context
  that is allowed but blocked. `sinkId` {type: "none"}, a missing device and
  trust-headless use a silent real-time sink. `OfflineAudioContext` renders
  as fast as it can on its own thread, suspending at scheduled quanta. The
  graph is ordered by strongly connected components (cycles are muted);
  nodes not actively processing output one silent channel and are skipped.
  Implemented: sample-accurate `AudioParam` automation, speaker channel
  mixing, `OscillatorNode` (band-limited `PeriodicWave` tables), `GainNode`,
  `DynamicsCompressorNode` (TRust's documented knee/detector/envelope
  curves), `AudioBufferSourceNode`, `ConstantSourceNode`,
  `BiquadFilterNode` (the spec's cookbook coefficients, their limits where
  they degenerate), `IIRFilterNode`, `DelayNode` (a DelayNode in a cycle
  becomes a DelayReader/DelayWriter pair clamped to one render quantum),
  `StereoPannerNode`, `ChannelSplitterNode`/`ChannelMergerNode`,
  `WaveShaperNode` (windowed-sinc 2x/4x oversampling), `AnalyserNode` (the
  last 32768 down-mixed frames shared with the control thread, which runs
  the FFT and smoothing when script asks), `ConvolverNode` (two-stage
  uniformly partitioned FFT convolution without added latency; the tail
  stage's work lands every 8 render quanta), `AudioListener` (parameters
  only), `AudioBuffer` (content acquired into the host, `getChannelData()`
  copies back lazily) and `decodeAudioData()` (decoding threads, first
  audio track, windowed-sinc resampling). Keep the rendering deterministic;
  the compressor probe test pins it. `PannerNode`/HRTF are deliberately not
  implemented. `createMediaElementSource()` reroutes the element's player
  into the graph through a `media::audio::Tap` output (also for later
  players of the element): the node pulls the player's audio and resamples
  it (streaming windowed sinc); while nothing pulls (the context is
  suspended) the element plays on with its output ignored, consumed in real
  time. A CORS-cross-origin resource (a no-cors response from another
  origin, after redirects) is heard as silence. `ScriptProcessorNode`
  sends each `bufferSize` chunk of input as an `audioprocess` task while it
  is connected; the filled `outputBuffer` plays two buffers later (double
  buffering), and an `OfflineAudioContext` waits for each answer (closing
  the context, or 10 s without one, ends the wait).
- AudioWorklet: each context's `AudioWorkletGlobalScope` is a Lumen engine on
  its rendering thread (`src/audio_worklet_host.rs`), built by the first
  `addModule()` from the worker platform plus `js_audio_worklet.js`
  (`js::audio_worklet_prelude`, which removes what a WorkletGlobalScope does
  not expose: worker messaging, timers, fetch, storage, `performance`).
  Modules load through the page's module fetch (`module_dependency_loader`,
  blocking on that thread, so a load during playback can glitch); a module
  runs once per scope; fetch failure rejects with AbortError and a parse
  error with SyntaxError. `registerProcessor()` descriptors reach the
  control thread before `addModule()` resolves. Each `AudioWorkletNode`'s
  processor gets its options and a transferred `MessagePort` through the
  page's port registry (`Wake::Callback` wakes the rendering thread, which
  runs the scope's port tasks before each quantum and while idle); per
  quantum `process()` gets frozen input/output/parameter arrays reused until
  their shape changes (`workletPrepare`/`workletRun`). An exception fires
  `processorerror` (ErrorEvent) and silences the processor. AudioWorklet is
  `[SecureContext]`. A collected node is released to the rendering thread and
  leaves the graph once silent; a started source and a running context stay
  alive until they end or suspend.
- WebAssembly exception handling runs both the legacy (`try`/`catch`/
  `rethrow`/`delegate`) and the standardized (`try_table`, `throw_ref`,
  `exnref`) instructions in the Wasmi fork; exceptions caught by reference
  live in a collected per-store arena. Tags are store objects that modules
  import and export by identity. `WebAssembly.Tag`, `WebAssembly.Exception`
  (`getArg`, `is`, `traceStack`/`stack`) and the per-agent `WebAssembly.JSTag`
  follow the JS API: an uncaught Wasm exception reaches JavaScript as its
  cached `Exception` object (agent-wide object caches, GC-traced while Wasm
  still references the exception), or as the original value for JSTag; any
  throw completion of an imported function (including import TypeErrors and
  RuntimeErrors that crossed JavaScript) is catchable in Wasm and keeps its
  identity when thrown back out. Host interruptions stay uncatchable traps.
  As the JS API requires, exnref never crosses into JavaScript (TypeError for
  signatures, globals, tables and `Exception` payloads), and v128 fields of
  `Exception` objects are unreadable. JavaScript values thrown through Wasm
  are interned like other externrefs and retained for the agent's lifetime.
- XML MIME types (text/xml, application/xml and any +xml type, including
  application/xhtml+xml and image/svg+xml) navigate to XML documents built by
  the XML parser (`src/dom/xml.rs`), in frames and in the page actor; the
  static pre-script render still parses XHTML as HTML. Fragment parsing in an
  XML document uses the XML fragment algorithm. Documents without a browsing
  context (DOMParser, createDocument, createHTMLDocument) parse and serialize
  with scripting disabled.
- Import maps cover static/dynamic imports, scopes, blocked specifiers, and
  integrity metadata. Navigation parses nested declarative Shadow DOM and
  shadow-scoped styles; ordinary `innerHTML` and `DOMParser` keep declarations
  inert.
- Input stepping and `valueAsNumber` are shared DOM behavior for number, range,
  date, month, week, time, and datetime-local. Preserve step/bounds validation
  and separate current/default values; desktop number inputs add spin controls.
  trust-desktop shows a select's options in its own drop-down box (a toggling
  list box for `multiple`), reported open through `PageCmd::PickerOpen`; picks
  and toggles reach the page as `PageCmd::SelectPick`.
- `<input type=file>`: activation (or `showPicker()`) with transient
  activation asks the frontend for one prompt at a time (`PageEvt::FilePicker`
  → `PageCmd::FilePickerResult`); a dismissal fires `cancel`, a choice fires
  `input`/`change`. trust-desktop shows rfd's native dialog on its tokio
  runtime (XDG Desktop Portal via ashpd/zbus on Linux, no GTK; IFileOpenDialog
  on Windows); the terminal shows its own browser (`src/file_picker.rs`: path
  entry with Tab completion, Space/Insert marks for `multiple`, Ctrl-O
  confirms, Ctrl-T lists files `accept` or a leading dot hides, Esc cancels);
  trust-headless dismisses. Only chosen or dropped paths become readable: the
  actor registers them (`file_select::UserFiles`) and File objects hold opaque
  ids whose ranges are read lazily against a length/mtime/inode snapshot
  (NotReadableError/NotFoundError), each read and every assembled upload
  bounded by the 512 MiB body limit. Form navigations send multipart bodies
  assembled from those ranges; a changed file fails the submission.
  `webkitdirectory` expands at most 10,000 files, 64 levels deep, with
  `webkitRelativePath`. `webkitEntries` and `webkitGetAsEntry()` expose the
  Entries API over the same registry: a dropped directory's members are
  listed through the host by path (at most 10,000 per listing), registering
  each as it is listed. Without page scripts (`set js off`) the terminal picker
  fills the painted control, and static submissions apply the submitter's
  form* overrides, run constraint validation (`form_validation`) and encode
  the enctype (multipart reads the files at submission).
- trust-desktop runs HTML drag-and-drop for files dragged in from other
  applications: dragenter/dragover/dragleave/drop with `DataTransfer` in
  protected mode until the drop, and a mutable file input takes dropped files
  as its selection. winit 0.30 reports file drags without positions, so the
  pointer is queried (X11 `QueryPointer`, Win32 `GetCursorPos`) on each 350ms
  iteration; it delivers no file drags on Wayland. No drags start in pages.
- TRust's User-Agent is `Mozilla/5.0 (<frozen platform>) TRust/0.2`
  (`USER_AGENT` in `src/http.rs`). Fetch leaves it implementation-defined but
  encourages the `Mozilla/5.0 (` shape; a bare `TRust/` token is refused by
  bot-management services (DataDome) before any script runs. The platform
  token is the frozen value major browsers send per OS family;
  `navigator.platform` names the real one (`Linux aarch64`). Script sees
  HTML's Gecko navigator compatibility mode. Held in reserve, not enabled:
  appending a recent Chrome's tokens (` Chrome/153.0.0.0 AppleWebKit/537.36
  Safari/537.36`, as Ladybird does) automatically switches `navigator` to
  Chrome mode and sends low-entropy `Sec-CH-UA*` hints to potentially
  trustworthy URLs. Try it when a site or Cloudflare rejects the plain form;
  on 2026-10-08 it failed DataDome's device check on Reuters, which then
  expects Chrome's own script environment. `TRUST_UA_FIREFOX` swaps in
  Firefox's User-Agent for A/B tests.
- The default language is US English regardless of OS locale:
  `Accept-Language: en-US,en;q=0.9`, `navigator.language === "en-US"`, and native
  Intl default `en-US`. Explicit locale requests remain supported.
- Navigated documents are decoded by `src/document_encoding.rs`: HTML's encoding
  sniffing (BOM, `Content-Type` charset, `<meta>` prescan, a same-origin
  container's encoding, then the en-US default windows-1252), re-decoding from
  memory when the tree builder finds a later declaration. Only local files are
  autodetected, and only as UTF-8. That encoding is `document.characterSet` and
  the fallback for classic scripts and style sheets; module scripts are UTF-8.
  Form submission uses HTML's selected form encoding (`accept-charset`, else
  the document's), with `&#N;` references for unencodable characters, and
  URL query strings in hyperlinks, `location`, frame, image, media, script,
  link and Worker URLs are encoding-parsed with the document's encoding
  (`src/form_encoding.rs`). Fetch, XHR, `URL` and module requests stay UTF-8.
- CSSOM keeps every `@import` as its own sheet (`ImportedSheet` records in
  `src/dom/cssom.rs`) while the cascade still splices them in place; scripts
  see nested `CSSStyleSheet`s with origin-clean checks, and imports added by
  CSSOM are fetched once and restyle on arrival.
- `document.evaluate()` and the DOM XPath interfaces run the XPath 1.0 engine
  in `src/dom/xpath.rs` over the arena DOM. Namespace nodes are usable inside
  expressions; returning one as a result throws `NotSupportedError`, since the
  DOM Standard defines no node type for them.
- `document.execCommand()` implements every command of the W3C execCommand
  draft except `paste` (script may never read the clipboard, so it is
  unsupported); `copy`/`cut` need transient activation and write text through
  `PageEvt::ClipboardWrite`. Commands, native contenteditable edits and the
  history share one per-Document undo stack built from mutation records
  (`historyUndo`/`historyRedo`, Ctrl+Z/Ctrl+Y). The non-standard `heading`
  command is not supported.
- SVG elements reflect their attributes through the SVG 2 `SVGAnimated*`
  interfaces; `animVal` always equals `baseVal`, because SMIL animation
  elements (`animate`, `set`, `animateMotion`, `animateTransform`) are inert
  and exposed only as `SVGElement`.

### Local files and persistent state

- All frontends accept absolute, explicit `./`/`../`, and existing relative
  paths, plus `file:` URLs. Preserve literal filename `#`, `?`, `%`, and spaces
  when converting paths to URLs. Explicit URLs use percent encoding; their
  query/fragment is not part of the filesystem path.
- File documents have opaque origins. Relative images, stylesheets/imports,
  classic scripts, and frames work; Fetch/XHR, modules, web fonts, and other
  CORS-dependent resources need HTTP(S). Keep external `file:` SVG sprites out
  of the shared cache. Web content cannot read or navigate into local files;
  explicit user navigation and local bookmarks remain available.
- Only empty/`localhost` file authorities and GET of regular files up to
  512 MiB are accepted. Use ordinary filesystem permissions; reject directories,
  special files, other authorities, and other methods without writing files.
- Both frontends share versioned `$XDG_DATA_HOME/trust/bookmarks.json`
  (default `~/.local/share/trust/bookmarks.json`). Saves are atomic and coordinated
  across instances. Duplicate destinations reuse entries; only explicit titles
  rename them. `status` reports storage directories.
- HTTP(S) bookmarks permit expiring cookies and localStorage to persist in
  `$XDG_DATA_HOME/trust/site-data/`. Permission groups sites using the Public
  Suffix List including private suffixes, across subdomains, HTTP/HTTPS, and
  ports; cookie domain/path and localStorage origin rules still apply. Non-web
  bookmarks grant no web-storage permission. Session cookies/sessionStorage end
  on exit; IndexedDB, Cache Storage, and service-worker state stay temporary.
- Bookmarking captures eligible live state. Removing the last web bookmark for
  a site immediately deletes saved state but preserves session memory; undo can
  save that live state again. Expiry and site-requested deletion still apply.
- Third-party embedded/background requests cannot send/set cookies, except for
  the user-approved HTTP challenge-cookie policy: Cloudflare's `cf_clearance`
  and `cf_chl_*` cookies of any HTTPS site are honored in every context. A
  cross-site request (embedded frame, subresource, fetch, worker) sets and sends
  them in the ordinary first-party jar, so one clearance serves the site at top
  level and in every embed; they persist like that site's other cookies. Only
  HTTP may use the exception, and SameSite still applies: cross-site responses
  store them only as `Secure; SameSite=None`, and cross-site requests receive
  only those. Every other cookie (including `__cf_bm`) stays blocked cross-site.
  Third-party frames still cannot access document cookies.
  Main navigation follows SameSite rules.
- Storage is partitioned like LibreWolf's (Firefox dFPI) and Chrome's
  (Client-Side Storage Partitioning, Privacy CG): a frame or worker with a
  cross-site ancestor keys localStorage, sessionStorage, IndexedDB, Cache
  Storage, BroadcastChannel and shared workers by its top-level site as well
  as its origin (`CookieContext::storage_partition`, `__trust_cfg.
  storagePartition`, `storageKeyFor` in the prelude; buckets are
  `<kind>:<origin>^<top-level site>`). An embed's state under one site is
  invisible under every other and to its own first-party pages; an A-B-A frame
  is partitioned too. Partitions stay in memory for the session, even under
  bookmarked sites; there is no Storage Access API or storage-access heuristic. `set cookies off` disables cookie access/capture
  without clearing saved cookies or localStorage; cookie deletion does not
  itself delete localStorage.
- localStorage quotas are 5 MiB per origin (per partition for embeds) and
  64 MiB total. Saved files have
  private permissions but are unencrypted. Writes are atomic, off the UI thread,
  and coordinated with bookmark edits across instances; surface storage errors.
- Windows defaults: config/bookmarks/site data in `%APPDATA%\trust`, state/cache
  in `%LOCALAPPDATA%\trust\state` and `\cache`, downloads in
  `%USERPROFILE%\Downloads`. Absolute XDG overrides and explicit
  `TRUST_KNOWN_HOSTS`/`TRUST_GEMINI_KNOWN_HOSTS`/`TRUST_IDENTITIES`/
  `TRUST_DICT_CREDENTIALS` overrides take precedence.

### Telnet and small-net protocols

- Telnet shares protocol, terminal, input, paste, mouse, and encoding behavior
  between frontends. COMMAND must not resize the remote terminal or lose the
  line draft/selection. Submitted ordinary text stays selected for resend or
  replacement; passwords clear. Sent-line history is RAM-only, capped at 500,
  skips consecutive duplicates, excludes hidden input, and restores the draft
  when moving past the newest entry. COMMAND history remains separate.
- Keep remote keys and mouse reporting intact. Desktop local copy/paste/search
  use Ctrl+Shift shortcuts; Shift overrides remote mouse reporting. Terminal
  copy uses OSC 52 and paste uses bracketed paste. Ctrl+B/Alt+B retain their
  remote meanings during Telnet; bookmarks must not disconnect the session.
- UTF-8/CP437 work in both directions; unsupported CP437 input is a visible
  error. Bracketed paste is one queue entry; rejected lines/multiline pastes
  preserve the draft. Remote echo and LINEMODE editing are independent.
- Primary-screen resize reflows soft wraps; alternate screens keep coordinates.
  History reading hides the live cursor and retains position as output arrives
  until history expires. Scrollback is capped at 10,000 rows and one million
  cells. See `docs/telnet-revamp.txt` for the ANSI/BBS target and VT/xterm limits;
  do not claim complete xterm emulation.
- Small-net replies stream progressively. Stop, timeout, truncation, and failed
  follow-up requests retain received data with visible incomplete/error notices.
  Keep byte limits and display-work limits separate; source saving preserves
  original content bytes rather than reconstructed displayed text.
- Gopher preserves selector bytes through navigation, copying, and bookmarks.
  Search-endpoint bookmarks prompt again; query bookmarks repeat the search.
  `gophers://` defaults to port 70, preserves TLS for same-host/port links,
  searches, and formats, and never silently falls back to plaintext.
- Gopher/Gopher+ page bodies are capped at 2 MiB; downloads stream to disk with
  a 2 GiB ceiling. A dot terminator or declared byte count completes the reply
  without waiting for socket close. Strip Gopher+ framing before display/save.
  Fetch item information/formats on demand. Binary items use bounded format
  detection before display or Save/Open offers.
- Gopher+ ASK forms (`?` items, UMN Gopher+ §2.8; local copy of the memo in
  the standards library) render as line-model forms in both frontends; their
  answers live in `gopher::View::ask` (the core keeps it on the page, so
  `gopher::prepare_form` runs when a page is committed). Answer encoding
  follows the UMN reference client: AskL sends a count line then its lines,
  Select sends 1/0, Choose its option text, ChooseF one file as the data
  block's remainder (`+<length>` framing; otherwise `+-1` with dot-stuffing).
  Answers are single bounded lines (4 KiB each, 64 KiB total, 16 MiB file)
  in the page's display encoding; untouched defaults keep the server's bytes.
  A submission's data block rides beside the URL (`GopherUrl::ask`), is sent
  at most once, and never reaches Display/history/bookmarks (`public()` is
  the form's `?` address, whose reload shows the form again). Typed AskP
  answers are cleared on submit. An AskF answer, or a download item type,
  turns the reply into a Save/Open offer that sends the form when chosen.
- Gopher views preserve authored spacing, bounded reading history, and decoding
  independent of selectors (automatic UTF-8, otherwise Latin-1; explicit CP437
  is available). Desktop ANSI SGR colors are bounded; discard terminal movement
  commands and use plain text for wrapping/search/copy. Wrapped links have one
  keyboard stop and remain clickable across rows.
- Gemini defaults to a 96-character column (configurable 20–240); local `.gmi`,
  `.gemini`, `.gemtext`, and `.gmni` files preview Gemtext. Sensitive input is
  masked and excluded from history/stored addresses. Client identities
  (`gemini::identity`) are PEM files with `.scopes` files; a request sends
  the identity whose explicit scope (host, port, path subtree) is the most
  specific match, so different paths may use different identities, redirects
  and links never carry one outside its scope, and existing PEM files need an
  explicit first choice. A 6x answer renders a page that names the identity
  sent, warns on TLS 1.2, and lets the user choose the scope (the request's
  path subtree by default, any parent, or the whole capsule, for capsules
  that ask on one page such as `/join` and recognize the certificate
  everywhere) and create or pick an identity; 61/62 choices supersede the
  rejected identity for that exact scope. `identity [capsule|page|revoke]`
  shows, widens, narrows or withdraws the identity for the current page, and
  `about:certificates` lists identities, scopes and pins. New identity names
  are never prefilled from the local account; key files are published
  atomically (0600). Binary
  saving consumes the original response connection without resubmitting input.
  Cross-protocol redirects show a link. Text decodes in its MIME `charset`
  (`src/gemini/media.rs`): IANA names through the WHATWG label table, except
  that US-ASCII/ISO-8859 keep C1 controls and unmarked UTF-16/UTF-32 follow
  the BOM, else big-endian. Unknown or WHATWG "replacement" charsets produce
  an explicit notice.
- Gemini uses trust on first use (`gemini::tofu`): the SHA-256 fingerprint
  and notAfter of a capsule's certificate are pinned per host:port in
  `gemini_known_hosts` (0600) once a handshake succeeds. A different
  certificate before the pin expires stops the request before anything is
  sent and shows both certificates with an explicit "trust the new one"
  choice; after expiry the replacement is pinned with a notice. Gophers still
  encrypts without CA, hostname, expiry, or pin validation, including
  replacement certificates. HTTPS uses WebPKI; Telnet TLS retains
  `known_hosts` pinning.
- Finger preserves columns, blank lines, eight-cell tabs, URL links, and the
  previous successful reply for comparisons. WHOIS retains raw answers and
  provenance alongside parsed summaries; unrecognized formats use a transcript.
- WHOIS referrals detect cycles within a four-server, 15-second, 1 MiB shared
  budget. Follow IANA `refer:`; leave `whois:` service metadata as a link.
  Automatic decoding tries UTF-8 then Latin-1. Single-server exports preserve
  exact bytes; multi-server exports add headings. RDAP is an explicit alternative
  using IANA bootstrap, preferring HTTPS; preserve original JSON and never
  automatically switch WHOIS protocols.
- DICT keeps server result order, source attribution, and exact CRLF transcript
  bytes; spelling suggestions use the same server/database. Commands default to
  all dictionaries unless a per-server preference is saved in
  `$XDG_CONFIG_HOME/trust/dict.json` (default `~/.config/trust/dict.json`). Explicit
  URLs override preferences; an omitted URL database means `!` (first match).
  DICT/WHOIS URLs decode once, accept bracketed IPv6, and keep fragments local.
- DICT pipelines CLIENT and OPTION MIME (RFC 2229 §§3.10, 4). With MIME on,
  each text response's header is parsed (RFC 2045/2046: charsets, base64,
  quoted-printable, multipart); text is shown and other parts become saveable
  attachments; 500 to OPTION means a pre-MIME server. AUTH (§3.11) uses
  per-server credentials in `$XDG_CONFIG_HOME/trust/dict-credentials.json`
  (0600, beside `dict.json`; the secret is stored unencrypted because AUTH
  needs it each session and is never shown or logged). With credentials the
  lookup waits for the banner's msg-id and sends `AUTH user md5(msg-id
  secret)` first; 230/531/5xx are reported and the lookup proceeds either
  way. `dict://user;AUTH@host/…` asks for the secret before connecting;
  SASLAUTH is rejected explicitly. `dict-auth [server] [user]` and
  `dict-auth --forget` manage credentials. Reply codes must start with 1–5
  (§2.4.1).

## Documentation Notes

Keep installation in `INSTALL.md`, development guidance here, and diagnostic
commands/environment variables in `DIAGNOSTICS.md`. Interactive commands are
available through the built-in `help` (`src/command.rs`) and `gemini-help`.
Detailed architecture and standards notes live in module and item comments
throughout `src/`; keep them aligned with the Lumen-only implementation.
