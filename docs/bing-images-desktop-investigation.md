# Bing Images desktop investigation

Investigation begun 19 September 2026; responsiveness follow-up on 20 September. Scope: the actual `trust-desktop` window,
with LibreWolf as the primary visual reference. This is a standards investigation,
not a Bing compatibility mode. No domain checks, rewritten site CSS, fabricated
search results, or browser-identity spoofing were added.

## Current responsiveness checkpoint

The latest native release reviewed here is pass 46. Its immediate unprobed
capture is one character short and its suggestions lag the query. **Brave parity
is not yet achieved.** Pass 46 passed its focused conformance, DOM, layout,
strict Clippy and release checks (6m52s). The parallel full suite crashed in
the system Vulkan loader; the isolated serial renderer suite passed.
No binary is installed or promoted.

The matched-width physical-key comparison is below. These are single live-page
trials; pass 46 needs a clean repeat because an attempted crash-debugger launch
briefly overlapped the measured run. Its separate unprobed capture and CPU
measurement had no such overlap. The suggestion row filters out ancillary XHRs.

| Metric, median / maximum | TRust pass 46 | Brave 1.95.104 |
| --- | --- | --- |
| Native key to input event | 24.3 / 49.9 ms | 4.2 / 10.8 ms |
| Native key to rAF callback | 44.0 / 90.3 ms | 6.5 / 16.8 ms |
| Network completion to suggestion XHR delivery | 709 / 1110 ms | 6.1 / 14.1 ms |

rAF runs before presentation: these are not key-to-photon numbers. The physical
burst contains 26 characters with 25 ms key-down and 25 ms key-up spacing. Both
browsers displayed a 1920×1080 page, while Brave's separate 40px sidebar is
excluded from the gallery's comparison area. Probed runs measure events; separate
unprobed TRust captures check that diagnostics are not the source of the symptom.

- [TRust immediate, unprobed](../target/bing-review/pass46-unprobed-immediate.png)
- [TRust settled, unprobed](../target/bing-review/pass46-unprobed-settled.png)
- [Brave immediate, measured](../target/bing-review/brave-pass44-wall-immediate.png)
- [Interactive native comparison](../target/bing-review/visual-comparison.html)

## Method and evidence

Release executables were run as native Wayland clients in an isolated labwc
session. The compositor used the machine's NVIDIA GB10 GLES renderer; captures
were taken from the compositor with `grim`. Virtual keyboard and pointer events
drove the actual desktop window. This exercises the desktop controller, page
actor, shared layout, graphical scene, raster backend, and window presentation.
It is not a screenshot of serialized HTML in another browser.

LibreWolf 153.0.4-1 ran in the same isolated session with a separate profile.
Chromium was a supplementary source of computed styles and DOM measurements.
The main comparison viewport was 1920 × 1080 CSS pixels at scale 1. Browser chrome
was excluded using native fullscreen/kiosk presentation. Narrower-window review
is recorded below. The user's ordinary browser and desktop sessions were left
running.

Screenshots and logs live in `target/bing-review/`, which is intentionally outside
version control. The sequence includes the original baseline and successive
release builds, captures during loading and typing, and settled captures. Useful
reference files are:

- `librewolf-native-home.png`
- `librewolf-native-typing.png`
- `librewolf-native-results.png`
- `librewolf-native-viewer.png`

Bing delivered different images, advertisements, suggestion text, and occasionally
different header controls between requests and browsers. Visual judgments therefore
compare geometry and behavior of equivalent components, with DOM inspection used
to distinguish absent source content from a painting failure. Whole-page pixel
differences across these live responses are not a meaningful conformance score.


A [visual comparison page](../target/bing-review/visual-comparison.html) provides
a draggable divider between native TRust, Brave and LibreWolf captures. Earlier visual-review
images can also be opened directly:

| State | TRust | LibreWolf |
| --- | --- | --- |
| Wide home | [native capture](../target/bing-review/pass21-native-home.png) | [reference](../target/bing-review/librewolf-native-home.png) |
| Wide typing/autocomplete | [native capture](../target/bing-review/pass21-native-typing.png) | [reference](../target/bing-review/librewolf-native-typing.png) |
| Narrow typing/autocomplete | [native capture](../target/bing-review/pass19-narrow-typing-settled.png) | [reference](../target/bing-review/librewolf-native-narrow-typing.png) |
| Search results | [native capture](../target/bing-review/pass21-native-results.png) | [reference](../target/bing-review/librewolf-native-results.png) |
| Image viewer | [native capture](../target/bing-review/pass21-native-viewer.png) | [reference](../target/bing-review/librewolf-native-viewer.png) |

The [typing-in-progress capture](../target/bing-review/final-native-typing-immediate.png)
and [settled capture](../target/bing-review/final-native-typing-settled.png)
show why the correct final appearance is not sufficient for responsiveness.
The [custom-size menu](../target/bing-review/final-native-filter-menu.png) is also
retained as an example of the remaining native-control appearance differences.

## Findings and general fixes

### Search field, text, selection, and keyboard input

The native editor previously painted a second input surface over the CSS control.
Its text and caret did not use the page's shaped glyph geometry. The editor now
uses the CSS control's text style, content origin, clipping, and Parley glyph runs.
Selection, caret placement, horizontal scrolling, and IME geometry share that
coordinate system. The cyan overlay from the supplied screenshot is gone.

Negative margins on inline controls were being clamped to zero. Preserving those
margins fixes both the search field's width and the position of its text. At the
wide comparison size, the text starts at approximately x=212 in both browsers.
The submit control also needs `text-indent` to move its label without moving its
box or clip; honoring this removes the stray “Sear” text over the magnifier.

The desktop now preserves the order of queued keyboard events and waits for
cancelable page handling before applying an editing default. Editing dispatches
trusted `beforeinput`, applies the uncanceled value and selection change, and then
dispatches trusted `input`. Selection is stored as UTF-16 offsets with direction;
the native editor converts those offsets to its shaped-text byte positions.
`selectionStart`, `selectionEnd`, `selectionDirection`, `setSelectionRange`, and
`select()` share that state. Tests cover cancellation, reversed/clamped ranges,
non-BMP characters, unsupported control types, script assignments, and event order.

Autocomplete also depended on ARIA IDL reflection. Nullable string reflection was
added for the supported ARIA properties and `role`, with tests for missing
attributes, null removal, literal values, invalid receivers, and SVG/XML elements.
This lets the page's suggestion code run and paint its normal dropdown.

