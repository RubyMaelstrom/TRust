# Layout performance and reuse

The layout engine retains typed box inputs and fragment outputs across
transactions. A DOM or resource change invalidates dependent inputs; a layout
request may reuse an output only when its complete supported constraint set
matches. CSSOM measurements, observers, graphical painting and terminal
adaptation consume the settled result.

## Failure found in September 2026

An absolutely positioned descendant made every containing item result
uncacheable. Grid sizing measures an item with more than one constraint, so
nested grids repeated their descendants' work at every level. A synthetic
12-level grid with 16 measured boxes made 8,191 item requests per transaction,
even with unchanged input. Eight levels made 511 requests. Counting output
boxes or cache hits alone hid this problem.

Positioned placeholders now retain immutable `Arc<BoxNode>` inputs. They are
portable between transactions and remain at their static positions until the
positioned pass resolves the final containing blocks. A result containing an
external fixed-list index is still excluded from the formatting cache.

## Supported reuse contracts

- **Intrinsic measurement:** node/style, inherited inline context and min/max
  mode. Size containment and subgrid rules still govern the calculation.
- **Independent item layout:** content width, percentage basis, definite or
  indefinite height, aspect-ratio transfer mode, node/style and inline context.
  Active inherited subgrid constraints bypass independent-item reuse.
- **Normal block flow:** containing-block position/width/height, incoming
  cursor position, positive and negative collapsed margins, node/style and
  inline context. Reuse restores outgoing cursor/margins, margin flush history,
  anchors and grid tracks. Incoming or escaping floats bypass this cache;
  contained floats remain eligible. Position is currently an explicit input,
  so moving a later block can require recalculation.
- **Environment:** viewport, base URL, font and SVG metadata revisions,
  intrinsic image sizes and form/control bindings. Unknown global dependencies
  expire results; form changes invalidate affected branches and ancestors.

The fragment tree owns its dependencies. Immutable line payloads share shaped
text, pieces and atom geometry. Line clamping, vertical placement and terminal
canvas adaptation use copy-on-write before modifying a line. The canonical
measurement transaction transfers its completed fragments directly into
shared ownership instead of copying the whole tree for retention.

Both formatting and box-tree caches retain their existing memory bounds.
Shared allocations are counted conservatively from every owning cache entry.
Oversized results are rejected before cloning. Constraint variants use local
LRU replacement; adding a ninth variant no longer discards the other seven.

## Container-size dependencies

Size queries record the subject, candidate container and dimensions read,
including false conditions and eligible containers without a measured box.
Registered container-relative lengths record their size reads as well. A
changed dimension invalidates its readers, inheriting descendants (including
slot projection), and layout ancestors. Pure selector matches survive.

The read graph is bounded to 65,536 edges. Overflow falls back to full value
invalidation and starts a new graph. Broad style revisions also reset it.
Old edges within a revision may conservatively invalidate extra elements.
Generated counter/quote state retains a geometry fallback because its effects
can reach later siblings. This is a deliberate limit of the current dependency
model, rather than an assumption that generated content is local.

Container-relative units participate even without an `@container` rule.
CSSOM handles first discovery after an earlier geometry read, including on a
hidden element, before returning a computed registered-property value.

## Selector rejection

The rule index uses required subject IDs, classes, tags and attribute presence,
including unions of fully indexable `:is()` / `:where()` alternatives. A
2,048-bit ancestor filter additionally rejects impossible child/descendant
chains before the recursive matcher runs. Each rule retains at most 32 bit
positions. Sibling combinators stop ancestor extraction; uncertain logical
conditions stay with the matcher. Hash collisions only admit extra candidates.

The filter advances through current ancestry only when a candidate actually
needs more required keys. A near match stops the search. An unrelated descendant
rule must not make simple rules traverse every ancestor. Extra rejection work
is bounded to 64 ancestor nodes and 256 keys per subject; an incomplete filter
admits the full matcher. Depth and budget regressions protect these fallbacks.
The filter cannot retain an old answer across reparenting, class changes or shadow-root
changes. Scope, sibling ordering, negation, relational selectors, state and
specificity remain full-matcher decisions. Tests compare the candidate path
against a scan of every rule, including after mutations and in shadow trees.

