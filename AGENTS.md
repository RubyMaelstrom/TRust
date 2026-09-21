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
- `src/http.rs`: hand-built HTTP/1.1 client, WebPKI HTTPS, redirects,
  keep-alive pool, cookies, page/subresource caching, HTML parsing, form
  extraction, and full/incremental layout entry points.
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
  - `measure.rs`: fragment geometry for CSSOM View and observers.
  - `boundary.rs`: safe incremental-layout boundaries.
  - `paint.rs`: CSS painting order, cell compositor, clipping, fixed layers,
    regions, carousels, and the single CSS-pixel-to-cell quantization boundary.
  - `contract.rs`: `Row`, `Item`, regions, fixed items, geometry, terminal-cell
    metrics, and the public layout/rendering contract.
- `src/img.rs`: guarded raster/SVG decoding, sizing, tinting, alpha detection,
  compositing, and terminal graphics encoding.
- `src/ui.rs`: Ratatui rendering for browser documents, inline images, fixed
  layers, scrollbars, dialogs, and terminal sessions.
- `src/core/`: shared browser controller and event delivery for desktop and
  headless clients.
- `src/bin/trust-desktop.rs`: winit window, native input, and presentation loop.
- `src/render/`: renderer-neutral display list, Parley-shaped text, Vello CPU
  reference renderer, Vello Hybrid/wgpu backend, and windowless
  `trust::render::headless` pipeline. See module comments for backend limits.
- `src/telnet.rs`, `src/gopher.rs`, `src/gemini.rs`, `src/oneshot.rs`,
  `src/tls.rs`, and `src/ws.rs`: protocol implementations.
- `vendor/`: owned forks of wasmi and frontend/font/image dependencies.
  The Lumen engine is maintained separately at the sibling
  `../Lumen` checkout (integration revision below). A bug whose correct
  fix belongs in an engine should be fixed in its engine repository rather
  than hidden behind a downstream workaround.

## Important Design Invariants

- Layout geometry remains `f32` CSS pixels until `layout2::paint`; quantize
  edges to terminal cells exactly once at the paint boundary.
- Widths resolve top-down from containing blocks; heights resolve from content.
- JavaScript geometry comes from layout fragments, not reconstructed painted
  text.
- The DOM/CSS cascade is the single style authority. Layout consumes typed
  snapshots rather than reparsing declaration strings.
- HTML output uses positioned `Row`/`Item` data. Gopher, Gemini, one-shot
  protocols, and plain text use the simpler line model.
- Only one foreground page engine remains live. Navigation drops the old page
  actor and its heap.
- Incremental layout is an optimization. If a patch cannot be proven safe,
  fall back to the always-correct full relayout.
- Image intrinsic sizes must be shared with the live page geometry pass so DOM
  measurements match rendered boxes.
- Background panics and per-resource failures must remain contained; only the
  terminal-owner thread may tear down the TUI.
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

Keep the sibling Lumen checkout at integration revision
`328b4bbd1de1e93547cc74bebe08b98d97088eaa`. Cargo uses
`../Lumen/crates/lumen` directly; TRust and Lumen are developed together while
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
Release uses thin LTO and one codegen unit on all targets. Native ARM64/x86-64
Linux and Windows cross-build instructions are in `INSTALL.md`; use the same Lumen revision for each. With Wine installed,
`cargo xwin test --target x86_64-pc-windows-msvc` runs the Windows tests.

Focused checks and diagnostics:

- `python3 tools/check_css_wpt.py` runs a pinned official WPT subset for CSS
  variables, registered properties, and background shorthands in the release
  headless browser. It caches upstream sources (`--offline` reuses them), writes
  JSON and browser logs to `target/css-wpt-results`, and fails for failed tests
  or incomplete harness runs. Keep the full failure results.
- `cargo test --lib webgl -- --include-ignored` runs hardware-dependent WebGL
  pixel, shader, and API tests.
- `src/fixtures/number_input.html` is the manual number-input acceptance page.
- `DIAGNOSTICS.md` contains environment variables, ignored live-site gates,
  headless tools, and benchmark commands. `TRUST_DESKTOP_TRACE=1` enables frame
  timings; `desktop_pipeline_bench` covers text, flex/grid, compositing, long
  scrolling pages, DOM mutation, images, and graphical Telnet.

### Approved Release Promotion

The user tests TRust from the release target. Build `target/release/trust` with
`cargo build --release` for acceptance testing; do not substitute a debug build
when reproducing or validating user-visible behavior.

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