Search submission exposed an independent HTML algorithm error: a script calling
`form.submit()` inside a canceled submit event was incorrectly blocked by the
event reentry guard. The method's submitted-from-submit flag must bypass that
guard. The fix has a focused regression and was exercised by typing a query and
pressing Enter in the native window.

### Header buttons and generated images

CSS `content: url(...)` was not represented as an image in generated content.
The layout model now distinguishes generated text from generated images, discovers
their resources, and preserves them through live-page presentation snapshots.
This repairs the trending-topic pictures and several sprite/icon surfaces.
Element image replacement changes presentation without replacing DOM children.

Transformed generated images now retain ancestor clips in the ancestor's coordinate
space. This repairs cropped logo/menu sprites. Submit controls outside forms retain
their visible surface and click target while having no invented form owner.

The graphics backends now implement ordered brightness, contrast, invert, opacity,
grayscale, and sepia color filters. Filters operate on a composited group and use
straight-alpha sRGB component calculations with clamping between operations.
CPU and hybrid GPU output are checked against the same fixtures, including alpha,
overlap, filter order, and containing-block behavior. Small owned Vello forks carry
the necessary shared command and shader support; their `README.trust.txt` files
record the changes.

Later native review found three further shared CSS errors. Float avoidance was
clamping a block to its containing block even on the side with no float, erasing
a negative margin and moving the navigation labels 160 pixels. It now constrains
the box only against actual float margin boxes. Inline-blocks now export their
last normal-flow line baseline, with the bottom-margin fallback for a block-axis
scroll container/no line boxes; `overflow:clip` retains a content baseline, and
table baselines are excluded as required by CSS Align's legacy rule. Nested
search-header boxes had each added an unnecessary
font descent. Finally, intrinsic probes retain atomic control widths rather than
clamping them to the probe's near-zero available width. That lets an absolutely
positioned `nowrap` filter menu include the full custom-size input row. Focused
tests cover both float sides, ended floats, nested baselines, floating/positioned
descendants, non-visible overflow, and definite/automatic control widths.

### Image panel and initial viewport

The live page actor originally started with a default viewport even when the native
window was wider. Early media-query results led page JavaScript to build the wrong
number of columns. The configured viewport and device-pixel ratio now reach the
actor before the first script runs, including asynchronous navigation. The wide
home page consequently builds six columns rather than the original four.

The missing pictures were not all image-decoder failures. Some were CSS generated
images, and some depended on scripts that had stopped after DOM API errors.
`getElementsByTagName` now matches qualified names instead of parsing its argument
as a CSS selector. The rules preserve HTML-only case folding, namespace behavior,
wildcards, and live collection updates.

`childNodes` and `children` now retain their live, same-object collection behavior.
Previously, loops that removed children while checking a cached collection length
could spin indefinitely. Tests cover detached construction, moves, live iteration,
and changing lengths. Internal mutation loops that require a snapshot explicitly
take one.

### Image viewer, iframe navigation, and fonts

Opening a result exposed a separate child-document navigation failure. Relative
Location navigation must use the entry document's API base URL, including the
creation-time fallback base of `about:blank`. Lumen now records script entry realm
separately from incumbent callback context. TRust routes a child's navigation back
to that child and loads the new document without rewriting the iframe's `src` or
`srcdoc` attribute. Focused tests cover cross-realm calls, callbacks, relative URLs,
unchanged attributes, repeated navigation, and discarded pending child loads.

A viewBox-only SVG used as a generated image had been sized from the decoder's
default raster dimensions. Generated anonymous replaced boxes now use intrinsic
dimensions/ratio and the applicable containing block. The viewer magnifier is
consequently an icon rather than an enormous image.

Viewer toolbar icons also use a `data:` WOFF2 font inside the child document.
Font descriptor parsing now respects CSS tokens, preserving semicolons, commas,
parentheses, and the complete data URL. Applicable embedded font faces are
activated in a retained document/tree font environment. Child documents start
from installed fonts; their downloadable fonts do not leak into parents or siblings.
Removing or changing a stylesheet updates the available faces, while an existing
layout snapshot retains the environment it used. Shadow-tree declarations and
inherited font references are tested separately. Installed font resources share
weak-backed byte storage across these environments.

The aurora viewer initially seemed to have an `object-fit` error: its small preview
was enlarged while LibreWolf showed a 474 × 316 image. Tracing the request and
waiting for completion showed the distinction was timing. The original image request
fails, the page applies its `error` class, and TRust then paints the same 474 × 316
fallback at the same position. No compensating image-sizing rule was added.

Closing the viewer exposed previously empty `history.back()`, `forward()`, and
`go()` implementations. Delta traversal now reaches the frontend history owner
as an asynchronous request. Same-document traversal keeps the live page actor,
restores URL, scroll and serialized state, and dispatches `popstate` before the
separately queued `hashchange`. Out-of-range deltas are no-ops; multi-entry jumps
retain skipped entries. Both desktop and terminal frontends use the same stack
traversal helper. Tests include Web IDL conversion, detached/cross-realm receivers,
state copying, event order, and a real page-actor click/request/reply round trip.
The native viewer's close button now returns to the results. This is not a claim
that nested joint session history is completely implemented.


The final viewer review found a further observer-dispatch defect. The rendering
loop called only the top Window's IntersectionObserver and ResizeObserver
registries. Child registrations neither requested their initial rendering update
nor received it. The thumbnail elements remained visible with empty `src`
attributes, so this was not a failed image fetch. Rendering phases now snapshot
active child Documents in shadow-including container order, include nested
registrations in scheduling/liveness checks, and skip Documents retired during a
parent callback. Intersection notifications retain their separate task source.
Tests cover nested and shadow-hosted frames, initial updates without a DOM change,
parent-before-child ordering, unchanged geometry, and retirement during callbacks.

The almost invisible next-image arrow exposed another SVG sizing case. For a
background image with a natural ratio and no natural dimensions, CSS Backgrounds
3 specifies `contain` sizing for `background-size:auto auto`. The graphics adapter
was instead treating the SVG decoder's fallback pixels as natural dimensions.
Background sizing now uses the resource's ratio metadata and the positioning area;
explicit dimensions, single automatic axes, percentages, contain/cover, and
ordinary dimensioned images have regression coverage.

### Script and typing performance