For large candidate sets, lazy searches retain the subject's selector ancestors
for required ID/class/tag keys. They record only queried keys, and advance only
when matching needs another candidate. A near match avoids scanning the whole
chain; a completed unsuccessful search is reused by later rules and nested
logical selectors. Every returned ancestor still runs the complete prefix
matcher. Scope comes from selector ancestry, including featureless hosts.
Hash collisions fall back to ordinary matching. Results belong to one subject
and one immutable pass, bounded to 4,096 key/node entries and 64 KiB of key text.
Budget exhaustion disables reuse without treating a partial search as complete.

Long selector chains with at least three branching combinators additionally
reuse recursive partial matches during one element's immutable matching pass.
The key includes the selector prefix, element, query scope and shadow host.
This temporary memo admits at most 4,096 entries and expires at the end of the
pass. Short selectors take the direct path: an experiment caching every
recursive call measurably slowed ordinary matching. A 24-level ambiguous
ancestor chain now has a regression limit of fewer than 1,000 computed
subproblems, alongside full-match parity tests for scopes and live state.

Existing Text nodes in shadow trees preserve selector results when emptiness
and first strong direction remain unchanged. Layout invalidation follows both
DOM ancestry and ancestry through assigned slots, including forwarded slots.
Replacing text children, distribution changes and direction changes retain
the conservative fallback. Tests include direct assigned text, assigned
elements and text inside a nested shadow root.

## Correctness and measurement

`layout2::engine_bench::incremental_layout_matches_fresh_across_formatting_models`
compares warm and fresh CSSOM boxes, grid tracks, overflow bounds, graphical
display lists/hit tests and terminal rows after text, style, structure and
resource changes. The workload matrix includes blocks, inline text, flex,
grid, tables, floats, positioned descendants, container queries, shadow slots,
images, vertical text and line clamping. Focused tests additionally bound
nested-grid work and verify block margin/float state.

Equivalence alone cannot establish conformance: an old full layout may itself
be wrong. The release acceptance fixture also asserts expected geometry and
computed values. It exposed a slot projection defect in both paths: authored
`display: contents` used light children instead of assigned nodes. Slot
projection now happens before display-based box generation, and author display
can create or suppress the slot box. Focused tests check assigned/fallback
content, display changes, cache reuse, and ordinary shadow/slot ancestry.