- `--renderer=auto` selects Hybrid only after surface, adapter, device, and
  capability initialization succeed; recoverable later failures fall back to
  Vello CPU. `--renderer=cpu` forces the reference renderer, while
  `--renderer=hybrid` reports initial GPU failure. Backend selection must not
  change DOM, CSS, layout, hit testing, or display-list output.
- COMMAND overlays the page without changing layout or the JavaScript viewport.
  Its image-cache readout counts decoded pixel bytes, excluding GPU copies,
  chrome, and other allocations; do not add heap scans or a reporting timer.
- Fragment navigation handles percent-encoded IDs and legacy named anchors,
  preserves the live page/scripts, updates the address, and restores scrolling
  through Back/Forward. Desktop pointer lock releases on Escape, focus loss, or
  navigation; the terminal frontend reports it unsupported.
- WebGL 1 uses lazy-loaded EGL/OpenGL ES with Rust shader preparation and
  validation, without bundled ANGLE. A missing robust context makes
  `getContext("webgl")` return `null`. WebGL 2 and multisample antialiasing are
  unimplemented. Estimated buffer/image storage has a shared 256 MiB page budget
  and at most 16 contexts; driver overhead and CPU copies are additional.
- Web Audio supports context construction/lifecycle only. Contexts stay
  suspended with a stationary clock; renderer acquisition errors asynchronously
  and `resume()` rejects with `NotSupportedError`. Graph nodes, decoding,
  worklets, offline rendering, and playback are unimplemented.
- Import maps cover static/dynamic imports, scopes, blocked specifiers, and
  integrity metadata. Navigation parses nested declarative Shadow DOM and
  shadow-scoped styles; ordinary `innerHTML` and `DOMParser` keep declarations
  inert.
- Input stepping and `valueAsNumber` are shared DOM behavior for number, range,
  date, month, week, time, and datetime-local. Preserve step/bounds validation
  and separate current/default values; desktop number inputs add spin controls.
- The default language is US English regardless of OS locale:
  `Accept-Language: en-US,en;q=0.9`, `navigator.language === "en-US"`, and native
  Intl default `en-US`. Explicit locale requests remain supported.

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
- Third-party embedded/background requests cannot send/set cookies; third-party
  frames cannot access localStorage, even for bookmarked sites. Main navigation
  follows SameSite rules. `set cookies off` disables cookie access/capture
  without clearing saved cookies or localStorage; cookie deletion does not
  itself delete localStorage.
- localStorage quotas are 5 MiB per origin and 64 MiB total. Saved files have
  private permissions but are unencrypted. Writes are atomic, off the UI thread,
  and coordinated with bookmark edits across instances; surface storage errors.
- Windows defaults: config/bookmarks/site data in `%APPDATA%\trust`, state/cache
  in `%LOCALAPPDATA%\trust\state` and `\cache`, downloads in
  `%USERPROFILE%\Downloads`. Absolute XDG overrides and explicit
  `TRUST_KNOWN_HOSTS`/`TRUST_IDENTITIES` overrides take precedence.

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
  Fetch item information/formats on demand; ASK submission is unimplemented.
  Binary items use bounded format detection before display or Save/Open offers.
- Gopher views preserve authored spacing, bounded reading history, and decoding
  independent of selectors (automatic UTF-8, otherwise Latin-1; explicit CP437
  is available). Desktop ANSI SGR colors are bounded; discard terminal movement
  commands and use plain text for wrapping/search/copy. Wrapped links have one
  keyboard stop and remain clickable across rows.
- Gemini defaults to a 96-character column (configurable 20–240); local `.gmi`,
  `.gemini`, `.gemtext`, and `.gmni` files preview Gemtext. Sensitive input is
  masked and excluded from history/stored addresses. Client-certificate consent
  covers host, port, and path subtree, including existing PEM identities. Binary
  saving consumes the original response connection without resubmitting input.
  Cross-protocol redirects show a link. Support UTF-8, ASCII, and ISO-8859-1;
  unsupported charsets produce an explicit notice.
- Gophers/Gemini currently encrypt without CA, hostname, expiry, or server-pin
  validation, including replacement certificates; Gemini server pins are
  ignored. Client identities require explicit scope consent. HTTPS uses WebPKI;
  Telnet TLS retains `known_hosts` pinning.
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
  DICT authentication and MIME attachments are unimplemented.

## Documentation Notes

Keep installation in `INSTALL.md`, development guidance here, and diagnostic
commands/environment variables in `DIAGNOSTICS.md`. Interactive commands are
available through the built-in `help` (`src/command.rs`) and `gemini-help`.
Detailed architecture and standards notes live in module and item comments
throughout `src/`; keep them aligned with the Lumen-only implementation.