Profiling identified repeated named-property enumeration on forms. A form's named
properties can shadow inherited properties such as `parentNode`; simply bypassing
the proxy would change web behavior. Instead, candidate names are indexed per DOM
revision, preserving tree order, past names, reassociation, and image fallback.
Form ownership now comes from the canonical arena algorithm, avoiding repeated
JavaScript ancestor/property walks and avoiding author-defined getters during an
internal ownership query.

The canonical arena also supplies form named-item candidates in one native call,
avoiding repeated JavaScript property access for each candidate while preserving
the named-property algorithm.

Further traces found three synchronous layout reads per edit, including a temporary
text-measurement element. Input value changes are no longer treated as child-list
mutations when independence is provable. Directional selectors retain a full
fallback around automatic-direction ancestors; an ordinary insertion under fixed
direction does not invalidate unrelated document subtrees. Placeholder and relational
selector interactions have focused regression coverage. CSSOM reads still observe
current layout; no stale width is substituted to improve timings.

A further invalidation defect involved detached shadow trees created by feature
probes. Mutating an unrelated detached measurement element invalidated the entire
connected page merely because a shadow root existed. Shadow dependencies now
require a shared shadow-including root. Tests retain inheritance invalidation
inside a detached shadow tree and compare warm/cold styles after connection.

For the same 26-character phrase, `this is how it should look`, injected as ordinary
keys over roughly 1.3 seconds, the intermediate measured input-event spans were:

| Release stage | First-to-last input event |
| --- | ---: |
| Before form-property optimization | about 28.3 s |
| With a form named-property index | about 17.0 s |
| With native form-owner lookup | about 12.1 s |
| With native form named-item candidates and narrower value invalidation | about 9.1 s |
| With detached shadow dependency isolation, repeated run | about 10.0 s |

These measurements exposed a real responsiveness problem; they are not claims of
acceptable latency. The initial delay was also checked visually without tracing:
after six seconds only the beginning of the phrase was visible. The shadow fix did not
produce a demonstrated timing improvement on this workload. At that checkpoint, remaining costs
included synchronous script layout reads and native edit acknowledgement waiting
for a complete presentation update. The untraced pass-21
viewer also showed substantial action backlog: the first next-image click had
not appeared after six seconds, and later captures showed queued image advances
before the close action finally returned to results. Correct settled rendering
does not establish responsive interaction. The next-image and close captures are
retained as `pass21-native-viewer-next-retry.png` and
`pass21-native-viewer-closed-settled.png`.

### Responsiveness follow-up (20 September)

Brave 1.95.104 (Chromium 153), from the official portable ARM64 release, was
added as the responsiveness reference. It ran as a native Wayland client on the
same compositor. Its 40-pixel sidebar was accommodated by a 1960 × 1080 output,
leaving the same 1920 × 1080 page viewport used by TRust. Keyboard injection sent
26 ordinary press/release pairs in approximately 1.31 seconds.

| Measured stage | Bing first-to-last input event |
| --- | ---: |
| Pass 22 | 9,992 ms |
| Pass 23 | 8,348 ms |
| Pass 24 | 4,211 ms |
| Pass 25, warmed autocomplete | 2,188 ms |
| Pass 26, warmed autocomplete | 2,249 ms |
| Pass 27, warmed autocomplete | 1,440 ms (animation callbacks starved) |
| Pass 28, warmed autocomplete | 1,456 ms |
| Pass 29, warmed autocomplete, two instrumented runs | 1,521 / 1,446 ms |
| Pass 31, warmed autocomplete, instrumented | 2,041 ms |
| Pass 32, warmed autocomplete, instrumented | 1,789 ms |
| Pass 34, warmed autocomplete, instrumented | 2,107 ms |
| Pass 36 baseline (pass 34), three interleaved runs | 2,302 / 2,379 / 2,302 ms |
| Pass 36 GC deferral, three interleaved runs | 1,815 / 1,849 / 1,817 ms |
| Pass 37 queued-input budget, instrumented | 1,278 ms |
| Pass 38 partial scene damage, instrumented | 1,264 ms |
| Pass 39 listener/base search caches, instrumented | 1,282 ms |
| Pass 40 native Window-name revision, instrumented | 1,290 ms |
| Pass 41 Web IDL visibility order prototype, instrumented | 1,272 ms |
| Brave reference, two native runs | 1,256 / 1,283 ms |

The pass-24 immediate native capture still contains only the beginning of the
query; the Brave immediate capture contains the full query and suggestions.
The gap is real even though settled geometry is substantially improved.

The follow-up changes narrow form-layout invalidation to changed controls and
ancestors, retain unrelated formatting contexts, and remove unnecessary global
selector invalidation for `:checked`, link state, fixed direction, and provably
unrelated simple `:has()` dependencies. Detached measurement attributes no longer
invalidate connected layout. An actor FIFO now allows keyup to follow the input
checkpoint without an extra standalone presentation-acknowledgement round trip;
script value/selection changes still precede the following native editing default.

A separate 300-card fixture reproduces synchronous geometry reads around a
transient measurement span. Its input-event span fell from approximately 12,787 ms
in pass 22 to 2,187 ms in pass 23 and 1,317 ms in pass 24. Brave took 1,260 ms.
These are diagnostic runs, not a broad benchmark suite. Native inspection also
found and fixed a text clipping error for blockified controls: their text clip
must retain the available content width, rather than the previous value's glyph
advance. Block, flex, and grid regressions cover the same general behavior.

Profiling also measured 384 rebuilds of Window's named-element index during one
26-character run, totaling about 1,744 ms. The index used public DOM NodeList,
localName, and getAttribute calls after every unrelated DOM revision. The implementation now reads canonical, tree-scoped native candidate records and retains its
index when they agree. Tests cover document/shadow boundaries, foreign namespaces,
duplicate collections, reordering, renamed child navigables, cross-origin target
precedence, and author-overridden DOM methods.

Further layout traces showed that merely restyling a header containing an SVG
cleared every formatting cache. SVG invalidation now rebuilds SVG resources and
their containing layout paths while retaining independent HTML contexts. Tests
compare reused and cold geometry, paint, hit-testing, and terminal rows, including
reference-target mutation, text changes, removal, and reinsertion.

Structural invalidation now also checks the possible parents of child-index,
adjacency, and emptiness dependencies. A rule for a nested list does not force
restyling that list when a sibling is inserted beside the list's ancestor.
Positive type/ID/class and ancestry guards are conservative: logical/state tests
are omitted, sibling relationships that may change are not assumed, and complex
relational/filtered-index cases retain the full fallback. Regressions cover new
adjacency after removal, negated positional selectors, ancestor conditions,
`:empty` effects on following siblings, and cache retention for unrelated parents.

