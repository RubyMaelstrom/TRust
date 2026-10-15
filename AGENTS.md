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

TRust is a browser written in Rust. It supports HTTP(S), Telnet and
Telnet-over-TLS, Gopher, Gemini, Finger, WHOIS, DICT, WebSockets, WebGL,
JavaScript, Web Workers, and WebAssembly. It features a terminal,
desktop, and headless mode.

The project favors:

- Standards-correct, general implementations over compatibility hacks.
- Pure-Rust engines and libraries where practical.
- Hand-built, auditable protocol layers.
- Bounded CPU and memory use, with zero idle spin.
- Graceful degradation: one failed script, image, request, or background task
  should not destroy an otherwise usable page or terminal session.

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
  `src/http/http3_io.rs` bounds encoded metadata and guards stream cancellation.
  Both multiplexed transports share field semantics in `src/http/wire.rs`.
- `src/dom.rs`: html5ever-backed arena DOM, mutations, selectors, CSS parsing,
  cascade, computed values, serialization, and live geometry state.
- `src/js.rs`: JavaScript/browser contract and engine-neutral
  DOM/resource helpers. It delegates the resident page actor to
  `src/trust_js_backend.rs`, backed by the maintained sibling TRust JS checkout.
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
  The TRust JS engine is maintained separately at the sibling
  `../trust-js` checkout. A bug whose correct
  fix belongs in an engine should be fixed in its engine repository rather
  than hidden behind a downstream workaround.

## Development and Verification

Cargo uses the sibling TRust JS checkout (`../trust-js/crates/trust-js`) directly;
TRust and TRust JS are developed together on their `main` branches while
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

TRust JS is the only JavaScript engine and is always compiled. No backend-selection
features or comparison targets exist. `--all-features` also enables the synthetic
`trust-js-spike` benchmark harness.

Ordinary release builds produce `trust`, `trust-desktop`, and `trust-headless`.
`trust-browser-replay` is a developer-only example target, not a distributed
executable. Run it explicitly with
`cargo run --release --example trust-browser-replay -- FIXTURE.html`.
Release uses optimized, incremental, parallel code generation without LTO so
acceptance testing and local installation stay fast. Public numbered releases
use `cargo build --profile numbered-release --locked`; that explicit profile
uses Fat-LTO and one codegen unit and is not part of the ordinary development
or local-promotion cycle. Native ARM64/x86-64 Linux and Windows cross-build
instructions are in `INSTALL.md`; use the same TRust JS checkout for each. With Wine installed,
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

The user tests TRust from the release target. Build `target/release/trust-desktop`
with `cargo build --release` for acceptance testing; do not substitute a debug
build when reproducing or validating user-visible behavior.

Do not substitute the `numbered-release` profile for routine local promotion.
Build that Fat-LTO artifact only when the user explicitly requests a public,
numbered release; its binaries land under `target/numbered-release/`.

Do not overwrite the installed executable merely because a release build
succeeds. Wait until the user explicitly says that the build is tested,
approved, and promoted. Once promotion is authorized:

1. Confirm that `cargo build --release` succeeded for the current source tree.
2. Install that exact artifact with
   `install -m 0755 target/release/trust-desktop /home/ruby/.local/bin/trust-desktop`.
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
allocator uses `--no-default-features`; TRust JS remains enabled. Several
performance diagnostics and safety switches are controlled by `TRUST_*`
environment variables documented near their use.

## Documentation Notes

Keep installation in `INSTALL.md`, development guidance here, and diagnostic
commands/environment variables in `DIAGNOSTICS.md`. Interactive commands are
available through the built-in `help` (`src/command.rs`) and `gemini-help`.
Detailed architecture and standards notes live in module and item comments
throughout `src/`; keep them aligned with the TRust JS-only implementation.