`TRUST_LAYOUT_PROFILE` reports actual operation counts and exclusive timings.
Nested instrumented work is charged once, to its own operation. Profiling adds
clock-read overhead; disable it when measuring interaction latency. The engine
matrix supports a separate profiling run and reports median/max wall time.
See [DIAGNOSTICS.md](../DIAGNOSTICS.md#desktop-and-layout-benchmarks) for commands.
HTML parsing and mutation application precede the timed layout transaction.
Browser interaction traces separately include event handling and rendering.

Do not divide a transaction's time by its output box count and describe that
as one box calculation. Transactions may calculate multiple constraints,
copy cached geometry, project boxes, and resolve positioned descendants.
Compare cold layout, unchanged layout, local edits, constraint changes, and
resource changes separately. Machine-specific timings are diagnostics;
ordinary tests assert result equivalence and deterministic work bounds.

## Measurements, September 2026

These are optimized local diagnostics on the ARM64 workstation, pinned to
Cortex-X925 core 5, without a competing build. The baseline executable retained
the earlier engine with workload and operation instrumentation. The profiled
comparison uses median transactions (11 baseline samples, 7 current samples).
The baseline did not contain the added resize phases. Shadow timings before
the slot correctness fix are not a valid performance comparison because that
engine omitted the assigned content.

| Workload / transaction | Earlier, profiled | Current, profiled |
|---|---:|---:|
| 220 block siblings, unchanged | 3.165 ms | 0.519 ms |
| 220 block siblings, local text edit | 3.296 ms | 0.918 ms |
| Long inline context, edit | 4.188 ms | 4.204 ms |
| 220 flex items, edit | 1.738 ms | 1.592 ms |
| 220 grid items, edit | 2.319 ms | 2.167 ms |
| 220 small tables, edit | 4.360 ms | 2.625 ms |
| 12 nested grids with positioned child, unchanged | 80.908 ms | 0.039 ms |
| 12 nested grids with positioned child, edit | 87.250 ms | 0.621 ms |

The nested-grid edit improved about 140 times. Its output still contains
16 measured boxes; removing the repeated calculation matters far more than
reducing the arithmetic in any one box. The ordinary block edit now performs
four block calculations and one inline layout, instead of calculating all
224 block boxes and 221 inline contexts.

Current transactions with profiling disabled (11 samples for warm/edit/resize;
cold is a single sample after process font initialization):

| Workload | Cold | Unchanged | Local text edit |
|---|---:|---:|---:|
| Blocks | 7.456 ms | 0.443 ms | 0.779 ms |
| Flex | 10.253 ms | 0.485 ms | 1.273 ms |
| Grid | 15.771 ms | 0.483 ms | 1.739 ms |
| Tables | 25.238 ms | 1.551 ms | 2.428 ms |
| Container queries | 32.468 ms | 0.714 ms | 1.871 ms |
| Shadow slots | 12.358 ms | 0.474 ms | 1.637 ms |
| Images | 19.238 ms | 0.596 ms | 1.295 ms |
| Vertical text and line clamping | 10.178 ms | 0.699 ms | 1.217 ms |
| 12 nested grids with positioned child | — | 0.035 ms | 0.558 ms |

Resizing a query container took 3.392 ms; changing intrinsic image sizes took
16.478 ms. The shadow edit rebuilt six box-tree nodes and reused 440 item
results, versus 228 rebuilt nodes before narrowing shadow text invalidation
(with slot projection already fixed).

### Individual operations

The cold block workload recorded 1,184.4 microseconds of exclusive block
calculation across 224 calls: about **5.3 microseconds per calculation**.
Its 221 inline layouts took 1,998.2 microseconds, about **9.0 microseconds per
paragraph**. These are workload averages; a table or a long paragraph is more
expensive than one small block.

After an edit, the four remaining block calculations took 35.1 microseconds
in total and one inline layout took 39.9 microseconds. The 224 cache requests,
including copying hits, took 81.9 microseconds (about 0.37 microseconds each).
Storing the four changed results took 118.8 microseconds; the positioned walk
took 134.9 microseconds over 445 visits. The remainder includes style/tree
work, traversal, geometry projection and bookkeeping. Profiling itself also
adds clock reads; use the unprofiled table for transaction latency.

### Selector workloads

The following compare modes in the same executable and assert exactly equal
matched `(element, rule)` pairs:

| Workload | Subject indexing | Rejection and matching reuse |
|---|---:|---:|
| 2,000 rules / 220 sections | 129.883 ms | 10.494 ms |
| 2,000 rules sharing an ancestor 100 levels away | 6.339 ms | 0.404 ms |
| Captured 14,849-rule stylesheet / 382 elements | 39.610 ms | 17.180 ms |

The first workload's candidate count fell from 445,220 to 660. The captured
stylesheet's count fell from 136,224 to 64,968. These isolate matching, not
parsing, declaration application, script execution, or presentation.

Two additional 500-level cases exposed unnecessary work in an early version
of the filter: unrelated descendant rules and immediately matching ancestors
cost about 5–8 ms. Lazy construction and traversal brought candidate filtering
and matching to 0.044 ms and 0.124 ms respectively. The full-matching scratch
index is normally disabled for their small candidate sets. Another discarded
experiment memoized every recursive call and slowed ordinary matching; the
retained memo activates only on long branching chains.

### Browser latency remains separate

The release browser gate also passed with its actor pinned to core 5. After
the first three inputs, key acknowledgments ranged from 73.82 to 90.75 ms,
with an 81.24 ms median. A representative key recorded about 11 ms in key
dispatch, 17 ms in the editing default, 14 ms in its microtask checkpoint,
and a 5 ms geometry transaction whose flow work took 3 ms. The preceding
rendering update took 24 ms, including approximately 3 ms of graphical paint.
These acknowledgment measurements include queued work and do not directly
measure the time from physical input to displayed pixels.

Initial acknowledgments were 890.94, 641.10 and 113.86 ms. The focus handler
itself recorded 18 ms plus a 20 ms checkpoint, excluding subsequent style,
layout and presentation work. The first acknowledgment includes focus and
queued work, so it does not isolate editor activation. Broad restyles still
took about 158–162 ms of cascade work, including roughly 144–147 ms of matching.
Stylesheet installation/parsing added further work during hydration.

An unpinned acceptance run on the same heterogeneous workstation recorded
roughly 145–169 ms steady acknowledgments. Both runs completed correctly;
CPU affinity must be controlled for useful timing comparisons. The engine
results do not establish instantaneous browser interaction. Application
execution, serialization/presentation and broad style invalidation remain
measured follow-up areas.

## Verification

The final source passed 2,061 tests in each of debug and release, with
32 tests ignored by default per suite. Formatting, Clippy across all targets,
and the ordinary release build passed. Release acceptance covered the layout
fixture's 20 mutation/measurement cycles, editing/Range cases, iframe focus,
the captured typing page, the desktop pipeline matrix (including CPU and
Hybrid rendering), and the hydrated browser typing/menu workload.

The pinned CSS WPT subset completed all 16 pages and 208 assertions:
**170 passed, 38 failed, zero incomplete**. A separate release build of the
clean original commit `317040c` produced exactly the same outcome for every
assertion. All 38 failures predate this change; the subset remains failing.
Full failures and browser logs are retained in
`target/css-wpt-validated/` and `target/css-wpt-baseline-317040c/`.
Verification and benchmark logs for this run use the
`/tmp/trust-engine-ready-` prefix.

## Extending the engine

New layout features should describe the inputs their results read, the state
they return, and how mutations invalidate those inputs. Add a constraint or a
dependency edge to these existing mechanisms when possible. If the dependency
is not represented, use the full calculation until it is. Keep the full path
available as a correctness reference and compare all its observable outputs.

Operation counts are part of performance verification. A cache-hit percentage
can hide thousands of unnecessary requests, and a small output tree can hide
exponential measurement. Regressions should bound repeated work on increasing
depths or sibling counts; wall-time benchmarks then establish the actual cost.

Remaining limits are explicit:

- Editing a long inline formatting context still lays out that context as a
  whole. Independent block/item reuse does not implement partial line layout.
- Font/SVG metadata or intrinsic-image changes can expire the environment
  cache. Resource-to-consumer tracking can narrow this further.
- Generated counters and quote state retain a broad fallback when styles or
  element order change. Text-only edits with stable selector state are local.
- Cache hits still copy fragment structure and project geometry. Warm layout
  cost is not independent of the number of returned boxes.
- Cold/global styling and complex logical selectors still require substantial
  work. The rejection filters and temporary matching caches preserve full
  selector evaluation for the candidates that remain.
- Event dispatch, application JavaScript, serialization, and presentation are
  separate costs. Sub-millisecond layout does not establish sub-millisecond
  input-to-display latency.

The reuse contracts allow these areas to improve within the same engine.
They do not establish constant-time work for arbitrary documents or guarantee
that future specifications will never require architectural changes.

## Standards consulted

Local CSSWG snapshot `81c27f68690138345b2b3b6af8ccc42dad3dca1d`, fetched
2026-09-06; this is not an assertion of current upstream text:

- CSS 2, [collapsing margins](https://drafts.csswg.org/css2/#collapsing-margins),
  [block formatting contexts](https://drafts.csswg.org/css2/#block-formatting)
  and [normal block height](https://drafts.csswg.org/css2/#normal-block).
- Flexbox 1, [layout algorithm](https://drafts.csswg.org/css-flexbox-1/#layout-algorithm)
  and [cross-size layout](https://drafts.csswg.org/css-flexbox-1/#algo-stretch).
- Positioned Layout 3, [containing blocks](https://drafts.csswg.org/css-position-3/#def-cb).
- Grid 2, [layout algorithm](https://drafts.csswg.org/css-grid-2/#layout-algorithm)
  and [absolutely positioned children](https://drafts.csswg.org/css-grid-2/#abspos-items).
- Conditional Rules 5, [container queries](https://drafts.csswg.org/css-conditional-5/#container-queries),
  [style change events](https://drafts.csswg.org/css-conditional-5/#animated-containers)
  and [container-relative lengths](https://drafts.csswg.org/css-conditional-5/#container-lengths).
- CSSOM, [getComputedStyle](https://drafts.csswg.org/cssom-1/#dom-window-getcomputedstyle).
- Selectors 4, [matching against an element](https://drafts.csswg.org/selectors-4/#match-against-element)
  and [attribute selectors](https://drafts.csswg.org/selectors-4/#attribute-representation).
- CSS Shadow 1, [flattening](https://drafts.csswg.org/css-shadow-1/#flattening)
  and [slot display](https://drafts.csswg.org/css-shadow-1/#slots-in-shadow-tree);
  Display 3, [box generation](https://drafts.csswg.org/css-display-3/#box-generation).

Algorithmic equivalence permits reuse; these specifications do not permit
omitting required percentage resolution, inherited styles, margin state,
static positions or changed container-query results.

The Text/slot work also consulted the local WHATWG DOM snapshot
`a2331a45360129e8645ef7e0a04740241b6e3726`:
[replace data](https://dom.spec.whatwg.org/#concept-cd-replace) preserves the
node, and [finding slottables](https://dom.spec.whatwg.org/#find-slotables)
depends on node identity, tree position and slot names.