The pass-26 full library run passed 1,812 tests (25 ignored), with four test
threads; all 48 desktop tests passed (2 ignored). The iframe network fixture now
starts its existing accept deadline after platform bootstrap, so platform initialization
cannot consume the request window before navigation begins. Its behavioral
assertions and timeout duration remain unchanged.

The pass-26 immediate capture still showed only “this is how it” at the end of
the 1.32-second injection. Its later complete value took 2.25 seconds from the
first input event. Warm diagnostic repeats varied with background work and
suggestion responses (approximately 1.67–1.86 seconds); none established Brave
parity. Traces separated approximately 25–27 ms per insertion, including three
synchronous layout reads, from approximately 23–24 ms preparing a presentation.
Click/hover listener discovery alone took about 4 ms per presentation.

The next release retains listener-discovery ID lists until actual registration,
removal, connection, or Window retirement changes them. It does not retain detached
objects through those caches. Tests cover duplicate registration, capture matching,
`once` removal before invocation, abort signals, detach/reinsert, child registries,
and retiring child Documents in all three Lumen execution tiers.

Ordinary native text keys now carry an insertion intent to the resident actor.
Keyboard cancellation and its microtask checkpoint run first; the edit is then
formed from the canonical current value and UTF-16 selection, followed by the
existing cancelable `beforeinput`, insertion, `input`, and checkpoint. A later
keyup or typed character stays in the same FIFO sequence without waiting for a
complete desktop presentation round trip. Glyph navigation and IME retain their
native editing path and presentation barrier. Actor-owned insertions do not mark
the native editor as optimistically ahead of the DOM, so each published frame can
update the displayed value while more keys remain queued.

The actor also bounds continuous input preference to a rendering interval, letting
ready rendering and other task sources progress under a stream of keyboard tasks.
A dirty task can use an already-due rendering opportunity rather than adding an
extra 16.7 ms wait after completing its work. No keyboard event or microtask
checkpoint is dropped. The focused queued-key regression covers cancellation,
non-BMP selection replacement, input and keyup microtasks changing the next
insertion, read-only controls, trusted-event flags, and final painted state.
Pass 27 reduced the warmed input-event span to 1,440 ms, but its immediate
capture still showed an incomplete query. Its animation callbacks were delayed
until the input stream drained (median input-to-rAF approximately 744 ms), and the
autocomplete popup remained stale. This was rejected as an acceptance result.

The next scheduling correction runs animation callbacks in the rendering
opportunity before layout and ResizeObserver, including a snapshot of active
Documents in container order and a common frame timestamp. Reentrant callbacks
wait for the next opportunity; retired Documents are skipped. A separate actor
regression verifies that an input-handler layout change followed by an animation
callback does not expose the intermediate geometry to ResizeObserver. At that stage, after a
render, one other ready task source could run before the next bounded input burst.
This ensured progress but, as the next trace showed, still allowed short tasks to accumulate.

Earlier rendering opportunities are restricted to interactive work. The existing
coalescing interval remains for ordinary background messages; the focused
background-message regression passes after this correction. In pass 28, median
input-to-rAF fell from 744 ms to 28.7 ms (maximum 62.7 ms); the 26 input events
spanned 1,456 ms. The immediate native capture still showed only “this is how it
sh”, so this also remained below the acceptance target. This input-event span
excludes time before the first event reaches JavaScript. A separate phase trace
found a 405 ms timer followed by a 169 ms checkpoint immediately before the first
key. A second trace with inactive autocomplete was excluded from comparison.

Profiling the long timer found repeated offset reads walking ancestor chains:
approximately 4,000 computed-style object creations and hundreds of `offsetLeft`,
`offsetTop`, and `offsetParent` calls per batch. The next optimization reads the
required positioning bits directly from the canonical cascade, avoiding public
`getComputedStyle` calls and their proxy allocations. The offset algorithm still
walks the flat tree and respects Document boundaries. Tests cover an overridden
author `getComputedStyle`, positioning/transform/filter/contain changes, inline
and stylesheet mutation, detach/reinsert, slots, and the Filter Effects exception
for each Document root. This does not add support for previously unsupported
perspective or will-change declarations.

The native Window entry also opts out of constructing terminal-only presentation
metadata on each retained paint. Terminal entry points retain their metadata and
adapter behavior. A focused comparison checks identical graphical paint,
geometry and hit-test metadata with that opt-out, and exact terminal rows,
anchors, fixed layers and scroll regions against the complete adapter.

Pass 28 passed 1,816 library tests (25 ignored) and 48 desktop tests (2 ignored),
plus formatting and Clippy. Pass 29's default-concurrency run exposed the existing
wall-clock-sensitive background-coalescing fixture again; its focused rerun
passed without changing its assertions or deadline. The complete pass 29 run with four test threads passed 1,817 library tests and
48 desktop tests. Formatting and Clippy passed. The uninstrumented native capture
still showed only “this is how it should lo” immediately after the 1.31-second
injection, with the popup still showing old trending suggestions. This did not
establish Brave parity. An instrumented pass 29 trace measured 24 ms median
per insertion and 17 ms preparing each rendering update; terminal metadata
construction fell from about 3.2 ms to 0.23 ms of remaining paint bookkeeping.

A further trace showed short timer tasks accumulating by approximately 300 ms.
The next scheduler revision gives ready background sources up to eight task
selections or four milliseconds between input bursts, always allowing at least
one selected task. Each task still runs to completion and receives its own
microtask checkpoint. FIFO within each source, input progress, timers, messages,
and animation callbacks are exercised together by a finite queued-key test.
A diagnostic Resource Timing/XHR comparison separated wire time from delivery:
responses took approximately 77–92 ms to arrive, while delivery during the key
stream accumulated hundreds of milliseconds to over one second. That run
included tracing and concurrent compilation, so it diagnoses queueing rather
than establishing native acceptance latency.

Native insertion now also explicitly routes to a child control’s Window, with
a three-tier regression covering UTF-16 replacement, cancellation, event identity,
and the absence of parent-document input events.

The synchronous-read trace identified a second unnecessary cost: inserting and
removing a measurement span directly inside a form invalidated all existing
control styles, even after the selector dependency analysis had proved them
independent. Forms now use that same proof as other ordinary containers.
Structural and relational selectors and association-dependent state keep their
existing fallbacks. Tests check form ownership and warm/cold style equivalence;
the layout regression also compares geometry, graphical paint, hit testing,
and terminal rows with a full recalculation. The native phase trace reduced each
measurement-span geometry read from about 7 ms to 3 ms, rebuilding one style
instead of 54. However, pass 31 did not meet acceptance: its uninstrumented
immediate capture showed only “this is how it” after the 1.31-second injection.
Suggestions now changed during typing, but servicing them made the input lag
worse. A separate instrumented run spanned 2,041 ms. Its phase breakdown contained
540 ms of insertion work, 611 ms of render preparation, 167 ms in response tasks,
and three roughly 100 ms checkpoints. These categories diagnose the workload;
they are not independent end-to-end latency measurements.

A further cache defect made each change to listener activation metadata discard
all formatting contexts. The next revision invalidates the changed activation
subtrees and ancestor paths, preserving unrelated CSS styles and layout. The
regression compares warm and cold geometry, graphical paint, hit metadata, and
terminal rows for added/removed nested targets and slotted content. Pass 32's
release build and focused test passed. The native phase trace shortened the
input-event span to 1,789 ms, with a 20.2 ms median input-to-rAF delay (24.5 ms
maximum), but the uninstrumented immediate capture still showed only “this is
how it sh”. It remains below the responsiveness target. Whole-tree rebuilding
on every response disappeared; a page-wide class transition still incurs one
broad restyle. Two approximately 100 ms checkpoints coincide with Lumen's cycle
collector reclaiming about 16,000 objects from a roughly 123,000-object retained
heap. Investigation of these remaining costs is ongoing.

A native CPU sample profile (9,188 samples, five typing bursts) separated the
resident actor from the GUI thread. Within graphical paint, roughly a quarter
of sampled work repeatedly resolved root/body background propagation for each
fragment. Pass 33 resolves that source once per immutable paint transaction and
skips image geometry for all-`none` background layers. Mixed-layer indices and
bottom-layer color clipping remain intact. Twenty focused graphical tests passed,
including root/body style transitions and nested document clipping. The release
build succeeded, but three uninstrumented immediate captures still showed only
“this is how it” or “this is how it s”. This change does not establish responsiveness
parity.

Function tracing then found that `hasRenderingUpdate` repeatedly exported the
entire shadow-including node tree to JavaScript to order a few child Documents.
One diagnostic typing burst spent about 252 ms in those checks. The current
revision filters navigable containers in the native DOM walk before returning
IDs. This retains the same shadow-including order, DOM-epoch invalidation, active
Document snapshot, and retirement checks. A three-tier regression covers closed
shadow roots, unrelated nodes/attribute edits, reordering, removal, and invalid
roots; existing reentrant-rAF and nested-observer tests also pass. Diagnostic
traces taken during compilation identify work but are excluded from acceptance
timing comparisons.

Pass 34 still failed native acceptance: three immediate captures showed only
“this is how i” or “this is how it”. The phase trace recorded 616 ms of insertion
work, 617 ms preparing rendering updates, and two task-boundary cycle scans of
144 and 159 ms. These are diagnostic observations from one run, not an
interleaved performance comparison. The earlier reference also differed in cookie
banner state, which must be matched in subsequent comparisons.

The next scheduling change defers optional task-boundary cycle scans while native
input is queued or has arrived within 150 ms. Allocation safepoints, allocation
limits, promise jobs, and ClearKeptObjects remain active. A pending request survives
quiet checkpoints; a maintenance wake services it after the input burst even when
future page timers would otherwise prevent indefinite-idle collection. This is
the host's permitted collection policy under ECMA-262's liveness rules, not a
change to JavaScript job ordering. Engine tests exercise all three tiers, including
allocation-triggered collection during deferral and WeakRef consistency within a
job. The browser test verifies checkpoint progress and reclamation with a future
timer pending. All 1,088 engine tests pass. Native performance validation is still
required before attributing an end-to-end improvement to this change.

Three interleaved baseline/candidate runs were then performed without compilation
or tests running, with the consent banner dismissed through its native Reject
button in both variants. The median input-event span fell from 2,302 to 1,817 ms
(21%). Baseline runs collected two or three times during the burst; candidates
collected after it. Peak RSS grew by 41–48 MiB above the pre-burst sample for the
baseline and 58–74 MiB for deferral. Absolute post-burst RSS also varied; longer
idle measurements are still needed to distinguish allocator retention and live
page differences from a lasting memory cost. Post-collection live object counts
were approximately 176,000–179,000 in both variants. This is a bounded scheduling
tradeoff, not a claim that collection became cheaper or that memory was unchanged.

Pass 37 lets already-queued native tasks run for up to two 60 Hz intervals before
yielding to rendering/background work. An empty input queue yields immediately;
no event or checkpoint is merged. Five focused input tests, including cancellation,
UTF-16 selection, child realms, and task-source progress, passed. Its release build
succeeded. An instrumented native burst spanned 1,278 ms, with 18 rendering updates
instead of 25–27. The uninstrumented immediate capture still missed two final
characters, and the instrumented capture missed one. GUI rasterization still took
about 20 ms per changed frame, while the suggestion popup was also behind the query.
Matching the input-event span alone therefore does not establish Brave parity.

The subsequent desktop damage fix compares aligned display-list commands
individually. Identical transforms, clips, and compositing scopes between separate
leaf edits no longer force a full repaint or contribute their unchanged contents
to damage. Changed state, unaligned stateful edits, viewport/canvas changes, and
invalid stacks retain full fallback. The crop still replays the complete scene in
CSS painting order. Focused tests verify the conservative bounds and fallback;
native CPU pixel comparisons match a fresh full frame at 1×, 1.5×, and 2×, in both
forward and reverse edits. The broader renderer suite passed 78 tests (one ignored),
desktop suite 49 (two ignored), and resident-actor suite 35. Native validation
of pass 38 is recorded below.

## Standards consulted

The local official library was used through `web-standards-skill`. These are local
snapshots fetched on 6 September 2026, not claims of checking current upstream text.

| Source / recorded revision | Relevant clauses |
| --- | --- |
| WHATWG HTML, `e5071a20c8569d8a3ec02ed27dd01b948773f850` | [form named properties](https://html.spec.whatwg.org/multipage/forms.html#dom-form-nameditem), [form ownership](https://html.spec.whatwg.org/multipage/form-control-infrastructure.html#form-owner), [text selection](https://html.spec.whatwg.org/multipage/form-control-infrastructure.html#textFieldSelection), form submission, History delta traversal/state restoration/popstate, Location navigation, document base URLs, entry settings, image requests, directionality, [Window named access](https://html.spec.whatwg.org/multipage/nav-history-apis.html#named-access-on-the-window-object), [event-loop scheduling](https://html.spec.whatwg.org/multipage/webappapis.html#event-loop-processing-model), and rendering opportunities |
| WHATWG DOM, `a2331a45360129e8645ef7e0a04740241b6e3726` | [collections](https://dom.spec.whatwg.org/#old-style-collections), qualified-name matching, insertion/removal, connectedness, [listener addition](https://dom.spec.whatwg.org/#add-an-event-listener), [removal](https://dom.spec.whatwg.org/#remove-an-event-listener), and once-listener invocation |
| CSSWG drafts, `81c27f68690138345b2b3b6af8ccc42dad3dca1d` | CSS 2 inline margins/replaced sizing/float avoidance/shrink-to-fit; CSS Align 3 baseline export and CSS Overflow 3 scroll containers; CSS Images 3 sizing; CSS Backgrounds 3 background-size; CSS Content 3 generated images; CSS Pseudo 4 inheritance; Media Queries 4 viewport; CSSOM View element geometry; CSS Syntax 3 declarations; CSS Fonts 4 font-face/document isolation; CSS Shadow 1 tree-scoped names; Selectors 4 directionality/placeholder state, structural pseudo-classes, and sibling/ancestor combinators |
| Filter Effects 1, local CSSWG/FXTF sources | Filter order, color calculations, group compositing, clipping, and containing blocks |
| Web IDL, `8f182624f632a0ce485e236edbc1df18ca385b1d` | [named-property visibility](https://webidl.spec.whatwg.org/#dfn-named-property-visibility), [named-properties object GetOwnProperty](https://webidl.spec.whatwg.org/#named-properties-object-getownproperty), legacy named properties |
| WAI-ARIA and Web IDL, local official sources | Nullable DOMString reflection, integer conversion, receiver checks, and legacy named properties |
| Intersection Observer, `633339846d89a39b3797168bcf249d8739e0cb00`; Resize Observer, local CSSWG source | Initial observations, observer lifetime, notification task source, and HTML rendering updates across active Documents |
| UI Events, Input Events, Selection API, local official sources | Keyboard/editing order, cancelable beforeinput, input data, selection state, and queued selection notifications |
| High Resolution Time, `1f0b9faf793db3b06c8e85462448e8a812d00fb9` | [relative high resolution time](https://w3c.github.io/hr-time/#dfn-relative-high-resolution-time): translate one rendering timestamp to each Window’s time origin |
| ECMA-262, `e28783d5fc9dc12b3de905961e2c71410b38a202` | GetFunctionRealm, execution contexts, function calls, callback entry context, and [WeakRef liveness](https://tc39.es/ecma262/#sec-liveness) |

The authoring sources are under `/big/web-standards/repositories/`. Nearby source
comments and focused tests record the clauses used for each implementation.

For the responsiveness changes, the consulted local authoring sections include
[HTML task-source ordering](/big/web-standards/repositories/whatwg/html/source:122760),
[rendering opportunities](/big/web-standards/repositories/whatwg/html/source:123454),
[text-control selection units](/big/web-standards/repositories/whatwg/html/source:63202),
[UI Events keyboard ordering](/big/web-standards/repositories/w3c/uievents/sections/event-keyboardevent.txt:585),
[DOM listener addition/removal](/big/web-standards/repositories/whatwg/dom/dom.bs:1212),
[once-listener invocation](/big/web-standards/repositories/whatwg/dom/dom.bs:1706),
[rendering algorithm order](/big/web-standards/repositories/whatwg/html/source:123170),
[relative timestamps](/big/web-standards/repositories/w3c/hr-time/index.bs:567),
[form ownership](/big/web-standards/repositories/whatwg/html/source:60713),
[child-index selectors](/big/web-standards/repositories/w3c/csswg-drafts/selectors-4/Overview.bs:3888),
and [offset geometry](/big/web-standards/repositories/w3c/csswg-drafts/cssom-view-1/Overview.bs:1850).
The official clause links and snapshot identities above describe the same sources.

## Validation checkpoints

At the initial visual checkpoint, twenty-one release stages were built throughout the investigation. The current
review artifacts are `target/release/trust` and
`target/release/trust-desktop`, built with `cargo build --release`. Native visual
review used saved release binaries, never a debug executable.

- `cargo test -- --test-threads=4`: 1,800 library tests passed, 25 ignored;
  48 desktop tests passed, 2 ignored; 15 other binary tests passed.
- Lumen library suite: 1,086 passed.
- `cargo clippy --lib --bin trust-desktop`, `cargo fmt --check`, and diff
  whitespace checks in both repositories passed.
- Default-concurrency runs exposed timing-sensitive failures in the existing
  rendering-coalescing and iframe fixture tests. The complete final run with
  four test threads passed; no test deadlines or assertions were relaxed to
  obtain that result.

The initial native visual review included the home page and masonry image panel, typing
and settled autocomplete, selection, Enter submission, filter controls, and the
image viewer. Both 1920×1080 and 1280×720 windows were compared with reference
captures. The narrow topic grid has four columns and the wide grid six; the
cards, their spacing, and headings align with the reference. The header's extra
roughly 6.6px is removed. The filter popup now encloses its custom-size controls,
although default control appearance and the popup's precise dimensions still
differ from the reference.

## Scope and remaining limits

This investigation does not establish complete web-platform conformance or a
pixel-perfect result for every response from Bing. In particular, dynamic remote
font fetching still uses the existing resource pipeline; the new synchronous
activation path covers embedded data fonts. Full font descriptor/range support is
not implied. The existing automatic-direction computation also has limitations;
this work preserves conservative invalidation around that behavior. Observer
dispatch across child Documents is corrected, but this is not a full rewrite of
the existing IntersectionObserver root/clipping or ResizeObserver box/depth
algorithms.

The initial checkpoint’s remaining acceptance issues were typing and viewer
interaction latency, and the default appearance
and baseline of plain input buttons/text fields in the custom-size filter row.
Child-document thumbnail loading and the next-image arrow were addressed in the
last standards pass; the final native release capture verifies their displayed result. The custom-size popup
measured about 511×223px in TRust versus about 520×207px in the supplementary
Chromium reference. Its former 192px background no longer leaves the controls
outside the panel, but this is still a visible difference. The desktop's existing
Escape-as-Stop shortcut also preempts the page's viewer Escape handler; the close
button is the verified route back to results.

The Lumen changes are in the maintained sibling checkout, `/big/Code/Lumen`, based
on `ea7cc86b38b6190568358bd794563d757c7f28dd`. They must accompany the TRust changes.
TRust started from `870afd1`. The exact default theme of native form controls is partly user-agent-defined;
these remaining appearance differences should not all be described as normative
CSS violations. No release executable has been installed or promoted,
and neither repository has been pushed. At the user’s explicit request, the
work is now saved in local commits: Lumen `fc16506` (entry realms) and `328b4bb`
(optional collection scheduling), TRust `32f8cd4` (renderer primitives) and
`a3d1455` (browser integration), followed by the style-cache checkpoint containing
this report. These commits do not mark responsiveness accepted.

### Responsiveness pass 38: native damage and network delivery

The release build succeeded (6m20s), and strict all-target Clippy passed. Native
review confirmed that the damage change is effective: 15 of 19 frames during the
instrumented typing interval used partial damage; isolated text frames took about
1.7–2.1 ms in the desktop raster/presentation trace, instead of about 20 ms for
the previous full frames. The uninstrumented immediate capture still lacked the
last character, so this is not Brave parity.

The added diagnostic XHR listener separates wire completion from delivery to
JavaScript using Resource Timing. Suggestion requests completed on the wire in
roughly 70–100 ms, but their delivery delay grew from 55 ms to 1.7 seconds during
the 26-character burst. Old suggestion results accumulated in TRust's task queues.
No requests, callbacks, or microtask checkpoints were dropped to hide this delay.
The input event span was 1264 ms for 1314 ms of physical key injection.

The pass-38 instrumented run used 1,012,328 KiB RSS before typing, peaked at
1,039,468 KiB, settled to 903,724 KiB after 3 seconds and 858,720 KiB after another
30 seconds. These live-page measurements show post-burst reclamation, but are not
a controlled proof of unchanged memory consumption.

Pass 39 adopted type-specific listener invalidation after tests covering exact
case matching, once-listener removal, abort, connection and child retirement.
It also caches the first base-element search by document root and DOM revision,
including absence, while resolving the selected URL against the current fallback.
The 13 responsive-image tests, listener tests, strict Clippy and release build pass.
The native immediate screenshot still lacked two characters. Network delivery
had a 1,061 ms median and 1,437 ms maximum in the instrumented run; the network
itself took a median 87 ms. RSS was 990,628 KiB before typing, 1,040,236 KiB peak,
894,104 KiB after three seconds and 854,940 KiB after another 30 seconds.

Pass 40 separates the native Window-name candidate revision from changes to
input values and ordinary styling. The JS index keeps checking DOM/navigable
revisions for frame-origin changes, while avoiding candidate-array exports when
element names and tree membership cannot have changed. Tests exercise all three
execution tiers, preserved collections, renaming, tree order, cross-document
adoption, foreign element IDs, shadow boundaries and frame target names.
Its release SHA-256 is `6998d2472d0df49b774dc76ed27c399059c82c0d200bbfe7cc1cbf27e6b29b43`.

Pass 41 also corrects Web IDL named-property visibility order: supported-name
membership precedes own-property and prototype checks. A regression installs an
author Proxy in the Window prototype chain and proves that unsupported names
cause no visibility traps, while supported names still obey prototype masking.
This removes needless prototype checks on missing-global reads. The native
prototype input span was 1,272 ms, but response delivery still had a 968 ms median
and 1,320 ms maximum. Native captures were inspected both with and without probes;
this does not establish autocomplete parity.

A fresh Brave diagnostic confirms the request pattern: all 26 suggestion requests
complete, with approximately 90–110 ms network time and typically single-digit
to low-tens-of-milliseconds callback delivery. It was run during compilation and
at a 1880-pixel page width, so it is diagnostic evidence of queue behavior, not
an acceptance timing comparison. TRust is still building a backlog of callbacks.

The next scheduling regression isolates repeated resetting of timer preference
by input/rendering turns. A finite queued input stream, ready XHR callbacks,
rendering requests and timers that consume the service budget make the starvation
observable while checking FIFO and microtask ordering. It failed before the
scheduler change with “network callbacks waited for the entire input stream”,
then passed after retaining background-source preference across input, hover and
rendering. Timer/network FIFO and individual checkpoints remain intact. The
release build succeeded; all 36 actor tests, three Window-name tests, strict
Clippy and formatting pass. Its unprobed immediate capture shows the full query,
but suggestions still belong to an earlier prefix. A separate native-key sampler
records the injection's wall-clock timestamp before every physical key press and
compares it with input/rAF timestamps. Pass 42 measured 20.6 ms median / 50.1 ms
maximum native-key-to-input and 41.6 / 96.1 ms native-key-to-rAF. rAF is before
presentation, so these are not key-to-photon measurements. Response delivery
still measured 992 ms median / 1,357 ms maximum. Acceptance remains unmet.

Pass 43 gives ready background tasks at most one nominal display interval
(16.667 ms) of preferential service, still capped at eight tasks, with each task
and checkpoint atomic. This lets a native response handoff and its queued
networking callback share a service opportunity. Its input/network/timer tests
pass; release validation is pending. The Window visibility regression also covers
a name removed by an author prototype trap after membership was checked: HTML's
named getter returns a live, empty collection in that case.

The pass-43 release succeeded (6m24s; SHA-256
`8f4d17dd6d81854d10e0b5ae6ac0be862ea8dc59967a5c5be7ccc9c0954ed207`).
Native unprobed immediate/settled images were visually reviewed. The immediate
image is one character short and still shows an old suggestion prefix. The
instrumented run measured 23.1/48.6 ms median/maximum native-key-to-input and
40.6/91.6 ms native-key-to-rAF; networking delivery remained 893/1212 ms across
28 measured XHR completions (including two ancillary requests). Nine suggestion
callbacks ran during the burst. This change does not establish Brave parity.

Pass 44 profiles callback internals rather than attributing the remaining lag
solely to queue policy. Native-host and JS-method diagnostic passes identify
three synchronous geometry measurements per edit, UTF-8 response decoding
(2.7 ms median per response, about 67 ms across the burst), and rebuilding the
Window-name index with wrappers for every candidate. Window-name membership now
retains native identities and only resolves the requested element/collection.
A release-43 prototype of that JS change delivered eleven callbacks during the
burst, with 741 ms median delivery delay; this is still diagnostic, not acceptance.

The UTF-8 path uses native bulk validation, preserving Encoding's maximal
ill-formed subsequence handling and the unconsumed I/O queue on fatal errors.
BOM serialization waits for an actual output scalar, including after empty or
partial streaming chunks. The new conformance fixture failed before the change
on a split BOM and passes afterward in interpreter, bytecode and JIT tiers. It
also exercises all pairs of chunk boundaries, invalid continuation bytes,
overlong sequences, surrogates, out-of-range scalars, fatal continuation, copied
pending input, view offsets and a response-sized mixed-Unicode string. Relevant
authority: local Encoding snapshot `a985b62a9b45c17da3e17a9f0a0b4e30c34c4a8a`,
[UTF-8 decoder](https://encoding.spec.whatwg.org/#utf-8-decoder),
[TextDecoder.decode](https://encoding.spec.whatwg.org/#dom-textdecoder-decode)
and [serialize I/O queue](https://encoding.spec.whatwg.org/#concept-td-serialize),
read from `/big/web-standards/repositories/whatwg/encoding/encoding.bs`.
The existing legacy-decoder and worker codec implementations are outside this
UTF-8 optimization's scope; this is not a claim of complete Encoding API conformance.

Pass 44's standard release SHA-256 is
`da2df852cd9603282eb9d02b4400315007688d3e4be1ee7230ac6265f58db8e3`.
The unprobed native immediate and settled screenshots were inspected. The final
character still arrives after the immediate capture, and autocomplete still
shows an earlier prefix. The measured callback median fell to roughly 14 ms
from 17 ms, but only ten suggestion callbacks ran during the typing burst.
XHR delivery was 721 ms median / 1162 ms maximum, with 77 ms median wire time.
Native-key-to-input was 25.4/57.9 ms and native-key-to-rAF 43.0/96.9 ms.
RSS was 930216 KiB before typing, 1000040 KiB peak, 902600 KiB after 3 seconds
and 837216 KiB after 30 additional seconds. These are live-page observations,
not a controlled memory-regression confidence interval. Acceptance is still unmet.


## Pass 45 — bounded listener-discovery indexes and saturated actor

The pointer-target discovery caches now maintain sets of targets with active
click/hover listeners. Changing one autocomplete subtree no longer rescans the
entire listener registry. Membership follows listener add/remove, `once`, abort,
detachment/reinsertion, event-handler properties and child-Window retirement.
The sets are subsets of the existing strong registry and introduce no longer
lifetime for detached objects. DOM listener order and invocation are unchanged.
DOM snapshot `a2331a45360129e8645ef7e0a04740241b6e3726`,
[add/remove](https://dom.spec.whatwg.org/#concept-event-listener) and
[inner invoke](https://dom.spec.whatwg.org/#concept-event-listener-inner-invoke)
were consulted before the change.

The extended regression runs in all three Lumen tiers. The focused test, three
listener tests, detached-node test, strict Clippy, and standard release build
(6m13s) pass. The native immediate and settled captures were visually reviewed:
text is still one character short at the immediate capture and suggestions still
represent an older query. This is not accepted as Brave-equivalent behavior.

In a CPU-quiet unprobed run the page actor used 1.31 CPU seconds during a 1.314
second physical typing burst. The measured run used 1.32 CPU seconds during the
1.366 second injection (including injector startup). This confirms saturation,
rather than an idle actor waiting unnecessarily. Measured native key-to-input
was 21.7 ms median / 50.6 ms maximum; key-to-rAF was 41.2 / 95.6 ms. XHR delivery
remained 891 / 1205 ms (network time 89.6 / 142.1 ms). Live page variation means
these single trials do not establish a speedup relative to pass 44.

The burst logged 88 geometry rebuilds, including three synchronous geometry reads
per insertion. Median rebuild time was 3 ms; the 19 rendering updates spent a
median 8 ms in the graphical adapter, including 5 ms in paint extraction.
Further work targets actor CPU demand. Separate captures:
[immediate](../target/bing-review/pass45-unprobed-immediate.png),
[settled](../target/bing-review/pass45-unprobed-settled.png).


## Pass 46 — warm computed values and CPU profile

A native frame-pointer profile of pass 45 attributed 2,682 of 2,961 samples to
its page actor, versus 247 to the GUI thread. Synchronous geometry rebuilding
accounted for about 31% of actor samples; computed-style work was a substantial
part of those rebuilds. The actor is busy rather than simply waiting for input.

Tracked non-inherited computed values now use the existing per-node/property
cache, with the same style and font invalidation as inherited values. Used values
and custom-property substitution remain in their existing consumers. CSS-wide
keyword detection avoids tokenization only when the first token cannot match;
comments, escapes, case folding and possible identifiers retain full parsing.
The conformance regression exercises explicit inheritance, percentages,
custom-property changes, structural/relational selectors and escaped/commented
keywords. Authority: local CSSWG snapshot
`81c27f68690138345b2b3b6af8ccc42dad3dca1d`,
[CSS Cascade computed values](https://drafts.csswg.org/css-cascade-5/#computed)
and [CSS Syntax tokenization](https://drafts.csswg.org/css-syntax-3/#consume-token).

The focused test, 241 DOM tests (3 ignored), 472 layout tests (3 ignored), strict
all-target Clippy and release build pass. Release SHA-256:
`4e0636b711886e9ad0bd6915423e26553dfa84fd87f97b69db439326e7b0f11f`.
A subsequent formatting-only change affects the new test, not the executable.

The native immediate and settled captures were visually inspected. The immediate
field still lacks its last character and suggestions represent an earlier prefix.
The unprobed actor consumed 1.30 CPU seconds during a 1.312-second physical burst.
The measured run delivered 10 suggestion callbacks during typing; 26 suggestion
requests took 72.7 ms median / 88.2 ms maximum on the wire, then waited 709 / 1110 ms
for callback delivery. These single trials do not demonstrate a material speedup.

The complete two-thread test run terminated with SIGSEGV. The core places the
crashing thread in `loader_scanned_icd_add` in `libvulkan.so.1`, while another
thread was entering the NVIDIA driver through D-Bus. This identifies the crash
site, not a proven root cause. All 18 isolated renderer tests passed serially
(1 ignored). A clean complete suite remains outstanding.

The imported upstream renderer sources contain existing trailing whitespace in
`pixmap.rs`, `clear.wesl` and `render.wesl`. It was retained to avoid mechanical
vendor edits; browser source and subsequent diffs pass whitespace checks.
