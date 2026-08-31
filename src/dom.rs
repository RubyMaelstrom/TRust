//! The live, scriptable document: an arena DOM built straight from
//! html5ever, mutated by the selected JavaScript backend, then either laid
//! out by `layout2` or serialized back to HTML for the app to re-parse.
//!
//! Deliberately NOT rcdom: a mutable DOM can't live with rcdom's
//! Node::drop force-clearing children, and an arena of indices gives JS a flat,
//! GC-free handle type — wrappers hold a `NodeId`, and the whole arena drops
//! with the page.

use std::borrow::Cow;
use std::cell::{Ref, RefCell};

use rustc_hash::{FxHashMap, FxHashSet};

use html5ever::interface::{ElementFlags, NodeOrText, QuirksMode, TreeSink};
use html5ever::tendril::{StrTendril, TendrilSink};
use html5ever::{Attribute, Namespace, ParseOpts, Prefix, QualName, ns};

pub type NodeId = usize;

type SerializationCache = (u64, FxHashMap<(NodeId, u8), String>);

pub enum NodeData {
    Document,
    /// A document fragment: template contents, fragment-parse roots.
    Fragment,
    Doctype,
    Comment(String),
    Text(String),
    Element {
        name: QualName,
        attrs: Vec<Attribute>,
        /// `<template>` parses its children into a separate fragment.
        template_contents: Option<NodeId>,
    },
}

/// Failure modes of `Document.adoptNode`, kept distinct so the JS binding can
/// expose the DOM-mandated exception names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdoptError {
    TargetNotDocument,
    InvalidNode,
    Document,
    ShadowRoot,
}

pub struct Node {
    pub parent: Option<NodeId>,
    pub first_child: Option<NodeId>,
    pub last_child: Option<NodeId>,
    pub prev_sibling: Option<NodeId>,
    pub next_sibling: Option<NodeId>,
    /// The node document that owns this node.  Keeping this explicitly in the
    /// arena matters for detached `DOMParser` documents: once a subtree is
    /// adopted, its owner document changes even while the subtree remains
    /// detached (DOM Standard §4.5).
    pub owner_document: NodeId,
    pub data: NodeData,
}

/// Elements that close themselves in HTML serialization.
const VOID_ELEMENTS: [&str; 14] = [
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr",
];

thread_local! {
    /// Diagnostic only (`TRUST_NET_TRACE`): `trace_ms()` of the most recent
    /// DOM mutation, for sizing the DOM-stable→load-finish tail.
    static LAST_MUTATION_MS: std::cell::Cell<u128> = const { std::cell::Cell::new(0) };
    /// Diagnostic only (`TRUST_DIAG_FRAME`): per-layout cascade cost breakdown,
    /// to split the live-reparse peg into CSS-parse vs selector-match vs flow.
    /// Counting is a cheap increment (always on); the one-shot CSS-parse time is
    /// the only Instant. Read+reset with `take_casc_diag()` after each layout.
    static CASC_DIAG: std::cell::Cell<CascDiag> = const { std::cell::Cell::new(CascDiag::ZERO) };
}

/// Cascade-cost counters accumulated during one layout pass (diagnostic).
#[derive(Clone, Copy, Default)]
pub struct CascDiag {
    /// `build_style_index` (parse every `<style>`/`<link>` CSS + bucket) time.
    pub style_index_us: u64,
    /// Times the rule index was (re)built — once per cold-cache layout.
    pub style_index_builds: u64,
    /// Total author rules parsed into the index.
    pub rules: u64,
    /// Cumulative time building per-element cascade winner maps (one build
    /// per element per epoch — the inline-style parse + matched-decl scan).
    pub cascaded_us: u64,
    /// Cold per-element selector-match memo builds and the candidate rules
    /// tested while building them.
    pub matched_rule_builds: u64,
    pub matched_candidates: u64,
    /// Cumulative selector matching time for those cold memo builds.
    pub matched_us: u64,
}

impl CascDiag {
    const ZERO: Self = CascDiag {
        style_index_us: 0,
        style_index_builds: 0,
        rules: 0,
        cascaded_us: 0,
        matched_rule_builds: 0,
        matched_candidates: 0,
        matched_us: 0,
    };
}

/// Cached once: are the cascade counters active? Off in production (no env) so
/// the hot selector-match path pays only a single relaxed atomic load + branch.
fn casc_diag_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("TRUST_DIAG_FRAME").is_some())
}

#[inline]
fn casc_bump(f: impl FnOnce(&mut CascDiag)) {
    if !casc_diag_on() {
        return;
    }
    CASC_DIAG.with(|c| {
        let mut d = c.get();
        f(&mut d);
        c.set(d);
    });
}

/// Read and reset the per-layout cascade counters (diagnostic).
pub fn take_casc_diag() -> CascDiag {
    CASC_DIAG.with(|c| c.replace(CascDiag::ZERO))
}

/// The `trace_ms()` of the last DOM mutation on this thread (diagnostic).
pub fn last_mutation_ms() -> u128 {
    LAST_MUTATION_MS.with(|c| c.get())
}

pub struct Dom {
    nodes: Vec<Node>,
    /// host element → shadow root fragment (attachShadow).
    shadow_roots: FxHashMap<NodeId, NodeId>,
    /// and the reverse: shadow root fragment → host element.
    shadow_hosts: FxHashMap<NodeId, NodeId>,
    /// Set by every tree/attribute mutation; the living page takes it
    /// to decide whether a dispatch warrants re-extraction at all.
    dirty: bool,
    /// Monotonic mutation counter (bumped with `dirty`); keys the
    /// cached visibility cascade so it rebuilds only after changes.
    epoch: u64,
    /// Geometry invalidations retained independently of the frontend's incremental-render
    /// queue. TRust stores nested Documents in one arena, but HTML §7.3.1.3 gives each child
    /// navigable its own active Document: a mutation inside that Document cannot change the
    /// embedding iframe's box in its container Document. The CSSOM View cache consumes this log
    /// only when deciding whether a cached top-level iframe rectangle remains usable.
    geometry_dirty_nodes: FxHashMap<NodeId, DirtyKind>,
    geometry_dirty_attributed: bool,
    /// Monotonic STYLE epoch: advances only when the SHEET SET can have
    /// changed — exactly the triggers the standards define for sheet
    /// (re)creation (HTML §4.2.6: a `<style>`'s sheet re-creates when its
    /// child text changes or it enters/leaves the document; `<link>` sheets
    /// respond to attribute changes; CSSOM: `@media` re-evaluates against
    /// the viewport) plus our adopted/external-sheet attach points.
    /// `style_cache` keys on THIS instead of `epoch`, so ordinary content
    /// mutations no longer force a full CSS re-parse + rule-hash rebuild on
    /// the next style read — on a CSS-heavy live page that re-parse was
    /// paid per mutate-then-read cycle (script layout-thrash, live
    /// serializes, measure passes). INVARIANT: never advances without
    /// `epoch` advancing too (every bump routes through `touch_style` →
    /// `touch`), so the per-epoch match/cascade memos — whose stored rule
    /// INDICES point into the current index — can never outlive a rebuild.
    style_epoch: u64,
    /// adoptedStyleSheets text per scope (DOCUMENT or a shadow root
    /// fragment), pushed by the prelude on adoption/replaceSync.
    adopted_styles: FxHashMap<NodeId, String>,
    /// Fetched `<link rel=stylesheet>` text, keyed by the link element.
    external_sheets: FxHashMap<NodeId, String>,
    /// Lazily built visibility cascade, valid for one STYLE epoch.
    style_cache: RefCell<Option<(u64, std::rc::Rc<StyleIndex>)>>,
    /// Memoized inherited `computed_value` results for the current epoch,
    /// keyed (node, property index). Inheritance walks ancestors, so the
    /// layout's per-element reads would re-walk without this; cleared when
    /// the epoch advances.
    computed_cache: RefCell<ComputedCache>,
    /// Memoized inherited custom-property source values for the current DOM
    /// epoch. CSS Custom Properties §2 makes every unregistered `--*`
    /// property inherited; a deep application tree otherwise re-walks the
    /// same ancestor chain for every `var()` in every resolved box property.
    custom_prop_cache: RefCell<CustomPropCache>,
    /// Memoized selector-match results for the current epoch: for an element,
    /// the indices (into its tree scope's rule vec) of every author rule whose
    /// selector matches it. Selector matching is the cascade's dominant cost on
    /// CSS-heavy pages, and the layout/serializer read 30+ properties per
    /// element — without this each read re-matched every rule (O(elements ×
    /// rules × props)). With it, each element is matched ONCE per epoch (via the
    /// rightmost-key buckets), then every property/pseudo read reuses the list.
    matched_cache: RefCell<NodeCache<std::rc::Rc<Vec<u32>>>>,
    /// Memoized per-element cascade WINNER MAPS for the current epoch (see
    /// `cascaded_maps`): the layout/serializer read 30+
    /// properties per element (across the flow AND the intrinsic-measurement
    /// re-descents), so the winners for EVERY declared property — element
    /// box plus `::before`/`::after` — are resolved in ONE pass on the first
    /// read, then each read is a slot lookup (epoch-stamp invalidated).
    /// Pure memoization (identical results), so it never affects the
    /// cascade outcome.
    cascaded_cache: RefCell<NodeCache<std::rc::Rc<CascadedMaps>>>,
    /// Memoized `is_hidden` results for the current epoch. `is_hidden` reads ~15
    /// cascaded properties and runs once per `flow_element` visit (and the same
    /// node is re-visited by every measurement pass that re-descends through it),
    /// so without this the visibility test is the layout's most-repeated work.
    hidden_cache: RefCell<NodeCache<bool>>,
    /// Memoized computed `font-size` in CSS px for the current epoch (see
    /// `font_px`): every `em`/`rem` length resolution consults it, and the
    /// numeric composition walks ancestors, so it's cached like the other
    /// per-element cascade reads.
    font_cache: RefCell<NodeCache<f32>>,
    /// Memoized line-decoration propagation for the current DOM epoch.
    /// `text-decoration-line` does not inherit, but decorations established by
    /// an ancestor propagate through its in-flow descendant boxes. Layout asks
    /// for the accumulated pair more than once per element, so recursively
    /// share each ancestor's result instead of re-walking the full chain.
    decoration_cache: RefCell<NodeCache<(bool, bool)>>,
    /// Repeated DOM string getters (textContent/innerHTML/outerHTML) can be
    /// hot in framework render loops. Cache each completed value only for the
    /// current DOM epoch; the next tree/attribute/text mutation drops the map
    /// wholesale, so old page-sized strings are not retained forever.
    serialization_cache: RefCell<SerializationCache>,
    /// The CSS-pixel viewport used to evaluate `@media` queries when the
    /// cascade is built; `(0, 0)` = unknown
    /// (width/height queries then conservatively don't match, as if skipped).
    /// Set by `execute_js` from `PageEnv`.
    viewport_px: (f32, f32),
    /// Output-device pixel density used by HTML responsive-image selection.
    /// It does not enter CSS layout geometry; it only chooses a source whose
    /// normalized density is appropriate for the device.
    device_pixel_ratio: f32,
    /// Per-element inner-scroll state (CSSOM View `element.scrollTop`, Phase 3).
    /// Keyed by node; absent = never scrolled / not a measured scroll box.
    scroll_state: FxHashMap<NodeId, ScrollBox>,
    /// Page-initiated scroll writes `(node, top, left)` (px) since the last
    /// drain — delivered to the app as `PageEvt::Scrolled` so a pure scroll (no
    /// DOM mutation) re-windows a region WITHOUT a full re-parse/relayout.
    scroll_changes: Vec<(NodeId, f64, f64)>,
    /// The document's URL (DOM §4.5 "documents have an associated URL"). Set
    /// by `load_page` from `PageEnv`; the live serializer resolves sprite
    /// `<use>` hrefs against it (the SAME base `rewrite_inline_svgs` uses on
    /// the layout side, so the `SPRITE_SHEETS` key matches). `None` = unknown
    /// (sprite refs then count as unrenderable, the conservative answer).
    doc_url: Option<url::Url>,
    /// Incremental layout (incremental-layout contract): the element nodes mutated
    /// since the last `take_dirty_targets`, with the kind of change. A mutation
    /// confined to a relayout boundary's subtree lets the app re-lay ONLY that
    /// boundary instead of the whole document. Content = childList/text (the
    /// boundary may be the recorded node itself); Attr = an attribute change (the
    /// node's OWN box may move, so the boundary must strictly enclose it).
    dirty_nodes: Vec<(NodeId, DirtyKind)>,
    /// False once any *un*attributed mutation occurred this cycle (a global
    /// style/viewport change, or a mutator that can't name its node) — then the
    /// app must do a full relayout, never a patch. Reset to true on take.
    dirty_attributed: bool,
    /// Actor nodes eligible for pointer hit correlation (computed by the
    /// actor's `hover_set` before each serialize): CSS-only pages keep sparse
    /// selector candidates; pages exposing pointer/mouse boundary events carry
    /// every element so target and relatedTarget remain exact. The serializer
    /// bakes `data-trust-hover` on them. Deliberately a DEDICATED attribute —
    /// `data-trust-node` is load-bearing for incremental-layout boundary
    /// sparsity and scroll-region correlation, so hit targets must not carry it.
    hover_hosts: std::collections::HashSet<NodeId>,
    /// Whether `hover_hosts` names every rendered element rather than only CSS
    /// hover candidates. UI Events requires the actual hit-test target (and
    /// `relatedTarget`) even when that element has no listener of its own, so
    /// any page with pointer/mouse boundary listeners uses complete markers.
    /// Kept separately because a CSS-only `:hover` page can retain its sparse
    /// candidate set without losing observable event targets.
    hover_hits_complete: bool,
    /// Selector subjects of paint-only hover rules. These receive a separate
    /// presentation marker so graphical frontends can update baked paint style
    /// in place without treating ordinary inline/flex elements as relayout
    /// boundaries.
    paint_patch_hosts: std::collections::HashSet<NodeId>,
    /// Frontend-neutral activation targets for the current render. This is
    /// actor-owned rendering metadata, not an HTML attribute: direct layout
    /// consumes it without serializing `data-trust-*` markers into a second
    /// DOM. A target in this set routes activation through the resident page
    /// actor so the HTML event/default-action algorithms run before navigation.
    render_clickables: std::collections::HashSet<NodeId>,
    /// Whether this DOM currently backs a resident page actor. Static layouts
    /// retain ordinary links/forms but do not manufacture actor commands.
    render_live: bool,
    /// The live `:hover` chain: the committed hover target + its composed
    /// ancestors (empty at rest / no pointer). Consulted by selector matching
    /// (`Compound.hover`); moved by `set_hover_chain`, which bumps the epoch
    /// only when the move can change the render (see the hover probes).
    hover_chain: FxHashSet<NodeId>,
    /// Elements whose POPOVER is currently SHOWING (HTML §the popover
    /// attribute — the "popover visibility state"). Written by the
    /// `__dom_popover` syscall as page JS calls `showPopover`/`hidePopover`;
    /// read by the UA hide rule in `is_hidden` and the `:popover-open`
    /// pseudo-class. A removed node's stale entry is harmless (the id stops
    /// rendering with the node).
    popover_open: FxHashSet<NodeId>,
    /// Top-layer order for showing popovers. CSS Positioned Layout 4 §3 uses
    /// an ordered set and paints its last element on top; a hash-set alone
    /// loses that observable order when more than one manual popover is open.
    popover_order: Vec<NodeId>,
}

/// The kind of DOM mutation, for incremental-layout boundary mapping. See
/// `Dom::relayout_boundary`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DirtyKind {
    /// A childList or text-content change: the content INSIDE a node changed.
    Content,
    /// A style transition whose changed declarations are paint/hit-test only.
    /// Its own border box cannot move, so a frontend that retained its fragment
    /// geometry may patch the selector subject itself. Other consumers walk to
    /// their ordinary safe layout boundary or use the full fallback.
    Paint,
    /// An attribute change: the element's own styling/box may have changed.
    Attr,
}

/// Per-element inner-scroll state (CSSOM View). The scroll POSITION
/// (`scrollTop`/`scrollLeft`, px) is owned by the page (its `scrollTop=` /
/// `scrollTo`) and the terminal wheel write-back; `top` is the single source of
/// truth the `scrollTop` getter, the live serializer (baked as
/// `data-trust-scroll-top`), and the wheel write-back read.
///
/// `scrollHeight` is NOT stored: it must reflect the CURRENT content (CSSOM
/// View), so the getter reads the actor's own fresh measure pass (`__dom_rect`,
/// re-measured per DOM epoch) — pushing it from the app would lag one render and
/// break the conditional pin (`if scrollTop + clientHeight >= scrollHeight`).
/// Only `clientHeight`/`clientWidth` (the CLIP box) round-trip from the app: an
/// `absolute; top:0; bottom:0` chat needs layout to know its viewport height,
/// which the actor can't compute. `None` until the first push ⇒ the getter falls
/// back to the rect (the pre-Phase-3 behaviour, used only at cold load).
#[derive(Clone, Copy, Default)]
struct ScrollBox {
    top: f64,
    left: f64,
    client_h: Option<f64>,
    client_w: Option<f64>,
}

/// Per-epoch memo for `computed_value`: the epoch the entries are valid for,
/// and inherited results keyed `(node, property index)`. FxHash: the keys
/// are arena-internal, so SipHash's DoS resistance buys nothing.
type ComputedCache = (u64, FxHashMap<(NodeId, usize), Option<String>>);

/// Per-epoch memo for inherited custom-property source values. Custom
/// properties form an open-ended, case-sensitive name space, so keep a small
/// name map per node rather than allocating a `(node, String)` key on every
/// cache hit. The cached value is the same cascaded-or-inherited token stream
/// that [`Dom::custom_prop`] returned before memoization; `var()` dependency
/// resolution and cycle detection still happen at the use site.
type CustomPropCache = (u64, FxHashMap<NodeId, FxHashMap<String, Option<String>>>);

/// A node-indexed, epoch-STAMPED slot cache for the per-epoch memos keyed
/// by bare `NodeId`. NodeIds are dense arena indices, so a Vec slot
/// replaces hashing entirely, and the stamp compare replaces the per-epoch
/// clear: advancing the epoch invalidates every slot at once, for free.
/// A stale value lingers in its slot until overwritten — bounded by the
/// arena, the same steady-state the old cleared-and-refilled maps had.
struct NodeCache<T> {
    slots: Vec<(u64, Option<T>)>,
}

impl<T> Default for NodeCache<T> {
    fn default() -> Self {
        NodeCache { slots: Vec::new() }
    }
}

impl<T> NodeCache<T> {
    /// The value cached for `id` at `epoch`, if still live. (An empty or
    /// stale slot's `Option` gates it — the initial stamp is never trusted
    /// on its own.)
    fn get(&self, id: NodeId, epoch: u64) -> Option<&T> {
        match self.slots.get(id) {
            Some((stamp, Some(v))) if *stamp == epoch => Some(v),
            _ => None,
        }
    }

    fn put(&mut self, id: NodeId, epoch: u64, v: T) {
        if self.slots.len() <= id {
            self.slots.resize_with(id + 1, || (0, None));
        }
        self.slots[id] = (epoch, Some(v));
    }
}

/// One element's author-cascade winners, per target box: the element itself
/// plus its `::before`/`::after` generated boxes (their rules ride the same
/// matched list, bucketed by the rule's pseudo target). An absent key = no
/// author declaration for that property (the cascade's `None`).
#[derive(Default)]
struct CascadedMaps {
    elem: FxHashMap<String, String>,
    before: FxHashMap<String, String>,
    after: FxHashMap<String, String>,
}

impl CascadedMaps {
    fn pseudo(&self, which: PseudoEl) -> &FxHashMap<String, String> {
        match which {
            PseudoEl::Before => &self.before,
            PseudoEl::After => &self.after,
        }
    }
}

/// The document node is always index 0.
pub const DOCUMENT: NodeId = 0;

/// Lazy pre-order walk of a subtree (see `Dom::descendants`). Advancing
/// costs O(1) amortized: first child, else next sibling, else the first
/// ancestor below `root` with a next sibling.
pub struct Descendants<'a> {
    dom: &'a Dom,
    root: NodeId,
    next: Option<NodeId>,
}

impl Iterator for Descendants<'_> {
    type Item = NodeId;

    fn next(&mut self) -> Option<NodeId> {
        let cur = self.next?;
        let mut n = self.dom.nodes[cur].first_child;
        if n.is_none() {
            let mut up = cur;
            while up != self.root {
                if let Some(s) = self.dom.nodes[up].next_sibling {
                    n = Some(s);
                    break;
                }
                // Every visited node was reached from `root`, so the parent
                // chain leads back to it; `None` is pure defense.
                match self.dom.nodes[up].parent {
                    Some(p) => up = p,
                    None => break,
                }
            }
        }
        self.next = n;
        Some(cur)
    }
}

impl Default for Dom {
    fn default() -> Self {
        Self::new()
    }
}

impl Dom {
    pub fn new() -> Self {
        let mut dom = Dom {
            nodes: Vec::new(),
            shadow_roots: FxHashMap::default(),
            shadow_hosts: FxHashMap::default(),
            dirty: false,
            epoch: 0,
            geometry_dirty_nodes: FxHashMap::default(),
            geometry_dirty_attributed: true,
            style_epoch: 0,
            adopted_styles: FxHashMap::default(),
            external_sheets: FxHashMap::default(),
            style_cache: RefCell::new(None),
            computed_cache: RefCell::new((u64::MAX, FxHashMap::default())),
            custom_prop_cache: RefCell::new((u64::MAX, FxHashMap::default())),
            matched_cache: RefCell::new(NodeCache::default()),
            cascaded_cache: RefCell::new(NodeCache::default()),
            hidden_cache: RefCell::new(NodeCache::default()),
            font_cache: RefCell::new(NodeCache::default()),
            decoration_cache: RefCell::new(NodeCache::default()),
            serialization_cache: RefCell::new((u64::MAX, FxHashMap::default())),
            viewport_px: (0.0, 0.0),
            device_pixel_ratio: 1.0,
            scroll_state: FxHashMap::default(),
            scroll_changes: Vec::new(),
            doc_url: None,
            dirty_nodes: Vec::new(),
            dirty_attributed: true,
            hover_hosts: std::collections::HashSet::new(),
            hover_hits_complete: false,
            paint_patch_hosts: std::collections::HashSet::new(),
            render_clickables: std::collections::HashSet::new(),
            render_live: false,
            hover_chain: FxHashSet::default(),
            popover_open: FxHashSet::default(),
            popover_order: Vec::new(),
        };
        dom.new_node(NodeData::Document);
        dom
    }

    /// Replace the hover-host set (elements holding hover-type listeners) the
    /// serializer marks with `data-trust-hover`. Refreshed by the actor
    /// wherever the clickable set is refreshed — a pure marking input, so it
    /// deliberately does NOT touch the dirty bit or the epoch.
    pub fn set_hover_hosts(&mut self, hosts: std::collections::HashSet<NodeId>, complete: bool) {
        self.hover_hosts = hosts;
        self.hover_hits_complete = complete;
    }

    /// Whether the last live snapshot carried exact actor ids on every
    /// element for standards-correct pointer boundary event targeting.
    pub fn hover_hits_complete(&self) -> bool {
        self.hover_hits_complete
    }

    /// Replace the paint-only selector-subject marker set used by live
    /// serialization. Like hover hit-test markers, this is presentation
    /// metadata and must not dirty the canonical DOM.
    pub fn set_paint_patch_hosts(&mut self, hosts: std::collections::HashSet<NodeId>) {
        self.paint_patch_hosts = hosts;
    }

    pub fn paint_patch_host(&self, id: NodeId) -> bool {
        self.paint_patch_hosts.contains(&id)
    }

    /// Install the actor's current activation set for direct box-tree layout.
    /// This does not mutate the document and therefore does not advance either
    /// DOM epoch. The set replaces the historical `data-trust-click` transport
    /// that was baked into serialized presentation markup.
    pub fn set_render_clickables(
        &mut self,
        clickables: std::collections::HashSet<NodeId>,
        live: bool,
    ) {
        self.render_clickables = clickables;
        self.render_live = live;
    }

    pub fn render_clickable(&self, id: NodeId) -> bool {
        self.render_live && self.render_clickables.contains(&id)
    }

    pub fn render_live(&self) -> bool {
        self.render_live
    }

    /// Extend paint markers for a newly serialized incremental boundary while
    /// retaining the document-wide markers established by the last full
    /// snapshot.
    pub fn extend_paint_patch_hosts(&mut self, hosts: impl IntoIterator<Item = NodeId>) {
        self.paint_patch_hosts.extend(hosts);
    }

    /// Whether no element holds a hover-type listener (the auto-Static gate:
    /// a hover-only page must keep its engine).
    pub fn hover_hosts_is_empty(&self) -> bool {
        self.hover_hosts.is_empty()
    }

    /// Whether any stylesheet rule depends on the live `:hover` chain AND
    /// declares render-affecting properties — the CSS half of the auto-Static
    /// gate (such a page must stay resident to restyle under the pointer).
    pub fn hover_css_affects_rendering(&self) -> bool {
        !self.style_index().hover_probes.is_empty()
    }

    /// The elements a render-affecting `:hover` rule could match — pure-CSS
    /// hover targets like `.menu` of
    /// `.menu:hover .drop{display:block}`. They carry no listener, so the
    /// listener registry can't name them; the serializer still needs to mark
    /// them (`data-trust-hover`) or the app can never resolve a pointer cell
    /// to them and the chain never moves there. A selector whose hover is
    /// nested in a logical/relational pseudo has an any-element probe; in that
    /// standards-required case every rendered candidate is marked because
    /// omitting the designated element would be a false negative.
    pub fn hover_css_candidates(&self) -> Vec<NodeId> {
        self.hover_css_candidates_in(&[DOCUMENT])
    }

    /// `hover_css_candidates` restricted to the given subtrees (each root plus
    /// its composed descendants; DOCUMENT itself never matches a probe, so the
    /// doc-wide call composes through here unchanged). The incremental patch
    /// path serializes only its dirty boundaries, so it only needs candidates
    /// inside them — this keeps that path from paying a whole-document probe
    /// walk per patch.
    pub fn hover_css_candidates_in(&self, roots: &[NodeId]) -> Vec<NodeId> {
        let idx = self.style_index();
        let probes: Vec<&HoverProbe> = idx.hover_probes.iter().collect();
        if probes.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::new();
        for &r in roots {
            for e in std::iter::once(r).chain(self.composed_descendants(r)) {
                if self.tag_name(e).is_some() && probes.iter().any(|p| p.could_match(self, e)) {
                    out.push(e);
                }
            }
        }
        out
    }

    /// Plausible selector subjects of paint-only `:hover` rules, restricted to
    /// rendered subtrees. RuleBuckets uses the selector's rightmost compound,
    /// so common `.menu`, `#id`, and element subjects stay sparse while
    /// universal/attribute-only selectors conservatively mark every candidate.
    pub fn hover_paint_subject_candidates_in(&self, roots: &[NodeId]) -> Vec<NodeId> {
        let index = self.style_index();
        let mut out = Vec::new();
        for &root in roots {
            for id in std::iter::once(root).chain(self.composed_descendants(root)) {
                if self.tag_name(id).is_none() {
                    continue;
                }
                let scope = self.tree_scope(id);
                let (Some(rules), Some(buckets)) =
                    (index.scopes.get(&scope), index.hover_buckets.get(&scope))
                else {
                    continue;
                };
                let mut candidates = Vec::new();
                buckets.candidates(self, id, &mut candidates);
                if candidates
                    .into_iter()
                    .any(|rule| rule_is_paint_only(&rules[rule as usize]))
                {
                    out.push(id);
                }
            }
        }
        // `:host(:hover)` subjects live outside their stylesheet scope and do
        // not enter its ordinary rightmost buckets.
        for (&host, &scope) in &self.shadow_roots {
            if roots.iter().any(|&root| {
                root == crate::dom::DOCUMENT
                    || root == host
                    || self.composed_descendants(root).contains(&host)
            }) && index.scopes.get(&scope).is_some_and(|rules| {
                rules.iter().any(|rule| {
                    rule_is_paint_only(rule)
                        && rule_uses_hover(rule)
                        && matches!(rule.selector.0.as_slice(), [(_, compound)] if compound.host)
                })
            }) {
                out.push(host);
            }
        }
        out.sort_unstable();
        out.dedup();
        out
    }

    /// Set/clear an element's popover SHOWING state (HTML §the popover
    /// attribute). Bumps the main epoch on change — the UA hide rule and
    /// `:popover-open` both read this, so hidden/match memos must refresh —
    /// and marks the document dirty (an open/closed popover renders
    /// differently by definition). Never touches `style_epoch`: the sheet set
    /// is unchanged.
    pub fn set_popover_open(&mut self, id: NodeId, open: bool) {
        let changed = if open {
            let inserted = self.popover_open.insert(id);
            if inserted {
                self.popover_order.push(id);
            }
            inserted
        } else {
            let removed = self.popover_open.remove(&id);
            if removed {
                self.popover_order.retain(|&node| node != id);
            }
            removed
        };
        if changed {
            self.touch();
        }
    }

    /// Whether this element is currently in the document's popover top layer.
    /// Live arenas own the state directly; serialized presentation arenas carry
    /// the internal order marker because open state is not an HTML attribute.
    pub fn is_popover_showing(&self, id: NodeId) -> bool {
        self.popover_open.contains(&id) || self.attr(id, "data-trust-popover-open").is_some()
    }

    /// Stable position in the document top layer, used by the painter. The
    /// presentation marker transports the actor arena's ordered-set index.
    pub fn popover_top_layer_order(&self, id: NodeId) -> Option<usize> {
        self.popover_order
            .iter()
            .position(|&node| node == id)
            .or_else(|| {
                self.attr(id, "data-trust-popover-open")
                    .and_then(|value| value.parse().ok())
            })
    }

    /// Move the live `:hover` state to `target`, its flat-tree ancestors, and
    /// any HTML labeled controls whose labels consequently match `:hover`.
    ///
    /// Selectors 4 §9.1 makes `:hover` match the designated element and its
    /// flat-tree ancestors, while the selector containing it can have a
    /// different subject (`.menu:hover .dropdown`, `li:has(a:hover)`, …).
    /// Consequently the dirty nodes are the selector SUBJECTS whose match
    /// result changes, not merely the elements entering/leaving the chain.
    /// We compare only hover-dependent rules from the normal rightmost-key
    /// rule buckets. The epoch advances once for the complete transition and
    /// every changed subject is recorded as an attributed style/box mutation;
    /// the existing safe-boundary logic remains the patch-vs-full authority.
    pub fn set_hover_chain(&mut self, target: Option<NodeId>) -> bool {
        let mut chain: FxHashSet<NodeId> = FxHashSet::default();
        let mut cur = target.filter(|&t| self.is_valid(t));
        while let Some(c) = cur {
            chain.insert(c);
            cur = self.parent_flat(c);
        }
        // HTML §pseudo-classes adds the labeled control itself when a matching
        // label is hovered. It explicitly does NOT make the control designated,
        // so the control's ancestors are not added (the standard's `span#b`
        // counterexample relies on this distinction).
        let labeled_controls: Vec<_> = chain
            .iter()
            .copied()
            .filter(|&node| self.tag_name(node) == Some("label"))
            .filter_map(|label| self.labeled_control(label))
            .collect();
        chain.extend(labeled_controls);
        if chain == self.hover_chain {
            return false;
        }
        let index = self.style_index();
        if index.hover_probes.is_empty() {
            self.hover_chain = chain;
            return false;
        }
        if !chain
            .symmetric_difference(&self.hover_chain)
            .any(|&element| {
                index
                    .hover_probes
                    .iter()
                    .any(|probe| probe.could_match(self, element))
            })
        {
            // The designated element still changes for DOM `matches(:hover)`,
            // but no render-affecting hover-bearing compound can observe this
            // transition. Avoid even the subject-bucket walk.
            self.hover_chain = chain;
            return false;
        }

        // Snapshot OLD selector applicability. RuleBuckets keeps this
        // proportional to plausible subjects rather than elements × all rules.
        type HoverMatchSnapshot = (NodeId, NodeId, Vec<(u32, bool)>);
        let mut old: Vec<HoverMatchSnapshot> = Vec::new();
        for id in self.composed_descendants(DOCUMENT) {
            if self.tag_name(id).is_none() {
                continue;
            }
            let scope = self.tree_scope(id);
            let (Some(rules), Some(buckets)) =
                (index.scopes.get(&scope), index.hover_buckets.get(&scope))
            else {
                continue;
            };
            let mut candidates = Vec::new();
            buckets.candidates(self, id, &mut candidates);
            if candidates.is_empty() {
                continue;
            }
            old.push((
                id,
                scope,
                candidates
                    .into_iter()
                    .map(|ri| {
                        (
                            ri,
                            self.matches_complex(id, &rules[ri as usize].selector.0, None),
                        )
                    })
                    .collect(),
            ));
        }

        // `:host(:hover)` is matched in a shadow scope against a subject that
        // lives outside it, so it cannot enter the ordinary buckets above.
        let mut old_hosts: Vec<HoverMatchSnapshot> = Vec::new();
        for (&host, &scope) in &self.shadow_roots {
            let Some(rules) = index.scopes.get(&scope) else {
                continue;
            };
            let matches: Vec<_> = rules
                .iter()
                .enumerate()
                .filter(|(_, rule)| rule_affects_render(rule) && rule_uses_hover(rule))
                .filter(|(_, rule)| {
                    matches!(rule.selector.0.as_slice(), [(_, compound)] if compound.host)
                })
                .map(|(ri, rule)| (ri as u32, self.host_rule_matches(host, rule)))
                .collect();
            if !matches.is_empty() {
                old_hosts.push((host, scope, matches));
            }
        }

        self.hover_chain = chain;

        let mut changed: FxHashMap<NodeId, DirtyKind> = FxHashMap::default();
        for (id, scope, prior) in old {
            let rules = &index.scopes[&scope];
            let mut kind = None;
            for (ri, was) in prior {
                let rule = &rules[ri as usize];
                if was != self.matches_complex(id, &rule.selector.0, None) {
                    let this = if rule_is_paint_only(rule) {
                        DirtyKind::Paint
                    } else {
                        DirtyKind::Attr
                    };
                    if this == DirtyKind::Attr {
                        kind = Some(this);
                        break;
                    }
                    kind = Some(this);
                }
            }
            if let Some(kind) = kind {
                changed.insert(id, kind);
            }
        }
        for (host, scope, prior) in old_hosts {
            let rules = &index.scopes[&scope];
            let mut kind = None;
            for (ri, was) in prior {
                let rule = &rules[ri as usize];
                if was != self.host_rule_matches(host, rule) {
                    let this = if rule_is_paint_only(rule) {
                        DirtyKind::Paint
                    } else {
                        DirtyKind::Attr
                    };
                    if this == DirtyKind::Attr {
                        kind = Some(this);
                        break;
                    }
                    kind = Some(this);
                }
            }
            if let Some(kind) = kind {
                changed
                    .entry(host)
                    .and_modify(|old| {
                        if kind == DirtyKind::Attr {
                            *old = kind;
                        }
                    })
                    .or_insert(kind);
            }
        }
        if changed.is_empty() {
            return false;
        }
        self.mark();
        for (&node, &kind) in &changed {
            self.record_geometry_dirty(node, kind);
        }
        self.dirty_nodes.extend(changed);
        true
    }

    /// Parent in CSS Scoping's flattened element tree. Shadow-root children
    /// are associated with the host, and a slottable is associated with the
    /// first matching `<slot>` in its parent's shadow tree.
    pub(crate) fn parent_flat(&self, id: NodeId) -> Option<NodeId> {
        if let Some(slot) = self.assigned_slot(id) {
            return Some(slot);
        }
        let parent = self.nodes[id].parent?;
        self.shadow_hosts.get(&parent).copied().or(Some(parent))
    }

    /// Whether `id` is below an element omitted from CSS Display's flat-tree
    /// box construction. Presentation snapshots use this to avoid retaining
    /// adapter metadata for nodes that cannot occur in any layout fragment.
    pub(crate) fn omitted_from_flat_box_tree(&self, id: NodeId) -> bool {
        let mut current = Some(id);
        while let Some(node) = current {
            if self.tag_name(node).is_some() && self.is_hidden(node) {
                return true;
            }
            current = self.parent_flat(node);
        }
        false
    }

    /// DOM Standard §4.2.2.3 "finding a slot": return the first slot in
    /// the light parent's shadow tree whose name matches this slottable.  The
    /// event dispatcher uses this as Node's `get the parent` algorithm; the
    /// public `assignedSlot` IDL getter additionally hides closed-tree slots.
    pub fn assigned_slot(&self, id: NodeId) -> Option<NodeId> {
        let host = self.nodes[id].parent?;
        let shadow = self.shadow_root(host)?;
        let wanted = self.attr(id, "slot").unwrap_or("").trim();
        self.descendants(shadow).find(|&candidate| {
            self.tag_name(candidate) == Some("slot")
                && self.attr(candidate, "name").unwrap_or("").trim() == wanted
        })
    }

    fn labeled_control(&self, label: NodeId) -> Option<NodeId> {
        let labelable = |node| match self.tag_name(node) {
            Some("button" | "meter" | "output" | "progress" | "select" | "textarea") => true,
            Some("input") => !self
                .attr(node, "type")
                .is_some_and(|ty| ty.eq_ignore_ascii_case("hidden")),
            _ => false,
        };
        if let Some(target) = self.attr(label, "for") {
            let scope = self.tree_scope(label);
            return self
                .descendants(scope)
                .find(|&node| self.attr(node, "id") == Some(target) && labelable(node));
        }
        self.descendants(label).find(|&node| labelable(node))
    }

    /// True when anything mutated since the last call; resets the flag.
    pub fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    /// Total arena slots (diagnostic): the tree size the layout walks.
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// The monotonic mutation counter. Anything memoized against the DOM's
    /// current shape (the geometry box map in the active JavaScript backend,
    /// like the cascade caches here) keys on this and rebuilds when it advances.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// The common core of every mutation: the dirty bit for the living page +
    /// the epoch for the cached visibility cascade.
    fn mark(&mut self) {
        self.dirty = true;
        self.epoch = self.epoch.wrapping_add(1);
        // Diagnostic: record WHEN the DOM last changed, so we can size the
        // gap between DOM-stability and load-finish (the telemetry/idle
        // tail). Gated on the trace flag.
        if std::env::var_os("TRUST_NET_TRACE").is_some() {
            LAST_MUTATION_MS.with(|c| c.set(crate::http::trace_ms()));
        }
    }

    /// Coalesce repeated geometry invalidations per arena node. This keeps the queue bounded by
    /// the DOM itself even on a page that mutates forever without reading layout. Attribute
    /// changes dominate content changes, which dominate paint-only changes, because the strongest
    /// retained kind is the one the cache must prove isolated before reusing a box.
    fn record_geometry_dirty(&mut self, id: NodeId, kind: DirtyKind) {
        let strength = |kind| match kind {
            DirtyKind::Paint => 0,
            DirtyKind::Content => 1,
            DirtyKind::Attr => 2,
        };
        self.geometry_dirty_nodes
            .entry(id)
            .and_modify(|old| {
                if strength(kind) > strength(*old) {
                    *old = kind;
                }
            })
            .or_insert(kind);
    }

    /// An UNATTRIBUTED mutation — one we can't pin to a single element (a global
    /// stylesheet/viewport change). Forces the next render to a full relayout
    /// (no incremental patch), since it may have changed anything.
    fn touch(&mut self) {
        self.mark();
        self.dirty_attributed = false;
        self.geometry_dirty_attributed = false;
    }

    /// An attribute change on `id` (its own styling/box may have changed).
    fn touch_attr(&mut self, id: NodeId) {
        self.mark();
        self.dirty_nodes.push((id, DirtyKind::Attr));
        self.record_geometry_dirty(id, DirtyKind::Attr);
    }

    /// A mutation that can change the SHEET SET (`<style>`/`<link>` tree,
    /// text, or attribute changes; adopted/external sheet attaches; viewport
    /// changes): advances the style epoch — invalidating the parsed style
    /// index — AND forces the next render to a full relayout via `touch`
    /// (a changed stylesheet can restyle anything, so no incremental patch
    /// is sound). This is the ONLY writer of `style_epoch`, which keeps the
    /// "style epoch never advances without the main epoch" invariant.
    fn touch_style(&mut self) {
        self.style_epoch = self.style_epoch.wrapping_add(1);
        self.touch();
    }

    /// A style-sheet-set mutation rooted at `scope`. Connected style changes
    /// can restyle arbitrary rendered descendants and therefore remain a full
    /// invalidation. A disconnected tree has no effect on the rendered
    /// document (DOM connectedness / HTML style processing), but its CSSOM and
    /// cascade epoch still change; retain the concrete target so the live-page
    /// pipeline can discard it as detached. If that tree is later inserted,
    /// the insertion path independently invalidates the connected sheet set.
    fn touch_style_at(&mut self, scope: NodeId) {
        self.style_epoch = self.style_epoch.wrapping_add(1);
        if self.is_connected(scope) {
            // A stylesheet invalidates its whole tree scope, so the frontend still needs the
            // conservative full-render signal. Retain the scope for geometry separately: HTML's
            // child Document cannot restyle the navigable container in its container Document.
            self.mark();
            self.dirty_attributed = false;
            self.record_geometry_dirty(scope, DirtyKind::Attr);
        } else {
            self.touch_content(Some(scope));
        }
    }

    /// Bump the style epoch when a tree mutation involving `child` (being
    /// appended/inserted under, or detached from, `parent`) can change the
    /// sheet set: the node is — or its subtree contains — a `<style>`/
    /// `<link>` element, or it's a text node directly under a `<style>`
    /// (HTML §4.2.6: the style element's sheet re-creates when its child
    /// nodes change or it enters/leaves the document). The dominant append
    /// (a fresh leaf node) pays one tag check; only subtree attaches walk,
    /// early-exiting on the first sheet element found.
    fn note_tree_style_mutation(&mut self, parent: Option<NodeId>, child: NodeId) {
        let styled = parent.is_some_and(|parent| self.tree_mutation_changes_style(parent, child));
        if styled && let Some(scope) = parent {
            self.touch_style_at(scope);
        }
    }

    /// Whether adding/removing `child` directly below `parent` can change the
    /// active sheet set. Kept separate from the invalidation itself so DOM's
    /// replace-all algorithm can coalesce an ordered group of removals and the
    /// insertion into one cache invalidation.
    fn tree_mutation_changes_style(&self, parent: NodeId, child: NodeId) -> bool {
        match &self.nodes[child].data {
            NodeData::Text(_) => self.tag_name(parent) == Some("style"),
            NodeData::Element { .. } | NodeData::Fragment => self.subtree_has_style(child),
            _ => false,
        }
    }

    /// Whether `root`'s composed subtree (inclusive) contains a `<style>` or
    /// `<link>` element. Early-exits on the first hit; a childless node is a
    /// single tag check.
    fn subtree_has_style(&self, root: NodeId) -> bool {
        let mut stack = vec![root];
        while let Some(id) = stack.pop() {
            if matches!(self.tag_name(id), Some("style" | "link")) {
                return true;
            }
            self.push_composed_children(id, &mut stack);
        }
        false
    }

    /// A childList/text change whose content lives under `id` (the parent whose
    /// children changed, or a text node's parent element). `None` = a structural
    /// no-op for the rendered tree (detaching an already-orphan node) — still
    /// dirties the epoch but records no target and does NOT force a full relayout.
    fn touch_content(&mut self, id: Option<NodeId>) {
        self.mark();
        if let Some(i) = id {
            self.dirty_nodes.push((i, DirtyKind::Content));
            self.record_geometry_dirty(i, DirtyKind::Content);
        }
    }

    /// Take the element nodes mutated since the last call, for incremental
    /// layout. `None` = an unattributed mutation occurred this cycle ⇒ the caller
    /// MUST do a full relayout. `Some(targets)` = every mutation named a node
    /// (possibly empty, meaning only no-op detaches happened).
    pub fn take_dirty_targets(&mut self) -> Option<Vec<(NodeId, DirtyKind)>> {
        let attributed = std::mem::replace(&mut self.dirty_attributed, true);
        let nodes = std::mem::take(&mut self.dirty_nodes);
        attributed.then_some(nodes)
    }

    /// Consume geometry invalidations accumulated since the last full CSSOM View measure pass.
    /// `None` means an unscoped change (viewport/global sheet state) occurred; otherwise every
    /// returned node names the tree scope in which the change occurred. This queue is separate
    /// from [`Self::take_dirty_targets`] because synchronous geometry reads happen before or after
    /// arbitrary frontend render checkpoints.
    pub fn take_geometry_dirty_targets(&mut self) -> Option<Vec<(NodeId, DirtyKind)>> {
        let attributed = std::mem::replace(&mut self.geometry_dirty_attributed, true);
        let nodes = std::mem::take(&mut self.geometry_dirty_nodes)
            .into_iter()
            .collect();
        attributed.then_some(nodes)
    }

    /// Whether a concrete mutation target can affect rendered boxes or paint.
    ///
    /// CSS Display 3 §2/§2.5 says `display:none` omits an element's entire
    /// subtree from the box tree, so ordinary child-list/text churn below it
    /// needs no frontend render notification. Attribute and style-transition
    /// targets remain conservative: the mutation may itself reveal/remove a
    /// box, or alter a selector subject outside the omitted subtree.
    ///
    /// `:has()` and `:empty` can make a content mutation below an omitted box
    /// change an ancestor or following-sibling selector subject. Suppression is
    /// disabled when either occurs in the active sheet set; the always-correct
    /// render path wins over this optimization.
    ///
    /// This filters only frontend rendering work. The DOM mutation, epoch,
    /// JavaScript state, and MutationObserver delivery have already occurred
    /// and remain observable as required by DOM §4.3.
    pub fn dirty_target_can_render(&self, node: NodeId, kind: DirtyKind) -> bool {
        if kind != DirtyKind::Content {
            return true;
        }
        let mut current = Some(node);
        while let Some(id) = current {
            if self.subtree_omitted_from_box_tree(id) {
                return self.style_index().boxless_content_may_escape;
            }
            current = self.parent_flat(id);
        }
        true
    }

    /// The nearest LIVE scroll-region ancestor (the Tier-1 relayout boundary,
    /// incremental-layout contract §4b) a mutation at `node` is confined to —
    /// `None` when none encloses it (the change reaches non-region content ⇒ full
    /// relayout, OR Tier 2). `live_regions` is the set the APP confirmed are
    /// currently CLIPPED scroll viewports (a fixed band → content changes can't
    /// alter their outer box; CSS Containment L2). It is NOT just "has
    /// overflow:auto" — a fitting (non-overflowing) box renders inline and is
    /// height-elastic (Tier 2), so patching it as a region would fail; gating on
    /// the app's live set avoids that failed-patch→resync churn. For a `Content`
    /// change the boundary may be `node` itself (appending INTO a region is
    /// contained); for an `Attr` change the node's own box may move, so the
    /// boundary must STRICTLY enclose it.
    pub fn relayout_boundary(
        &self,
        node: NodeId,
        kind: DirtyKind,
        live_regions: &std::collections::HashSet<NodeId>,
    ) -> Option<NodeId> {
        let mut cur = match kind {
            DirtyKind::Content | DirtyKind::Paint => Some(node),
            DirtyKind::Attr => self.parent_composed(node),
        };
        while let Some(c) = cur {
            if live_regions.contains(&c) {
                return Some(c);
            }
            cur = self.parent_composed(c);
        }
        None
    }

    /// Whether `id` establishes an **independent formatting context** — a box
    /// whose inside cannot change the layout of anything outside it (and into
    /// which outside floats cannot intrude). This is the spec-exact form of "the
    /// mutation can't affect anything outside its container"
    /// (incremental-layout contract §13a): CSS2 §9.4.1 block-formatting-context
    /// triggers (`overflow ≠ visible`, `float`, out-of-flow, `display:flow-root`/
    /// table-cell/inline-block), CSS Flexbox/Grid §3 (a flex/grid container AND a
    /// flex/grid item each establish one for their contents), and CSS Containment
    /// L2 (`contain: layout|paint|size|content|strict`). A plain in-flow block
    /// does NOT qualify (its margins collapse through, its floats can escape), so
    /// it is never a relayout boundary. This set is deliberately SPARSE — it is
    /// what makes baking `data-trust-node` on boundaries cheap (§3). The actor
    /// proposes the nearest such ancestor; the app proves the box is also
    /// width-stable geometrically (§13a). Cascade-only (no layout).
    pub fn establishes_independent_formatting_context(&self, id: NodeId) -> bool {
        if self.tag_name(id).is_none() {
            return false; // text/comment/document — not an element box
        }
        // overflow ≠ visible on EITHER axis → BFC (a scroll/clip viewport).
        for prop in ["overflow", "overflow-x", "overflow-y"] {
            if let Some(v) = self.computed_style(id, prop)
                && v.split_whitespace().any(|t| {
                    matches!(
                        t.to_ascii_lowercase().as_str(),
                        "hidden" | "clip" | "scroll" | "auto"
                    )
                })
            {
                return true;
            }
        }
        // display values that establish an independent context for their
        // contents (`effective_display` = cascade ELSE the tag's UA default, so a
        // bare `<td>`/`<table>` is caught too).
        if let Some(d) = self.effective_display(id)
            && matches!(
                d.trim().to_ascii_lowercase().as_str(),
                "flow-root"
                    | "inline-block"
                    | "table-cell"
                    | "table-caption"
                    | "table"
                    | "inline-table"
                    | "flex"
                    | "inline-flex"
                    | "grid"
                    | "inline-grid"
            )
        {
            return true;
        }
        // A flex/grid ITEM establishes a new formatting context for its contents
        // (CSS Flexbox §3) — detected via the parent's effective display.
        if let Some(p) = self.parent_composed(id)
            && let Some(pd) = self.effective_display(p)
            && matches!(
                pd.trim().to_ascii_lowercase().as_str(),
                "flex" | "inline-flex" | "grid" | "inline-grid"
            )
        {
            return true;
        }
        // Out-of-flow (absolute/fixed) and floats establish a BFC.
        if let Some(pos) = self.computed_style(id, "position")
            && matches!(
                pos.trim().to_ascii_lowercase().as_str(),
                "absolute" | "fixed"
            )
        {
            return true;
        }
        if let Some(f) = self.computed_style(id, "float")
            && matches!(
                f.trim().to_ascii_lowercase().as_str(),
                "left" | "right" | "inline-start" | "inline-end"
            )
        {
            return true;
        }
        // Layout containment (CSS Containment L2) establishes one explicitly.
        if let Some(c) = self.computed_style(id, "contain")
            && c.split_whitespace().any(|t| {
                matches!(
                    t.to_ascii_lowercase().as_str(),
                    "layout" | "paint" | "size" | "content" | "strict"
                )
            })
        {
            return true;
        }
        false
    }

    /// The nearest ancestor (or `self`, for a `Content` change) of a mutation at
    /// `node` that establishes an independent formatting context — the GENERAL
    /// relayout boundary (incremental-layout contract §13a). Unlike
    /// `relayout_boundary` (which only finds an app-confirmed live scroll
    /// region), this returns ANY independent-formatting-context ancestor, the
    /// boundary whose interior the app will re-lay and splice once the general
    /// `Doc.rows` splice lands (incremental-layout design §13c step 4). Until then it drives only the
    /// diagnostic (`confined_boundaries`) so a live page reveals which boundaries
    /// the splice must handle. An `Attr` change may move the node's OWN box, so we
    /// start the walk at its parent; a `Content` change is contained, so `node`
    /// itself may be the boundary.
    pub fn relayout_boundary_general(&self, node: NodeId, kind: DirtyKind) -> Option<NodeId> {
        let mut cur = match kind {
            DirtyKind::Content | DirtyKind::Paint => Some(node),
            DirtyKind::Attr => self.parent_composed(node),
        };
        while let Some(c) = cur {
            if self.establishes_independent_formatting_context(c) {
                return Some(c);
            }
            cur = self.parent_composed(c);
        }
        None
    }

    /// The nearest independent-formatting-context ancestor (or `self`, for a
    /// `Content` change) that the app has CACHED as a splice-able boundary —
    /// walking UP past any IFC boundary the app couldn't cache
    /// (incremental-layout contract §14). A mutation is contained by EVERY IFC
    /// ancestor, so the nearest cached one is a valid (if larger) patch target.
    /// This is what lets a deep mutation — an animated viewer counter that's a
    /// flex-ROW item sharing its row (so its own box can't be a `Doc.rows`
    /// boundary) — patch its enclosing cached SECTION instead of forcing a
    /// whole-document render. `cached` is the app's `Doc.boundaries` node set
    /// (`live_boundaries`), keyed by the same arena ids walked here.
    pub fn nearest_cached_boundary(
        &self,
        node: NodeId,
        kind: DirtyKind,
        cached: &std::collections::HashSet<usize>,
    ) -> Option<NodeId> {
        let mut cur = match kind {
            DirtyKind::Content | DirtyKind::Paint => Some(node),
            DirtyKind::Attr => self.parent_composed(node),
        };
        while let Some(c) = cur {
            if cached.contains(&c)
                && (kind == DirtyKind::Paint || self.establishes_independent_formatting_context(c))
            {
                return Some(c);
            }
            cur = self.parent_composed(c);
        }
        None
    }

    /// Serialize a relayout boundary's subtree as a self-contained fragment for
    /// an incremental patch (incremental-layout contract §4a). The boundary is
    /// wrapped in a context `<div>` carrying the inherited computed values from
    /// ABOVE it, so the app's `computed_value`/`text_decoration` over the
    /// re-parsed fragment — which has no real ancestors — resolve EXACTLY as in
    /// the full document. The boundary keeps its own baked style (its own cascade
    /// wins over the wrapper); the wrapper only supplies what it inherits.
    pub fn serialize_patch(
        &self,
        boundary: NodeId,
        clickable: &std::collections::HashSet<NodeId>,
    ) -> String {
        let from = self.parent_composed(boundary).unwrap_or(DOCUMENT);
        let mut style = String::new();
        for &p in INHERITED_LAYOUT_PROPS {
            // font-size is carried RESOLVED below — its declared string
            // (`62.5%`, `1.4rem`) would re-resolve against the fragment's
            // synthesized root and land on the wrong number.
            if p == "font-size" {
                continue;
            }
            if let Some(v) = self.computed_value(from, p) {
                style.push_str(p);
                style.push(':');
                style.push_str(&v);
                style.push(';');
            }
        }
        // The boundary's inherited font-size, resolved to px — the `em` basis
        // for everything inside the fragment.
        style.push_str(&format!("font-size:{}px;", self.font_px(from)));
        // text-decoration PROPAGATES (it doesn't inherit), so carry the
        // accumulated lines entering the boundary explicitly.
        let (underline, strike) = self.text_decoration(from);
        if underline || strike {
            style.push_str("text-decoration:");
            if underline {
                style.push_str("underline ");
            }
            if strike {
                style.push_str("line-through");
            }
            style.push(';');
        }
        // The fragment re-parses STANDALONE, so its synthesized root would
        // reset the `rem` basis to the 16px initial — carry the document's
        // real root font-size on an explicit `<html>` shell (the parser
        // adopts a leading `<html>`'s attributes as the root's). This is
        // what kept archive.org's `minmax(16rem, 1fr)` tile grid flipping
        // between 3 and 5 columns: full parses saw the 10px root, patches
        // didn't ("size-fighting").
        format!(
            "<html style=\"font-size:{}px;\"><body><div data-trust-frag=\"\" style=\"{}\">{}</div></body></html>",
            self.root_font_px(),
            escape_attr(&style),
            self.serialize_live(boundary, clickable)
        )
    }

    /// Set the CSS-pixel viewport that `@media` queries evaluate against.
    /// Invalidates the cascade cache when it changes so breakpoint-gated rules
    /// re-resolve. Device scale and terminal cell metrics never enter this
    /// state (Media Queries 4 §5.1).
    pub fn set_viewport_px(&mut self, width: f32, height: f32) {
        let width = width.max(0.0);
        let height = height.max(0.0);
        if self.viewport_px != (width, height) {
            self.viewport_px = (width, height);
            self.touch_style(); // @media re-evaluates against the viewport
        }
    }

    /// Evaluate a CSS media-query text (as `window.matchMedia(query).matches`)
    /// against the current viewport — the SAME evaluator the `@media` cascade
    /// uses, so JS `matchMedia` and stylesheet `@media` agree. Covers
    /// width/height/orientation + `screen`/`all`/`not`/`only`/`and`/comma;
    /// unrecognized features (e.g. `prefers-*`, `hover`, `pointer`) don't match
    /// (the conservative default the old stub had for every query).
    pub fn media_matches(&self, query: &str) -> bool {
        media_query_matches_with_density(query, self.viewport_px, self.device_pixel_ratio)
    }

    /// Evaluate against an explicit CSS viewport. Environment-sensitive
    /// algorithms that already carry their layout pass's viewport use this so
    /// a stale/default DOM environment cannot disagree with that pass.
    pub fn media_matches_at(&self, query: &str, width: f32, height: f32) -> bool {
        self.media_matches_at_density(query, width, height, self.device_pixel_ratio)
    }

    pub fn media_matches_at_density(
        &self,
        query: &str,
        width: f32,
        height: f32,
        device_pixel_ratio: f32,
    ) -> bool {
        media_query_matches_with_density(
            query,
            (width.max(0.0), height.max(0.0)),
            device_pixel_ratio,
        )
    }

    /// Current CSS-pixel viewport for environment-sensitive HTML algorithms.
    pub fn viewport_px(&self) -> (f32, f32) {
        self.viewport_px
    }

    /// Set the output device density used by `srcset` candidate selection.
    /// HTML §4.8.4.3.13 permits reselection when the environment changes; the
    /// DOM epoch invalidates geometry/current-source consumers together.
    pub fn set_device_pixel_ratio(&mut self, ratio: f32) {
        let ratio = if ratio.is_finite() && ratio > 0.0 {
            ratio
        } else {
            1.0
        };
        if self.device_pixel_ratio != ratio {
            self.device_pixel_ratio = ratio;
            self.touch_style(); // resolution/device-pixel-ratio media queries
        }
    }

    pub fn device_pixel_ratio(&self) -> f32 {
        self.device_pixel_ratio
    }

    /// Set the document's URL (DOM §4.5). No `touch()`: it only affects how
    /// the serializer resolves sprite `<use>` hrefs, not the cascade.
    pub fn set_doc_url(&mut self, url: Option<url::Url>) {
        self.doc_url = url;
    }

    pub fn doc_url(&self) -> Option<&url::Url> {
        self.doc_url.as_ref()
    }

    /// Read a scroll metric (CSSOM View, px). `which`: 0=scrollTop, 1=scrollLeft,
    /// 4=clientHeight, 5=clientWidth. Position (0/1) defaults to 0; the clip box
    /// (4/5) is `None` until the app has pushed it (`set_scroll_geom`), so the
    /// getter falls back to the element's rect. `scrollHeight`/`scrollWidth`
    /// (2/3) are deliberately always `None` here: the JS geometry cache reads
    /// their fresh scrolling-area extents from the same fragment pass that
    /// supplies the border box, never from lagging frontend state.
    pub fn scroll_metric(&self, id: NodeId, which: u8) -> Option<f64> {
        let sb = self.scroll_state.get(&id);
        match which {
            0 => Some(sb.map_or(0.0, |s| s.top)),
            1 => Some(sb.map_or(0.0, |s| s.left)),
            4 => sb.and_then(|s| s.client_h),
            5 => sb.and_then(|s| s.client_w),
            _ => None,
        }
    }

    /// Set a scroll position (px). The CSSOM View binding has already clamped
    /// it to the element's scrolling area.
    /// `record` (a page-JS write) queues a `Scrolled` delivery so the app
    /// re-windows the scrolling box cheaply; the frontend write-back passes
    /// `record=false` (it already moved its retained offset). This applies to
    /// BOTH axes: graphical horizontal scrollers do not participate in the
    /// terminal-only `RegionGeom` round trip. NEVER sets the dirty bit — a
    /// scroll paints no content of its own; the position rides the next serialize
    /// (baked) and the `Scrolled` channel. Returns whether the position changed,
    /// which the CSSOM View binding uses to queue `scroll`/`scrollend`.
    pub fn set_scroll_pos(&mut self, id: NodeId, top: f64, left: f64, record: bool) -> bool {
        let sb = self.scroll_state.entry(id).or_default();
        let changed = sb.top != top || sb.left != left;
        sb.top = top;
        sb.left = left;
        if record && changed {
            self.scroll_changes.push((id, top, left));
        }
        changed
    }

    /// Store the app-measured CLIP box (px) for a scroll region — the viewport
    /// height/width the `clientHeight`/`clientWidth` getters report. (Pure
    /// measurement backing: no dirty, no scroll record. The scrolling-area
    /// dimensions remain actor-owned fragment geometry; see `ScrollBox`.)
    pub fn set_scroll_geom(&mut self, id: NodeId, client_h: f64, client_w: f64) {
        let sb = self.scroll_state.entry(id).or_default();
        sb.client_h = Some(client_h);
        sb.client_w = Some(client_w);
    }

    /// Drain the page-initiated scroll writes for `PageEvt::Scrolled` delivery.
    pub fn take_scroll_changes(&mut self) -> Vec<(NodeId, f64, f64)> {
        std::mem::take(&mut self.scroll_changes)
    }

    /// A vertical scroll container (CSS Overflow L3 `overflow-y: auto|scroll`).
    /// The live serializer marks these with `data-trust-node` + a baked
    /// `data-trust-scroll-top` so the app's `flow_region` can re-seed the
    /// region's scroll offset across the per-message re-parse.
    pub fn is_scroll_container(&self, id: NodeId) -> bool {
        let v = match self.computed_style(id, "overflow-y") {
            Some(v) => v,
            None => match self.computed_style(id, "overflow") {
                // shorthand `overflow: x [y]` — the y component defaults to x.
                Some(sh) => {
                    let mut toks = sh.split_whitespace();
                    let x = toks.next().unwrap_or("");
                    toks.next().unwrap_or(x).to_string()
                }
                None => return false,
            },
        };
        matches!(v.trim().to_ascii_lowercase().as_str(), "auto" | "scroll")
    }

    /// A horizontal scroll container (`overflow-x: auto|scroll`) — the strip
    /// axis of a carousel.
    pub fn is_hscroll_container(&self, id: NodeId) -> bool {
        let v = match self.computed_style(id, "overflow-x") {
            Some(v) => v,
            None => match self.computed_style(id, "overflow") {
                Some(sh) => sh.split_whitespace().next().unwrap_or("").to_string(),
                None => return false,
            },
        };
        matches!(v.trim().to_ascii_lowercase().as_str(), "auto" | "scroll")
    }

    /// Whether this element clips its overflow on the BLOCK (vertical) axis —
    /// `overflow-y: hidden|clip` (longhand, else the `overflow` shorthand's y
    /// component, which defaults to x). On `html`/`body` this is the signal
    /// that the VIEWPORT can't scroll the document (CSS Overflow L3 §3.1). Read
    /// on the block axis only: the ubiquitous `overflow-x:hidden` "no sideways
    /// scrollbar" trick must NOT read as a locked viewport.
    fn clips_block_axis(&self, id: NodeId) -> bool {
        let v = match self.computed_style(id, "overflow-y") {
            Some(v) => v,
            None => match self.computed_style(id, "overflow") {
                Some(sh) => {
                    let mut toks = sh.split_whitespace();
                    let x = toks.next().unwrap_or("");
                    toks.next().unwrap_or(x).to_string()
                }
                None => return false,
            },
        };
        matches!(v.trim().to_ascii_lowercase().as_str(), "hidden" | "clip")
    }

    /// Whether `id` is the page's PRINCIPAL scroll container — the one a LOCKED
    /// viewport delegates document scrolling to (the SPA app-shell pattern where
    /// `html`/`body` are `overflow:hidden` and one inner `overflow:auto` box
    /// carries the main flow, e.g. Twitch's `root-scrollable` inside `<main>`).
    /// It stays a genuine scroll `Region`, but the terminal presents it as "the
    /// page": the main scrollbar reflects its position, the page-level scroll
    /// gestures (wheel off a nested region, PgUp/PgDn, Home/End) drive it, and
    /// its offset is user-locked across live re-renders (the page's own scroll
    /// signal never resets it). Read purely from the page's declarations: CSS
    /// Overflow §3.1 (the root element's overflow propagates to the viewport; if
    /// the root is `visible` but `<body>` is not, the body's propagates) + HTML
    /// sectioning landmarks (`<main>` is the dominant content, `<nav>`/`<aside>`
    /// are complementary) — never the host.
    ///
    /// ONE upward walk from `id` to the root: a scroll-container ancestor ⇒ `id`
    /// is NESTED ⇒ not principal (a real inner region); the nearest sectioning
    /// landmark above `id` decides main-flow (`<main>`) vs a complementary
    /// sidebar (`<nav>`/`<aside>`, stays a plain region); and the viewport must
    /// be block-axis LOCKED. Principal ⇔ locked AND (inside `<main>` OR the page
    /// declares no enclosing landmark at all, i.e. this outermost scroller
    /// carries the flow). Shared by both layout engines.
    pub fn is_principal_scroller(&self, id: NodeId) -> bool {
        if !self.is_scroll_container(id) {
            return false;
        }
        let mut viewport_locked = false;
        let mut in_main = false;
        let mut landmark_seen = false;
        let mut cur = self.parent_composed(id);
        while let Some(p) = cur {
            // A scroll-container ancestor ⇒ a nested inner region, never the page.
            if self.is_scroll_container(p) {
                return false;
            }
            match self.tag_name(p) {
                Some("main") if !landmark_seen => {
                    in_main = true;
                    landmark_seen = true;
                }
                Some("nav" | "aside") if !landmark_seen => landmark_seen = true,
                Some("html" | "body") if self.clips_block_axis(p) => viewport_locked = true,
                _ => {}
            }
            cur = self.parent_composed(p);
        }
        // Inside `<main>` the landmark is the signal. Landmark-LESS, the scroller
        // is the page only when it is the SOLE content spine of the app shell
        // (`<body><div>…<div overflow:auto>`) — otherwise two panels of a flex
        // row would BOTH read as principal (the humantooth over-match).
        viewport_locked && (in_main || (!landmark_seen && self.is_sole_spine_to_body(id)))
    }

    /// Whether every ancestor between `id` and `<body>`/`<html>` has `id`'s
    /// path child as its SOLE rendered box child — i.e. `id` is the single
    /// content spine of the app shell, not one column among siblings. A
    /// landmark-less locked-viewport page promotes its scroller to the principal
    /// (page) scroller only when it is this sole spine.
    fn is_sole_spine_to_body(&self, id: NodeId) -> bool {
        let mut child = id;
        let mut cur = self.parent_composed(id);
        while let Some(p) = cur {
            // Reaching the document root ends the spine (body/html carry the page).
            if matches!(self.tag_name(p), Some("body" | "html")) {
                return true;
            }
            // `p` must have no rendered box child other than the one we came from.
            if self
                .composed_children(p)
                .into_iter()
                .any(|c| c != child && self.renders_as_box(c))
            {
                return false;
            }
            child = p;
            cur = self.parent_composed(p);
        }
        true
    }

    /// Whether `c` generates a box in normal flow — an element that isn't hidden
    /// (`display:none`, closed dialog/popover, …) and isn't document metadata.
    /// Text/comment nodes and metadata (`<script>`/`<style>`/`<link>`/…) don't
    /// count as content siblings for the app-shell spine test.
    fn renders_as_box(&self, c: NodeId) -> bool {
        match self.tag_name(c) {
            None => false, // text / comment — not a box for the spine test
            Some(
                "script" | "style" | "link" | "meta" | "title" | "base" | "head" | "template"
                | "noscript",
            ) => false,
            Some(_) => !self.is_hidden(c),
        }
    }

    /// Parse a full HTML document into a fresh arena.
    pub fn parse_document(html: &str) -> Self {
        let sink = Sink {
            dom: RefCell::new(Dom::new()),
        };
        html5ever::parse_document(sink, ParseOpts::default()).one(StrTendril::from(html))
    }

    fn new_node(&mut self, data: NodeData) -> NodeId {
        self.nodes.push(Node {
            parent: None,
            first_child: None,
            last_child: None,
            prev_sibling: None,
            next_sibling: None,
            owner_document: DOCUMENT,
            data,
        });
        self.nodes.len() - 1
    }

    pub fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id]
    }

    /// Change the node document for a whole shadow-including subtree.  The
    /// arena keeps template contents and shadow roots in side structures, so
    /// they are included explicitly rather than relying only on parent links.
    fn set_owner_document_subtree(&mut self, root: NodeId, document: NodeId) {
        if !self.is_valid(root) {
            return;
        }
        let mut stack = vec![root];
        while let Some(id) = stack.pop() {
            self.nodes[id].owner_document = document;
            stack.extend(self.child_iter(id));
            let template_contents = match &self.nodes[id].data {
                NodeData::Element {
                    template_contents: Some(contents),
                    ..
                } => Some(*contents),
                _ => None,
            };
            if let Some(contents) = template_contents {
                stack.push(contents);
            }
            if let Some(&shadow) = self.shadow_roots.get(&id) {
                stack.push(shadow);
            }
        }
    }

    /// The document that owns `id` (DOM §4.5's *node document*).
    pub fn owner_document(&self, id: NodeId) -> Option<NodeId> {
        self.is_valid(id).then_some(self.nodes[id].owner_document)
    }

    pub fn is_valid(&self, id: NodeId) -> bool {
        id < self.nodes.len()
    }

    pub fn create_element(&mut self, tag: &str) -> NodeId {
        let tag = tag.to_ascii_lowercase();
        // Script-created templates need their content fragment exactly
        // like parser-created ones (Lit renders through them).
        let template_contents = (tag == "template").then(|| self.new_node(NodeData::Fragment));
        let name = QualName::new(None, ns!(html), tag.into());
        self.new_node(NodeData::Element {
            name,
            attrs: Vec::new(),
            template_contents,
        })
    }

    /// Create an element with the exact expanded name produced by DOM's
    /// `validate and extract` algorithm. Validation and Web IDL conversion live
    /// in the engine-neutral platform prelude; the arena must preserve the
    /// resulting namespace, optional prefix, and local name without HTML case
    /// folding (DOM §4.5 `createElementNS`).
    pub fn create_element_ns(
        &mut self,
        namespace: &str,
        prefix: Option<&str>,
        local_name: &str,
    ) -> NodeId {
        let namespace = Namespace::from(namespace);
        let template_contents = (namespace == ns!(html) && local_name == "template")
            .then(|| self.new_node(NodeData::Fragment));
        let name = QualName::new(prefix.map(Prefix::from), namespace, local_name.into());
        self.new_node(NodeData::Element {
            name,
            attrs: Vec::new(),
            template_contents,
        })
    }

    pub fn create_text(&mut self, text: &str) -> NodeId {
        self.new_node(NodeData::Text(text.to_string()))
    }

    pub fn create_fragment(&mut self) -> NodeId {
        self.new_node(NodeData::Fragment)
    }

    pub fn create_comment(&mut self, text: &str) -> NodeId {
        self.new_node(NodeData::Comment(text.to_string()))
    }

    /// Unlink a node from its parent and siblings (the node and its
    /// subtree stay in the arena; arenas only ever grow — page-lifetime
    /// memory is the deal).
    pub fn detach(&mut self, id: NodeId) {
        let (parent, prev, next) = {
            let n = &self.nodes[id];
            (n.parent, n.prev_sibling, n.next_sibling)
        };
        // The PARENT's child list is what changed; `None` (an already-orphan
        // node, e.g. a fresh child about to be appended) is a no-op for the
        // rendered tree — dirties the epoch but records no relayout target.
        // An ATTACHED node leaving may take stylesheet(s) with it (the
        // orphan case skips the check entirely — the fresh-node append path
        // stays one tag check total, paid on the append side).
        if parent.is_some() {
            self.note_tree_style_mutation(parent, id);
        }
        self.touch_content(parent);
        if let Some(prev) = prev {
            self.nodes[prev].next_sibling = next;
        }
        if let Some(next) = next {
            self.nodes[next].prev_sibling = prev;
        }
        if let Some(parent) = parent {
            if self.nodes[parent].first_child == Some(id) {
                self.nodes[parent].first_child = next;
            }
            if self.nodes[parent].last_child == Some(id) {
                self.nodes[parent].last_child = prev;
            }
        }
        let n = &mut self.nodes[id];
        n.parent = None;
        n.prev_sibling = None;
        n.next_sibling = None;
    }

    pub fn append(&mut self, parent: NodeId, child: NodeId) {
        let owner_document = self.nodes[parent].owner_document;
        self.detach(child);
        self.note_tree_style_mutation(Some(parent), child);
        let old_last = self.nodes[parent].last_child;
        self.nodes[child].parent = Some(parent);
        self.nodes[child].prev_sibling = old_last;
        if let Some(last) = old_last {
            self.nodes[last].next_sibling = Some(child);
        } else {
            self.nodes[parent].first_child = Some(child);
        }
        self.nodes[parent].last_child = Some(child);
        self.set_owner_document_subtree(child, owner_document);
        self.touch_content(Some(parent));
    }

    /// Link a newly-created, detached node while constructing another detached
    /// subtree. This is not a DOM mutation: no script can observe the nodes yet,
    /// and the eventual insertion performs ownership and cache invalidation for
    /// the completed subtree exactly once.
    fn append_fresh(&mut self, parent: NodeId, child: NodeId) {
        debug_assert!(self.nodes[child].parent.is_none());
        debug_assert!(self.nodes[child].prev_sibling.is_none());
        debug_assert!(self.nodes[child].next_sibling.is_none());

        let old_last = self.nodes[parent].last_child;
        self.nodes[child].parent = Some(parent);
        self.nodes[child].prev_sibling = old_last;
        self.nodes[child].owner_document = self.nodes[parent].owner_document;
        if let Some(last) = old_last {
            self.nodes[last].next_sibling = Some(child);
        } else {
            self.nodes[parent].first_child = Some(child);
        }
        self.nodes[parent].last_child = Some(child);
    }

    /// Unlink a node while html5ever is constructing its private parse arena.
    /// Parser tree surgery is not a mutation of the live page, so it must not
    /// invalidate style/layout caches or walk node-document ownership.
    fn parser_detach(&mut self, id: NodeId) {
        let (parent, previous, next) = {
            let node = &self.nodes[id];
            (node.parent, node.prev_sibling, node.next_sibling)
        };
        if let Some(previous) = previous {
            self.nodes[previous].next_sibling = next;
        }
        if let Some(next) = next {
            self.nodes[next].prev_sibling = previous;
        }
        if let Some(parent) = parent {
            if self.nodes[parent].first_child == Some(id) {
                self.nodes[parent].first_child = next;
            }
            if self.nodes[parent].last_child == Some(id) {
                self.nodes[parent].last_child = previous;
            }
        }
        let node = &mut self.nodes[id];
        node.parent = None;
        node.prev_sibling = None;
        node.next_sibling = None;
    }

    /// Append within html5ever's private arena. Unlike [`Self::append_fresh`],
    /// this accepts an already-linked node because the HTML adoption-agency and
    /// foster-parenting algorithms move existing parser nodes.
    fn parser_append(&mut self, parent: NodeId, child: NodeId) {
        self.parser_detach(child);
        let previous = self.nodes[parent].last_child;
        self.nodes[child].parent = Some(parent);
        self.nodes[child].prev_sibling = previous;
        if let Some(previous) = previous {
            self.nodes[previous].next_sibling = Some(child);
        } else {
            self.nodes[parent].first_child = Some(child);
        }
        self.nodes[parent].last_child = Some(child);
    }

    /// Insert within html5ever's private arena, preserving DOM pre-insert's
    /// self-reference behavior even though well-formed parser calls ordinarily
    /// supply a distinct reference node.
    fn parser_insert_before(&mut self, parent: NodeId, child: NodeId, reference: NodeId) {
        debug_assert_eq!(self.nodes[reference].parent, Some(parent));
        let reference = if reference == child {
            let Some(next) = self.nodes[child].next_sibling else {
                self.parser_append(parent, child);
                return;
            };
            next
        } else {
            reference
        };
        self.parser_detach(child);
        let previous = self.nodes[reference].prev_sibling;
        self.nodes[child].parent = Some(parent);
        self.nodes[child].prev_sibling = previous;
        self.nodes[child].next_sibling = Some(reference);
        self.nodes[reference].prev_sibling = Some(child);
        if let Some(previous) = previous {
            self.nodes[previous].next_sibling = Some(child);
        } else {
            self.nodes[parent].first_child = Some(child);
        }
    }

    fn parser_append_text(&mut self, parent: NodeId, text: &str) {
        if let Some(last) = self.nodes[parent].last_child
            && let NodeData::Text(existing) = &mut self.nodes[last].data
        {
            existing.push_str(text);
            return;
        }
        let text = self.new_node(NodeData::Text(text.to_owned()));
        self.parser_append(parent, text);
    }

    fn parser_insert_text_before(&mut self, sibling: NodeId, text: &str) {
        if let Some(previous) = self.nodes[sibling].prev_sibling
            && let NodeData::Text(existing) = &mut self.nodes[previous].data
        {
            existing.push_str(text);
            return;
        }
        let Some(parent) = self.nodes[sibling].parent else {
            return;
        };
        let text = self.new_node(NodeData::Text(text.to_owned()));
        self.parser_insert_before(parent, text, sibling);
    }

    /// DOM Standard §4.2.3's *replace all* tree operation for a list of freshly
    /// parsed, detached roots. The caller performs the observable custom-element,
    /// MutationObserver, and navigable steps around this arena operation. Here we
    /// preserve removal/insertion order while coalescing equivalent internal
    /// ownership and render-cache bookkeeping for the completed mutation.
    pub fn replace_all_children(&mut self, parent: NodeId, new_children: Vec<NodeId>) {
        let had_children = self.nodes[parent].first_child.is_some();
        if !had_children && new_children.is_empty() {
            return;
        }

        let mut style_changed = new_children
            .iter()
            .any(|&child| self.tree_mutation_changes_style(parent, child));
        if !style_changed {
            style_changed = self
                .child_iter(parent)
                .any(|child| self.tree_mutation_changes_style(parent, child));
        }

        // Snapshot each next link before severing it. Removed subtrees remain
        // intact and detached, retaining their node identities and listeners.
        let mut old = self.nodes[parent].first_child;
        while let Some(child) = old {
            old = self.nodes[child].next_sibling;
            let node = &mut self.nodes[child];
            node.parent = None;
            node.prev_sibling = None;
            node.next_sibling = None;
        }
        self.nodes[parent].first_child = None;
        self.nodes[parent].last_child = None;

        let owner_document = self.nodes[parent].owner_document;
        let mut previous = None;
        for child in new_children {
            debug_assert!(self.nodes[child].parent.is_none());
            debug_assert!(self.nodes[child].prev_sibling.is_none());
            debug_assert!(self.nodes[child].next_sibling.is_none());
            self.nodes[child].parent = Some(parent);
            self.nodes[child].prev_sibling = previous;
            if let Some(previous) = previous {
                self.nodes[previous].next_sibling = Some(child);
            } else {
                self.nodes[parent].first_child = Some(child);
            }
            self.set_owner_document_subtree(child, owner_document);
            previous = Some(child);
        }
        self.nodes[parent].last_child = previous;

        if style_changed {
            self.touch_style_at(parent);
        } else {
            self.touch_content(Some(parent));
        }
    }

    /// Insert `child` under `parent` immediately before `reference`;
    /// append when `reference` is None (DOM insertBefore semantics).
    pub fn insert_before(&mut self, parent: NodeId, child: NodeId, reference: Option<NodeId>) {
        let Some(reference) = reference else {
            self.append(parent, child);
            return;
        };
        if self.nodes[reference].parent != Some(parent) {
            // A real DOM throws NotFoundError; tolerate with an append.
            self.append(parent, child);
            return;
        }
        // Pre-insert (WHATWG DOM §4.2.4): inserting a node before ITSELF is
        // legal — the reference becomes the node's next sibling (an in-place
        // move). Without this the splice below would point the node's
        // prev/next at itself, corrupting the sibling list into a cycle that
        // hangs every later sibling walk (children/serialize/descendants).
        let reference = if reference == child {
            match self.nodes[child].next_sibling {
                Some(next) => next,
                // Already the last child: an in-place move is a re-append.
                None => {
                    self.append(parent, child);
                    return;
                }
            }
        } else {
            reference
        };
        let owner_document = self.nodes[parent].owner_document;
        self.detach(child);
        self.note_tree_style_mutation(Some(parent), child);
        let prev = self.nodes[reference].prev_sibling;
        self.nodes[child].parent = Some(parent);
        self.nodes[child].prev_sibling = prev;
        self.nodes[child].next_sibling = Some(reference);
        self.nodes[reference].prev_sibling = Some(child);
        match prev {
            Some(prev) => self.nodes[prev].next_sibling = Some(child),
            None => self.nodes[parent].first_child = Some(child),
        }
        self.set_owner_document_subtree(child, owner_document);
        self.touch_content(Some(parent));
    }

    /// Implement the DOM Standard's adopt algorithm (DOM §4.5): remove the
    /// node from its old parent, then retarget the node document for the whole
    /// shadow-including subtree.  The caller supplies the target `Document`.
    pub fn adopt_node(&mut self, document: NodeId, id: NodeId) -> Result<NodeId, AdoptError> {
        if !self.is_valid(document) || !matches!(self.nodes[document].data, NodeData::Document) {
            return Err(AdoptError::TargetNotDocument);
        }
        if !self.is_valid(id) {
            return Err(AdoptError::InvalidNode);
        }
        if matches!(self.nodes[id].data, NodeData::Document) {
            return Err(AdoptError::Document);
        }
        if self.shadow_hosts.contains_key(&id) {
            return Err(AdoptError::ShadowRoot);
        }
        let old_document = self.nodes[id].owner_document;
        self.detach(id);
        self.set_owner_document_subtree(id, document);
        Ok(old_document)
    }

    /// Append text, merging into a trailing text node like a parser would.
    pub fn append_text(&mut self, parent: NodeId, text: &str) {
        if let Some(last) = self.nodes[parent].last_child
            && let NodeData::Text(existing) = &mut self.nodes[last].data
        {
            existing.push_str(text);
            if self.tag_name(parent) == Some("style") {
                self.touch_style_at(parent); // the sheet's text grew
            }
            self.touch_content(Some(parent));
            return;
        }
        let t = self.create_text(text);
        self.append(parent, t);
    }

    pub fn children(&self, id: NodeId) -> Vec<NodeId> {
        let mut out = Vec::new();
        let mut next = self.nodes[id].first_child;
        while let Some(c) = next {
            out.push(c);
            next = self.nodes[c].next_sibling;
        }
        out
    }

    /// The children of `id` as a LAZY iterator (no Vec) — for read-only
    /// walks (the serializers, queries, text extraction). Use `children()`
    /// (materialized) when the tree is mutated mid-iteration.
    pub fn child_iter(&self, id: NodeId) -> impl Iterator<Item = NodeId> + '_ {
        std::iter::successors(self.nodes[id].first_child, move |&c| {
            self.nodes[c].next_sibling
        })
    }

    /// The subtree under `root` in document (pre-)order, excluding `root`,
    /// as a LAZY allocation-free iterator: O(1) state over the first_child/
    /// next_sibling/parent pointers — no per-node child Vec, no whole-subtree
    /// out Vec, and early-exiting callers (getElementById, querySelector's
    /// first match) stop walking at the hit. Borrowing `&self` for the walk
    /// also makes mutation-during-iteration a compile error; callers that
    /// mutate mid-walk collect first (`rewrite_inline_svgs`).
    pub fn descendants(&self, root: NodeId) -> Descendants<'_> {
        Descendants {
            dom: self,
            root,
            next: self.nodes[root].first_child,
        }
    }

    pub fn tag_name(&self, id: NodeId) -> Option<&str> {
        match &self.nodes[id].data {
            NodeData::Element { name, .. } => Some(&name.local),
            _ => None,
        }
    }

    /// The element's namespace URI (DOM `Element.namespaceURI`): the full URI
    /// string carried in its `QualName` — `http://www.w3.org/1999/xhtml` for
    /// HTML, `…/2000/svg` for SVG, `…/1998/Math/MathML` for MathML. `None`
    /// (→ `null` in JS) for non-elements or the null namespace. Vue 3's
    /// hydration reads `el.namespaceURI.includes("svg")`, so a missing value
    /// throws on every SSR Vue/Nuxt page.
    pub fn namespace_uri(&self, id: NodeId) -> Option<&str> {
        match &self.nodes[id].data {
            NodeData::Element { name, .. } => {
                let ns = &*name.ns;
                (!ns.is_empty()).then_some(ns)
            }
            _ => None,
        }
    }

    /// DOM `Element.prefix`, retained as part of the element's qualified name.
    pub fn namespace_prefix(&self, id: NodeId) -> Option<&str> {
        match &self.nodes[id].data {
            NodeData::Element { name, .. } => name.prefix.as_deref(),
            _ => None,
        }
    }

    pub fn attr(&self, id: NodeId, name: &str) -> Option<&str> {
        match &self.nodes[id].data {
            NodeData::Element { attrs, .. } => attrs
                .iter()
                .find(|a| str::eq_ignore_ascii_case(&a.name.local, name))
                .map(|a| &*a.value),
            _ => None,
        }
    }

    /// The `content` of the first `<meta>` whose `property`/`name` matches
    /// `key` (case-insensitive) — the Open Graph / page-metadata channel
    /// (`og:image`, `twitter:image`, `og:type`, …). Empty content is treated
    /// as absent. Host-agnostic: this is the standard cross-site preview/typing
    /// surface, used to give an unplayable `<video>` a preview thumbnail.
    pub fn meta_content(&self, key: &str) -> Option<&str> {
        self.descendants(DOCUMENT)
            .filter(|&id| self.tag_name(id) == Some("meta"))
            .find(|&id| {
                self.attr(id, "property")
                    .or_else(|| self.attr(id, "name"))
                    .is_some_and(|k| k.eq_ignore_ascii_case(key))
            })
            .and_then(|id| self.attr(id, "content"))
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }

    pub fn set_attr(&mut self, id: NodeId, name: &str, value: &str) {
        // An attribute change on a sheet-bearing element can change the
        // sheet set (`<link rel/href/disabled>`; conservatively any).
        let sheet_el = matches!(self.tag_name(id), Some("style" | "link"));
        if let NodeData::Element {
            name: qname, attrs, ..
        } = &mut self.nodes[id].data
        {
            // DOM setAttribute folds the name to lowercase ONLY for elements
            // in the HTML namespace; SVG/MathML attributes are case-sensitive
            // (`viewBox`, `preserveAspectRatio`). Folding unconditionally
            // pushed a duplicate lowercase attr beside the parser's cased one
            // and left reads (case-insensitive, first match) on the stale
            // value — a D3-style `setAttribute("viewBox", …)` never took.
            let name = if qname.ns == ns!(html) {
                name.to_ascii_lowercase()
            } else {
                name.to_string()
            };
            if let Some(a) = attrs.iter_mut().find(|a| *a.name.local == name) {
                // Idempotent writes are free: no dirty, no redraw.
                if *a.value == *value {
                    return;
                }
                a.value = StrTendril::from(value);
            } else {
                attrs.push(Attribute {
                    name: QualName::new(None, ns!(), name.into()),
                    value: StrTendril::from(value),
                });
            }
            if sheet_el {
                self.touch_style_at(id);
            }
            self.touch_attr(id);
        }
    }

    pub fn remove_attr(&mut self, id: NodeId, name: &str) {
        let sheet_el = matches!(self.tag_name(id), Some("style" | "link"));
        if let NodeData::Element { attrs, .. } = &mut self.nodes[id].data {
            let before = attrs.len();
            attrs.retain(|a| !str::eq_ignore_ascii_case(&a.name.local, name));
            // Idempotent removes are free (like `set_attr`): a redundant
            // `removeAttribute` must not dirty the page or bust the epoch
            // caches — frameworks call it unconditionally per render pass.
            if attrs.len() != before {
                if sheet_el {
                    self.touch_style_at(id);
                }
                self.touch_attr(id);
            }
        }
    }

    pub fn attr_names(&self, id: NodeId) -> Vec<String> {
        match &self.nodes[id].data {
            NodeData::Element { attrs, .. } => {
                attrs.iter().map(|a| a.name.local.to_string()).collect()
            }
            _ => Vec::new(),
        }
    }

    /// Is this element hidden — by the `hidden` attribute, or by the
    /// cascaded `display` (inline style, `<style>` elements, shadow sheets,
    /// adoptedStyleSheets, fetched `<link>` sheets)? Winner per property is
    /// the lexicographic max of (!important, inline, layer, specificity,
    /// source order) — inline beats sheets except under !important, the
    /// real rules for a single author origin (`@media`/`@supports`/`@layer`
    /// evaluated at index build). Hidden subtrees don't render. This reads
    /// the author cascade directly (`cascaded`), NOT inheritance. For
    /// inherited/UA-defaulted values use `computed_value`.
    /// Whether `id` is the host of an editing region — it carries a truthy
    /// `contenteditable` attribute (`""`/`true`/`plaintext-only`). This is the
    /// editor ROOT (where the attribute sits); descendants merely inherit
    /// editability and are not themselves hosts. TRust treats such a host like a
    /// textarea: one editable widget whose subtree we don't flow.
    pub fn is_contenteditable_host(&self, id: NodeId) -> bool {
        match self.attr(id, "contenteditable") {
            Some(v) => {
                let v = v.trim().to_ascii_lowercase();
                v.is_empty() || v == "true" || v == "plaintext-only"
            }
            None => false,
        }
    }

    pub fn is_hidden(&self, id: NodeId) -> bool {
        // Per-epoch memo: `is_hidden` reads ~15 cascaded properties and runs once
        // per `flow_element` visit, with the same node re-tested by every
        // measurement re-descent through it — the layout's most-repeated check.
        if let Some(&hit) = self.hidden_cache.borrow().get(id, self.epoch) {
            return hit;
        }
        let hidden = self.is_hidden_inner(id);
        self.hidden_cache.borrow_mut().put(id, self.epoch, hidden);
        hidden
    }

    fn is_hidden_inner(&self, id: NodeId) -> bool {
        if self.attr(id, "hidden").is_some() {
            return true;
        }
        // UA default `dialog:not([open]) { display:none }`: a closed dialog
        // is a modal that hasn't been shown — never render its content (its
        // text otherwise bleeds into the page). An author rule setting the
        // dialog's `display` wins, so only apply when the cascade is silent.
        if self.tag_name(id) == Some("dialog")
            && self.attr(id, "open").is_none()
            && self.cascaded(id, "display").is_none()
        {
            return true;
        }
        // HTML Popover: hidePopover removes the element from the top layer AND
        // applies display:none. This is visibility state, not an ordinary UA
        // declaration which author `display:block` can override. Tooltip
        // libraries commonly retain that inline display while hidden; allowing
        // it to win resurrects every closed menu in the presentation snapshot.
        if !self.is_popover_showing(id) && self.attr(id, "popover").is_some() {
            return true;
        }
        // `display:none` generates NO box (the element and subtree occupy no
        // space). `visibility:hidden` is NOT here — like `opacity:0` it is
        // paint suppression (laid out, painted blank), routed through
        // `visibility_hidden`/`Ctx.invisible`, so a `visibility:hidden` element
        // keeps its box (CSS2 §11.2) and a `visibility:visible` descendant of it
        // is still painted.
        // CSS Variables L1 §3: `var()` is substituted at computed-value time,
        // before the property's value is interpreted. A declaration such as
        // Stack Exchange's `display:var(--_po-d)` therefore has to participate
        // in the display:none check after substitution, not as the literal
        // token stream returned by the cascade.
        if self.computed_display(id).as_deref() == Some("none") {
            return true;
        }
        // Visually-hidden / "sr-only" accessibility text: the universal idiom
        // for screen-reader-only content is a 1px, clipped, absolutely
        // positioned box (Bootstrap `.visually-hidden`, Tailwind / HTML5BP
        // `.sr-only`, archive.org's `aria-describedby` targets, …). It carries
        // text meant to be invisible to sighted users — render nothing, as a
        // browser does, instead of leaking it into the page (it's also often
        // wider than its sibling content, distorting flex/grid sizing).
        // `position` is checked first so the hot path short-circuits for the
        // overwhelming majority of nodes that aren't absolutely positioned.
        if self.cascaded(id, "position").as_deref() == Some("absolute")
            && self.cascaded(id, "overflow").as_deref() == Some("hidden")
            && self
                .cascaded(id, "width")
                .as_deref()
                .is_some_and(css_len_at_most_1px)
        {
            return true;
        }
        // The OTHER visually-hidden idiom: shove an absolutely/fixed-positioned
        // box far off the top-left corner (`left:-9999px`, `top:-1000px`).
        // YouTube's "Skip navigation" button hides this way; without honoring it
        // we clamp the negative offset to row/col 0 in `place_positioned_children`
        // and the hidden text paints at the very top-left. `position` is checked
        // first so the hot path short-circuits for non-positioned nodes.
        if matches!(
            self.cascaded(id, "position").as_deref(),
            Some("absolute" | "fixed")
        ) {
            // CSS Position 3 §5.1 resolves an over-constrained axis using
            // the inset equation. When both opposing insets are large and
            // negative *and* the corresponding margins are auto, those auto
            // margins absorb the free space and center the replaced box.
            // Amazon's homepage uses this image-centering idiom:
            // `left:-9999px; right:-9999px; margin:auto`. A single negative
            // inset is still the usual screen-reader/off-screen pattern, but
            // treating either side in isolation incorrectly drops the
            // centered image from the render tree.
            let centered_axis = |start: &str, end: &str, margin_start: &str, margin_end: &str| {
                self.cascaded(id, start)
                    .as_deref()
                    .is_some_and(css_len_offscreen_neg)
                    && self
                        .cascaded(id, end)
                        .as_deref()
                        .is_some_and(css_len_offscreen_neg)
                    && self.cascaded(id, margin_start).as_deref() == Some("auto")
                    && self.cascaded(id, margin_end).as_deref() == Some("auto")
            };
            let offscreen_x = self
                .cascaded(id, "left")
                .as_deref()
                .is_some_and(css_len_offscreen_neg)
                && !centered_axis("left", "right", "margin-left", "margin-right");
            let offscreen_y = self
                .cascaded(id, "top")
                .as_deref()
                .is_some_and(css_len_offscreen_neg)
                && !centered_axis("top", "bottom", "margin-top", "margin-bottom");
            if offscreen_x || offscreen_y {
                return true;
            }
        }
        // A box collapsed to ZERO on an axis, with `overflow:hidden`/`clip` on
        // that axis, clips ALL its content to nothing — the standard "keep it
        // in the DOM but show nothing" idiom (a preloaded hero copy, a closed
        // `max-height:0` drawer/accordion, a `height:0` mega-menu). A browser
        // paints none of it; we used to render it (Steam's
        // `.menu_takeover_background{height:0;overflow:hidden}` preload copy of
        // the banner drew a full-width 1-row sliver). EXCEPTION: a `height:0`
        // box whose PADDING reserves the height is the responsive-image
        // "intrinsic ratio" box (`padding-bottom:56.25%` → a 16:9 thumbnail
        // whose absolutely-positioned child fills the padding box, Humble
        // Bundle's tiles) — its content box is zero but the padding box isn't,
        // so it is NOT empty; spare it (`intrinsic_ratio_container_rows` sizes
        // the child off exactly this).
        let clips = |v: Option<String>| {
            v.as_deref().is_some_and(|s| {
                let mut toks = s.split_whitespace().peekable();
                toks.peek().is_some() && toks.all(|t| matches!(t, "hidden" | "clip"))
            })
        };
        let overflow = self.cascaded(id, "overflow");
        let zero = |prop| {
            self.cascaded(id, prop)
                .as_deref()
                .is_some_and(css_len_is_zero)
        };
        let oy = clips(self.cascaded(id, "overflow-y")) || clips(overflow.clone());
        let ox = clips(self.cascaded(id, "overflow-x")) || clips(overflow);
        let h_zero = zero("height") || zero("max-height");
        let w_zero = zero("width") || zero("max-width");
        if (oy && h_zero && !self.has_axis_padding(id, true))
            || (ox && w_zero && !self.has_axis_padding(id, false))
        {
            return true;
        }
        // A REPLACED element (img/svg/video/canvas/…) sized to a definite zero on
        // EITHER axis paints nothing: its raster scales into a zero content box,
        // so — unlike a normal block, whose overflow can still show — there is
        // nothing to overflow, and `overflow` is irrelevant (hence no `ox`/`oy`
        // gate here). This is the OTHER half of the copyable-but-unseen idiom:
        // `font-size:0` hides sibling TEXT (which never affects a replaced box),
        // while images are collapsed by a separate zero-size rule (Mastodon's
        // `.invisible img{width:0!important;height:0!important}`). Without this
        // our image box clamps to a 1-cell sliver instead of vanishing.
        if (w_zero || h_zero)
            && matches!(
                self.tag_name(id),
                Some(
                    "img" | "svg" | "video" | "canvas" | "picture" | "iframe" | "embed" | "object"
                )
            )
        {
            return true;
        }
        // `opacity:0` is NOT hidden — CSS separates box generation (`display`)
        // from painting. `opacity` (like `visibility`) suppresses only the
        // PAINT: an `opacity:0` element is fully laid out and occupies its
        // normal space (`getBoundingClientRect`/`scrollHeight` report its real
        // box), it is merely painted fully transparent. Collapsing it here (no
        // box) is what broke React virtualized lists — Mastodon's off-screen
        // placeholders are `opacity:0` PRECISELY so they keep their measured
        // height. Paint suppression rides `paint_suppressed`/`Ctx.invisible`
        // instead (laid out, painted blank); the slideshow that used to lean on
        // this branch still resolves to its active slide (an inactive slide is
        // out-of-flow → reserves no space, and paints blank → can't cover the
        // active one). See `paint_suppressed`.
        false
    }

    /// Whether an authored CSS/HTML state omits this element and all descendants
    /// from the box tree. Keep this narrower than [`Self::is_hidden`]: UA-hidden
    /// metadata/resource elements can affect the document outside their own
    /// boxes, while clipped screen-reader text and other visual suppression
    /// heuristics are not `display:none` at all.
    fn subtree_omitted_from_box_tree(&self, id: NodeId) -> bool {
        let Some(tag) = self.tag_name(id) else {
            return false;
        };
        // These elements can update stylesheet/document/browser state despite
        // having no principal CSS box (notably `<title>` and sheet-bearing
        // elements). Never infer render-inertness from their display value.
        if matches!(
            tag,
            "base" | "head" | "link" | "meta" | "script" | "style" | "title"
        ) {
            return false;
        }
        if self.attr(id, "hidden").is_some() || self.computed_display(id).as_deref() == Some("none")
        {
            return true;
        }
        // HTML's UA default for a closed dialog is display:none. Preserve the
        // existing author override behavior until origins are represented in
        // the cascade proper.
        if self.tag_name(id) == Some("dialog")
            && self.attr(id, "open").is_none()
            && self.cascaded(id, "display").is_none()
        {
            return true;
        }
        // A non-showing popover is in the UA's display:none state regardless
        // of a retained inline display declaration; showing state is separate
        // from the content attribute and author cascade.
        !self.is_popover_showing(id) && self.attr(id, "popover").is_some()
    }

    /// Whether the element's own PAINT is suppressed by an effective
    /// `opacity` of exactly zero. Unlike `is_hidden` (box generation),
    /// this does NOT remove the element from layout: CSS Color/Compositing lays
    /// out and measures an `opacity:0` element exactly as if visible, then
    /// paints it (and its subtree, as a group) fully transparent. The layout
    /// threads this down the inline formatting context (`Ctx.invisible`, like
    /// `font_zero`) so the whole subtree reserves its real box but writes blank
    /// cells — opacity is a group property a descendant cannot re-reveal, and
    /// `effective_opacity` already honors the `animation-fill-mode:forwards`
    /// slideshow reveal. Gated so a page with no `opacity` rules pays nothing.
    pub fn paint_suppressed(&self, id: NodeId) -> bool {
        let has_inline_opacity = || {
            self.attr(id, "style")
                .is_some_and(|s| s.contains("opacity"))
        };
        (self.style_index().has_opacity || has_inline_opacity())
            && self.effective_opacity(id) <= 0.0
    }

    /// Whether the element's own PAINT is suppressed by `visibility:hidden`
    /// (or `collapse`) — CSS2 §11.2. Like `opacity:0` this keeps the box (the
    /// element is fully laid out and occupies its normal space; only its cells
    /// paint blank), but UNLIKE opacity `visibility` INHERITS and is
    /// RE-CLEARABLE: a `visibility:visible` descendant of a hidden ancestor IS
    /// painted. So this reads the *computed* value (`computed_value` resolves the
    /// inheritance/override per element) rather than an accumulated flag — the
    /// layout never threads it as sticky (that's `Ctx.invisible`'s opacity
    /// chain); each element re-derives it here.
    pub fn visibility_hidden(&self, id: NodeId) -> bool {
        matches!(
            self.computed_value(id, "visibility").as_deref(),
            Some("hidden" | "force-hidden" | "collapse")
        )
    }

    /// Whether this element's generated boxes may participate in point hit
    /// testing. CSS UI 4 §6.2 removes `pointer-events:none` boxes, CSS Display
    /// 4 §5 excludes invisible boxes, and HTML/CSS UI inertness suppresses the
    /// whole flat-tree subtree. Opacity is deliberately absent: a fully
    /// transparent box remains hit-testable.
    pub fn point_hit_testable(&self, id: NodeId) -> bool {
        if self.visibility_hidden(id)
            || self
                .computed_value(id, "pointer-events")
                .is_some_and(|v| v.trim().eq_ignore_ascii_case("none"))
            || self
                .computed_value(id, "interactivity")
                .is_some_and(|v| v.trim().eq_ignore_ascii_case("inert"))
        {
            return false;
        }
        let mut cur = Some(id);
        while let Some(node) = cur {
            if self.attr(node, "inert").is_some() {
                return false;
            }
            cur = self.parent_composed(node);
        }
        true
    }

    /// Whether `id` or one of its DOM descendants carries native hyperlink
    /// activation semantics and remains eligible for point hit testing. This
    /// lets layout retain an otherwise paint-suppressed out-of-flow subtree
    /// only when discarding it would also discard a real interaction surface.
    pub fn subtree_has_point_hit_target(&self, id: NodeId) -> bool {
        (self.tag_name(id) == Some("a")
            && self.attr(id, "href").is_some()
            && self.point_hit_testable(id))
            || self
                .child_iter(id)
                .any(|child| self.subtree_has_point_hit_target(child))
    }

    /// Whether an element reserves height (`vertical`) or width via positive
    /// padding on that axis — the responsive-image "intrinsic ratio" idiom
    /// (`padding-bottom:56.25%` on a `height:0` box). A non-zero/`auto`/unknown
    /// value counts (we only treat a provably-zero box as empty), so this
    /// returns `true` to SPARE a box from the zero-axis hide above.
    fn has_axis_padding(&self, id: NodeId, vertical: bool) -> bool {
        let props: [&str; 2] = if vertical {
            ["padding-top", "padding-bottom"]
        } else {
            ["padding-left", "padding-right"]
        };
        props.iter().any(|p| {
            self.cascaded(id, p)
                .as_deref()
                .is_some_and(|v| !css_len_is_zero(v))
        })
    }

    /// The element's effective opacity for visibility: its cascaded `opacity`
    /// (default 1), or — when an `animation-fill-mode:forwards|both` animation
    /// names a keyframe set whose END opacity is known — that resting value.
    /// So `.slides{opacity:0}` hides, while `.slides.active{animation:fade-in
    /// forwards}` (ending `opacity:1`) shows, with no slideshow-specific code.
    pub fn effective_opacity(&self, id: NodeId) -> f32 {
        // CSS Custom Properties 2 §6: substitution happens at computed-value
        // time.  Parsing the token stream before resolving `var()` incorrectly
        // turns values such as `var(--backdrop-opacity, .6)` into opacity 1.
        let declared = self.computed_value_resolved(id, "opacity");
        let base = declared.as_deref().and_then(parse_alpha).unwrap_or(1.0);
        // Only a fully transparent base is worth the animation lookup; every
        // non-zero value must survive for group compositing in graphical paint.
        if base > 0.0 {
            return base.clamp(0.0, 1.0);
        }
        for (name, fill) in self.animations_of(id) {
            if matches!(fill.as_deref(), Some("forwards" | "both"))
                && let Some(end) = self
                    .style_index()
                    .keyframes
                    .get(&name)
                    .and_then(|rule| rule.end_value("opacity"))
                    .and_then(parse_alpha)
            {
                return end;
            }
        }
        base.clamp(0.0, 1.0)
    }

    /// The element's animations as `(name, fill-mode)` pairs. Both the
    /// longhands (`animation-name`/`animation-fill-mode`) and the `animation`
    /// shorthand are COMMA lists (css-animations-1 §4: one animation per
    /// comma-separated item; a too-short fill-mode list repeats). The old
    /// single-animation reader whitespace-split the whole shorthand, so
    /// `animation: fade-in 1s forwards, pulse 2s infinite` glommed
    /// `forwards,pulse` into one token and lost the name.
    fn animations_of(&self, id: NodeId) -> Vec<(String, Option<String>)> {
        let shorthand: Vec<(Option<String>, Option<String>)> = self
            .cascaded(id, "animation")
            .map(|s| {
                split_top_level(&s, ',')
                    .into_iter()
                    .map(parse_animation_segment)
                    .collect()
            })
            .unwrap_or_default();
        let names: Vec<Option<String>> = match self.cascaded(id, "animation-name") {
            Some(n) => n.split(',').map(|t| Some(t.trim().to_string())).collect(),
            None => shorthand.iter().map(|(n, _)| n.clone()).collect(),
        };
        let fills: Vec<Option<String>> = match self.cascaded(id, "animation-fill-mode") {
            Some(f) => f.split(',').map(|t| Some(t.trim().to_string())).collect(),
            None => shorthand.iter().map(|(_, f)| f.clone()).collect(),
        };
        names
            .into_iter()
            .enumerate()
            .filter_map(|(i, n)| {
                let n = n.filter(|n| !n.is_empty() && n != "none")?;
                let fill = if fills.is_empty() {
                    None
                } else {
                    fills[i % fills.len()].clone()
                };
                Some((n, fill))
            })
            .collect()
    }

    /// Resolve CSS Animations 1's comma-matched animation lists and attach
    /// the retained keyframe values used by graphical paint. Shorter
    /// longhand lists repeat to the `animation-name` list length (§3.2).
    pub(crate) fn css_animation_definitions(&self, id: NodeId) -> Vec<CssAnimationDefinition> {
        let shorthand = self
            .cascaded(id, "animation")
            .map(|value| {
                split_top_level(&value, ',')
                    .into_iter()
                    .map(parse_full_animation_segment)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let list = |property: &str| {
            self.cascaded(id, property)
                .map(|value| {
                    split_top_level(&value, ',')
                        .into_iter()
                        .map(|item| item.trim().to_string())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        };
        let names = {
            let values = list("animation-name");
            if values.is_empty() {
                shorthand
                    .iter()
                    .map(|animation| animation.name.clone().unwrap_or_else(|| "none".into()))
                    .collect::<Vec<_>>()
            } else {
                values
            }
        };
        let durations = list("animation-duration");
        let timings = list("animation-timing-function");
        let iterations = list("animation-iteration-count");
        let directions = list("animation-direction");
        let fills = list("animation-fill-mode");
        let delays = list("animation-delay");
        let play_states = list("animation-play-state");
        let style_index = self.style_index();

        names
            .iter()
            .enumerate()
            .filter_map(|(index, raw_name)| {
                let name = raw_name.trim().trim_matches(['\'', '"']).to_string();
                if name.is_empty() || name.eq_ignore_ascii_case("none") {
                    return None;
                }
                let shorthand = shorthand.get(index % shorthand.len().max(1));
                let duration_seconds = animation_list_value(&durations, index)
                    .and_then(parse_animation_time)
                    .or_else(|| shorthand.map(|animation| animation.duration_seconds))
                    .unwrap_or(0.0);
                if duration_seconds <= 0.0 {
                    return None;
                }
                let delay_seconds = animation_list_value(&delays, index)
                    .and_then(parse_animation_time)
                    .or_else(|| shorthand.map(|animation| animation.delay_seconds))
                    .unwrap_or(0.0);
                let iteration_count = animation_list_value(&iterations, index)
                    .and_then(parse_iteration_count)
                    .or_else(|| shorthand.and_then(|animation| animation.iteration_count))
                    .unwrap_or(Some(1.0));
                let direction = animation_list_value(&directions, index)
                    .map(str::to_ascii_lowercase)
                    .or_else(|| shorthand.map(|animation| animation.direction.clone()))
                    .unwrap_or_else(|| "normal".into());
                let fill_mode = animation_list_value(&fills, index)
                    .map(str::to_ascii_lowercase)
                    .or_else(|| shorthand.map(|animation| animation.fill_mode.clone()))
                    .unwrap_or_else(|| "none".into());
                let timing_function = animation_list_value(&timings, index)
                    .map(str::to_ascii_lowercase)
                    .or_else(|| shorthand.map(|animation| animation.timing_function.clone()))
                    .unwrap_or_else(|| "ease".into());
                let running = animation_list_value(&play_states, index)
                    .map(|value| !value.eq_ignore_ascii_case("paused"))
                    .or_else(|| shorthand.map(|animation| animation.running))
                    .unwrap_or(true);
                let rule = style_index.keyframes.get(&name)?;
                let tops = rule.properties.get("top");
                let transforms = rule.properties.get("transform");
                let mut offsets = tops
                    .into_iter()
                    .flatten()
                    .chain(transforms.into_iter().flatten())
                    .map(|frame| frame.offset)
                    .collect::<Vec<_>>();
                offsets.sort_by(f32::total_cmp);
                offsets.dedup();
                let keyframes = offsets
                    .into_iter()
                    .map(|offset| CssAnimationKeyframe {
                        offset,
                        top: tops
                            .and_then(|values| values.iter().find(|frame| frame.offset == offset))
                            .map(|frame| frame.value.clone()),
                        transform: transforms
                            .and_then(|values| values.iter().find(|frame| frame.offset == offset))
                            .map(|frame| frame.value.clone()),
                    })
                    .collect::<Vec<_>>();
                (!keyframes.is_empty()).then_some(CssAnimationDefinition {
                    name,
                    duration_seconds,
                    delay_seconds,
                    iteration_count,
                    direction,
                    fill_mode,
                    timing_function,
                    running,
                    keyframes,
                })
            })
            .collect()
    }

    /// The cascaded `display` value for an element (the mini-cascade
    /// winner), or `None` when no rule sets it. `hidden` attribute counts
    /// as `display:none`. Drives block/inline flow in the layout pass and
    /// is baked into the serialized HTML so the re-parsed layout arena
    /// sees the same computed display the engine did.
    pub fn computed_display(&self, id: NodeId) -> Option<String> {
        if self.attr(id, "hidden").is_some() {
            return Some("none".to_string());
        }
        let v = self.cascaded(id, "display")?;
        // CSS Variables L1 §3: custom properties are substituted at computed
        // value time. Keep this resolution in the canonical display path so
        // `display:var(--state)` has the same box-generation effect as its
        // substituted keyword (and invalid substitutions do not leak a box).
        let v = self.resolve_vars(id, &v);
        if v.trim().is_empty() {
            // An invalid-at-computed-value-time declaration uses display's
            // initial value, inline, rather than silently becoming the UA
            // default for the element's tag.
            return Some("inline".to_string());
        }
        match wide_keyword(&v) {
            // `display` doesn't inherit: `inherit` takes the parent's
            // computed display, `initial`/`unset` the initial value
            // (`inline`), `revert` the UA display table (`None` here — the
            // `effective_display` fallback).
            Some(WideKeyword::Inherit) => self.nodes[id]
                .parent
                .and_then(|p| self.effective_display(p)),
            Some(WideKeyword::Initial | WideKeyword::Unset) => Some("inline".to_string()),
            Some(WideKeyword::Revert) => None,
            None => Some(v),
        }
    }

    /// The EFFECTIVE `display` — the author's cascaded `display` if set, else
    /// the tag's UA-stylesheet default (so an un-styled `<table>` reports
    /// `"table"`, a `<tr>` `"table-row"`, a `<td>` `"table-cell"`). Unlike
    /// `computed_display` (cascade-only, `None` when no rule sets it) this is
    /// never `None` for a known element, so the layout can route the CSS table
    /// formatting context off a bare HTML `<table>` with no CSS at all.
    pub fn effective_display(&self, id: NodeId) -> Option<String> {
        if let Some(d) = self.computed_display(id) {
            return Some(d);
        }
        Some(ua_display(self.tag_name(id)?).to_string())
    }

    /// True when `id` must establish a table formatting context for its
    /// children EVEN THOUGH its own `display` is not `table`/`inline-table` —
    /// i.e. it holds misparented "proper table children" (table rows /
    /// row-groups) that, per CSS 2.1 §17.2.1 "generate missing parents", are
    /// wrapped in an anonymous `table` box ("a row group box is misparented
    /// when its parent is neither a 'table' box nor an 'inline-table' box").
    /// The common real-world trigger is markdown CSS (GitHub, many doc themes)
    /// forcing `display:block;width:max-content;overflow:auto` onto a `<table>`
    /// so a wide table scrolls horizontally: the `<thead>`/`<tbody>` keep their
    /// table displays, so the table still lays as a table. Without this the
    /// cells block-stack (every `<td>` on its own line). The layout routes such
    /// an element through `flow_table`, which collects rows from the children
    /// regardless of the element's own display — the element acts as the
    /// generated anonymous table.
    pub fn establishes_anonymous_table(&self, id: NodeId) -> bool {
        // An element already displayed as a table is handled by its own
        // display; a table-internal box (row/cell/group) is owned by its
        // ancestor table — neither needs an anonymous wrapper here.
        if let Some(d) = self.effective_display(id)
            && (d == "table" || d == "inline-table" || d.starts_with("table-"))
        {
            return false;
        }
        let is_row_ish = |c: NodeId| {
            matches!(
                self.effective_display(c).as_deref(),
                Some("table-row" | "table-row-group" | "table-header-group" | "table-footer-group")
            )
        };
        // Classify over the FLAT tree: a shadow host's misparented rows live in
        // its shadow (or are slotted). The common (non-host) path stays a lazy
        // light-child scan — no allocation on the hot `display_of` route.
        if self.shadow_root(id).is_some() {
            self.flat_children(id).into_iter().any(is_row_ish)
        } else {
            self.child_iter(id).any(is_row_ish)
        }
    }

    /// The cascaded value of any tracked property (the layout reads
    /// margin/padding/text-align through this), or `None` when unset.
    /// Author cascade only (no UA defaults, no inheritance) — the
    /// non-inherited box properties the layout reads directly, and the
    /// value the serializer bakes.
    pub fn computed_style(&self, id: NodeId, prop: &str) -> Option<String> {
        self.cascaded(id, prop)
            .and_then(|value| self.resolve_pending_shorthand(id, prop, &value))
    }

    /// True when an ATTRIBUTE mutation on `node` cannot change a single painted
    /// cell, so it need not be serialized or re-rendered. The case: `node` lies
    /// within an out-of-flow (`position:absolute`/`fixed`) subtree that PAINTS
    /// NOTHING — no text, no replaced element, no generated content, no drawn
    /// border. Out-of-flow ⇒ the change can't reflow in-flow painted content;
    /// paints-nothing ⇒ the box contributes no cells of its own (we render no
    /// color/background — the cyberpunk-monochrome deviation). The exact shape of
    /// Twitch's decorative `highlight__progress-bar` (an absolute, `z-index:-1`,
    /// textless bar whose width animates every frame, repainting nothing). Only
    /// an ATTR mutation qualifies — a childList/text change could add or remove
    /// painted text — so the caller gates on `DirtyKind::Attr`. CONSERVATIVE:
    /// an in-flow box (no positioned ancestor) or ANY painting descendant ⇒ NOT
    /// inert (we process the mutation). A wrong "inert" would leave a stale frame
    /// until the next real change; the checks below admit no false "inert".
    pub fn inert_positioned_attr(&self, node: NodeId) -> bool {
        let Some(oof) = self.nearest_out_of_flow(node) else {
            return false;
        };
        !self.subtree_paints(oof)
    }

    /// Nearest self-or-ancestor out of normal flow (`position:absolute`/`fixed`);
    /// `None` if the node is in flow to the root.
    fn nearest_out_of_flow(&self, node: NodeId) -> Option<NodeId> {
        let mut cur = Some(node);
        while let Some(id) = cur {
            if matches!(
                self.computed_style(id, "position").as_deref(),
                Some("absolute" | "fixed")
            ) {
                return Some(id);
            }
            cur = self.nodes[id].parent;
        }
        None
    }

    /// Whether the subtree rooted at `root` (inclusive) has any CSS-painted
    /// content: text, a replaced/control/marker element, generated content,
    /// border, or background. This is canonical paintability; a frontend's
    /// inability or choice not to draw a primitive cannot change box-tree
    /// construction. Early-exits on the first painting node. Generic
    /// containers (`div`/`span`/headings/…) paint only via their text children,
    /// which are checked; only the POSITIVE painting tags below count as
    /// self-painting, so an unlisted generic tag is correctly non-painting and an
    /// unlisted MEDIA tag would conservatively need adding (none known missing).
    fn subtree_paints(&self, root: NodeId) -> bool {
        // Replaced / form-control / marker-bearing tags that produce cells with
        // NO text of their own. Generic containers are deliberately absent.
        const PAINTS: &[&str] = &[
            "img", "svg", "canvas", "video", "iframe", "object", "embed", "picture", "input",
            "textarea", "select", "button", "progress", "meter", "hr", "li", "summary", "details",
            "audio", "math", "source", "track", "marquee",
        ];
        let mut stack = vec![root];
        while let Some(id) = stack.pop() {
            match &self.nodes[id].data {
                NodeData::Text(t) if !t.chars().all(char::is_whitespace) => return true,
                NodeData::Element { .. } => {
                    if self.tag_name(id).is_some_and(|t| PAINTS.contains(&t)) {
                        return true;
                    }
                    if self.pseudo_content(id, PseudoEl::Before).is_some()
                        || self.pseudo_content(id, PseudoEl::After).is_some()
                    {
                        return true;
                    }
                    let background = self
                        .computed_style(id, "background-image")
                        .is_some_and(|value| !matches!(value.trim(), "" | "none"))
                        || self
                            .computed_style(id, "background-color")
                            .is_some_and(|value| {
                                !matches!(value.trim(), "" | "transparent" | "rgba(0, 0, 0, 0)")
                            });
                    if self.has_drawn_border(id) || background {
                        return true;
                    }
                    let mut c = self.nodes[id].first_child;
                    while let Some(k) = c {
                        stack.push(k);
                        c = self.nodes[k].next_sibling;
                    }
                }
                _ => {}
            }
        }
        false
    }

    /// Any side has a non-zero border width (only consulted when borders render).
    fn has_drawn_border(&self, id: NodeId) -> bool {
        [
            "border-top-width",
            "border-right-width",
            "border-bottom-width",
            "border-left-width",
        ]
        .iter()
        .any(|p| {
            self.computed_style(id, p)
                .as_deref()
                .and_then(|v| crate::layout2::css_length_px(v, crate::layout2::Units::of(self, id)))
                .is_some_and(|px| px > 0.0)
        })
    }

    /// Whether `id` CLIPS `label` out of view: a definite `width` under
    /// horizontal `overflow:hidden/clip` narrower than the label's display
    /// width. The accessible-name fallback in `serialize_live_node` uses this to
    /// honor an author's icon-sized clip box — a control clipped to its icon
    /// never paints its `aria-label` (CSS Overflow §overflow). `width:auto`/`%`
    /// (`css_length_px` → `None`) is not a clip box, so the name shows.
    fn name_is_clipped_out(&self, id: NodeId, label: &str) -> bool {
        // Resolve `var()` — the live (pre-bake) cascade stores raw values, and a
        // styled-components control sizes its icon box with a custom property
        // (`width:var(--button-size-default)`). `computed_value_resolved`
        // substitutes it (`→ 3.2rem`); the raw `computed_style` would not, so the
        // clip would never be detected. Horizontal clip: the `overflow-x`
        // longhand else the `overflow` shorthand's first token (mirrors
        // `layout::axis_overflow`).
        let overflow_x = self.computed_value_resolved(id, "overflow-x").or_else(|| {
            self.computed_value_resolved(id, "overflow")
                .and_then(|s| s.split_whitespace().next().map(str::to_owned))
        });
        if !matches!(
            overflow_x.as_deref().map(str::trim),
            Some("hidden") | Some("clip")
        ) {
            return false;
        }
        let u = crate::layout2::Units::of(self, id);
        let Some(width_px) = self
            .computed_value_resolved(id, "width")
            .and_then(|v| crate::layout2::css_length_px(&v, u))
        else {
            return false;
        };
        let style = crate::text::TextStyle {
            family: self
                .computed_value(id, "font-family")
                .unwrap_or_else(|| String::from("sans-serif")),
            size: u.fs,
            weight: self
                .computed_value(id, "font-weight")
                .as_deref()
                .and_then(crate::layout2::css_font_weight)
                .unwrap_or(400.0),
            italic: self
                .computed_value(id, "font-style")
                .as_deref()
                .is_some_and(crate::layout2::css_is_italic),
            ..crate::text::TextStyle::default()
        };
        crate::text::shape(label, &style).advance > width_px
    }

    /// Whether `id` is a content-less full-area POSITIONED OVERLAY — a click
    /// SCRIM (a click-to-play / click-to-dismiss hit target) that fills its
    /// containing block. A browser paints nothing for it, so its accessible name
    /// must not be surfaced as a clickable HANDLE (the live serializer) or a
    /// LABEL (`layout::icon_only_label`): either would float phantom body text
    /// over the content the scrim covers. (Twitch's player carries a full-bleed
    /// `<button aria-label="Play" style="position:absolute;width:100%;
    /// height:100%">`.) Emptiness is the caller's precondition — both callers
    /// only reach here for a control with no text and no icon glyph. `var()` is
    /// resolved so a styled-components size still reads as `100%`.
    pub(crate) fn is_overlay_scrim(&self, id: NodeId) -> bool {
        let pos = self.computed_value_resolved(id, "position");
        if !matches!(pos.as_deref().map(str::trim), Some("absolute" | "fixed")) {
            return false;
        }
        let fills = |prop: &str, full: &[&str]| {
            self.computed_value_resolved(id, prop)
                .is_some_and(|v| full.contains(&v.trim()))
        };
        fills("width", &["100%", "100vw"]) && fills("height", &["100%", "100vh"])
    }

    /// The computed value of a property — the single inheritance authority.
    /// For an inherited property (per the registry) an element that doesn't
    /// set it resolves to the parent's computed value; otherwise this is the
    /// specified value (author cascade, else the UA default). The CSS-wide
    /// keywords (css-cascade-4 §7.3) resolve here: `inherit` takes the
    /// parent's computed value regardless of inheritedness, `initial` the
    /// property's initial value (`None` — the caller's default), `unset`
    /// whichever of those the property's inheritedness selects, and
    /// `revert`/`revert-layer` roll the author origin back to the UA origin.
    /// Memoized per epoch because the layout reads it per element.
    /// getComputedStyle and the layout's inherited-text reads both go through
    /// here, so a property inherits everywhere by being marked `inherited`
    /// once.
    pub fn computed_value(&self, id: NodeId, name: &str) -> Option<String> {
        let Some(idx) = prop_index(name) else {
            // Untracked: no UA default, no inheritance — author cascade.
            return self.cascaded(id, name);
        };
        let inherited = PROPS[idx].inherited;
        // HTML Rendering §15.5.13 supplies `overflow:hidden !important` for
        // marquee viewports. This UA-important declaration outranks author
        // overflow on every axis and keeps only the animated contents clipped.
        if self.tag_name(id) == Some("marquee")
            && matches!(name, "overflow" | "overflow-x" | "overflow-y")
        {
            return Some(String::from("hidden"));
        }
        if inherited && let Some(hit) = self.computed_cache_get(id, idx) {
            return hit;
        }
        let parent_computed = || {
            self.style_parent(id)
                .and_then(|p| self.computed_value(p, name))
        };
        let author = self
            .cascaded(id, name)
            .and_then(|value| self.resolve_pending_shorthand(id, name, &value));
        let v = match author.as_deref().and_then(wide_keyword) {
            Some(WideKeyword::Inherit) => parent_computed(),
            Some(WideKeyword::Initial) => None,
            Some(WideKeyword::Unset) => inherited.then(parent_computed).flatten(),
            // `revert` re-enters the defaulting chain below the author
            // origin: the UA origin, else the property's normal defaulting.
            Some(WideKeyword::Revert) => self
                .ua_default(id, name)
                .or_else(|| inherited.then(parent_computed).flatten()),
            None => author
                .or_else(|| self.ua_default(id, name))
                .or_else(|| inherited.then(parent_computed).flatten()),
        };
        if inherited {
            self.computed_cache_put(id, idx, v.clone());
        }
        v
    }

    /// `computed_value` with `var()` references substituted — what
    /// getComputedStyle exposes to JS. CSS variables resolve in computed
    /// style (`Supports.variable` sets `margin-right:var(--x)` and reads
    /// `marginRight` back as the substituted value). A no-op when the value
    /// has no `var(`.
    pub fn computed_value_resolved(&self, id: NodeId, name: &str) -> Option<String> {
        self.computed_value(id, name)
            .map(|v| self.resolve_vars(id, &v))
    }

    /// The resolved value exposed by `getComputedStyle()`.
    ///
    /// Internally, `computed_value` uses `None` as a compact signal for an
    /// undeclared property's initial value, because layout already supplies
    /// those defaults. CSSOM cannot expose that sentinel: CSS Cascade 5 §4
    /// assigns every property a specified and computed value, and CSSOM §9
    /// requires `getComputedStyle()` to return its resolved value. Materialize
    /// the initial values for the positional/sizing surface implemented by
    /// TRust so script cannot confuse `""` with a non-`auto` inset.
    pub fn cssom_resolved_value(&self, id: NodeId, name: &str) -> Option<String> {
        // CSS Fonts 4 §2.5 defines the computed value of `font-size` as an
        // absolute length. Do not expose the authored percentage/relative
        // token (or the internal `None` used for initial `medium`) through
        // CSSOM: percentages and font-relative units have already composed
        // numerically with inheritance in `font_px`.
        if name == "font-size" {
            return Some(format!("{}px", self.font_px(id)));
        }
        self.computed_value_resolved(id, name)
            .or_else(|| cssom_initial_value(name).map(str::to_string))
    }

    /// Whether text placed DIRECTLY in this element renders at zero font size —
    /// `Some(true)`/`Some(false)` when the element's own `font-size` is
    /// definitive, `None` to defer to the inherited value (so the layout, which
    /// threads inheritance down its formatting context, keeps the parent's
    /// answer). See [`classify_font_size_zero`].
    pub fn font_size_zero(&self, id: NodeId) -> Option<bool> {
        self.cascaded(id, "font-size")
            .as_deref()
            .and_then(classify_font_size_zero)
    }

    /// The document's root element (`<html>`) — the element `rem` units and
    /// `:root` refer to.
    pub(crate) fn document_element(&self) -> Option<NodeId> {
        self.child_iter(DOCUMENT)
            .find(|&c| self.tag_name(c).is_some())
    }

    /// The root element's computed `font-size` in CSS px — the `rem` basis.
    /// Twitch-idiom sites set `html { font-size: 62.5% }` so 1rem = 10px;
    /// resolving rem against a fixed 16px inflated every rem length 1.6×.
    pub(crate) fn root_font_px(&self) -> f32 {
        self.document_element()
            .map_or(FONT_SIZE_INITIAL, |r| self.font_px(r))
    }

    fn style_scope_root_element(&self, id: NodeId) -> Option<NodeId> {
        let scope = self.tree_scope(id);
        if matches!(self.tag_name(scope), Some("iframe" | "frame")) {
            self.child_iter(scope)
                .find(|&child| self.tag_name(child) == Some("html"))
        } else {
            self.document_element()
        }
    }

    /// The element's COMPUTED `font-size` in CSS px (CSS Fonts §6.1) — the
    /// `em` basis, and (on the root) the `rem` basis. Numeric composition,
    /// not string inheritance: the own declaration resolves against the
    /// PARENT's computed size (`%`/`em` multiply it, `rem` multiplies the
    /// root's, absolute units and keywords stand alone); with no declaration
    /// the UA factor for the tag applies (headings, `<small>`/`<big>`,
    /// `<sub>`/`<sup>`), else the parent's number is inherited as-is.
    /// Unresolvable declarations (`calc()`, dangling `var()`) inherit —
    /// fail-open, like the rest of the cascade. Memoized per epoch.
    pub(crate) fn font_px(&self, id: NodeId) -> f32 {
        if let Some(&v) = self.font_cache.borrow().get(id, self.epoch) {
            return v;
        }
        let parent_px = match self.style_parent(id) {
            Some(p) if p != DOCUMENT => self.font_px(p),
            _ => FONT_SIZE_INITIAL,
        };
        // `rem` on the root element itself resolves against the initial
        // value (a self-reference otherwise, per CSS Values §6.2.1).
        let scope_root = self.style_scope_root_element(id);
        let root_px = if Some(id) == scope_root {
            FONT_SIZE_INITIAL
        } else {
            scope_root.map_or(FONT_SIZE_INITIAL, |root| self.font_px(root))
        };
        let v = self
            .cascaded(id, "font-size")
            .map(|raw| self.resolve_vars(id, &raw))
            .and_then(|decl| font_size_px(&decl, parent_px, root_px))
            .or_else(|| {
                self.tag_name(id)
                    .and_then(ua_font_factor)
                    .map(|f| f * parent_px)
            })
            .unwrap_or(parent_px);
        self.font_cache.borrow_mut().put(id, self.epoch, v);
        v
    }

    fn computed_cache_get(&self, id: NodeId, idx: usize) -> Option<Option<String>> {
        let cache = self.computed_cache.borrow();
        (cache.0 == self.epoch)
            .then(|| cache.1.get(&(id, idx)).cloned())
            .flatten()
    }

    fn computed_cache_put(&self, id: NodeId, idx: usize, v: Option<String>) {
        let mut cache = self.computed_cache.borrow_mut();
        if cache.0 != self.epoch {
            cache.0 = self.epoch;
            cache.1.clear();
        }
        cache.1.insert((id, idx), v);
    }

    /// The user-agent default stylesheet, for the inherited properties the
    /// layout used to apply as hardcoded tag behavior: `<b>/<strong>` bold,
    /// `<i>/<em>` italic, `<pre>` pre white-space, and the list marker style
    /// (`<ul>` disc/circle/square by nesting depth, `<ol>` decimal or its
    /// `type` attribute). Non-inherited tag defaults stay where they belong:
    /// block/inline display (the layout's tag tables), `<a>` linking, heading
    /// sizing, and `<u>/<s>` decoration (`text_decoration`, which accumulates
    /// rather than inherits).
    fn ua_default(&self, id: NodeId, name: &str) -> Option<String> {
        let tag = self.tag_name(id)?;
        let v = match name {
            "font-weight" if matches!(tag, "b" | "strong") => "bold",
            "font-style" if matches!(tag, "i" | "em") => "italic",
            "white-space" if tag == "pre" => "pre",
            "list-style-type" if tag == "ul" => self.ul_marker_default(id),
            "list-style-type" if tag == "ol" => match self.attr(id, "type") {
                Some("a") => "lower-alpha",
                Some("A") => "upper-alpha",
                Some("i") => "lower-roman",
                Some("I") => "upper-roman",
                _ => "decimal",
            },
            "display" => ua_display(tag),
            // WHATWG HTML Rendering §15.3.10: these widgets use border-box
            // sizing in the UA origin. An authored 30px button therefore
            // remains 30px including its padding and border.
            "box-sizing" if tag == "button" || tag == "select" => "border-box",
            "box-sizing"
                if tag == "input"
                    && matches!(
                        self.input_type(id).as_str(),
                        "radio" | "checkbox" | "reset" | "button" | "submit" | "color" | "search"
                    ) =>
            {
                "border-box"
            }
            _ => return None,
        };
        Some(v.to_string())
    }

    /// The default bullet for a `<ul>` by nesting depth, matching browsers:
    /// disc at the top level, circle one deep, square thereafter. An inner
    /// list inherits this through `computed_value`, so authors can still
    /// override it anywhere.
    fn ul_marker_default(&self, id: NodeId) -> &'static str {
        let mut depth = 0u32;
        let mut cur = Some(id);
        while let Some(c) = cur {
            if self.tag_name(c) == Some("ul") {
                depth += 1;
            }
            cur = self.nodes[c].parent;
        }
        match depth {
            0 | 1 => "disc",
            2 => "circle",
            _ => "square",
        }
    }

    /// The accumulated `(underline, line-through)` for an element's text.
    ///
    /// CSS Text Decoration 3 §2.1 says line decorations are not inherited:
    /// they propagate through the box tree and accumulate with decorations
    /// established by descendants. In particular, `none` establishes no line;
    /// it does not inhibit a line propagated from an ancestor. The memoized
    /// parent recursion implements that accumulation for the element ancestry
    /// represented by the current layout.
    pub fn text_decoration(&self, id: NodeId) -> (bool, bool) {
        if let Some(&hit) = self.decoration_cache.borrow().get(id, self.epoch) {
            return hit;
        }

        let (mut underline, mut strike) = self
            .parent_composed(id)
            .map_or((false, false), |parent| self.text_decoration(parent));

        // An author declaration on the same element outranks the HTML UA
        // decoration for <u>/<s> and friends. `none` therefore suppresses that
        // element's UA line while leaving the parent's propagated lines alone.
        if let Some(value) = self
            .cascaded(id, "text-decoration-line")
            .or_else(|| self.cascaded(id, "text-decoration"))
        {
            if !value.split_whitespace().any(|token| token == "none") {
                underline |= value.split_whitespace().any(|token| token == "underline");
                strike |= value
                    .split_whitespace()
                    .any(|token| token == "line-through");
            }
        } else {
            match self.tag_name(id) {
                Some("u" | "ins") => underline = true,
                Some("s" | "strike" | "del") => strike = true,
                _ => {}
            }
        }

        let result = (underline, strike);
        self.decoration_cache
            .borrow_mut()
            .put(id, self.epoch, result);
        result
    }

    /// The author-cascade winner for one property on the element itself:
    /// one hash lookup into the element's per-epoch winner maps. Inline
    /// styles beat tree rules, `!important`/layers/specificity/source order
    /// resolved by `CascadeKey` when the maps are built.
    fn cascaded(&self, id: NodeId, prop: &str) -> Option<String> {
        self.cascaded_maps(id).elem.get(prop).cloned()
    }

    /// Whether the author cascade supplies this property on the element.
    ///
    /// This differs deliberately from [`computed_value`](Self::computed_value):
    /// an explicit author `height:auto` computes to the same value as the
    /// property's initial value, but still outranks an HTML `height` attribute's
    /// presentational hint. Replaced-element sizing needs that distinction when
    /// it inserts HTML dimension attributes at their specified cascade origin.
    pub(crate) fn author_declares(&self, id: NodeId, prop: &str) -> bool {
        self.cascaded_maps(id).elem.contains_key(prop)
    }

    /// The element's full cascade winner maps for the current epoch, built
    /// on the first read of ANY of its properties (one pass over its author
    /// sources), then shared by every further read.
    fn cascaded_maps(&self, id: NodeId) -> std::rc::Rc<CascadedMaps> {
        if let Some(hit) = self.cascaded_cache.borrow().get(id, self.epoch) {
            return hit.clone();
        }
        let _t = casc_diag_on().then(std::time::Instant::now);
        let maps = std::rc::Rc::new(self.build_cascaded_maps(id));
        if let Some(t) = _t {
            let us = t.elapsed().as_micros() as u64;
            casc_bump(|d| d.cascaded_us += us);
        }
        self.cascaded_cache
            .borrow_mut()
            .put(id, self.epoch, maps.clone());
        maps
    }

    /// ONE pass over the element's author sources — its inline `style`
    /// (parsed once, where it used to be re-parsed per property read), its
    /// matched rules (each rule's declarations land in the map for the box
    /// the rule targets: the element, or its `::before`/`::after`), and its
    /// shadow root's `:host` rules — resolving the cascade winner for EVERY
    /// declared property at once. Winner selection is identical to the old
    /// per-property scan: the same `CascadeKey` per declaration,
    /// lexicographic max, later-wins on ties. Untracked properties present
    /// in the INLINE style are kept (sheet parsing already filtered its
    /// side): getComputedStyle of an inline-only property reads through
    /// here, matching real-browser behavior for the properties we don't
    /// track.
    fn build_cascaded_maps(&self, id: NodeId) -> CascadedMaps {
        type Winners = FxHashMap<String, (CascadeKey, String)>;
        // Clone only on first sight or a WIN — a losing declaration costs a
        // lookup and a key compare, never an allocation.
        fn consider_into(map: &mut Winners, prop: &str, key: CascadeKey, value: &str) {
            match map.get_mut(prop) {
                Some(slot) => {
                    if key >= slot.0 {
                        *slot = (key, value.to_string());
                    }
                }
                None => {
                    map.insert(prop.to_string(), (key, value.to_string()));
                }
            }
        }
        let mut elem = Winners::default();
        let mut before = Winners::default();
        let mut after = Winners::default();
        if let Some(style) = self.attr(id, "style") {
            for decl in style.split(';') {
                let Some((k, v, important)) = parse_decl(decl) else {
                    continue;
                };
                for (pk, pv) in expand_box_shorthand(&k, &v) {
                    // Element-attached: the inline flag outranks the layer
                    // component, so the (unlayered) encoding is inert. This
                    // declaration belongs to the element's OUTER tree context.
                    // (Inline styles can't target a pseudo-element.)
                    consider_into(
                        &mut elem,
                        &pk,
                        (
                            important,
                            !important,
                            true,
                            encode_layer(&[], important),
                            (0, 0, 0),
                            usize::MAX,
                        ),
                        &pv,
                    );
                }
            }
        }
        let index = self.style_index();
        if let Some(rules) = index.scopes.get(&self.tree_scope(id)) {
            for &ri in self.matched_rules(id).iter() {
                let r = &rules[ri as usize];
                // A `div::before{…}` rule targets the generated box, not
                // the element — its winners land in that box's own map.
                let target = match rule_pseudo(r) {
                    None => &mut elem,
                    Some(PseudoEl::Before) => &mut before,
                    Some(PseudoEl::After) => &mut after,
                };
                for (pk, (imp, v)) in &r.decls {
                    consider_into(
                        target,
                        pk,
                        (
                            *imp,
                            !*imp,
                            false,
                            r.layer_key(*imp),
                            r.specificity,
                            r.order,
                        ),
                        v,
                    );
                }
            }
        }
        // CSS Shadow 1 §3.2.4: `::slotted()` is an alias for the flattened
        // element assigned to a slot; it does not create a box of its own.
        // The light-DOM element still receives the declaration in its own
        // cascade map, with the shadow-tree encapsulation context preserved.
        for &(scope, ri) in &index.slotted_rules {
            let Some(rules) = index.scopes.get(&scope) else {
                continue;
            };
            let r = &rules[ri as usize];
            if !self.slotted_rule_matches(id, scope, r) {
                continue;
            }
            for (pk, (imp, v)) in &r.decls {
                consider_into(
                    &mut elem,
                    pk,
                    (*imp, *imp, false, r.layer_key(*imp), r.specificity, r.order),
                    v,
                );
            }
        }
        // `:host` rules: a shadow root's own stylesheet styles ITS host element
        // (CSS Scoping §3.3) via `:host`/`:host(<compound>)`. The host lives in
        // the parent tree, so these aren't in its matched set — pull them from
        // the host's shadow scope. (`<style>` baked into the serialized HTML is
        // dropped, so this is the JS-pipeline adoptedStyleSheets path.)
        if let Some(&sr) = self.shadow_roots.get(&id)
            && let Some(rules) = index.scopes.get(&sr)
        {
            for r in rules {
                if rule_pseudo(r).is_some() || !self.host_rule_matches(id, r) {
                    continue;
                }
                for (pk, (imp, v)) in &r.decls {
                    consider_into(
                        &mut elem,
                        pk,
                        (*imp, *imp, false, r.layer_key(*imp), r.specificity, r.order),
                        v,
                    );
                }
            }
        }
        let strip = |m: Winners| m.into_iter().map(|(k, (_, v))| (k, v)).collect();
        CascadedMaps {
            elem: strip(elem),
            before: strip(before),
            after: strip(after),
        }
    }

    /// Match a `::slotted(<compound-selector>)` rule against a light-DOM
    /// element. The pseudo-element's originating element is the slot; a
    /// selector prefix (for example `slot::slotted(*)`) therefore matches the
    /// slot, while the argument matches the flattened assigned element.
    fn slotted_rule_matches(&self, id: NodeId, scope: NodeId, rule: &StyleRule) -> bool {
        let parts = &rule.selector.0;
        let Some((_, subject)) = parts.last() else {
            return false;
        };
        let Some(inner) = subject.slotted.as_deref() else {
            return false;
        };
        if !self.matches_compound(id, inner, None) {
            return false;
        }
        self.descendants(scope).any(|slot| {
            self.tag_name(slot) == Some("slot")
                && self.flat_assigned_slot_nodes(slot).contains(&id)
                && (parts.len() == 1 || self.matches_complex(slot, &parts[..parts.len() - 1], None))
        })
    }

    /// Whether a shadow-scope rule is a `:host`/`:host(<compound>)` rule that
    /// matches its host element. Only a single-compound selector is treated as
    /// host-matching (`:host`, `:host(.x)`); a `:host(.x) .y` rule targets
    /// shadow content and is matched against that content in its own scope.
    fn host_rule_matches(&self, host: NodeId, r: &StyleRule) -> bool {
        let parts = &r.selector.0;
        let [(_, c)] = parts.as_slice() else {
            return false;
        };
        c.host
            && c.host_inner
                .as_ref()
                .is_none_or(|inner| self.matches_compound(host, inner, None))
    }

    /// An element's computed value for a custom property (`--foo`): its own
    /// cascaded declaration, else inherited from the composed parent (custom
    /// properties inherit). `None` if undefined up the whole chain.
    fn custom_prop(&self, id: NodeId, name: &str) -> Option<String> {
        if let Some(hit) = {
            let cache = self.custom_prop_cache.borrow();
            (cache.0 == self.epoch)
                .then(|| cache.1.get(&id).and_then(|node| node.get(name)).cloned())
                .flatten()
        } {
            return hit;
        }
        let value = self.cascaded(id, name).or_else(|| {
            self.style_parent(id)
                .and_then(|parent| self.custom_prop(parent, name))
        });
        let mut cache = self.custom_prop_cache.borrow_mut();
        if cache.0 != self.epoch {
            cache.0 = self.epoch;
            cache.1.clear();
        }
        cache
            .1
            .entry(id)
            .or_default()
            .insert(name.to_owned(), value.clone());
        value
    }

    /// Substitute `var(--name, fallback)` references in a CSS value to a plain
    /// string — the public entry. Balanced-paren aware so `var()` inside
    /// `calc()` and nested `var()` both resolve. A value that is *invalid at
    /// computed-value time* (an undefined reference with no fallback, or one
    /// that closes a dependency cycle) yields the empty string, which the
    /// callers treat as unresolvable (skip baking it / expose `""`).
    fn resolve_vars(&self, id: NodeId, value: &str) -> String {
        self.substitute_vars(id, value, &mut Vec::new())
            .unwrap_or_default()
    }

    /// Resolve a shorthand that contains `var()` only after custom-property
    /// substitution, then extract this longhand from the resulting shorthand.
    /// CSS Variables §3 makes such shorthands pending-substitution values at
    /// parse time: expanding `background:var(--nav-bg)` before substitution
    /// incorrectly resets `background-color` to transparent.
    fn resolve_pending_shorthand(&self, id: NodeId, name: &str, value: &str) -> Option<String> {
        let Some(raw) = value.strip_prefix(PENDING_BACKGROUND_SHORTHAND) else {
            return Some(value.to_owned());
        };
        let substituted = self.substitute_vars(id, raw, &mut Vec::new())?;
        expand_background(&substituted)
            .into_iter()
            .find_map(|(longhand, value)| (longhand == name).then_some(value))
    }

    /// The computed value of custom property `name` on `id`, with its own
    /// `var()` references substituted (CSS Variables L1). `active` is the set of
    /// custom properties currently being resolved further up the call chain —
    /// the resolution stack used to detect dependency cycles ("Resolving
    /// Dependency Cycles"): a name already on it is a cycle, so we never recurse
    /// into a property that is its own (in)direct ancestor and the walk always
    /// terminates (each step either resolves a literal/fallback or pushes a new,
    /// finite custom-property name).
    fn resolve_custom_prop(&self, id: NodeId, name: &str, active: &mut Vec<String>) -> VarResult {
        if active.iter().any(|n| n == name) {
            return VarResult::Cycle;
        }
        let Some(raw) = self.custom_prop(id, name) else {
            return VarResult::Undefined; // unset up the whole chain
        };
        active.push(name.to_owned());
        let resolved = self.substitute_vars(id, &raw, active);
        active.pop();
        // A `None` here means the property's own value failed to substitute (it
        // hit a cycle or a fallback-less undefined reference): it is invalid at
        // computed-value time, i.e. the guaranteed-invalid value, which a
        // referencing `var()` treats like an undefined property — its fallback
        // applies. (The fallback within *this* property's own value was already
        // honored or correctly skipped while substituting `raw`.)
        match resolved {
            Some(v) => VarResult::Resolved(v),
            None => VarResult::Undefined,
        }
    }

    /// Substitute every `var(--name, fallback)` in `value` against `id`'s
    /// computed custom properties. Returns `None` when the value is *invalid at
    /// computed-value time* — a `var()` references a guaranteed-invalid/undefined
    /// property with no usable fallback, or it closes a dependency cycle.
    fn substitute_vars(&self, id: NodeId, value: &str, active: &mut Vec<String>) -> Option<String> {
        if find_var_function(value).is_none() {
            return Some(value.to_owned());
        }
        let mut out = String::new();
        let mut rest = value;
        let mut guard = 0;
        while let Some(pos) = find_var_function(rest) {
            guard += 1;
            if guard > 64 {
                out.push_str(rest);
                return Some(out);
            }
            out.push_str(&rest[..pos]);
            let after = &rest[pos + 4..];
            // Find the `)` that closes this `var(`.
            let mut depth = 1usize;
            let mut end = None;
            for (i, c) in after.char_indices() {
                match c {
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            end = Some(i);
                            break;
                        }
                    }
                    _ => {}
                }
            }
            let Some(end) = end else {
                out.push_str(&rest[pos..]); // unbalanced: leave as-is
                return Some(out);
            };
            let inner = &after[..end];
            let (name, fallback) = match inner.split_once(',') {
                Some((n, f)) => (n.trim(), Some(f.trim())),
                None => (inner.trim(), None),
            };
            match self.resolve_custom_prop(id, name, active) {
                VarResult::Resolved(v) => out.push_str(&v),
                // A dependency cycle: every property in it is invalid at
                // computed-value time, and — unlike a plain undefined reference
                // — its fallback is NOT consulted (CSS Variables L1 §3). The
                // whole value is invalid.
                VarResult::Cycle => return None,
                // Guaranteed-invalid / undefined target: substitute the
                // fallback if present, else this value is invalid.
                VarResult::Undefined => {
                    let fallback = fallback?;
                    out.push_str(&self.substitute_vars(id, fallback, active)?);
                }
            }
            rest = &after[end + 1..];
        }
        out.push_str(rest);
        Some(out)
    }

    /// The resolved `content` text for an element's `::before`/`::after`
    /// box, or `None` when no rule sets it (or it resolves to `none`/an
    /// unsupported value like `counter()`). Reads the pseudo's bucket of
    /// the element's winner maps (inline styles can't target a pseudo).
    pub fn pseudo_content(&self, id: NodeId, which: PseudoEl) -> Option<String> {
        let raw = self.cascaded_maps(id).pseudo(which).get("content")?.clone();
        // A hidden pseudo-element generates NO rendered content here — a
        // deliberate TERMINAL DEVIATION, distinct from Phase 2's element-level
        // `visibility:hidden` (which reserves a blank box). The width-reservation
        // idiom `[data-content]::before{content:attr(data-content);
        // font-weight:bold;visibility:hidden}` (Primer's UnderlineNav tabs, many
        // tab/button components) paints a hidden BOLD copy of the label ONLY to
        // reserve the selected (bold) pixel width, so switching a tab to bold
        // doesn't reflow. In a cell grid BOLD IS THE SAME WIDTH as normal, so the
        // reservation is vacuous: reserving its blank width would just append a
        // blank copy after the real label (bloating every tab), and rendering it
        // gives the doubled "CodeCode". Dropping it yields the correct terminal
        // result ("Code Issues PullRequests") with no reflow to prevent. (A
        // `visibility:hidden` ELEMENT still reserves its box — see
        // `visibility_hidden`; only the pseudo SIZER idiom drops, since its whole
        // purpose is pixel-width reflow-avoidance a cell grid doesn't have.)
        // `display:none` on a pseudo generates no box at all, likewise dropped.
        if matches!(
            self.pseudo_style(id, which, "visibility").as_deref(),
            Some("hidden" | "collapse")
        ) || self.pseudo_style(id, which, "display").as_deref() == Some("none")
        {
            return None;
        }
        // CSS Variables 1 §3 substitutes var() at computed-value time for
        // every ordinary property, including `content`. Font Awesome and many
        // icon systems keep the glyph in a custom property on the originating
        // element (`content:var(--icon)/""`). Parsing the specified token stream
        // directly would discard that otherwise-valid generated content.
        let resolved = self.resolve_vars(id, &raw);
        self.parse_content_value(id, &resolved)
    }

    /// The cascade-winning value of `prop` on `id`'s `::before`/`::after`
    /// pseudo-element, or `None` if no matching rule sets it. One hash
    /// lookup into the pseudo's bucket of the element's winner maps.
    pub fn pseudo_style(&self, id: NodeId, which: PseudoEl, prop: &str) -> Option<String> {
        self.cascaded_maps(id).pseudo(which).get(prop).cloned()
    }

    /// The layout-facing computed value on a generated `::before`/`::after`
    /// box. Tree-abiding pseudo-elements inherit from their originating
    /// element (CSS Pseudo 4 §4), while non-inherited properties take their
    /// initial value unless a pseudo rule declares them. During live rendering
    /// the resident arena's stylesheets are baked away; in that re-parsed arena
    /// the equivalent declarations ride `data-trust-*-style`.
    pub(crate) fn pseudo_layout_value(
        &self,
        id: NodeId,
        which: PseudoEl,
        prop: &str,
    ) -> Option<String> {
        let direct = self
            .pseudo_style(id, which, prop)
            .or_else(|| self.baked_pseudo_value(id, which, prop));
        let inherited = prop_index(prop).is_some_and(|i| PROPS[i].inherited);
        let inherited_value = || self.computed_value_resolved(id, prop);
        match direct.as_deref().and_then(wide_keyword) {
            Some(WideKeyword::Inherit) => inherited_value(),
            Some(WideKeyword::Initial) => None,
            Some(WideKeyword::Unset) => inherited.then(inherited_value).flatten(),
            Some(WideKeyword::Revert) => inherited.then(inherited_value).flatten(),
            None => direct
                .map(|value| self.resolve_vars(id, &value))
                .or_else(|| inherited.then(inherited_value).flatten()),
        }
    }

    /// Recover one declaration from the serialized pseudo style. The value was
    /// produced by `baked_pseudo_style` from the same parsed declaration model;
    /// parsing it through `parse_decl`/`expand_box_shorthand` keeps semicolons,
    /// `!important`, and shorthand expansion consistent with ordinary inline
    /// style instead of inventing a second CSS parser.
    fn baked_pseudo_value(&self, id: NodeId, which: PseudoEl, prop: &str) -> Option<String> {
        let attr = match which {
            PseudoEl::Before => "data-trust-before-style",
            PseudoEl::After => "data-trust-after-style",
        };
        let mut found = None;
        for decl in self.attr(id, attr)?.split(';') {
            let Some((name, value, _important)) = parse_decl(decl) else {
                continue;
            };
            for (name, value) in expand_box_shorthand(&name, &value) {
                if name == prop {
                    found = Some(value);
                }
            }
        }
        found
    }

    /// Whether `id` carries the clearfix idiom — a `::before`/`::after`
    /// pseudo-element that `clear`s floats (`.clearfix`, Bootstrap's `.row`,
    /// `.group`, …). Such a block CONTAINS its descendant floats (the universal
    /// pre-flexbox containment pattern: `::after{content:"";clear:both}`): the
    /// generated pseudo is the final in-flow clearing child. Without it a
    /// float grid leaks past its row and the next section paints on top of it.
    pub fn has_clearing_pseudo(&self, id: NodeId) -> bool {
        // The baked marker (set by the serializer when the real CSS was still
        // in scope) — the layout re-parses without the `::after{clear}` rule.
        if self.attr(id, "data-trust-clearfix").is_some() {
            return true;
        }
        [PseudoEl::Before, PseudoEl::After].into_iter().any(|p| {
            // `content:normal|none` (or display:none) generates no pseudo box,
            // so its `clear` declaration cannot affect layout (CSS Pseudo 4
            // §4.1 / CSS Content 3 §1). An empty STRING still generates
            // a box — precisely the standard clearfix — even though it has no
            // text for `pseudo_content` to return.
            self.pseudo_style(id, p, "content").is_some_and(|v| {
                let v = v.trim();
                !v.is_empty() && !matches!(v, "normal" | "none")
            }) && self.pseudo_style(id, p, "display").as_deref() != Some("none")
                && self.pseudo_style(id, p, "clear").is_some_and(|v| {
                    // css-logical-1 flow-relative values included (LTR-only:
                    // inline-start = left, inline-end = right), matching layout's
                    // `float_side`/`clear_floats`.
                    matches!(
                        v.trim(),
                        "both" | "left" | "right" | "inline-start" | "inline-end"
                    )
                })
        })
    }

    /// Resolve a `content` value to display text. The value is a
    /// whitespace-separated CONCATENATION of components (CSS2 §12.2 /
    /// css-content-3): quoted strings (with CSS `\HEX`/`\c` escapes) and
    /// `attr(name)` → the element's attribute (empty when absent) are
    /// joined; `none`/`normal` → `None`; a value containing any component
    /// we can't resolve (`counter()`, `url()`, quote keywords) is dropped
    /// whole. The old single-component reader mangled the common
    /// `content:"(" attr(data-n) ")"` decoration idiom.
    fn parse_content_value(&self, id: NodeId, raw: &str) -> Option<String> {
        // CSS Content 3 §1.2 puts optional speech alternative text after a
        // top-level slash. It is not part of the visual content list. Keep a
        // slash inside url()/attr()/a quoted string intact.
        let visual = split_top_level_slash(raw).map_or(raw, |(visual, _alt)| visual);
        let v = visual.trim();
        if v.is_empty() || v == "none" || v == "normal" {
            return None;
        }
        let mut out = String::new();
        for tok in split_top_level(v, ' ') {
            let tok = tok.trim();
            if tok.is_empty() {
                continue;
            }
            if let Some(s) = unquote_css(tok) {
                out.push_str(&s);
                continue;
            }
            if let Some(inner) = tok.strip_prefix("attr(").and_then(|r| r.strip_suffix(')')) {
                if let Some(a) = self.attr(id, inner.trim()) {
                    out.push_str(a);
                }
                continue;
            }
            return None;
        }
        // An empty <string> is still a valid content list. CSS Content 3 §2
        // distinguishes it from `none`: `content:""` creates an empty but fully
        // styleable pseudo box (the ubiquitous percentage-padding aspect-ratio
        // box), while `none` inhibits pseudo-element generation altogether.
        Some(out)
    }

    /// The root of a node's tree: DOCUMENT for the light DOM, the
    /// shadow fragment for shadow content. An element consults only its
    /// own scope's sheets (selector matching can't cross the boundary
    /// either — ancestor walks stop at fragment roots).
    fn tree_scope(&self, id: NodeId) -> NodeId {
        let mut cur = id;
        while let Some(p) = self.nodes[cur].parent {
            // A nested navigable's active document is a distinct tree scope.
            // TRust retains it below the iframe owner in one arena, so use the
            // owner as the scope key without making the owner itself part of
            // the child document's author cascade.
            if matches!(self.tag_name(p), Some("iframe" | "frame")) && self.frame_body(p).is_some()
            {
                return p;
            }
            cur = p;
        }
        cur
    }

    /// CSS parent without crossing from a child document element to its
    /// embedding iframe. Shadow-tree inheritance still uses `parent_flat()`
    /// and crosses to its host; separate Document trees do not inherit.
    fn style_parent(&self, id: NodeId) -> Option<NodeId> {
        let parent = self.parent_flat(id)?;
        if matches!(self.tag_name(parent), Some("iframe" | "frame"))
            && self.frame_body(parent).is_some()
        {
            None
        } else {
            Some(parent)
        }
    }

    /// The parsed style index, built on first use after a STYLE-epoch
    /// advance and shared until the next one. Keyed on `style_epoch`, NOT
    /// the main mutation epoch: content mutations invalidate the per-element
    /// match/cascade memos (they must — matching depends on attributes and
    /// tree shape) but never this parse, so a live page's churn re-MATCHES
    /// against a retained index instead of re-PARSING every sheet.
    fn style_index(&self) -> std::rc::Rc<StyleIndex> {
        let mut cache = self.style_cache.borrow_mut();
        if let Some((epoch, idx)) = cache.as_ref()
            && *epoch == self.style_epoch
        {
            return idx.clone();
        }
        let t = std::time::Instant::now();
        let built = self.build_style_index();
        let rules = built.scopes.values().map(|v| v.len() as u64).sum::<u64>();
        let us = t.elapsed().as_micros() as u64;
        casc_bump(|d| {
            d.style_index_us += us;
            d.style_index_builds += 1;
            d.rules += rules;
        });
        let idx = std::rc::Rc::new(built);
        *cache = Some((self.style_epoch, idx.clone()));
        idx
    }

    fn build_style_index(&self) -> StyleIndex {
        let mut index = StyleIndex::default();
        let mut order = 0;
        let media = MediaEnvironment {
            viewport: self.viewport_px,
            density: self.device_pixel_ratio,
        };
        // Cascade layers are scoped like the rules themselves ("scoped to
        // their origin and context" — css-cascade-5): one registry per tree
        // scope, shared across every sheet of that scope so `@layer` names
        // resolve to the same layer order document-wide.
        let mut layer_regs: std::collections::HashMap<NodeId, LayerRegistry> =
            std::collections::HashMap::new();
        for id in self.composed_descendants(DOCUMENT) {
            let css: Cow<str> = match self.tag_name(id) {
                Some("style") => Cow::Owned(self.text_content(id)),
                Some("link") => match self.external_sheets.get(&id) {
                    Some(css) => Cow::Borrowed(css.as_str()),
                    None => continue,
                },
                _ => continue,
            };
            let scope = self.tree_scope(id);
            parse_sheet(
                &css,
                &mut order,
                index.scopes.entry(scope).or_default(),
                &mut index.keyframes,
                media,
                layer_regs.entry(scope).or_default(),
                "",
            );
        }
        // Adopted sheets cascade after their scope's tree sheets (their
        // order values are necessarily higher); cross-scope order is
        // moot — an element only reads its own scope. Sort for
        // determinism across HashMap iteration.
        let mut adopted: Vec<_> = self.adopted_styles.iter().collect();
        adopted.sort_by_key(|(scope, _)| **scope);
        for (scope, css) in adopted {
            parse_sheet(
                css,
                &mut order,
                index.scopes.entry(*scope).or_default(),
                &mut index.keyframes,
                media,
                layer_regs.entry(*scope).or_default(),
                "",
            );
        }
        index.has_opacity = index
            .scopes
            .values()
            .flatten()
            .any(|r| r.decls.iter().any(|(k, _)| k == "opacity"));
        // The hover probes: only rules that could change what we paint under a
        // moved hover chain. Graphical paint properties such as `color` are in
        // the tracked registry too; they invalidate retained paint even when
        // terminal-cell geometry happens not to change.
        index.hover_probes = index
            .scopes
            .values()
            .flatten()
            .filter(|r| rule_affects_render(r))
            .flat_map(hover_probes_of)
            .collect();
        index.hover_buckets = index
            .scopes
            .iter()
            .map(|(scope, rules)| {
                (
                    *scope,
                    RuleBuckets::build_where(rules, |rule| {
                        rule_affects_render(rule) && rule_uses_hover(rule)
                    }),
                )
            })
            .collect();
        index.boxless_content_may_escape = index
            .scopes
            .values()
            .flatten()
            .any(|rule| complex_has_boxless_content_dependency(&rule.selector));
        // Build the rightmost-key buckets so `matched_rules` tests only
        // candidate rules per element instead of the whole scope.
        index.buckets = index
            .scopes
            .iter()
            .map(|(scope, rules)| (*scope, RuleBuckets::build(rules)))
            .collect();
        // CSS Shadow 1 §3.2.4: `::slotted()` rules are evaluated in a
        // shadow-tree stylesheet but apply to the flattened nodes assigned to
        // the originating slot, not to an element in that stylesheet's own
        // tree. Keep a sparse cross-tree index so ordinary matched-rule
        // lookup remains scoped and hot.
        index.slotted_rules = index
            .scopes
            .iter()
            .flat_map(|(scope, rules)| {
                rules.iter().enumerate().filter_map(move |(ri, rule)| {
                    rule.selector
                        .0
                        .last()
                        .and_then(|(_, c)| c.slotted.as_ref())
                        .map(|_| (*scope, ri as u32))
                })
            })
            .collect();
        index
    }

    /// The author rules (by index into the element's tree-scope rule vec) whose
    /// selectors match `id`, in the cascade context (no `:scope` root).
    /// Memoized per epoch: matching is the cascade's hot cost and the layout /
    /// serializer read 30+ properties per element, so doing it once and reusing
    /// the list is what keeps a CSS-heavy page (GitHub: ~8k rules) from going
    /// O(elements × rules × props). Candidate rules come from the rightmost-key
    /// buckets; only those are full-matched.
    fn matched_rules(&self, id: NodeId) -> std::rc::Rc<Vec<u32>> {
        if let Some(hit) = self.matched_cache.borrow().get(id, self.epoch) {
            return hit.clone();
        }
        let started = casc_diag_on().then(std::time::Instant::now);
        let mut candidate_count = 0u64;
        let index = self.style_index();
        let scope = self.tree_scope(id);
        let matched = match (index.scopes.get(&scope), index.buckets.get(&scope)) {
            (Some(rules), Some(b)) => {
                let mut out: Vec<u32> = Vec::new();
                let mut test = |dom: &Dom, ri: u32, out: &mut Vec<u32>| {
                    candidate_count += 1;
                    if dom.matches_complex(id, &rules[ri as usize].selector.0, None) {
                        out.push(ri);
                    }
                };
                for &ri in &b.universal {
                    test(self, ri, &mut out);
                }
                if let Some(idv) = self.attr(id, "id")
                    && let Some(v) = b.by_id.get(idv)
                {
                    for &ri in v {
                        test(self, ri, &mut out);
                    }
                }
                if let Some(classes) = self.attr(id, "class") {
                    for cls in classes.split_ascii_whitespace() {
                        if let Some(v) = b.by_class.get(cls) {
                            for &ri in v {
                                test(self, ri, &mut out);
                            }
                        }
                    }
                }
                if let Some(tag) = self.tag_name(id)
                    && let Some(v) = b.by_tag.get(tag)
                {
                    for &ri in v {
                        test(self, ri, &mut out);
                    }
                }
                // Cascade order is carried by each rule's `order` (the cascade
                // tiebreaker), so the matched list need not be ordered — but
                // sort for deterministic iteration, and dedup so a repeated
                // class token (`class="box box"`) can't list a rule twice.
                out.sort_unstable();
                out.dedup();
                std::rc::Rc::new(out)
            }
            _ => std::rc::Rc::new(Vec::new()),
        };
        self.matched_cache
            .borrow_mut()
            .put(id, self.epoch, matched.clone());
        if let Some(started) = started {
            casc_bump(|diag| {
                diag.matched_rule_builds += 1;
                diag.matched_candidates += candidate_count;
                diag.matched_us += started.elapsed().as_micros() as u64;
            });
        }
        matched
    }

    /// adoptedStyleSheets text for a scope (DOCUMENT or a shadow root),
    /// pushed by the prelude on adoption and on replace/replaceSync.
    /// Idempotent pushes are free — no dirty, no rebuild.
    pub fn set_adopted_styles(&mut self, scope: NodeId, css: &str) {
        if self.adopted_styles.get(&scope).map(String::as_str) == Some(css)
            || (css.trim().is_empty() && !self.adopted_styles.contains_key(&scope))
        {
            return;
        }
        self.adopted_styles.insert(scope, css.to_string());
        self.touch_style_at(scope);
    }

    fn is_stylesheet_link(&self, id: NodeId) -> bool {
        if self.tag_name(id) != Some("link") {
            return false;
        }
        let Some(rel) = self.attr(id, "rel") else {
            return false;
        };
        let mut words = rel.split_ascii_whitespace();
        // An applied stylesheet has `rel="stylesheet"`. `rel="alternate
        // stylesheet"` is an ALTERNATE — not applied unless the user selects it
        // (HTML §4.6.7) — and a `disabled` sheet is off; neither contributes to
        // the cascade, so don't fetch or attach them.
        let is_sheet = words.clone().any(|w| w.eq_ignore_ascii_case("stylesheet"));
        let is_alternate = words.any(|w| w.eq_ignore_ascii_case("alternate"));
        is_sheet && !is_alternate && self.attr(id, "disabled").is_none()
    }

    /// Raw hrefs of external stylesheets, document order, so the fetch
    /// pipeline can resolve and download them before scripts run.
    pub fn stylesheet_links(&self) -> Vec<String> {
        self.descendants(DOCUMENT)
            .filter(|&id| self.is_stylesheet_link(id))
            .filter_map(|id| self.attr(id, "href").map(str::to_string))
            .collect()
    }

    /// Text of author `<style>` sheets in document order. CSS Fonts 4 §4.1
    /// makes `@font-face` resources available from every stylesheet in the
    /// document, not only externally linked sheets; the HTTP preload phase
    /// uses this before the resident actor starts so first layout can shape
    /// with inline-declared web fonts.
    pub(crate) fn inline_stylesheets(&self) -> Vec<String> {
        self.descendants(DOCUMENT)
            .filter(|&id| self.tag_name(id) == Some("style"))
            .map(|id| self.text_content(id))
            .collect()
    }

    /// Attach fetched `<link rel=stylesheet>` bodies (keyed by the raw
    /// href attribute) to their link elements; the cascade reads them
    /// scope-aware like any `<style>`. ONE document walk collects the
    /// candidate links (this used to walk the whole document once per
    /// sheet — O(sheets × nodes) on a 48-sheet page).
    pub fn attach_external_sheets(&mut self, sheets: &[(String, String)]) {
        if sheets.is_empty() {
            return;
        }
        let links: Vec<(NodeId, String)> = self
            .descendants(DOCUMENT)
            .filter(|&id| self.is_stylesheet_link(id))
            .filter_map(|id| self.attr(id, "href").map(|h| (id, h.to_string())))
            .collect();
        for (href, css) in sheets {
            // First not-yet-attached link with this href (duplicate hrefs
            // attach to successive links, as before).
            let hit = links
                .iter()
                .find(|(id, h)| !self.external_sheets.contains_key(id) && h == href);
            if let Some(&(id, _)) = hit {
                self.external_sheets.insert(id, css.clone());
                self.touch_style_at(id);
            }
        }
    }

    /// Attach ONE fetched external stylesheet body to its `<link>` element —
    /// the incremental sibling of `attach_external_sheets`, for a sheet whose
    /// link was INJECTED by page JS after load (webpack's mini-css chunk
    /// loader). The cascade collects it at the link's document position like
    /// any tree sheet; `touch_style` re-parses the style index and forces the
    /// full relayout a sheet-set change requires. Replaces any earlier body on
    /// the same link (a loader may rewrite `href` and re-trigger the load).
    pub fn attach_sheet_to_link(&mut self, id: NodeId, css: String) {
        self.external_sheets.insert(id, css);
        self.touch_style_at(id);
    }

    /// Attach a shadow root (a fragment) to a host element; rendering
    /// flattens it in place of the host's light children, with `<slot>`
    /// projection. Idempotent per host, like the real API isn't — pages
    /// that double-attach get the same root back rather than a throw.
    pub fn attach_shadow(&mut self, host: NodeId) -> NodeId {
        if let Some(&root) = self.shadow_roots.get(&host) {
            return root;
        }
        let root = self.create_fragment();
        let owner_document = self.nodes[host].owner_document;
        self.set_owner_document_subtree(root, owner_document);
        self.shadow_roots.insert(host, root);
        self.shadow_hosts.insert(root, host);
        // Attaching changes only this host's composed contents. Preserve the
        // host as the mutation target: if it is connected the normal boundary
        // logic updates it; if it is a custom element being constructed in a
        // detached framework work tree, no rendered document is invalidated.
        self.touch_content(Some(host));
        root
    }

    /// Parent in the COMPOSED tree: shadow roots hand off to their host
    /// (event paths and ancestor checks cross shadow boundaries).
    pub fn parent_composed(&self, id: NodeId) -> Option<NodeId> {
        self.nodes[id]
            .parent
            .or_else(|| self.shadow_hosts.get(&id).copied())
    }

    /// Whether `id` is connected to the document (a render-affecting node). A
    /// mutation on a DETACHED subtree — the `createElement` + set-content that
    /// precedes `appendChild` — is invisible until the node is inserted, so
    /// incremental layout IGNORES it (incremental-layout contract): the insertion
    /// records the container, whose patch re-serializes the now-attached content.
    pub fn is_connected(&self, id: NodeId) -> bool {
        let mut cur = Some(id);
        while let Some(c) = cur {
            if c == DOCUMENT {
                return true;
            }
            cur = self.parent_composed(c);
        }
        false
    }

    /// Pre-insertion validity (WHATWG DOM §4.2.3): is `node` a *host-including
    /// inclusive ancestor* of `parent`? `appendChild`/`insertBefore`/
    /// `replaceChild` throw `HierarchyRequestError` when it is — the step that
    /// keeps the tree acyclic, since a node can never become a descendant of
    /// itself. "Inclusive" covers `node == parent`; "host-including" climbs
    /// across shadow boundaries via `parent_composed` (a shadow root hands off
    /// to its host), so a cycle can't form in the composed tree either — which
    /// is the tree the layout containing-block walk traverses, so enforcing
    /// this here is what lets that walk run unbounded like a browser's.
    pub fn is_host_including_inclusive_ancestor(&self, node: NodeId, parent: NodeId) -> bool {
        if node == parent {
            return true; // the "inclusive" case
        }
        // A *proper* ancestor must have at least one composed descendant — a
        // light child or a hosted shadow tree. A node with neither can't be one
        // (it appears on no ancestor chain), so we skip the walk entirely. This
        // is the dominant insertion: a freshly created / leaf node, made O(1).
        if self.nodes[node].first_child.is_none() && !self.shadow_roots.contains_key(&node) {
            return false;
        }
        let mut cur = self.parent_composed(parent);
        while let Some(p) = cur {
            if p == node {
                return true;
            }
            cur = self.parent_composed(p);
        }
        false
    }

    /// Document-order walk of the COMPOSED tree: light children plus
    /// every shadow tree (interactive content hides in there). Composes the
    /// shadow root of EVERY node including `root` itself — so a containing
    /// block that is a shadow host (archive.org's `<infinite-scroller>` keeps
    /// its positioned sentinel in its own shadow root) reaches its shadow
    /// descendants, not only its light subtree.
    pub fn composed_descendants(&self, root: NodeId) -> Vec<NodeId> {
        let mut out = Vec::new();
        let mut stack: Vec<NodeId> = Vec::new();
        self.push_composed_children(root, &mut stack);
        while let Some(id) = stack.pop() {
            out.push(id);
            self.push_composed_children(id, &mut stack);
        }
        out
    }

    /// Node ids in the shadow-including inclusive subtree rooted at `root`.
    ///
    /// This is the DOM Standard's shadow-including traversal, with the shadow
    /// root node itself included as well as its children. The JavaScript
    /// binding uses it only when a subtree changes connectedness: wrappers for
    /// connected platform objects retain identity and custom-element/shadow
    /// state, while wrappers below a detached root can return to weak storage.
    pub(crate) fn wrapper_subtree_ids(&self, root: NodeId) -> Vec<NodeId> {
        if !self.is_valid(root) {
            return Vec::new();
        }
        let mut out = Vec::new();
        let mut stack = vec![root];
        while let Some(id) = stack.pop() {
            out.push(id);

            let start = stack.len();
            let mut child = self.nodes[id].first_child;
            while let Some(child_id) = child {
                stack.push(child_id);
                child = self.nodes[child_id].next_sibling;
            }
            stack[start..].reverse();

            // A shadow root is not a light-tree child of its host, but it and
            // its descendants are part of the host's shadow-including subtree.
            if let Some(shadow) = self.shadow_root(id) {
                stack.push(shadow);
            }
        }
        out
    }

    pub fn shadow_root(&self, host: NodeId) -> Option<NodeId> {
        self.shadow_roots.get(&host).copied()
    }

    /// The composed-tree children of `id`: its light children plus, when it
    /// hosts a shadow root, that root's children. A slotted light child stays
    /// a child of its host here (it isn't re-parented under the `<slot>`), so a
    /// bottom-up walk that unions descendant boxes still reaches it — exactly
    /// what `measure_boxes` needs so a shadow host's box (and the document's
    /// scrollable height) counts the content rendered into its shadow tree.
    pub fn composed_children(&self, id: NodeId) -> Vec<NodeId> {
        let mut out = self.children(id);
        if let Some(shadow) = self.shadow_root(id) {
            out.extend(self.children(shadow));
        }
        out
    }

    /// The FLAT-TREE children of `id` (HTML §4.8.2 "flat tree"): a shadow HOST
    /// yields its shadow root's children IN PLACE of its light children, and any
    /// `<slot>` among them is replaced by its assigned light nodes (or the
    /// slot's own fallback children when nothing is assigned). This is what
    /// layout must iterate wherever it classifies children by ROLE — table
    /// rows/cells/captions, grid `<col>` tracks — so a component that renders a
    /// table/grid into its shadow (a `display:table` custom element slotting
    /// light `<tr>`s) is composed like a browser, not read as empty. `children`
    /// (light-only) and `composed_children` (light + shadow, no slot projection)
    /// are the wrong tools there. Unlike the box-tree `tree::children`, which
    /// hoists a `<slot>` transparently at the element level, this recursively
    /// flattens forwarding slots so classification sees the assigned nodes.
    pub fn flat_children(&self, id: NodeId) -> Vec<NodeId> {
        let base = match self.shadow_root(id) {
            Some(shadow) => self.children(shadow),
            None => self.children(id),
        };
        if !base.iter().any(|&c| self.tag_name(c) == Some("slot")) {
            return base; // no shadow slots to project — the common case
        }
        let mut out = Vec::with_capacity(base.len());
        for c in base {
            if self.tag_name(c) == Some("slot") {
                let assigned = self.flat_slot_nodes(c);
                if assigned.is_empty() {
                    out.extend(self.children(c)); // the slot's fallback content
                } else {
                    out.extend(assigned);
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    /// Pre-order walk of the flattened rendered tree, excluding `root`.
    ///
    /// CSS Shadow 1 §4.1 uses the flattened element tree for inheritance
    /// and box construction after selector matching. Presentation metadata
    /// (controls, image requests, accessibility children, and native hit-test
    /// ancestry) must walk that same tree: a light-DOM walk loses shadow
    /// content, while a composed walk also visits unassigned light children
    /// that generate no boxes.
    pub(crate) fn flat_descendants(&self, root: NodeId) -> Vec<NodeId> {
        let mut out = Vec::new();
        let mut stack = self.flat_children(root);
        stack.reverse();
        while let Some(id) = stack.pop() {
            out.push(id);
            let children = self.flat_children(id);
            stack.extend(children.into_iter().rev());
        }
        out
    }

    /// The light-DOM nodes assigned to a `<slot>` (HTML §4.8.2 slot
    /// assignment): the slot's shadow HOST's children whose `slot=` attribute
    /// matches this slot's `name` (the default slot is `name=""`/absent, where
    /// text nodes and slot-less children land). Returns empty when the slot is
    /// not inside a shadow tree, or nothing is assigned — the caller then falls
    /// back to the slot's own children (its fallback content). This is what
    /// projects a web component's light children into its shadow `<slot>`s so
    /// the flat (rendered) tree is complete — archive.org's `<router-slot>`
    /// shadow is just `<slot>`, with the routed `<home-page>` (and the
    /// `<infinite-scroller>` beneath it) assigned as a light child.
    pub fn slot_assigned_nodes(&self, slot: NodeId) -> Vec<NodeId> {
        let mut cur = self.nodes[slot].parent;
        let host = loop {
            match cur {
                Some(p) => {
                    if let Some(&h) = self.shadow_hosts.get(&p) {
                        break h;
                    }
                    cur = self.nodes[p].parent;
                }
                None => return Vec::new(),
            }
        };
        let want = self.attr(slot, "name").unwrap_or("").trim().to_owned();
        self.child_iter(host)
            .filter(|&c| self.attr(c, "slot").unwrap_or("").trim() == want)
            .collect()
    }

    /// Return the flattened slottables represented by `slot`. HTML's
    /// `assignedNodes({ flatten: true })` recursively substitutes a slot's
    /// assigned nodes (or fallback children) whenever another `<slot>` is
    /// encountered. Rendering crosses this forwarding pattern frequently:
    /// Reddit's gallery forwards its light-DOM pages through a gallery slot
    /// and then through the nested faceplate carousel slot. A one-level
    /// projection leaves the forwarding `<slot>` as an empty box and drops
    /// the images from both the serializer and the box tree.
    pub fn flat_slot_nodes(&self, slot: NodeId) -> Vec<NodeId> {
        fn flatten(dom: &Dom, id: NodeId, out: &mut Vec<NodeId>) {
            if dom.tag_name(id) == Some("slot") {
                let assigned = dom.slot_assigned_nodes(id);
                let source = if assigned.is_empty() {
                    dom.children(id)
                } else {
                    assigned
                };
                for child in source {
                    flatten(dom, child, out);
                }
            } else {
                out.push(id);
            }
        }

        let mut out = Vec::new();
        flatten(self, slot, &mut out);
        out
    }

    /// Flatten only nodes that are actually assigned to `slot`, excluding
    /// fallback children. `::slotted()` represents assigned slottables, not a
    /// slot's fallback content (CSS Shadow 1 §3.2.4).
    fn flat_assigned_slot_nodes(&self, slot: NodeId) -> Vec<NodeId> {
        fn flatten(dom: &Dom, id: NodeId, out: &mut Vec<NodeId>) {
            if dom.tag_name(id) == Some("slot") {
                for child in dom.slot_assigned_nodes(id) {
                    flatten(dom, child, out);
                }
            } else {
                out.push(id);
            }
        }
        let mut out = Vec::new();
        flatten(self, slot, &mut out);
        out
    }

    /// Composed-tree element ids whose tag is `name`, in document (pre-)order,
    /// piercing shadow roots — the catch-up upgrade set `customElements.define`
    /// needs. Done in Rust as a pointer walk (no per-node child Vec, no JS
    /// wrapper) because the prelude formerly walked the whole tree per `define`
    /// in JS — a `__dom_children`/`wrap` storm that dominated big-page boot
    /// (GitHub: ~O(defines × 16.8k nodes)).
    pub fn elements_by_tag_composed(&self, root: NodeId, name: &str) -> Vec<NodeId> {
        let mut out = Vec::new();
        if !self.is_valid(root) {
            return out;
        }
        let mut stack: Vec<NodeId> = vec![root];
        while let Some(id) = stack.pop() {
            if self.tag_name(id) == Some(name) {
                out.push(id);
            }
            self.push_composed_children(id, &mut stack);
        }
        out
    }

    /// Composed-tree element ids (root included, shadow-piercing, document
    /// order) whose tag is a custom-element name — i.e. contains a hyphen (the
    /// HTML naming rule for autonomous custom elements). Backs `ceScan`'s
    /// insertion-time upgrade/connect pass: the prelude can then touch only the
    /// custom-element candidates instead of wrapping every node in the inserted
    /// subtree.
    pub fn custom_elements_composed(&self, root: NodeId) -> Vec<NodeId> {
        let mut out = Vec::new();
        if !self.is_valid(root) {
            return out;
        }
        let mut stack: Vec<NodeId> = vec![root];
        while let Some(id) = stack.pop() {
            if self.tag_name(id).is_some_and(|t| t.contains('-')) {
                out.push(id);
            }
            self.push_composed_children(id, &mut stack);
        }
        out
    }

    /// Push `id`'s composed children (light children, then shadow-root
    /// children) onto `stack` in reverse, so a LIFO pop yields them in
    /// document order (pre-order: a parent is processed before its children).
    fn push_composed_children(&self, id: NodeId, stack: &mut Vec<NodeId>) {
        let start = stack.len();
        let mut c = self.nodes[id].first_child;
        while let Some(cid) = c {
            stack.push(cid);
            c = self.nodes[cid].next_sibling;
        }
        if let Some(shadow) = self.shadow_root(id) {
            let mut c = self.nodes[shadow].first_child;
            while let Some(cid) = c {
                stack.push(cid);
                c = self.nodes[cid].next_sibling;
            }
        }
        stack[start..].reverse();
    }

    /// Where innerHTML-ish operations land: a template's content
    /// fragment, everyone else themselves.
    pub fn content_target(&self, id: NodeId) -> NodeId {
        match &self.nodes[id].data {
            NodeData::Element {
                template_contents: Some(c),
                ..
            } => *c,
            _ => id,
        }
    }

    /// The `<body>` of an iframe's nested document, when the JS prelude has
    /// realized one (a same-origin scripted/`srcdoc` frame builds an
    /// `<html><head><body>` subtree under the `<iframe>`; see
    /// `FrameDocument`). The serializers retain an equivalent body formatting
    /// box inside the frame viewport instead of putting these nodes back under
    /// an `<iframe>`, whose children the HTML parser treats as RAWTEXT. `None`
    /// for an unrealized or cross-origin frame.
    pub fn frame_body(&self, id: NodeId) -> Option<NodeId> {
        let html = self
            .child_iter(id)
            .find(|&c| self.tag_name(c) == Some("html"))?;
        self.child_iter(html)
            .find(|&c| self.tag_name(c) == Some("body"))
    }

    /// The nearest iframe/frame whose active nested document contains `id`.
    ///
    /// Nested documents share this arena for layout, but CSSOM View hit testing
    /// is scoped to one `Document`: a top-level query returns the embedding
    /// iframe element, while the child document may return boxes inside it.
    /// Walking the composed parent chain also keeps shadow-tree descendants in
    /// their containing document without exposing a deeper nested document.
    pub fn frame_owner(&self, id: NodeId) -> Option<NodeId> {
        let mut current = Some(id);
        while let Some(node) = current {
            let parent = self.parent_composed(node)?;
            if matches!(self.tag_name(parent), Some("iframe" | "frame"))
                && self.frame_body(parent).is_some()
            {
                return Some(parent);
            }
            current = Some(parent);
        }
        None
    }

    fn serialized_frame_wrapper_style(&self, id: NodeId) -> String {
        let display = match self.effective_display(id).as_deref() {
            Some("none") => "none",
            Some("block" | "flow-root" | "flex" | "grid" | "table") => "block",
            _ => "inline-block",
        };
        let dimension = |property: &str, attr: &str, fallback: &str| {
            self.computed_value_resolved(id, property)
                .filter(|value| value.trim() != "auto")
                .or_else(|| {
                    self.attr(id, attr)
                        .and_then(|value| value.trim().parse::<f32>().ok())
                        .filter(|value| value.is_finite() && *value >= 0.0)
                        .map(|value| format!("{value}px"))
                })
                .unwrap_or_else(|| fallback.to_string())
        };
        // HTML §4.8.5 makes the iframe the container for a distinct content
        // navigable; CSS Display therefore gives the replaced viewport and the
        // child document separate boxes. Keep every authored outer-box
        // declaration (notably position/inset/z-index for player overlays),
        // then normalize only the replacement div's display, used dimensions,
        // and viewport clip. Dropping those outer declarations made fixed
        // frames participate in parent flow.
        let mut style = self.attr(id, "style").unwrap_or("").to_string();
        append_style(&mut style, &self.baked_element_style(id, false));
        append_style(
            &mut style,
            &format!(
                "display:{display};width:{};height:{};overflow:hidden;",
                dimension("width", "width", "300px"),
                dimension("height", "height", "150px")
            ),
        );
        style
    }

    /// Style for the presentation wrapper that stands in for a nested
    /// document's `<body>`. CSS Display 3 §2 says an element's inner display
    /// type selects the formatting context for its descendants, so serializing
    /// only the body's children is not equivalent: it loses flex/grid/block
    /// layout, alignment, sizing, overflow, and the inherited style context.
    fn serialized_frame_body_style(&self, body: NodeId) -> String {
        let mut style = self.attr(body, "style").unwrap_or("").to_string();
        // The child <html> cannot be emitted inside the parent document, but
        // inherited values that reached <body> through it still belong to the
        // child document's styling context. Materialize those values here.
        append_style(&mut style, &self.baked_element_style(body, true));
        style
    }

    fn write_serialized_frame_body_open(&self, body: NodeId, out: &mut String) {
        out.push_str("<div data-trust-frame-body=\"\"");
        let style = self.serialized_frame_body_style(body);
        if !style.is_empty() {
            out.push_str(" style=\"");
            out.push_str(&escape_attr(&style));
            out.push('"');
        }
        out.push('>');
    }

    /// Load `html` as an iframe's nested document (the HTML "navigate an
    /// `iframe` or `frame`" step, for src + srcdoc). The fetched bytes are
    /// parsed as a FULL HTML document and installed as the frame's content
    /// navigable, replacing whatever was there (an empty `about:blank`
    /// document on first load, or a prior navigation). Relative URLs in the
    /// new content are absolutized against `base` (the frame's own document
    /// URL): the serializer flattens the frame into the parent document, where
    /// link/resource resolution would otherwise use the PARENT's base, so we
    /// bake the frame's base in here. Returns the new `<body>`, or `None` if
    /// the markup had no parseable `<html>`. (`__dom_load_frame` syscall.)
    pub fn install_frame_document(
        &mut self,
        frame: NodeId,
        html: &str,
        base: &str,
    ) -> Option<NodeId> {
        let doc = Dom::parse_document(html);
        let src_html = doc
            .children(DOCUMENT)
            .into_iter()
            .find(|&c| doc.tag_name(c) == Some("html"))?;
        // Discard the previous content navigable (arenas only grow; the old
        // subtree is just unlinked).
        for c in self.children(frame) {
            self.detach(c);
        }
        let new_html = self.transplant(&doc, src_html);
        self.append(frame, new_html);
        if let Ok(base_url) = url::Url::parse(base) {
            self.absolutize_subtree_urls(new_html, &base_url);
        }
        self.frame_body(frame)
    }

    /// Rewrite a subtree's relative URL attributes to absolute, resolved
    /// against `base`. Absolute URLs and non-relative schemes (`javascript:`,
    /// `mailto:`, `data:`) pass through `Url::join` unchanged; fragment-only
    /// hrefs are left alone (they're in-page anchors, not navigations).
    fn absolutize_subtree_urls(&mut self, root: NodeId, base: &url::Url) {
        const URL_ATTRS: &[(&str, &str)] = &[
            ("a", "href"),
            ("area", "href"),
            ("link", "href"),
            ("img", "src"),
            ("script", "src"),
            ("source", "src"),
            ("iframe", "src"),
            ("frame", "src"),
            ("embed", "src"),
            ("audio", "src"),
            ("video", "src"),
            ("video", "poster"),
            ("object", "data"),
            ("form", "action"),
            ("input", "formaction"),
            ("button", "formaction"),
        ];
        let mut edits: Vec<(NodeId, &'static str, String)> = Vec::new();
        for id in self.descendants(root) {
            let Some(tag) = self.tag_name(id) else {
                continue;
            };
            for &(t, attr) in URL_ATTRS {
                if t != tag {
                    continue;
                }
                if let Some(v) = self.attr(id, attr) {
                    let v = v.trim();
                    if v.is_empty() || v.starts_with('#') {
                        continue;
                    }
                    if let Ok(abs) = base.join(v) {
                        edits.push((id, attr, abs.to_string()));
                    }
                }
            }
        }
        for (id, attr, val) in edits {
            self.set_attr(id, attr, &val);
        }
    }

    /// The host's light children assigned to a slot (by name, or the
    /// default slot). Text nodes always belong to the default slot.
    fn slot_assigned(&self, host: NodeId, slot_name: Option<&str>) -> Vec<NodeId> {
        self.child_iter(host)
            .filter(|&c| match (self.attr(c, "slot"), slot_name) {
                (Some(a), Some(n)) => a == n,
                (None, None) => true,
                _ => false,
            })
            .collect()
    }

    /// Concatenated descendant text (DOM textContent).
    /// A comment's data (Lit's binding markers live there).
    pub fn comment_text(&self, id: NodeId) -> Option<&str> {
        match &self.nodes[id].data {
            NodeData::Comment(t) => Some(t),
            _ => None,
        }
    }

    pub fn set_comment_text(&mut self, id: NodeId, text: &str) {
        if let NodeData::Comment(t) = &mut self.nodes[id].data
            && t != text
        {
            *t = text.to_string();
            // Comments never render; record the parent so this can't strand an
            // unattributed mutation, and let the per-boundary render-dedup drop it.
            let parent = self.nodes[id].parent;
            self.touch_content(parent);
        }
    }

    pub fn text_content(&self, id: NodeId) -> String {
        self.cached_string(id, 0, || self.text_content_uncached(id))
    }

    fn text_content_uncached(&self, id: NodeId) -> String {
        let mut out = String::new();
        if let NodeData::Text(t) = &self.nodes[id].data {
            return t.clone();
        }
        for d in self.descendants(id) {
            if let NodeData::Text(t) = &self.nodes[d].data {
                out.push_str(t);
            }
        }
        out
    }

    /// Cache a DOM string getter for one mutation epoch. `build` runs without
    /// the cache borrowed, which keeps nested reads in the serializer legal.
    /// The copied return value is still required by the JS boundary; this
    /// removes the more expensive tree walk/serialization on repeated reads.
    fn cached_string(&self, id: NodeId, kind: u8, build: impl FnOnce() -> String) -> String {
        let epoch = self.epoch;
        if let Some(value) = {
            let cache = self.serialization_cache.borrow();
            (cache.0 == epoch)
                .then(|| cache.1.get(&(id, kind)).cloned())
                .flatten()
        } {
            return value;
        }
        let value = build();
        let mut cache = self.serialization_cache.borrow_mut();
        if cache.0 != epoch {
            cache.0 = epoch;
            cache.1.clear();
        }
        cache.1.insert((id, kind), value.clone());
        value
    }

    /// Whether the subtree under `id` (inclusive for a text node) contains
    /// any non-whitespace text — the allocation-free, early-exiting form of
    /// `!text_content(id).trim().is_empty()`, which built the whole
    /// concatenated string only to test it (the live serializer runs this
    /// per clickable element).
    fn subtree_has_text(&self, id: NodeId) -> bool {
        let non_ws =
            |d: &NodeData| matches!(d, NodeData::Text(t) if !t.chars().all(char::is_whitespace));
        non_ws(&self.nodes[id].data)
            || self
                .flat_descendants(id)
                .into_iter()
                .any(|d| non_ws(&self.nodes[d].data))
    }

    /// The terminal glyph for an icon element/subtree — the dominant web icon
    /// idiom is a Font-Awesome-style `<svg class="...fa-NAME"><use href=
    /// "#...fa-NAME"></svg>` (also `icon-NAME`/`bi-NAME`). We don't rasterize
    /// SVG (an icon-sized raster is an unreadable smear in a terminal); instead
    /// we recognize the icon by NAME and render its Unicode glyph. Scans `id`
    /// and its descendants for the first recognizable name. `None` when nothing
    /// matches (a non-icon `<svg>` — a D3 chart, a logo — stays unrendered).
    pub fn icon_glyph(&self, id: NodeId) -> Option<&'static str> {
        for n in std::iter::once(id).chain(self.flat_descendants(id)) {
            for attr in ["class", "href", "xlink:href"] {
                if let Some(v) = self.attr(n, attr) {
                    for tok in v.split(|c: char| c.is_whitespace()) {
                        if let Some(name) = icon_token_name(tok)
                            && let Some(g) = icon_glyph_for(name)
                        {
                            return Some(g);
                        }
                    }
                }
            }
        }
        None
    }

    pub fn set_text(&mut self, id: NodeId, text: &str) {
        match &mut self.nodes[id].data {
            // Idempotent writes are free: no dirty, no redraw.
            NodeData::Text(t) if *t == text => (),
            NodeData::Text(t) => {
                *t = text.to_string();
                // A text node's content changed — its PARENT element is the
                // relayout target (text styling/flow is an element concern).
                let parent = self.nodes[id].parent;
                if let Some(parent) = parent
                    && self.tag_name(parent) == Some("style")
                {
                    self.touch_style_at(parent); // sheet text changed in place
                }
                self.touch_content(parent);
            }
            _ => {
                // A single-text-child rewrite to the same value is the
                // hot no-op (counters, clocks): skip it cheaply.
                let kids = self.children(id);
                if let [only] = kids[..]
                    && let NodeData::Text(t) = &self.nodes[only].data
                    && *t == text
                {
                    return;
                }
                self.touch_content(Some(id));
                for c in kids {
                    self.detach(c);
                }
                let t = self.create_text(text);
                self.append(id, t);
            }
        }
    }

    /// Deep-copy a subtree (or a single node when `deep` is false).
    /// Template content propagates per the HTML cloning steps: a cloned
    /// template always owns a fresh content fragment, populated when
    /// deep (webcomponents-loader probes exactly this).
    pub fn clone_subtree(&mut self, id: NodeId, deep: bool) -> NodeId {
        let data = match &self.nodes[id].data {
            NodeData::Document | NodeData::Fragment => NodeData::Fragment,
            NodeData::Doctype => NodeData::Doctype,
            NodeData::Comment(t) => NodeData::Comment(t.clone()),
            NodeData::Text(t) => NodeData::Text(t.clone()),
            NodeData::Element { name, attrs, .. } => NodeData::Element {
                name: name.clone(),
                attrs: attrs.clone(),
                template_contents: None,
            },
        };
        let src_content = match &self.nodes[id].data {
            NodeData::Element {
                template_contents: Some(c),
                ..
            } => Some(*c),
            _ => None,
        };
        let copy = self.new_node(data);
        if let Some(sc) = src_content {
            let frag = self.new_node(NodeData::Fragment);
            if let NodeData::Element {
                template_contents, ..
            } = &mut self.nodes[copy].data
            {
                *template_contents = Some(frag);
            }
            if deep {
                for c in self.children(sc) {
                    let cc = self.clone_subtree(c, true);
                    self.append(frag, cc);
                }
            }
        }
        if deep {
            for c in self.children(id) {
                let cc = self.clone_subtree(c, true);
                self.append(copy, cc);
            }
        }
        copy
    }

    /// Parse an HTML snippet in the context of `parent`'s tag and return
    /// the new nodes (already transplanted into this arena, detached).
    pub fn parse_fragment_into(&mut self, context_tag: &str, html: &str) -> Vec<NodeId> {
        let sink = Sink {
            dom: RefCell::new(Dom::new()),
        };
        let context = QualName::new(None, ns!(html), context_tag.to_ascii_lowercase().into());
        let frag: Dom =
            html5ever::parse_fragment(sink, ParseOpts::default(), context, Vec::new(), false)
                .one(StrTendril::from(html));
        // The fragment's children land under <html> under the document.
        let html_el = frag
            .child_iter(DOCUMENT)
            .find(|&c| frag.tag_name(c) == Some("html"))
            .unwrap_or(DOCUMENT);
        frag.child_iter(html_el)
            .map(|c| self.transplant(&frag, c))
            .collect()
    }

    /// Parse a full HTML document string into a DETACHED `Document` node in
    /// this arena (`DOMParser.parseFromString(str, "text/html")`). Returns the
    /// new document node, structured with the parser's real `<html>`/`<head>`/
    /// `<body>` split — a body-fragment parse (the old approach) collapses them,
    /// which breaks any consumer that reads `newDocument.head`/`.body` separately
    /// (a view-transitions swap, most notably).
    pub fn parse_document_into(&mut self, html: &str) -> NodeId {
        let src = Dom::parse_document(html);
        let doc = self.new_node(NodeData::Document);
        self.nodes[doc].owner_document = doc;
        for c in src.child_iter(DOCUMENT) {
            let cc = self.transplant(&src, c);
            self.append(doc, cc);
        }
        doc
    }

    /// Deep-copy a subtree from another arena into this one. Template
    /// content rides along (html5ever parks template children there).
    fn transplant(&mut self, other: &Dom, id: NodeId) -> NodeId {
        let data = match &other.nodes[id].data {
            NodeData::Document | NodeData::Fragment => NodeData::Fragment,
            NodeData::Doctype => NodeData::Doctype,
            NodeData::Comment(t) => NodeData::Comment(t.clone()),
            NodeData::Text(t) => NodeData::Text(t.clone()),
            NodeData::Element { name, attrs, .. } => NodeData::Element {
                name: name.clone(),
                attrs: attrs.clone(),
                template_contents: None,
            },
        };
        let src_content = match &other.nodes[id].data {
            NodeData::Element {
                template_contents: Some(c),
                ..
            } => Some(*c),
            _ => None,
        };
        let copy = self.new_node(data);
        if let Some(sc) = src_content {
            let frag = self.new_node(NodeData::Fragment);
            if let NodeData::Element {
                template_contents, ..
            } = &mut self.nodes[copy].data
            {
                *template_contents = Some(frag);
            }
            for c in other.child_iter(sc) {
                let cc = self.transplant(other, c);
                self.append_fresh(frag, cc);
            }
        }
        for c in other.child_iter(id) {
            let cc = self.transplant(other, c);
            self.append_fresh(copy, cc);
        }
        copy
    }

    /// First element (document order) whose id attribute matches.
    pub fn get_by_id(&self, target: &str) -> Option<NodeId> {
        self.descendants(DOCUMENT)
            .into_iter()
            .find(|&d| self.attr(d, "id") == Some(target))
    }

    /// Serialize a subtree to HTML (for the app to re-parse and lay
    /// out). `<script>` has done its job by now and `<noscript>` means
    /// "JS didn't run" — when this serializer is called, it did — so both
    /// are dropped, as are doctypes and `<template>` (inert by
    /// definition). The cascaded `display` is baked onto each element so
    /// the re-parsed layout arena flows it the way the engine computed.
    pub fn serialize(&self, root: NodeId) -> String {
        let mut out = String::new();
        self.serialize_node_inner(root, None, false, &mut out);
        out
    }

    /// Serialize for JavaScript (`outerHTML`) with HTML's DOM serialization
    /// semantics. Unlike the presentation serializer this keeps inert/hidden
    /// elements, template contents, iframe markup, and light DOM rather than
    /// substituting the painted shadow/frame tree.
    pub fn serialize_js(&self, root: NodeId) -> String {
        self.cached_string(root, 2, || {
            let mut out = String::new();
            self.serialize_node_inner(root, None, true, &mut out);
            out
        })
    }

    /// JS-facing `innerHTML`: preserves `<template>` content (single caller is
    /// `sys_inner_html`). See `serialize_js`.
    pub fn inner_html(&self, id: NodeId) -> String {
        self.cached_string(id, 1, || {
            let mut out = String::new();
            for c in self.child_iter(self.content_target(id)) {
                self.serialize_node_inner(c, None, true, &mut out);
            }
            out
        })
    }

    /// Replace each renderable inline `<svg>` with an `<img>` whose `src` is the
    /// SVG as a `data:` URL, so the existing image pipeline decodes, sizes,
    /// caches, and silhouette-tints it — a vector icon/logo becomes a rendered
    /// glyph rather than its accessible-name text. `currentColor` is resolved
    /// from the SVG element's computed CSS color before the graphical image
    /// path sees the source; the terminal encoder may still apply its own
    /// silhouette tint after that.
    /// Non-renderable SVG (a `<use>`-only sprite instance, or a hidden
    /// `<symbol>`/`<defs>` container) is left untouched, keeping the existing
    /// icon-glyph / accessible-name fallback. Runs once per DOM build, before
    /// image-URL collection and layout. This is the first slice of inline-SVG
    /// support: a static snapshot of self-contained vector markup.
    pub fn rewrite_inline_svgs(&mut self, base: Option<&url::Url>) {
        self.rewrite_inline_svgs_in(DOCUMENT, base);
    }

    /// `rewrite_inline_svgs` restricted to one retained subtree. Incremental
    /// graphical patches have already rewritten every unaffected sibling, so
    /// revisiting the complete document would turn a local style response into
    /// document-sized work.
    pub fn rewrite_inline_svgs_in(&mut self, root: NodeId, base: Option<&url::Url>) {
        // Materialize the candidate list first: the loop MUTATES the tree
        // (insert/detach), which can't overlap the lazy descendants walk.
        let svgs: Vec<NodeId> = std::iter::once(root)
            .chain(self.descendants(root))
            .filter(|&id| self.tag_name(id) == Some("svg"))
            .collect();
        for id in svgs {
            if self.ancestor_is_svg(id) || self.is_hidden(id) {
                continue;
            }
            let Some(parent) = self.nodes[id].parent else {
                continue;
            };
            // The vector geometry: an EXTERNAL sprite reference
            // (`<use href="file.svg#id">`) resolves to the primed sheet's
            // symbol; otherwise the svg's OWN inline geometry. An svg with
            // neither (an unfetched sprite, an empty svg) is left untouched and
            // renders nothing, exactly as before.
            let Some(svg) = self.svg_render_markup(id, base) else {
                continue;
            };
            let name = self.svg_accessible_name(id);
            // Carry the SVG element's box onto the replacement <img> so layout
            // sizes the vector the way the page does. A browser sizes a replaced
            // SVG by its CSS `width`/`height` (here baked into `style` by the JS
            // cascade — `width:2.7rem`, etc.) over its presentation `width`/
            // `height` attrs over the viewBox ratio over the 300×150 default.
            // Without this the <img> carried no size, so `image_used_box` fell
            // to the SVG's intrinsic (300×150 when the markup has no width/height
            // attr) and rendered logos page-sized. `style` also carries the
            // box's margin/display/position so the icon lands where the SVG did.
            let style = self.attr(id, "style").map(str::to_string);
            let w_attr = self.attr(id, "width").map(str::to_string);
            let h_attr = self.attr(id, "height").map(str::to_string);
            let img = self.create_element("img");
            let svg = self.resolve_svg_current_color(id, svg);
            self.set_attr(img, "src", &crate::img::svg_data_url(&svg));
            if !name.is_empty() {
                self.set_attr(img, "alt", &name);
            }
            if let Some(style) = style {
                self.set_attr(img, "style", &style);
            }
            if let Some(w) = w_attr {
                self.set_attr(img, "width", &w);
            }
            if let Some(h) = h_attr {
                self.set_attr(img, "height", &h);
            }
            self.insert_before(parent, img, Some(id));
            self.detach(id);
        }
    }

    /// Return the renderer-neutral image source and accessible fallback for an
    /// inline SVG without altering the DOM tree. Direct layout uses the SVG
    /// element itself as the replaced element, preserving its CSS box and node
    /// identity while the ordinary image pipeline rasterizes this data URL.
    ///
    /// CSS Display constructs a box tree from the document/flat tree; it does
    /// not require replacing source nodes with HTML `<img>` elements. The old
    /// presentation-DOM path performed that replacement only because it had
    /// already committed to a serialize/reparse handoff.
    pub fn svg_image_data(&self, id: NodeId, base: Option<&url::Url>) -> Option<(String, String)> {
        if self.tag_name(id) != Some("svg") || self.ancestor_is_svg(id) || self.is_hidden(id) {
            return None;
        }
        let svg = self.svg_render_markup(id, base)?;
        Some((
            crate::img::svg_data_url(&self.resolve_svg_current_color(id, svg)),
            self.svg_accessible_name(id),
        ))
    }

    /// SVG's `currentColor` is the computed CSS `color` of the element, not a
    /// renderer theme. The desktop image pipeline rasterizes the serialized
    /// SVG without the terminal's silhouette pass, so carry the resolved
    /// color into the resource itself (CSS Color 4 and SVG 2 paint servers).
    ///
    /// A missing `color` declaration reaches the CSS initial value, black;
    /// that is also the SVG initial `fill` color. The markup produced here is
    /// self-contained, so the serialized resource must carry the used value
    /// while preserving descendant-specific `color` declarations.
    fn resolve_svg_current_color(&self, id: NodeId, svg: String) -> String {
        replace_css_current_color(&svg, &self.svg_used_color(id))
    }

    /// Resolve the used `color` value for an SVG element. CSS Color 4 §15.5
    /// keeps `currentcolor` as a keyword on the `color` property itself; its
    /// used value is the inherited color, so walk ancestors when the computed
    /// value is still that keyword.
    fn svg_used_color(&self, id: NodeId) -> String {
        let mut current = Some(id);
        loop {
            let Some(node) = current else {
                return String::from("black");
            };
            if let Some(value) = self
                .computed_value_resolved(node, "color")
                .filter(|value| !value.trim().eq_ignore_ascii_case("currentcolor"))
            {
                return svg_resource_color(&value);
            }
            current = self.nodes[node].parent;
        }
    }

    /// Resolve the SVG 2 §5.6 `use` instance tree into self-contained markup
    /// for the image decoder. External references use the fetched sprite table;
    /// a same-tree `#fragment` target outside the outer `<svg>` is injected into
    /// that SVG's `<defs>` so the authored `<use>` resolves without changing
    /// the canonical DOM.
    fn svg_render_markup(&self, id: NodeId, base: Option<&url::Url>) -> Option<String> {
        let mut svg = if let Some((file, frag)) = self.svg_sprite_ref(id) {
            base.and_then(|b| b.join(&file).ok())
                .and_then(|abs| sprite_symbol_svg(abs.as_str(), &frag))?
        } else if let Some(target) = self.local_svg_use_target(id) {
            let mut outer = self.serialize(id);
            if !self.descendants(id).any(|node| node == target) {
                let open = outer.find('>')? + 1;
                let definition = format!("<defs>{}</defs>", self.serialize(target));
                outer.insert_str(open, &definition);
            }
            outer
        } else if self.svg_is_renderable(id) {
            self.serialize_svg_for_image(id)
        } else {
            return None;
        };
        // resvg needs the namespace; inline SVG in HTML may omit it.
        if !svg.contains("xmlns") {
            svg = svg.replacen("<svg", r#"<svg xmlns="http://www.w3.org/2000/svg""#, 1);
        }
        Some(svg)
    }

    /// Serialize an inline SVG while resolving `currentColor` per element.
    /// The ordinary HTML serializer intentionally drops `<style>` elements;
    /// this small SVG serializer keeps their text and applies the same
    /// computed-color substitution to each element's authored/baked attrs.
    fn serialize_svg_for_image(&self, root: NodeId) -> String {
        let mut out = String::new();
        self.serialize_svg_node_for_image(root, &mut out);
        out
    }

    fn serialize_svg_node_for_image(&self, id: NodeId, out: &mut String) {
        match &self.nodes[id].data {
            NodeData::Document | NodeData::Fragment => {
                for child in self.child_iter(id) {
                    self.serialize_svg_node_for_image(child, out);
                }
            }
            NodeData::Doctype => {}
            NodeData::Comment(text) => {
                out.push_str("<!--");
                out.push_str(&text.replace("--", "- -"));
                out.push_str("-->");
            }
            NodeData::Text(text) => out.push_str(&escape_text(text)),
            NodeData::Element { name, attrs, .. } => {
                let tag = name.local.as_ref();
                if matches!(tag, "script" | "noscript" | "template")
                    || (tag != "style" && self.is_hidden(id))
                {
                    return;
                }
                let color = self.svg_used_color(id);
                // SVG 2 §6.6 maps presentation attributes into the author
                // cascade at specificity zero, while ordinary author rules
                // (including shadow-scoped component rules) can override
                // them. The standalone raster resource has no document CSS,
                // so materialize each direct cascade winner as inline style.
                let paint = self.svg_resource_style(id);
                let mut serialized_attrs = String::new();
                self.write_attrs_with_extra_style(
                    id,
                    attrs,
                    &mut |name, value| {
                        let value = if SVG_PRESENTATION_PROPERTIES.contains(&name) {
                            self.resolve_vars(id, value)
                        } else {
                            value.to_string()
                        };
                        Some(replace_css_current_color(&value, &color))
                    },
                    &paint,
                    &mut serialized_attrs,
                );
                out.push('<');
                out.push_str(tag);
                out.push_str(&replace_css_current_color(&serialized_attrs, &color));
                out.push('>');
                if !VOID_ELEMENTS.contains(&tag) {
                    for child in self.child_iter(id) {
                        self.serialize_svg_node_for_image(child, out);
                    }
                    out.push_str("</");
                    out.push_str(tag);
                    out.push('>');
                }
            }
        }
    }

    /// Direct SVG presentation-property winners to carry into an isolated
    /// image resource. Inherited winners are emitted on the ancestor where
    /// they were declared, so the SVG renderer reconstructs inheritance.
    fn svg_resource_style(&self, id: NodeId) -> String {
        let mut out = String::new();
        for &property in SVG_PRESENTATION_PROPERTIES {
            let Some(raw) = self.cascaded(id, property) else {
                continue;
            };
            let value = self.resolve_vars(id, &raw);
            if value.trim().is_empty() {
                continue;
            }
            out.push_str(property);
            out.push(':');
            out.push_str(&value);
            out.push(';');
        }
        out
    }

    /// The first resolvable same-tree fragment referenced by a descendant
    /// `<use>`. SVG 2 reference lookup is tree-scoped: a shadow tree must not
    /// capture an equal id from the outer document.
    fn local_svg_use_target(&self, id: NodeId) -> Option<NodeId> {
        let scope = self.tree_scope(id);
        self.descendants(id).find_map(|use_node| {
            if self.tag_name(use_node) != Some("use") {
                return None;
            }
            let fragment = self
                .attr(use_node, "href")
                .or_else(|| self.attr(use_node, "xlink:href"))?
                .trim()
                .strip_prefix('#')?;
            (!fragment.is_empty()).then_some(())?;
            std::iter::once(scope)
                .chain(self.descendants(scope))
                .find(|&candidate| {
                    self.attr(candidate, "id") == Some(fragment)
                        && matches!(
                            self.tag_name(candidate),
                            Some(
                                "svg"
                                    | "symbol"
                                    | "g"
                                    | "path"
                                    | "rect"
                                    | "circle"
                                    | "ellipse"
                                    | "line"
                                    | "polyline"
                                    | "polygon"
                                    | "text"
                                    | "image"
                                    | "use"
                            )
                        )
                })
        })
    }

    /// A paintable inline SVG: not hidden, and carrying real vector geometry
    /// that resvg can render on its own (not just a `<use>` sprite reference,
    /// whose target lives in another element we don't serialize with it).
    fn svg_is_renderable(&self, id: NodeId) -> bool {
        if self.is_hidden(id) {
            return false;
        }
        self.descendants(id).into_iter().any(|d| {
            matches!(
                self.tag_name(d),
                Some("path" | "rect" | "circle" | "ellipse" | "line" | "polyline" | "polygon")
            ) && !self.in_svg_non_render(d)
        })
    }

    /// Whether `rewrite_inline_svgs` will turn this `<svg>` into a painted
    /// `<img>` on the next layout parse — the serializer-side mirror of that
    /// walk's per-candidate decision: an external sprite ref that resolves in
    /// the primed sheet table (against the document URL, the same base the
    /// rewrite uses), or renderable inline geometry.
    fn svg_will_render(&self, id: NodeId) -> bool {
        if self.is_hidden(id) || self.ancestor_is_svg(id) {
            return false;
        }
        if let Some((file, frag)) = self.svg_sprite_ref(id) {
            return self
                .doc_url
                .as_ref()
                .and_then(|b| b.join(&file).ok())
                .is_some_and(|abs| sprite_has_symbol(abs.as_str(), &frag));
        }
        self.local_svg_use_target(id).is_some() || self.svg_is_renderable(id)
    }

    /// Whether the subtree under `id` (inclusive) will PAINT an icon in the
    /// laid-out page: a visible `<img>` with a source, or an `<svg>` the next
    /// layout parse rewrites into one (`svg_will_render`). Such a clickable
    /// needs no injected text handle — the rendered image is its visible,
    /// selectable content (and its `alt` still carries the accessible name).
    fn subtree_paints_icon(&self, id: NodeId) -> bool {
        std::iter::once(id)
            .chain(self.flat_descendants(id))
            .any(|n| match self.tag_name(n) {
                Some("img") => {
                    self.attr(n, "src").is_some_and(|s| !s.trim().is_empty()) && !self.is_hidden(n)
                }
                Some("svg") => self.svg_will_render(n),
                _ => false,
            })
    }

    /// Whether the subtree already contains a visible native form control.
    ///
    /// HTML form controls have their own user-agent rendering (and the layout
    /// pass turns that rendering into a terminal-cell widget).  An accessible
    /// name is metadata for the accessibility tree, not additional visual
    /// content: emitting the name on a clickable wrapper as well would paint a
    /// second copy beside the control.  Hidden inputs are successful controls
    /// for submission but have no rendered widget, so they do not count here.
    /// (HTML Standard §4.10; Accessible Name and Description Computation §4.)
    fn subtree_paints_native_control(&self, id: NodeId) -> bool {
        std::iter::once(id)
            .chain(self.flat_descendants(id))
            .any(|n| {
                if self.is_hidden(n) {
                    return false;
                }
                match self.tag_name(n) {
                    Some("input") => self
                        .attr(n, "type")
                        .is_none_or(|ty| !ty.eq_ignore_ascii_case("hidden")),
                    Some("select" | "textarea") => true,
                    Some("button") => true,
                    _ => false,
                }
            })
    }

    /// Browser-generated visible content retained by the direct box-tree path
    /// for an otherwise empty living activation surface.
    ///
    /// This is the DOM-owned replacement for `serialize_live_node`'s former
    /// synthetic markup. It does not mutate the document or its accessible
    /// name; layout generates an anonymous UA box from the returned text.
    pub(crate) fn render_clickable_fallback(&self, id: NodeId) -> Option<String> {
        if !self.render_clickable(id) || self.is_contenteditable_host(id) {
            return None;
        }
        let mut ancestor = self.parent_flat(id);
        while let Some(node) = ancestor {
            if self.tag_name(node) == Some("a") || self.render_clickable(node) {
                return None;
            }
            ancestor = self.parent_flat(node);
        }
        if self.tag_name(id) == Some("button") {
            if self.subtree_has_text(id) || self.subtree_paints_icon(id) {
                return None;
            }
            return self
                .icon_glyph(id)
                .or_else(|| {
                    self.attr(id, "aria-label")
                        .or_else(|| self.attr(id, "title"))
                        .or_else(|| self.attr(id, "value"))
                        .and_then(|name| icon_glyph_for(&name.trim().to_ascii_lowercase()))
                })
                .map(str::to_string);
        }
        if self.tag_name(id) == Some("a")
            || self.subtree_has_text(id)
            || self.subtree_paints_icon(id)
            || self.subtree_paints_native_control(id)
        {
            return None;
        }
        if let Some(glyph) = self.icon_glyph(id) {
            return Some(glyph.to_string());
        }
        self.attr(id, "aria-label")
            .or_else(|| self.attr(id, "title"))
            .or_else(|| self.attr(id, "value"))
            .filter(|label| !self.name_is_clipped_out(id, label))
            .filter(|_| !self.is_overlay_scrim(id))
            .map(|label| format!("[{label}]"))
    }

    /// If this `<svg>`'s geometry lives in an EXTERNAL sprite sheet — a
    /// `<use href="file.svg#id">` (or legacy `xlink:href`) — return the sheet
    /// file's raw href and the fragment id. A same-document `<use href="#id">`
    /// (empty file part) is NOT a sprite: its `<symbol>`/`<defs>` target rides
    /// along in the same serialized svg, so it flows the normal inline path.
    fn svg_sprite_ref(&self, id: NodeId) -> Option<(String, String)> {
        for d in self.descendants(id) {
            if self.tag_name(d) != Some("use") {
                continue;
            }
            let Some(href) = self
                .attr(d, "href")
                .or_else(|| self.attr(d, "xlink:href"))
                .map(str::trim)
                .filter(|s| !s.is_empty())
            else {
                continue;
            };
            let Some((file, frag)) = href.split_once('#') else {
                continue;
            };
            if !file.is_empty() && !frag.is_empty() {
                return Some((file.to_string(), frag.to_string()));
            }
        }
        None
    }

    /// Absolute external SVG documents referenced by connected `<use>`
    /// elements, in document order. SVG 2 §5.6 requires an external `use`
    /// reference to be processed when the element becomes connected (and when
    /// its href changes); callers use this list to fetch the immutable source
    /// document before deriving the next pixel layout. Resolution uses the
    /// document base URL, including HTML's first `<base href>`.
    pub fn external_svg_use_sheets(&self, base: &url::Url) -> Vec<url::Url> {
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for id in self.flat_descendants(DOCUMENT) {
            if self.tag_name(id) != Some("use") {
                continue;
            }
            let Some(href) = self
                .attr(id, "href")
                .or_else(|| self.attr(id, "xlink:href"))
                .map(str::trim)
                .filter(|href| !href.is_empty())
            else {
                continue;
            };
            let Some((file, fragment)) = href.split_once('#') else {
                continue;
            };
            if file.is_empty() || fragment.is_empty() {
                continue;
            }
            let Some(url) = base.join(file).ok() else {
                continue;
            };
            if seen.insert(url.to_string()) {
                out.push(url);
            }
        }
        out
    }

    /// Whether a node sits inside a non-rendered SVG container (`<defs>` and
    /// friends define reusable shapes; they paint nothing on their own).
    fn in_svg_non_render(&self, id: NodeId) -> bool {
        let mut cur = self.nodes[id].parent;
        while let Some(p) = cur {
            match self.tag_name(p) {
                Some("defs" | "symbol" | "clipPath" | "mask" | "pattern" | "marker") => {
                    return true;
                }
                Some("svg") => return false,
                _ => cur = self.nodes[p].parent,
            }
        }
        false
    }

    fn ancestor_is_svg(&self, id: NodeId) -> bool {
        let mut cur = self.nodes[id].parent;
        while let Some(p) = cur {
            if self.tag_name(p) == Some("svg") {
                return true;
            }
            cur = self.nodes[p].parent;
        }
        false
    }

    /// The SVG's accessible name for `<img alt>` (a fallback shown only if the
    /// decode fails): `aria-label`, else its `<title>` text, else empty.
    fn svg_accessible_name(&self, id: NodeId) -> String {
        if let Some(l) = self
            .attr(id, "aria-label")
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return l.to_string();
        }
        for d in self.descendants(id) {
            if self.tag_name(d) == Some("title") {
                let t = self.text_content(d).trim().to_string();
                if !t.is_empty() {
                    return t;
                }
            }
        }
        String::new()
    }

    /// `js_serialization`: when true, run HTML's fragment serialization surface
    /// used by innerHTML/outerHTML, including inert, hidden, and template
    /// content. When false this is the presentation serializer: non-rendered
    /// nodes are deliberately omitted before layout.
    fn serialize_node_inner(
        &self,
        id: NodeId,
        host: Option<NodeId>,
        js_serialization: bool,
        out: &mut String,
    ) {
        match &self.nodes[id].data {
            NodeData::Document | NodeData::Fragment => {
                for c in self.child_iter(id) {
                    self.serialize_node_inner(c, host, js_serialization, out);
                }
            }
            NodeData::Doctype => {}
            // Comments survive round-trips (Lit's markers) and the
            // layout pass ignores them.
            NodeData::Comment(t) => {
                out.push_str("<!--");
                out.push_str(&t.replace("--", "- -"));
                out.push_str("-->");
            }
            NodeData::Text(t) => {
                // HTML §13.3, serializing HTML fragments: text whose parent is
                // a raw-text/script-data element is appended literally. This
                // is observable through `script.innerHTML`; template engines
                // commonly store markup in `<script type="text/template">`
                // and expect the getter to return markup, not `&lt;...&gt;`.
                let raw_text_parent = js_serialization
                    && self.nodes[id].parent.is_some_and(|parent| {
                        matches!(
                            self.tag_name(parent),
                            Some(
                                "style"
                                    | "script"
                                    | "xmp"
                                    | "iframe"
                                    | "noembed"
                                    | "noframes"
                                    | "plaintext"
                                    | "noscript"
                            )
                        )
                    });
                if raw_text_parent {
                    out.push_str(t);
                } else {
                    out.push_str(&escape_text(t));
                }
            }
            NodeData::Element { name, attrs, .. } => {
                let tag: &str = &name.local;
                // A `<template>` is dropped from the layout serializer (inert),
                // but the JS path serializes it WITH its content fragment as
                // children — handled before the is_hidden gate because a
                // template is UA `display:none` yet a browser always serializes
                // its contents.
                if tag == "template" {
                    if js_serialization {
                        out.push('<');
                        out.push_str(tag);
                        self.write_attrs(id, attrs, &mut |_, _| None, out);
                        out.push('>');
                        for c in self.child_iter(self.content_target(id)) {
                            self.serialize_node_inner(c, host, true, out);
                        }
                        out.push_str("</");
                        out.push_str(tag);
                        out.push('>');
                    }
                    return;
                }
                if !js_serialization
                    && (matches!(tag, "script" | "noscript" | "style") || self.is_hidden(id))
                {
                    return;
                }
                // An iframe/frame is a replaced viewport for a distinct child
                // navigable (HTML §4.8.5). Preserve that outer box even before
                // content realizes, and preserve the nested BODY as a separate
                // formatting box when it does. Putting BODY's children directly
                // in the viewport loses its display/flex/alignment semantics.
                if !js_serialization && matches!(tag, "iframe" | "frame") {
                    out.push_str("<div data-trust-frame=\"\" style=\"");
                    out.push_str(&escape_attr(&self.serialized_frame_wrapper_style(id)));
                    out.push_str("\">");
                    if let Some(body) = self.frame_body(id) {
                        self.write_serialized_frame_body_open(body, out);
                        for c in self.child_iter(body) {
                            self.serialize_node_inner(c, None, js_serialization, out);
                        }
                        out.push_str("</div>");
                    }
                    out.push_str("</div>");
                    return;
                }
                // <slot> inside a shadow tree: project the host's light
                // children (or the slot's own fallback content).
                if !js_serialization
                    && tag == "slot"
                    && let Some(h) = host
                {
                    let assigned = self.slot_assigned(h, self.attr(id, "name"));
                    if assigned.is_empty() {
                        for c in self.child_iter(id) {
                            self.serialize_node_inner(c, host, js_serialization, out);
                        }
                    } else {
                        for c in assigned.into_iter().flat_map(|c| {
                            if self.tag_name(c) == Some("slot") {
                                self.flat_slot_nodes(c)
                            } else {
                                vec![c]
                            }
                        }) {
                            self.serialize_node_inner(c, None, js_serialization, out);
                        }
                    }
                    return;
                }
                out.push('<');
                out.push_str(tag);
                self.write_attrs(id, attrs, &mut |_, _| None, out);
                out.push('>');
                if VOID_ELEMENTS.contains(&tag) {
                    return;
                }
                // A shadow root renders IN PLACE of the light children
                // (flattened — text extraction wants content, not
                // composition fidelity).
                if !js_serialization && let Some(root) = self.shadow_root(id) {
                    for c in self.child_iter(root) {
                        self.serialize_node_inner(c, Some(id), js_serialization, out);
                    }
                } else {
                    for c in self.child_iter(id) {
                        self.serialize_node_inner(c, host, js_serialization, out);
                    }
                }
                out.push_str("</");
                out.push_str(tag);
                out.push('>');
            }
        }
    }

    /// Serialize for a LIVING page: like `serialize`, but elements in
    /// `clickable` become followable — non-anchors are wrapped in
    /// `<a href="x-trust-js:<id>:">` markers (the form-marker trick),
    /// and live anchors get their href rewritten to
    /// `x-trust-js:<id>:<original-href>` so clicks route through the
    /// page actor (which navigates only if not defaultPrevented).
    pub fn serialize_live(
        &self,
        root: NodeId,
        clickable: &std::collections::HashSet<NodeId>,
    ) -> String {
        let mut out = String::new();
        self.serialize_live_node(root, None, clickable, false, &mut out);
        out
    }

    fn serialize_live_node(
        &self,
        id: NodeId,
        host: Option<NodeId>,
        clickable: &std::collections::HashSet<NodeId>,
        in_anchor: bool,
        out: &mut String,
    ) {
        let NodeData::Element { name, attrs, .. } = &self.nodes[id].data else {
            return self.serialize_node_with(
                id,
                &mut |c, o| self.serialize_live_node(c, host, clickable, in_anchor, o),
                out,
            );
        };
        let tag: &str = &name.local;
        if matches!(tag, "script" | "noscript" | "template" | "style") || self.is_hidden(id) {
            return;
        }
        // Retain the iframe's replaced viewport and the nested document BODY as
        // distinct boxes (see the static serializer + `frame_body`). This also
        // keeps an empty/unrealized iframe's normal replaced-element footprint.
        if matches!(tag, "iframe" | "frame") {
            out.push_str("<div data-trust-frame=\"\" style=\"");
            out.push_str(&escape_attr(&self.serialized_frame_wrapper_style(id)));
            out.push_str("\">");
            if let Some(body) = self.frame_body(id) {
                self.write_serialized_frame_body_open(body, out);
                for c in self.child_iter(body) {
                    // A child navigable starts a new document/tree scope. It
                    // cannot inherit a shadow host or anchor context from the
                    // element that embeds it in the parent document.
                    self.serialize_live_node(c, None, clickable, false, out);
                }
                out.push_str("</div>");
            }
            out.push_str("</div>");
            return;
        }
        if tag == "slot"
            && let Some(h) = host
        {
            let assigned = self.slot_assigned(h, self.attr(id, "name"));
            if assigned.is_empty() {
                for c in self.child_iter(id) {
                    self.serialize_live_node(c, host, clickable, in_anchor, out);
                }
            } else {
                for c in assigned.into_iter().flat_map(|c| {
                    if self.tag_name(c) == Some("slot") {
                        self.flat_slot_nodes(c)
                    } else {
                        vec![c]
                    }
                }) {
                    self.serialize_live_node(c, None, clickable, in_anchor, out);
                }
            }
            return;
        }
        let is_click = clickable.contains(&id);
        let is_anchor = tag == "a";
        // A non-anchor clickable becomes a followable `<a>` marker — BUT
        // never nest one inside an existing anchor. An `<a>` inside an `<a>`
        // is invalid HTML; when the app re-parses this serialized output for
        // layout, html5ever's adoption agency SPLITS the outer anchor into
        // empty fragments that still carry its `aria-label`, which then leaks
        // as duplicated link text (archive.org tiles: a `<button class=info>`
        // wrapped inside the tile's own `<a>` printed the title three times).
        // Inside an anchor the clickable simply inherits that anchor's link.
        // A contenteditable host is routed to the editable-field path (it gets a
        // `data-trust-node` below and the form walk binds it), so never wrap it
        // as a JsClick — that would make it "follow" instead of "edit" even
        // though rich editors also register click listeners on their root.
        // Native buttons already generate their own CSS box and carry the
        // actor id below. Wrapping one in an invented anchor changes flex item
        // identity: the anchor stretches while the button inside remains at
        // its intrinsic height (YouTube's 40px search control). Keep the button
        // itself as the flex/grid item; `data-trust-click` supplies the
        // terminal Link::JsClick compatibility path.
        // A button already inside an anchor inherits that anchor's activation
        // surface. Giving it an independent actor marker would recreate the
        // nested interactive-target ambiguity that avoiding the wrapper was
        // meant to solve.
        let direct_button = is_click && tag == "button" && !in_anchor;
        let wrap = is_click
            && !is_anchor
            && !in_anchor
            && !direct_button
            && !self.is_contenteditable_host(id);
        // Some component libraries install an icon's SVG in a later custom-
        // element reaction.  Until that authored child exists, preserve an
        // icon-only native button as a native button: its accessible name stays
        // metadata (AccName §4), while a recognized control action may receive
        // a compact UA pictogram inside the button's own content box.  Never
        // place that fallback beside the button, and never synthesize the full
        // accessible name as visible DOM text.
        let button_icon_fallback =
            (direct_button && !self.subtree_has_text(id) && !self.subtree_paints_icon(id))
                .then(|| {
                    self.icon_glyph(id).or_else(|| {
                        self.attr(id, "aria-label")
                            .or_else(|| self.attr(id, "title"))
                            .or_else(|| self.attr(id, "value"))
                            .and_then(|name| icon_glyph_for(&name.trim().to_ascii_lowercase()))
                    })
                })
                .flatten();
        // Whether this element opens an anchor context for its descendants:
        // a real `<a>`, the wrapper we just emitted, or an already-open one.
        let child_in_anchor = in_anchor || is_anchor || is_click;
        if wrap {
            out.push_str(&format!("<a href=\"x-trust-js:{id}:\">"));
            // An icon-only clickable would render as an empty (and so
            // unselectable) link: give it a visible handle WHEN it carries
            // meaning. An icon control (an `<svg>`/`<use>` Font-Awesome-style
            // glyph — the dominant web icon idiom) shows the icon's GLYPH; a
            // named-but-glyphless one shows its accessible name. An element with
            // NO text, NO icon glyph, and NO accessible name (aria-label/title/
            // value) conveys nothing to a text reader — its meaning lived only
            // in CSS (a carousel's pagination dots are click `<div>`s drawn as
            // background-coloured pills; Steam paints ~12 per carousel). Render
            // NOTHING rather than a marker per anonymous control: the empty
            // wrapper yields no layout item, so it neither shows nor steals a
            // selection stop. (Was a `·` marker — fine for a lone control,
            // debris in a group.) A clickable whose icon actually PAINTS (a
            // visible `<img>`, or an `<svg>` the layout parse rewrites into
            // one) needs no handle either — injecting one doubles the control
            // (ChatGPT's composer grew a `[Start dictation]` label beside the
            // rendered mic icon once sprite icons started rasterizing).
            if !self.subtree_has_text(id)
                && !self.subtree_paints_icon(id)
                && !self.subtree_paints_native_control(id)
            {
                if let Some(glyph) = self.icon_glyph(id) {
                    out.push_str(glyph);
                } else if let Some(label) = self
                    .attr(id, "aria-label")
                    .or_else(|| self.attr(id, "title"))
                    .or_else(|| self.attr(id, "value"))
                    // A control the author CLIPPED to an icon-sized box never
                    // paints its accessible NAME — a browser shows only the icon.
                    // Honoring that clip (don't surface a name wider than its
                    // definite `width` under `overflow:hidden/clip`) is what stops
                    // Twitch's per-message reply button — `aria-label="Click to
                    // reply to @user"` in a `width:3.2rem;overflow:hidden` box —
                    // from spamming every chat line. The empty wrapper then yields
                    // no layout item (same as an anonymous control).
                    .filter(|l| !self.name_is_clipped_out(id, l))
                    // A full-bleed positioned scrim (a click-to-play overlay)
                    // paints nothing in a browser — don't surface its name.
                    .filter(|_| !self.is_overlay_scrim(id))
                {
                    out.push('[');
                    out.push_str(&escape_text(label));
                    out.push(']');
                }
            }
        }
        out.push('<');
        out.push_str(tag);
        self.write_attrs(
            id,
            attrs,
            &mut |name, value| {
                (is_click && is_anchor && name == "href")
                    .then(|| format!("x-trust-js:{id}:{value}"))
            },
            out,
        );
        // A live anchor that never had an href still needs the marker.
        if is_click && is_anchor && self.attr(id, "href").is_none() {
            out.push_str(&format!(" href=\"x-trust-js:{id}:\""));
        }
        if direct_button {
            // Retain the established actor-marker encoding as metadata so
            // snapshot consumers can discover one activation namespace for
            // both anchors and direct native controls.  A data attribute has
            // no box-tree effect; unlike the former anchor wrapper it cannot
            // alter flex/grid item identity.
            out.push_str(&format!(" data-trust-click=\"x-trust-js:{id}:\""));
        }
        // Every hyperlink on a live page must run DOM click dispatch before
        // its HTML activation behavior. This is required even without an
        // authored listener: `target`, `<base target>`, `preventDefault()` on
        // an ancestor, and named child navigables are resolved by the resident
        // document, not by the presentation adapter's plain URL fallback.
        if is_anchor && !is_click && self.attr(id, "href").is_some() {
            out.push_str(&format!(" data-trust-click=\"x-trust-js:{id}:\""));
        }
        // The app re-parses this serialized HTML into a fresh layout DOM, so
        // form controls AND vertical scroll containers need an explicit pointer
        // back to the resident page actor's original node ids (form values /
        // the region's scroll position round-trip by it).
        let is_scroll = self.is_scroll_container(id);
        let is_hscroll = self.is_hscroll_container(id);
        // Bake the actor node id on every element the app re-correlates after a
        // re-parse: form controls + vertical scroll containers (values / region
        // scroll round-trip by it) AND every independent-formatting-context
        // boundary (incremental-layout contract §14 step 3). An IFC boundary is a
        // box whose interior can't reflow anything outside it, so the app can
        // re-lay ONLY that subtree and splice it back — but only if it can map the
        // patched fragment to the cached box by this id. IFC roots are SPARSE (not
        // every block), so the HTML/parse bloat stays bounded (§3).
        if matches!(tag, "form" | "input" | "button" | "select" | "textarea")
            || self.is_contenteditable_host(id)
            || is_scroll
            || is_hscroll
            || self.establishes_independent_formatting_context(id)
        {
            out.push_str(&format!(" data-trust-node=\"{id}\""));
        }
        // A hover-listener host gets its OWN marker (never data-trust-node —
        // that attribute's presence gates incremental-layout boundaries and
        // region correlation, and hover hosts are often ordinary flex divs
        // that must not inflate those sets). The app's layout threads this id
        // onto the items flowed beneath it, so a hovered cell resolves back to
        // the actor node whose listeners (or whose ancestors', via bubbling)
        // should hear the pointer.
        if self.hover_hosts.contains(&id) {
            out.push_str(&format!(" data-trust-hover=\"{id}\""));
        }
        // Paint-only hover selector subjects are correlated independently of
        // layout boundaries. Their box geometry does not change, so a native
        // frontend may copy freshly baked paint declarations onto its retained
        // presentation node and replay paint over existing fragments.
        if self.paint_patch_hosts.contains(&id) {
            out.push_str(&format!(" data-trust-paint-node=\"{id}\""));
        }
        // Popover visibility/top-layer membership is DOM state, not reflected
        // by the `popover` content attribute. Carry its ordered-set position
        // into the presentation DOM so layout can generate the box as a root
        // sibling and paint it after the document (CSS Position 4 §3).
        if let Some(order) = self.popover_top_layer_order(id) {
            out.push_str(&format!(" data-trust-popover-open=\"{order}\""));
        }
        // A scroll container's current `scrollTop` signal (CSSOM View) rides the
        // HTML in CSS pixels so it survives the live snapshot/re-parse exactly
        // like a baked form value. A terminal consumer quantizes this value only
        // when constructing its Region; graphical consumers keep it unchanged.
        if is_scroll
            && let Some(sb) = self.scroll_state.get(&id)
            && sb.top >= 1.0
        {
            out.push_str(&format!(" data-trust-scroll-top=\"{}\"", sb.top));
        }
        // The horizontal half of the same CSSOM state. This lets the retained
        // terminal carousel seed its strip offset after a mutation render; the
        // graphical frontend keeps the unquantized value in InteractionState.
        if is_hscroll
            && let Some(sb) = self.scroll_state.get(&id)
            && sb.left >= 1.0
        {
            out.push_str(&format!(" data-trust-scroll-left=\"{}\"", sb.left));
        }
        out.push('>');
        if !VOID_ELEMENTS.contains(&tag) {
            if let Some(glyph) = button_icon_fallback {
                // This is visual fallback content only; the button's authored
                // accessible name remains the name. Use an ordinary outline
                // font and a monochrome glyph rather than a color-emoji font,
                // whose bitmap/COLR payload is not an SVG/image resource and
                // therefore has no outline in the retained text display list.
                // The predicates above prove that the authored subtree has no
                // text or paintable image/SVG, so replacing it also keeps the
                // fallback centered instead of laying it beside an empty 24px
                // component placeholder.
                out.push_str(
                    "<span aria-hidden=\"true\" data-trust-ua-icon=\"\" \
                     style=\"display:inline-block;width:24px;height:24px;font-size:30px;\
                     font-family:sans-serif;line-height:24px;text-align:center\">",
                );
                out.push_str(glyph);
                out.push_str("</span>");
            } else if let Some(root) = self.shadow_root(id) {
                for c in self.child_iter(root) {
                    self.serialize_live_node(c, Some(id), clickable, child_in_anchor, out);
                }
            } else {
                for c in self.child_iter(id) {
                    self.serialize_live_node(c, host, clickable, child_in_anchor, out);
                }
            }
            out.push_str("</");
            out.push_str(tag);
            out.push('>');
        }
        if wrap {
            out.push_str("</a>");
        }
    }

    /// Write an element's attributes, baking the cascaded `display` into
    /// its `style` so the re-parsed layout arena flows it the way the
    /// engine (which has the external sheets) computed. `rewrite` lets the
    /// live serializer substitute an attribute value (anchor href markers).
    fn write_attrs(
        &self,
        id: NodeId,
        attrs: &[Attribute],
        rewrite: &mut dyn FnMut(&str, &str) -> Option<String>,
        out: &mut String,
    ) {
        self.write_attrs_with_extra_style(id, attrs, rewrite, "", out);
    }

    /// `write_attrs` with additional declarations that must share its one
    /// serialized `style` attribute. XML forbids duplicate attributes, so an
    /// isolated SVG cannot append a second attribute after the ordinary baked
    /// layout declarations have already synthesized the first one.
    fn write_attrs_with_extra_style(
        &self,
        id: NodeId,
        attrs: &[Attribute],
        rewrite: &mut dyn FnMut(&str, &str) -> Option<String>,
        extra_style: &str,
        out: &mut String,
    ) {
        // Bake the cascaded box/layout properties (the engine has the
        // sheets; the re-parsed layout arena doesn't) into the element's
        // inline style. `display:none` is dropped outright (never baked, see the
        // skip below); `visibility:hidden` IS kept + baked now (paint
        // suppression, Phase 2) so the re-parse paints it blank.
        let mut bake = self.baked_element_style(id, false);
        append_style(&mut bake, extra_style);
        let mut style_done = false;
        for a in attrs {
            let name: &str = &a.name.local;
            let replaced = rewrite(name, &a.value);
            let value = replaced.as_deref().unwrap_or(&a.value);
            out.push(' ');
            out.push_str(name);
            out.push_str("=\"");
            out.push_str(&escape_attr(value));
            if name == "style" && !bake.is_empty() {
                if !value.trim().is_empty() && !value.trim_end().ends_with(';') {
                    out.push(';');
                }
                out.push_str(&escape_attr(&bake));
                style_done = true;
            } else if name == "style" {
                style_done = true;
            }
            out.push('"');
        }
        if !bake.is_empty() && !style_done {
            out.push_str(" style=\"");
            out.push_str(&escape_attr(&bake));
            out.push('"');
        }
        // Bake generated content (the layout arena has no `<style>` to
        // re-cascade `::before`/`::after`); the layout reads these attrs.
        for (which, attr) in [
            (PseudoEl::Before, "data-trust-before"),
            (PseudoEl::After, "data-trust-after"),
        ] {
            if let Some(t) = self.pseudo_content(id, which) {
                out.push(' ');
                out.push_str(attr);
                out.push_str("=\"");
                out.push_str(&escape_attr(&t));
                out.push('"');

                // CSS Pseudo 4 §4.1: ::before/::after are fully styleable
                // child boxes. The layout arena re-parses this snapshot without
                // the resident document's stylesheets, so preserve the pseudo's
                // own box declarations just as `bake` above preserves the
                // originating element's declarations. This is load-bearing for
                // `content:"";display:block;padding-top:56.25%` ratio boxes:
                // retaining the empty content but losing its padding would still
                // collapse the box and its absolutely positioned children.
                let pseudo_style = self.baked_pseudo_style(id, which);
                if !pseudo_style.is_empty() {
                    let style_attr = match which {
                        PseudoEl::Before => "data-trust-before-style",
                        PseudoEl::After => "data-trust-after-style",
                    };
                    out.push(' ');
                    out.push_str(style_attr);
                    out.push_str("=\"");
                    out.push_str(&escape_attr(&pseudo_style));
                    out.push('"');
                }
            }
        }
        // Bake the clearfix signal for the same reason: the layout re-parses
        // this HTML with no `<style>`, so a `::after{clear:both}` rule (which
        // can't live in an inline `style`) would otherwise be lost and a float
        // grid would leak past its row. (`has_clearing_pseudo` reads the rule
        // here, the attribute at layout time.)
        if self.has_clearing_pseudo(id) {
            out.push_str(" data-trust-clearfix=\"\"");
        }
    }

    /// The declarations that must cross from the resident cascade into the
    /// stylesheet-free presentation arena. `materialize_inherited` is used for
    /// a nested document BODY because its HTML ancestor cannot survive the
    /// parent-document HTML reparse; normal elements retain inheritance through
    /// their serialized ancestors and therefore bake only direct winners.
    fn baked_element_style(&self, id: NodeId, materialize_inherited: bool) -> String {
        let mut bake = String::new();
        for definition in PROPS.iter().filter(|definition| definition.baked) {
            let prop = definition.name;
            let value = if materialize_inherited && definition.inherited {
                self.computed_value_resolved(id, prop)
            } else {
                self.cascaded(id, prop)
                    .and_then(|value| self.resolve_pending_shorthand(id, prop, &value))
                    .map(|value| self.resolve_vars(id, &value))
            };
            let Some(value) = value else {
                continue;
            };
            if prop == "display" && value == "none" {
                continue;
            }
            // An undefined `var()` with no fallback resolves to nothing — do
            // not bake an empty declaration.
            if value.trim().is_empty() {
                continue;
            }
            bake.push_str(prop);
            bake.push(':');
            bake.push_str(&value);
            bake.push(';');
        }
        // CSS Color 4 §3.3 requires opacity to be applied to the element as a
        // composited group. Preserve the exact computed alpha rather than
        // reducing it to a visible/hidden bit. `effective_opacity` also folds
        // in the limited fill-mode animation state supported by this engine.
        if self.cascaded(id, "opacity").is_some() {
            bake.push_str("opacity:");
            bake.push_str(&self.effective_opacity(id).to_string());
            bake.push(';');
        }
        bake
    }

    /// Serialize the pseudo-element's own tracked declarations for the
    /// stylesheet-free layout snapshot. Only declarations targeting the
    /// pseudo are emitted; inherited values continue to flow from the
    /// originating element in the box tree.
    fn baked_pseudo_style(&self, id: NodeId, which: PseudoEl) -> String {
        let mut out = String::new();
        for prop in PROPS.iter().filter(|p| p.baked).map(|p| p.name) {
            let Some(raw) = self.pseudo_style(id, which, prop) else {
                continue;
            };
            let value = self.resolve_vars(id, &raw);
            if value.trim().is_empty() {
                continue;
            }
            out.push_str(prop);
            out.push(':');
            out.push_str(&value);
            out.push(';');
        }
        if let Some(raw) = self.pseudo_style(id, which, "opacity") {
            let value = self.resolve_vars(id, &raw);
            if !value.trim().is_empty() {
                out.push_str("opacity:");
                out.push_str(&value);
                out.push(';');
            }
        }
        out
    }

    /// Non-element serialization shared between the plain and live
    /// serializers: documents/fragments recurse via `kids`, text
    /// escapes, the rest vanish.
    fn serialize_node_with(
        &self,
        id: NodeId,
        kids: &mut dyn FnMut(NodeId, &mut String),
        out: &mut String,
    ) {
        match &self.nodes[id].data {
            NodeData::Document | NodeData::Fragment => {
                for c in self.child_iter(id) {
                    kids(c, out);
                }
            }
            NodeData::Doctype => {}
            NodeData::Comment(t) => {
                out.push_str("<!--");
                out.push_str(&t.replace("--", "- -"));
                out.push_str("-->");
            }
            NodeData::Text(t) => out.push_str(&escape_text(t)),
            NodeData::Element { .. } => unreachable!("elements handled by callers"),
        }
    }

    /// All `<script>` elements in document order, as (src-attr, inline
    /// source, type-attr) — the execution schedule for the active JavaScript
    /// backend.
    /// Every `<script>` in document order: `(src, inline text, type, node)`.
    /// The node id lets the runner expose `document.currentScript` while a
    /// classic script executes.
    pub fn scripts(&self) -> Vec<(Option<String>, String, Option<String>, NodeId)> {
        self.descendants(DOCUMENT)
            .filter(|&d| self.tag_name(d) == Some("script"))
            .map(|d| {
                (
                    self.attr(d, "src").map(str::to_string),
                    self.text_content(d),
                    self.attr(d, "type").map(str::to_string),
                    d,
                )
            })
            .collect()
    }

    /// querySelector(All): match descendants of `root` against a
    /// selector list, document order.
    pub fn query(&self, root: NodeId, selectors: &SelectorList, first_only: bool) -> Vec<NodeId> {
        let mut out = Vec::new();
        for d in self.descendants(root) {
            // ParentNode queries only return elements. Avoid entering the full
            // selector matcher for text/comments in mixed-content trees.
            let Some(tag) = self.tag_name(d) else {
                continue;
            };
            // Selector matching is right-to-left. When every selector-list
            // branch has an explicit rightmost type selector, reject other
            // element types before entering compound/pseudo/ancestor matching.
            if !selectors.might_match_subject_tag(tag) {
                continue;
            }
            // `:scope` in the selector resolves to this query root.
            if self.matches_scoped(d, selectors, Some(root)) {
                out.push(d);
                if first_only {
                    break;
                }
            }
        }
        out
    }

    pub fn matches(&self, id: NodeId, selectors: &SelectorList) -> bool {
        self.matches_scoped(id, selectors, None)
    }

    fn matches_scoped(&self, id: NodeId, selectors: &SelectorList, scope: Option<NodeId>) -> bool {
        selectors
            .0
            .iter()
            .any(|c| self.matches_complex(id, &c.0, scope))
    }

    fn matches_complex(
        &self,
        id: NodeId,
        parts: &[(Combinator, Compound)],
        scope: Option<NodeId>,
    ) -> bool {
        let Some(((comb, compound), rest)) = parts.split_last() else {
            return false;
        };
        if !self.matches_compound(id, compound, scope) {
            return false;
        }
        if rest.is_empty() {
            return true;
        }
        match comb {
            Combinator::Child => self
                .selector_parent(id)
                .is_some_and(|p| self.matches_complex(p, rest, scope)),
            Combinator::Descendant | Combinator::None => {
                let mut up = self.selector_parent(id);
                while let Some(a) = up {
                    if self.matches_complex(a, rest, scope) {
                        return true;
                    }
                    up = self.selector_parent(a);
                }
                false
            }
            Combinator::NextSibling => self
                .prev_element_sibling(id)
                .is_some_and(|s| self.matches_complex(s, rest, scope)),
            Combinator::SubsequentSibling => {
                let mut sib = self.prev_element_sibling(id);
                while let Some(s) = sib {
                    if self.matches_complex(s, rest, scope) {
                        return true;
                    }
                    sib = self.prev_element_sibling(s);
                }
                false
            }
        }
    }

    /// Parent used by Selectors matching. CSS Shadow 1 §3.2/§4.1 draws an
    /// important boundary here: selectors operate on the DOM/light tree,
    /// while inheritance and box construction operate on the flat tree. A
    /// slotted light-DOM child therefore remains a child of its shadow host
    /// for `>`/descendant selectors even though [`style_parent`] correctly
    /// returns the slot for inheritance.
    fn selector_parent(&self, id: NodeId) -> Option<NodeId> {
        let parent = self.nodes[id].parent?;
        if matches!(self.tag_name(parent), Some("iframe" | "frame"))
            && self.frame_body(parent).is_some()
        {
            return None;
        }
        self.tag_name(parent).is_some().then_some(parent)
    }

    /// The nearest preceding sibling that is an element (skips text/comments).
    fn prev_element_sibling(&self, id: NodeId) -> Option<NodeId> {
        let mut p = self.nodes[id].prev_sibling;
        while let Some(s) = p {
            if self.tag_name(s).is_some() {
                return Some(s);
            }
            p = self.nodes[s].prev_sibling;
        }
        None
    }

    /// `:empty` — the element has no element children and no text children
    /// with non-whitespace content (comments don't count).
    fn is_element_empty(&self, id: NodeId) -> bool {
        let mut child = self.nodes[id].first_child;
        while let Some(c) = child {
            match &self.nodes[c].data {
                NodeData::Element { .. } => return false,
                NodeData::Text(t) if !t.chars().all(char::is_whitespace) => return false,
                _ => {}
            }
            child = self.nodes[c].next_sibling;
        }
        true
    }

    /// The element's 1-based position among its parent's element children
    /// (`of_type`: only same-tag siblings; `from_end`: counted from the
    /// last; `of`: only siblings matching the `of S` selector list — a
    /// subject that doesn't match S itself has no ordinal). `None` if it has
    /// no parent or isn't an element. One sibling pass, no Vec: count
    /// qualifying siblings and note our own ordinal.
    fn nth_position(
        &self,
        id: NodeId,
        of_type: bool,
        from_end: bool,
        of: Option<&[Complex]>,
        scope: Option<NodeId>,
    ) -> Option<i32> {
        let parent = self.nodes[id].parent?;
        let my_tag = self.tag_name(id)?;
        let mut count = 0i32;
        let mut ordinal = None;
        let mut child = self.nodes[parent].first_child;
        while let Some(c) = child {
            if let Some(t) = self.tag_name(c)
                && (!of_type || t == my_tag)
                && of.is_none_or(|sels| sels.iter().any(|cx| self.matches_complex(c, &cx.0, scope)))
            {
                count += 1;
                if c == id {
                    ordinal = Some(count);
                }
            }
            child = self.nodes[c].next_sibling;
        }
        let ordinal = ordinal?;
        Some(if from_end {
            count - ordinal + 1
        } else {
            ordinal
        })
    }

    fn matches_structural(&self, id: NodeId, st: &Structural, scope: Option<NodeId>) -> bool {
        match st {
            Structural::Empty => self.is_element_empty(id),
            Structural::Nth {
                nth,
                of_type,
                from_end,
                of,
            } => self
                .nth_position(id, *of_type, *from_end, of.as_deref(), scope)
                .is_some_and(|pos| nth.matches(pos)),
        }
    }

    fn matches_compound(&self, id: NodeId, c: &Compound, scope: Option<NodeId>) -> bool {
        if c.never {
            return false;
        }
        // `:host` targets the shadow host, which is NOT inside the shadow tree
        // these rules are scoped to — it's matched specially in `cascaded`
        // (`host_rule_matches`), never against in-scope elements here.
        if c.host {
            return false;
        }
        // `::slotted()` is matched by `slotted_rule_matches` against the
        // assigned light-DOM node and its originating slot; it is not a
        // normal selector on either tree's ordinary element walk.
        if c.slotted.is_some() {
            return false;
        }
        // `:scope` matches only the query root (None in the cascade → never).
        if c.scope && scope != Some(id) {
            return false;
        }
        // Live `:hover`: on the chain under the terminal's pointer. The
        // per-element match memos are epoch-keyed and `set_hover_chain` bumps
        // the epoch whenever rendering could change, so a stale chain can
        // never serve from cache.
        if c.hover && !self.hover_chain.contains(&id) {
            return false;
        }
        // Live `:popover-open`: the element's popover is currently showing.
        // `set_popover_open` bumps the epoch, so the match memos stay fresh.
        if c.popover_open && !self.is_popover_showing(id) {
            return false;
        }
        let Some(tag) = self.tag_name(id) else {
            return false;
        };
        // `:root` is the document root element (`<html>` in HTML).
        if c.root && tag != "html" {
            return false;
        }
        if let Some(want) = &c.tag
            && want != "*"
            && want != tag
        {
            return false;
        }
        if let Some(want) = &c.id
            && self.attr(id, "id") != Some(want.as_str())
        {
            return false;
        }
        if !c.classes.is_empty() {
            // No token Vec: this runs per candidate rule per element (the
            // rule-hash's hottest inner test), and compounds rarely want
            // more than one or two classes.
            let classes = self.attr(id, "class").unwrap_or("");
            if !c
                .classes
                .iter()
                .all(|w| classes.split_ascii_whitespace().any(|t| t == w))
            {
                return false;
            }
        }
        for sel in &c.attrs {
            match self.attr(id, &sel.name) {
                None => return false,
                Some(got) => {
                    if !sel.matches(got) {
                        return false;
                    }
                }
            }
        }
        if !c
            .structural
            .iter()
            .all(|st| self.matches_structural(id, st, scope))
        {
            return false;
        }
        if !c.states.iter().all(|st| self.matches_state(id, tag, st)) {
            return false;
        }
        // `:is()`/`:where()`: each invocation's group must have at least one
        // matching argument (full complex selectors, this element as the
        // subject). An empty (all-invalid, forgiving-dropped) group matches
        // nothing.
        if !c.selects.iter().all(|(group, _)| {
            group
                .iter()
                .any(|cx| self.matches_complex(id, &cx.0, scope))
        }) {
            return false;
        }
        // `:has(...)`: each invocation's forgiving list must have at least one
        // relative selector satisfied by an element in this element's subtree /
        // following-sibling forest. An empty (all-invalid) group matches
        // nothing. (Placed after the cheap own-element tests so `:has()`'s
        // subtree walk runs only for elements that already match the subject.)
        if !c
            .has
            .iter()
            .all(|group| group.iter().any(|h| self.matches_has(id, h)))
        {
            return false;
        }
        c.nots
            .iter()
            .flatten()
            .all(|n| !self.matches_compound(id, n, scope))
    }

    /// Evaluate one element-state pseudo-class (see [`StatePseudo`]) against
    /// the arena. `tag` is the element's tag name (the caller already has it).
    fn matches_state(&self, id: NodeId, tag: &str, st: &StatePseudo) -> bool {
        match st {
            StatePseudo::AnyLink => matches!(tag, "a" | "area") && self.attr(id, "href").is_some(),
            StatePseudo::Checked => match tag {
                "input" => {
                    matches!(self.input_type(id).as_str(), "checkbox" | "radio")
                        && self.attr(id, "checked").is_some()
                }
                "option" => self.attr(id, "selected").is_some(),
                _ => false,
            },
            StatePseudo::Indeterminate => match tag {
                "input" => self.input_type(id) == "radio" && !self.radio_group_has_checked(id),
                "progress" => self.attr(id, "value").is_none(),
                _ => false,
            },
            StatePseudo::Disabled => self.actually_disabled(id, tag),
            StatePseudo::Enabled => {
                matches!(
                    tag,
                    "button" | "input" | "select" | "textarea" | "optgroup" | "option" | "fieldset"
                ) && !self.actually_disabled(id, tag)
            }
            StatePseudo::Required => {
                self.required_applies(id, tag) && self.attr(id, "required").is_some()
            }
            StatePseudo::Optional => {
                self.required_applies(id, tag) && self.attr(id, "required").is_none()
            }
            StatePseudo::ReadWrite => self.read_write(id, tag),
            StatePseudo::ReadOnly => !self.read_write(id, tag),
            StatePseudo::PlaceholderShown => match tag {
                "input" => {
                    self.attr(id, "placeholder").is_some()
                        && self.attr(id, "value").is_none_or(str::is_empty)
                }
                "textarea" => self.attr(id, "placeholder").is_some() && self.is_element_empty(id),
                _ => false,
            },
            StatePseudo::Lang(ranges) => {
                let Some(lang) = self.inherited_lang(id) else {
                    return false;
                };
                let lang = lang.to_ascii_lowercase();
                ranges.iter().any(|r| {
                    r == "*" && !lang.is_empty()
                        || lang == *r
                        || lang
                            .strip_prefix(r.as_str())
                            .is_some_and(|s| s.starts_with('-'))
                })
            }
            StatePseudo::Dir(want_rtl) => self.direction_rtl(id) == *want_rtl,
        }
    }

    /// An `<input>`'s effective type: the `type` attribute, ASCII-lowercased,
    /// defaulting to `text`.
    fn input_type(&self, id: NodeId) -> String {
        self.attr(id, "type")
            .map(|t| t.trim().to_ascii_lowercase())
            .unwrap_or_else(|| "text".to_string())
    }

    /// Whether some radio in `id`'s radio button group is checked (HTML
    /// §4.10.5.1.16: the group is the radios sharing a `name` under the same
    /// form owner; a nameless radio forms a group of one).
    fn radio_group_has_checked(&self, id: NodeId) -> bool {
        let name = self.attr(id, "name").unwrap_or("");
        if name.is_empty() {
            return self.attr(id, "checked").is_some();
        }
        let root = self.nearest_form(id);
        self.descendants(root).any(|c| {
            self.tag_name(c) == Some("input")
                && self.input_type(c) == "radio"
                && self.attr(c, "name") == Some(name)
                && self.nearest_form(c) == root
                && self.attr(c, "checked").is_some()
        })
    }

    /// The nearest `<form>` ancestor (the form owner for grouping; the `form`
    /// content attribute's id indirection is not modeled), else the document.
    fn nearest_form(&self, id: NodeId) -> NodeId {
        let mut cur = self.nodes[id].parent;
        while let Some(a) = cur {
            if self.tag_name(a) == Some("form") {
                return a;
            }
            cur = self.nodes[a].parent;
        }
        DOCUMENT
    }

    /// HTML's "actually disabled": a disableable element with the `disabled`
    /// attribute, an `<option>` under a disabled `<optgroup>`, or a form
    /// control inside a disabled `<fieldset>` (outside its first `<legend>`).
    fn actually_disabled(&self, id: NodeId, tag: &str) -> bool {
        match tag {
            "button" | "input" | "select" | "textarea" => {
                self.attr(id, "disabled").is_some() || self.disabled_by_fieldset(id)
            }
            "optgroup" | "fieldset" => self.attr(id, "disabled").is_some(),
            "option" => {
                if self.attr(id, "disabled").is_some() {
                    return true;
                }
                let mut cur = self.nodes[id].parent;
                while let Some(a) = cur {
                    if self.tag_name(a) == Some("optgroup") {
                        return self.attr(a, "disabled").is_some();
                    }
                    cur = self.nodes[a].parent;
                }
                false
            }
            _ => false,
        }
    }

    /// The disabled-fieldset rule (HTML §4.10.15): a form control descending
    /// from a disabled `<fieldset>` is disabled, unless it sits inside that
    /// fieldset's FIRST `<legend>` child.
    fn disabled_by_fieldset(&self, id: NodeId) -> bool {
        let mut cur = self.nodes[id].parent;
        while let Some(a) = cur {
            if self.tag_name(a) == Some("fieldset") && self.attr(a, "disabled").is_some() {
                let in_first_legend = self
                    .child_iter(a)
                    .find(|&c| self.tag_name(c) == Some("legend"))
                    .is_some_and(|l| self.is_inclusive_ancestor(l, id));
                if !in_first_legend {
                    return true;
                }
            }
            cur = self.nodes[a].parent;
        }
        false
    }

    /// Is `anc` `node` itself or an ancestor of it? (Plain parent walk.)
    fn is_inclusive_ancestor(&self, anc: NodeId, node: NodeId) -> bool {
        let mut cur = Some(node);
        while let Some(n) = cur {
            if n == anc {
                return true;
            }
            cur = self.nodes[n].parent;
        }
        false
    }

    /// Whether the `required` attribute applies to this element (HTML: text-
    /// like/checkbox/radio/file inputs, `<select>`, `<textarea>` — not the
    /// button-like or `hidden`/`range`/`color` input types).
    fn required_applies(&self, id: NodeId, tag: &str) -> bool {
        match tag {
            "select" | "textarea" => true,
            "input" => !matches!(
                self.input_type(id).as_str(),
                "hidden" | "range" | "color" | "submit" | "image" | "reset" | "button"
            ),
            _ => false,
        }
    }

    /// HTML's `:read-write`: a mutable `input` (of a type `readonly` applies
    /// to) or `<textarea>` — neither `readonly` nor disabled — or an editable
    /// element (nearest `contenteditable` attribute not `false`). Everything
    /// else is `:read-only`.
    fn read_write(&self, id: NodeId, tag: &str) -> bool {
        let mutable = match tag {
            "textarea" => true,
            "input" => matches!(
                self.input_type(id).as_str(),
                "text"
                    | "search"
                    | "url"
                    | "tel"
                    | "email"
                    | "password"
                    | "date"
                    | "month"
                    | "week"
                    | "time"
                    | "datetime-local"
                    | "number"
            ),
            _ => false,
        };
        if mutable {
            return self.attr(id, "readonly").is_none() && !self.actually_disabled(id, tag);
        }
        // Editing hosts / editable elements: the nearest contenteditable
        // attribute decides (missing/"false" = not editable).
        let mut cur = Some(id);
        while let Some(n) = cur {
            if let Some(ce) = self.attr(n, "contenteditable") {
                return !ce.eq_ignore_ascii_case("false");
            }
            cur = self.style_parent(n);
        }
        false
    }

    /// The element's language: the nearest ancestor-or-self `lang` (or
    /// `xml:lang`) attribute.
    pub(crate) fn inherited_lang(&self, id: NodeId) -> Option<&str> {
        let mut cur = Some(id);
        while let Some(n) = cur {
            if let Some(l) = self.attr(n, "lang").or_else(|| self.attr(n, "xml:lang")) {
                return Some(l);
            }
            cur = self.style_parent(n);
        }
        None
    }

    /// The element's directionality per the nearest `dir` attribute; the
    /// document default is ltr, and `dir=auto` approximates to ltr (the
    /// engine lays out LTR only — no bidi resolution to consult).
    fn direction_rtl(&self, id: NodeId) -> bool {
        let mut cur = Some(id);
        while let Some(n) = cur {
            if let Some(d) = self.attr(n, "dir") {
                return match d.trim().to_ascii_lowercase().as_str() {
                    "rtl" => true,
                    "ltr" | "auto" => false,
                    _ => {
                        cur = self.style_parent(n);
                        continue;
                    }
                };
            }
            cur = self.style_parent(n);
        }
        false
    }

    /// The nearest following sibling that is an element (skips text/comments).
    fn next_element_sibling(&self, id: NodeId) -> Option<NodeId> {
        let mut n = self.nodes[id].next_sibling;
        while let Some(s) = n {
            if self.tag_name(s).is_some() {
                return Some(s);
            }
            n = self.nodes[s].next_sibling;
        }
        None
    }

    /// Does `subject` satisfy one `:has()` relative argument? Search the subject
    /// SUBTREE (descendant/child leading combinator) or the following-sibling
    /// forest (`+`/`~`), testing each candidate against the `:scope`-anchored
    /// relative complex with `scope = subject`. Iterative (no deep recursion),
    /// early-exits on the first match, and bounded by `HAS_MAX_VISITS` so a
    /// pathological `*:has(*)` on a huge subtree can't blow up (the cap is a
    /// hostile-page backstop far above any real selector's reach).
    fn matches_has(&self, subject: NodeId, h: &HasArg) -> bool {
        const HAS_MAX_VISITS: usize = 8192;
        let mut stack: Vec<NodeId> = if h.sibling {
            let mut sib = self.next_element_sibling(subject);
            let mut v = Vec::new();
            while let Some(s) = sib {
                v.push(s);
                sib = self.next_element_sibling(s);
            }
            v
        } else {
            self.child_iter(subject)
                .filter(|&c| self.tag_name(c).is_some())
                .collect()
        };
        let mut budget = HAS_MAX_VISITS;
        while let Some(node) = stack.pop() {
            if budget == 0 {
                break;
            }
            budget -= 1;
            if self.matches_complex(node, &h.complex.0, Some(subject)) {
                return true;
            }
            for c in self.child_iter(node) {
                if self.tag_name(c).is_some() {
                    stack.push(c);
                }
            }
        }
        false
    }
}

fn escape_text(s: &str) -> Cow<'_, str> {
    if !s.contains(['&', '<', '>']) {
        return Cow::Borrowed(s);
    }
    Cow::Owned(
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;"),
    )
}

fn append_style(style: &mut String, declarations: &str) {
    if declarations.is_empty() {
        return;
    }
    if !style.trim().is_empty() && !style.trim_end().ends_with(';') {
        style.push(';');
    }
    style.push_str(declarations);
}

/// SVG 2 presentation properties retained in an isolated inline-SVG image.
/// Keeping one list for attribute var() substitution and stylesheet winner
/// materialization prevents the two standards-defined cascade inputs from
/// diverging.
const SVG_PRESENTATION_PROPERTIES: &[&str] = &[
    "fill",
    "fill-opacity",
    "fill-rule",
    "stroke",
    "stroke-opacity",
    "stroke-width",
    "stroke-linecap",
    "stroke-linejoin",
    "stroke-miterlimit",
    "stroke-dasharray",
    "stroke-dashoffset",
    "clip-rule",
    "paint-order",
    "vector-effect",
    "shape-rendering",
    "stop-color",
    "stop-opacity",
];

fn escape_attr(s: &str) -> Cow<'_, str> {
    if !s.contains(['&', '<', '>', '"']) {
        return Cow::Borrowed(s);
    }
    Cow::Owned(
        s.replace('&', "&amp;")
            .replace('"', "&quot;")
            .replace('<', "&lt;")
            .replace('>', "&gt;"),
    )
}

/// Replace the CSS color `currentcolor` keyword without touching a longer
/// identifier that merely contains the same bytes. CSS identifiers are ASCII
/// case-insensitive here; the surrounding SVG markup remains byte-for-byte
/// unchanged.
fn replace_css_current_color(input: &str, replacement: &str) -> String {
    const NEEDLE: &str = "currentcolor";
    let lower = input.to_ascii_lowercase();
    let is_ident = |byte: Option<u8>| {
        byte.is_some_and(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    };
    let mut out = String::with_capacity(input.len());
    let mut cursor = 0;
    for (start, _) in lower.match_indices(NEEDLE) {
        let end = start + NEEDLE.len();
        if is_ident(lower.as_bytes().get(start.wrapping_sub(1)).copied())
            || is_ident(lower.as_bytes().get(end).copied())
        {
            continue;
        }
        out.push_str(&input[cursor..start]);
        out.push_str(replacement);
        cursor = end;
    }
    out.push_str(&input[cursor..]);
    out
}

/// Serialize a computed CSS color into the conservative sRGB syntax accepted
/// by the standalone SVG decoder. CSS Color 4 modern functional notation such
/// as `rgb(4 204 116 / 1)` is valid on the embedding HTML element, but the SVG
/// resource parser has no access to that HTML CSS implementation. Materialize
/// the used `currentColor` value as an equivalent legacy-compatible color.
fn svg_resource_color(value: &str) -> String {
    match crate::render::PaintColor::parse_css(value) {
        Some(crate::render::PaintColor::Rgba(r, g, b, 255)) => {
            format!("#{r:02x}{g:02x}{b:02x}")
        }
        Some(crate::render::PaintColor::Rgba(r, g, b, a)) => {
            let alpha = f32::from(a) / 255.0;
            format!("rgba({r},{g},{b},{alpha:.6})")
        }
        _ => value.to_string(),
    }
}

// ---- Selector subset ------------------------------------------------

/// The workhorse selector grammar: `tag`, `*`, `#id`, `.class` (CSS ident
/// escapes decoded — `.md\:flex` is the class `md:flex`), `[attr]`,
/// `[attr⊙=value]` (⊙ ∈ {ε, ~, |, ^, $, *}; trailing `i` = case-insensitive),
/// `:not(compound)`, `:is(complex…)`/`:where(complex…)` (forgiving lists;
/// `:where` = zero specificity), the structural pseudo-classes (`:empty`,
/// `:first-child`/`:last-child`/`:only-child`, `:*-of-type`,
/// `:nth-child(An+B)` and friends), compounds thereof, and the descendant
/// (space), child (`>`), next-sibling (`+`) and subsequent-sibling (`~`)
/// combinators, in comma lists. Interaction pseudos (`:hover`…) and
/// pseudo-elements parse but never match — valid CSS that can't be true in
/// our world.
pub struct SelectorList(Vec<Complex>, Option<Vec<String>>);

struct Complex(Vec<(Combinator, Compound)>);

/// One argument of a `:has()` relative-selector list, compiled for matching.
struct HasArg {
    /// The leading combinator is a sibling one (`+`/`~`) ⇒ search the subject's
    /// following-sibling forest; else (`>` or bare descendant) its own subtree.
    sibling: bool,
    /// The relative selector as a full complex anchored by a leftmost `:scope`
    /// compound (matched with `scope` = the `:has` subject). That `:scope`
    /// carries ZERO specificity, so this complex's specificity IS the
    /// argument's — exactly what Selectors 4 §17 asks `:has()` to contribute.
    complex: Complex,
}

/// Parse ONE `:has()` argument — a relative selector: an optional leading
/// combinator (`>`/`+`/`~`, default descendant) then a complex selector.
/// Returns `None` (the forgiving list drops it) for an unparsable argument, a
/// pseudo-element subject, or a NESTED `:has()` (both invalid per Selectors 4).
fn parse_relative(part: &str) -> Option<HasArg> {
    let part = part.trim();
    let mut chars = part.chars().peekable();
    let (comb, sibling) = match chars.peek() {
        Some('>') => (Combinator::Child, false),
        Some('+') => (Combinator::NextSibling, true),
        Some('~') => (Combinator::SubsequentSibling, true),
        _ => (Combinator::Descendant, false),
    };
    if comb != Combinator::Descendant {
        chars.next(); // consume the leading combinator
    }
    let rest: String = chars.collect();
    let mut cx = parse_complex(rest.trim())?;
    // A pseudo-element subject is invalid inside `:has()`; nested `:has()` too.
    if cx.0.last().is_some_and(|(_, c)| c.pseudo.is_some()) || complex_uses_has(&cx.0) {
        return None;
    }
    // Anchor at `:scope`: the leftmost real compound takes the leading
    // combinator, and a zero-specificity `:scope` compound is prepended so the
    // ancestor/sibling walk stops at the subject instead of escaping upward.
    if let Some(first) = cx.0.first_mut() {
        first.0 = comb;
    }
    let scope = Compound {
        scope: true,
        ..Default::default()
    };
    cx.0.insert(0, (Combinator::None, scope));
    Some(HasArg {
        sibling,
        complex: cx,
    })
}

/// Whether any compound in a complex selector uses `:has()` (Selectors 4
/// forbids `:has()` nested inside `:has()`).
fn complex_uses_has(parts: &[(Combinator, Compound)]) -> bool {
    parts.iter().any(|(_, c)| compound_uses_has(c))
}

fn compound_uses_has(c: &Compound) -> bool {
    !c.has.is_empty()
        || c.nots.iter().flatten().any(compound_uses_has)
        || c.selects
            .iter()
            .any(|(g, _)| g.iter().any(|cx| complex_uses_has(&cx.0)))
        || c.host_inner.as_deref().is_some_and(compound_uses_has)
}

/// Whether changing the child/text content of a boxless element could change a
/// selector subject outside that element. `:has()` is relational by definition;
/// `:empty` can escape through a following combinator (`.x:empty + .y`). The
/// suppression gate is deliberately document-conservative: a false positive
/// costs a render, while a false negative leaves stale pixels.
fn complex_has_boxless_content_dependency(complex: &Complex) -> bool {
    complex
        .0
        .iter()
        .any(|(_, compound)| compound_has_boxless_content_dependency(compound))
}

fn compound_has_boxless_content_dependency(compound: &Compound) -> bool {
    !compound.has.is_empty()
        || compound
            .structural
            .iter()
            .any(|state| matches!(state, Structural::Empty))
        || compound
            .nots
            .iter()
            .flatten()
            .any(compound_has_boxless_content_dependency)
        || compound
            .selects
            .iter()
            .any(|(group, _)| group.iter().any(complex_has_boxless_content_dependency))
        || compound
            .host_inner
            .as_deref()
            .is_some_and(compound_has_boxless_content_dependency)
}

/// The outcome of resolving a custom property's value during `var()`
/// substitution (CSS Variables L1 §3). `Resolved` carries its substituted
/// value; `Undefined` is the guaranteed-invalid value — the property is unset,
/// or became invalid at computed-value time — for which a referencing `var()`
/// uses its fallback; `Cycle` means the reference closes a dependency cycle (it
/// points back at a custom property still being resolved further up the stack),
/// which makes every property in the cycle invalid at computed-value time
/// *without* consulting their fallbacks.
enum VarResult {
    Resolved(String),
    Undefined,
    Cycle,
}

/// CSS function names are ASCII case-insensitive, while the custom-property
/// identifier inside `var()` is not. Returning a byte offset is safe because a
/// match starts on the ASCII `v`, never inside a UTF-8 continuation byte.
fn find_var_function(value: &str) -> Option<usize> {
    value
        .as_bytes()
        .windows(4)
        .position(|candidate| candidate.eq_ignore_ascii_case(b"var("))
}

#[derive(PartialEq)]
enum Combinator {
    /// Leftmost compound: nothing to its left.
    None,
    Descendant,
    Child,
    /// `A + B`: B's immediately-preceding element sibling is A.
    NextSibling,
    /// `A ~ B`: some preceding element sibling of B is A.
    SubsequentSibling,
}

/// The `::before` / `::after` generated-content pseudo-elements (CSS2
/// `:before`/`:after` legacy spelling too). The only pseudo-elements we
/// act on; others parse but never match.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PseudoEl {
    Before,
    After,
}

#[derive(Default)]
struct Compound {
    tag: Option<String>,
    id: Option<String>,
    classes: Vec<String>,
    attrs: Vec<AttrSel>,
    /// `:not(...)` arguments, one inner Vec per `:not()` invocation: the
    /// compound matches only if NO argument of ANY invocation does. The
    /// grouping matters only for specificity — each invocation contributes
    /// its MOST SPECIFIC argument (Selectors 4 §17), while separate
    /// invocations all add up.
    nots: Vec<Vec<Compound>>,
    /// `:is(...)`/`:where(...)` (+ the legacy `:matches` alias) argument
    /// groups, one per invocation (Selectors 4 §4.2–4.3): the compound
    /// matches only if, for EACH group, the element matches AT LEAST ONE of
    /// the group's complex selectors (full complex selectors — combinators
    /// allowed — matched with this element as the subject). The bool marks
    /// `:where`, which contributes ZERO specificity; `:is` contributes its
    /// most specific argument. Arguments are a FORGIVING list: unparsable
    /// ones are dropped individually, and an all-invalid group simply
    /// matches nothing (the rule survives).
    selects: Vec<(Vec<Complex>, bool)>,
    /// `:has(...)` (Selectors 4 §4.5, the relational pseudo-class), one entry
    /// per invocation. Each is a FORGIVING relative-selector list: the element
    /// matches an invocation if AT LEAST ONE of its `HasArg`s finds a matching
    /// element in this element's subtree (or following-sibling forest). Every
    /// invocation must hold (`.a:has(.b):has(.c)` needs both). Specificity is
    /// the most specific argument (Selectors 4 §17), summed in `spec()`.
    has: Vec<Vec<HasArg>>,
    /// `:hover` (live): the element must be on the chain under the terminal's
    /// pointer (`Dom.hover_chain` — the committed hover target + its composed
    /// ancestors). Empty chain at rest ⇒ a bare `:hover` compound is inert.
    hover: bool,
    /// `:popover-open` (live): the element's popover must currently be
    /// showing (`Dom.popover_open`, written by the popover API syscall).
    popover_open: bool,
    /// `:focus` and other pseudos we can't satisfy: parse fine,
    /// match never (fail-open — a never-matching hide rule hides nothing,
    /// and its comma-siblings stay alive).
    never: bool,
    /// Set alongside `never` for pseudos that are NOT genuinely false at
    /// rest (`:has(…)`, `:lang(…)`, …). Inside
    /// `:not()` a `never` compound would invert to ALWAYS-match — correct
    /// for an interaction pseudo (`:not(:hover)` really is true at rest),
    /// but a hide rule like `.x:not(:has(img))` must die instead of hiding
    /// every `.x`. The `:not` parser rejects these (rule dropped, fail-open).
    never_unknown: bool,
    /// Structural pseudo-classes (`:empty`, `:nth-child(…)`, `:first-child`,
    /// `:*-of-type`, …) the element must satisfy. All must hold (AND).
    structural: Vec<Structural>,
    /// Element-state pseudo-classes (`:checked`, `:disabled`, `:link`,
    /// `:lang(…)`, …) the element must satisfy. All must hold (AND).
    states: Vec<StatePseudo>,
    /// `:scope`: matches the element a rooted query (`querySelectorAll`/
    /// jQuery `.find()`) was called on. jQuery rewrites context-rooted comma/
    /// complex selectors to `:scope X, :scope Y`, so without this they match
    /// nothing (it silently broke deselection-style code). Inert in the
    /// stylesheet cascade (no query root there).
    scope: bool,
    /// `:root`: matches the document root element (`<html>`). The conventional
    /// home of custom-property definitions (`:root { --foo: … }`), so matching
    /// it is what lets `var(--foo)` resolve to a root-defined value.
    root: bool,
    /// `:host` / `:host(<compound>)` (CSS Scoping §3.3): in a shadow root's
    /// stylesheet, targets the SHADOW HOST (the element the root is attached to),
    /// which lives in the parent tree — so it's matched specially against the
    /// host in `cascaded`, never via the normal in-scope path (which would test
    /// it against shadow-internal elements). `host_inner` is the `(…)` argument
    /// the host must additionally match (`:host(.theme-dark)`).
    host: bool,
    host_inner: Option<Box<Compound>>,
    /// `::slotted(<compound>)` (CSS Shadow 1 §3.2.4): the pseudo-element is
    /// represented by the originating slot, while its argument selects the
    /// flattened light-DOM element receiving the declarations.
    slotted: Option<Box<Compound>>,
    /// `::before`/`::after`: the rule targets a generated-content box on
    /// the matched element, NOT the element itself. The element-property
    /// cascade skips these; `pseudo_content` consults only these.
    pseudo: Option<PseudoEl>,
    /// Pseudo-class count, for specificity only.
    pseudos: u32,
}

struct AttrSel {
    name: String,
    op: AttrOp,
    value: Option<String>,
    /// `[attr=value i]` (Selectors 4): compare ASCII case-insensitively.
    ci: bool,
}

/// `An+B` (the `:nth-child` micro-grammar): position `p` (1-based) matches
/// when `p = a*k + b` for some integer `k ≥ 0`.
struct Nth {
    a: i32,
    b: i32,
}

impl Nth {
    fn matches(&self, pos: i32) -> bool {
        if self.a == 0 {
            pos == self.b
        } else {
            let diff = pos - self.b;
            diff % self.a == 0 && diff / self.a >= 0
        }
    }
}

/// An element-state pseudo-class evaluable from the arena (Selectors 4 §9
/// link pseudos + the HTML "Pseudo-classes" section's input-state semantics).
/// All are static tests against attributes/tree state; content mutations bump
/// the epoch, so the per-epoch match memos stay fresh (the prelude routes
/// `.checked`/`.value` writes through `setAttribute`, which mutates the
/// arena).
enum StatePseudo {
    /// `:link` / `:any-link` — an `<a>`/`<area>` with an `href`. History-
    /// based styling isn't modeled yet, so `:link` matches every hyperlink
    /// and `:visited` is a never-pseudo for now.
    AnyLink,
    /// `:checked` — a checkbox/radio with checkedness set, or a selected
    /// `<option>`.
    Checked,
    /// `:indeterminate` — a radio group with no checked radio, or a
    /// `<progress>` with no `value`. (A checkbox's `indeterminate` is a
    /// JS-only property the arena doesn't model yet, so it can't match
    /// there until the prelude mirrors it.)
    Indeterminate,
    /// `:disabled` / `:enabled` — HTML's "actually disabled" definition,
    /// including the disabled-`<fieldset>` descendant rule.
    Disabled,
    Enabled,
    /// `:required` / `:optional` — controls the `required` attribute applies
    /// to, with/without it.
    Required,
    Optional,
    /// `:read-write` / `:read-only` — a mutable `input`/`textarea`, or an
    /// editable (`contenteditable`) element; `:read-only` is everything else.
    ReadWrite,
    ReadOnly,
    /// `:placeholder-shown` — a `placeholder`-bearing control whose value is
    /// empty.
    PlaceholderShown,
    /// `:lang(<ranges>)` — language ranges matched against the inherited
    /// `lang` attribute (RFC 4647 prefix filtering, `*` wildcard).
    Lang(Vec<String>),
    /// `:dir(rtl)` / `:dir(ltr)` — the nearest `dir` attribute (`auto`
    /// approximates to `ltr`: the engine lays out LTR only). `true` = rtl.
    Dir(bool),
}

/// Parse a state pseudo-class by name (+ its argument for the functional
/// two). `None` = not a state pseudo (the caller falls through); a present
/// but malformed argument also yields `None`, which the caller surfaces as a
/// parse failure of the whole selector (the engine-wide fail-open rule).
fn parse_state_pseudo(name: &str, arg: Option<&str>) -> Option<StatePseudo> {
    Some(match name {
        "link" | "any-link" => StatePseudo::AnyLink,
        "checked" => StatePseudo::Checked,
        "indeterminate" => StatePseudo::Indeterminate,
        "disabled" => StatePseudo::Disabled,
        "enabled" => StatePseudo::Enabled,
        "required" => StatePseudo::Required,
        "optional" => StatePseudo::Optional,
        "read-write" => StatePseudo::ReadWrite,
        "read-only" => StatePseudo::ReadOnly,
        "placeholder-shown" => StatePseudo::PlaceholderShown,
        "lang" => {
            let ranges: Vec<String> = split_top_level(arg?, ',')
                .into_iter()
                .map(|r| r.trim().trim_matches(['"', '\'']).to_ascii_lowercase())
                .filter(|r| !r.is_empty())
                .collect();
            if ranges.is_empty() {
                return None;
            }
            StatePseudo::Lang(ranges)
        }
        "dir" => match arg?.trim().to_ascii_lowercase().as_str() {
            "ltr" => StatePseudo::Dir(false),
            "rtl" => StatePseudo::Dir(true),
            _ => return None,
        },
        _ => return None,
    })
}

/// A structural pseudo-class: a positional/childless test that depends on
/// the element's siblings, not its own attributes.
enum Structural {
    /// `:empty` — no element or non-empty text children.
    Empty,
    /// `:nth-child(An+B)` and its variants. `of_type` counts only same-tag
    /// siblings; `from_end` counts position from the last sibling; `of` is
    /// Selectors 4 §5.5's `of <selector-list>` clause — only siblings
    /// matching it are counted (and the subject must match it too).
    /// (`:first-child` = `nth(1)`, `:last-child` = `nth(1)` from end, etc.)
    Nth {
        nth: Nth,
        of_type: bool,
        from_end: bool,
        of: Option<Vec<Complex>>,
    },
}

/// Split an `:nth-child()`/`:nth-last-child()` argument into its `An+B` text
/// and the optional `of <selector-list>` clause. The An+B micro-grammar
/// contains no ident but `odd`/`even`/`n`, so the first whitespace-delimited
/// `of` token is the divider.
fn split_nth_of(s: &str) -> (&str, Option<&str>) {
    let lower = s.to_ascii_lowercase();
    let b = lower.as_bytes();
    let mut i = 0;
    while let Some(pos) = lower[i..].find("of") {
        let at = i + pos;
        let bounded_left = at == 0 || b[at - 1].is_ascii_whitespace();
        let bounded_right = b.get(at + 2).is_none_or(u8::is_ascii_whitespace);
        if bounded_left && bounded_right && at > 0 {
            return (&s[..at], Some(s[at + 2..].trim()));
        }
        i = at + 2;
    }
    (s, None)
}

/// Parse the `An+B` argument of `:nth-child(...)` etc. — `odd`, `even`,
/// `2n+1`, `-n+3`, `n`, `3`, `+3`, with optional internal whitespace.
fn parse_nth(s: &str) -> Option<Nth> {
    let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    let s = s.to_ascii_lowercase();
    match s.as_str() {
        "odd" => return Some(Nth { a: 2, b: 1 }),
        "even" => return Some(Nth { a: 2, b: 0 }),
        _ => {}
    }
    if let Some(npos) = s.find('n') {
        let a = match &s[..npos] {
            "" | "+" => 1,
            "-" => -1,
            x => x.parse().ok()?,
        };
        let b_str = &s[npos + 1..];
        let b = if b_str.is_empty() {
            0
        } else {
            b_str.strip_prefix('+').unwrap_or(b_str).parse().ok()?
        };
        Some(Nth { a, b })
    } else {
        Some(Nth {
            a: 0,
            b: s.parse().ok()?,
        })
    }
}

/// The simple structural pseudo-classes (no argument), expanded to their
/// `:nth`-equivalents. `:only-*` is the conjunction of first and last.
fn structural_simple(name: &str) -> Option<Vec<Structural>> {
    let first = |of_type| Structural::Nth {
        nth: Nth { a: 0, b: 1 },
        of_type,
        from_end: false,
        of: None,
    };
    let last = |of_type| Structural::Nth {
        nth: Nth { a: 0, b: 1 },
        of_type,
        from_end: true,
        of: None,
    };
    Some(match name {
        "first-child" => vec![first(false)],
        "last-child" => vec![last(false)],
        "only-child" => vec![first(false), last(false)],
        "first-of-type" => vec![first(true)],
        "last-of-type" => vec![last(true)],
        "only-of-type" => vec![first(true), last(true)],
        _ => return None,
    })
}

/// Parse an `of <selector-list>` clause: a NON-forgiving complex selector
/// list (Selectors 4 §5.5) — any unparsable member (or a pseudo-element
/// subject) invalidates the whole selector.
fn parse_nth_of(sel: &str) -> Option<Vec<Complex>> {
    let mut out = Vec::new();
    for part in split_top_level(sel, ',') {
        let cx = parse_complex(part.trim())?;
        if cx.0.last().is_some_and(|(_, c)| c.pseudo.is_some()) {
            return None;
        }
        out.push(cx);
    }
    if out.is_empty() { None } else { Some(out) }
}

/// CSS attribute selector operators: `=`, `~=`, `|=`, `^=`, `$=`, `*=`.
#[derive(Clone, Copy)]
enum AttrOp {
    Exact,
    Includes,
    Dash,
    Prefix,
    Suffix,
    Substring,
}

impl AttrSel {
    fn matches(&self, got: &str) -> bool {
        let Some(want) = &self.value else {
            return true; // bare [attr]: presence is enough
        };
        if self.ci {
            // The `i` flag: fold both sides (ASCII, per Selectors 4).
            return attr_op_matches(
                self.op,
                &got.to_ascii_lowercase(),
                &want.to_ascii_lowercase(),
            );
        }
        attr_op_matches(self.op, got, want)
    }
}

fn attr_op_matches(op: AttrOp, got: &str, want: &str) -> bool {
    match op {
        AttrOp::Exact => got == want,
        AttrOp::Includes => got.split_ascii_whitespace().any(|w| w == want),
        AttrOp::Dash => got == want || got.strip_prefix(want).is_some_and(|r| r.starts_with('-')),
        AttrOp::Prefix => !want.is_empty() && got.starts_with(want),
        AttrOp::Suffix => !want.is_empty() && got.ends_with(want),
        AttrOp::Substring => !want.is_empty() && got.contains(want),
    }
}

impl Compound {
    fn is_empty(&self) -> bool {
        self.tag.is_none()
            && self.id.is_none()
            && self.classes.is_empty()
            && self.attrs.is_empty()
            && self.nots.is_empty()
            && self.selects.is_empty()
            && self.has.is_empty()
            && !self.never
            && !self.hover
            && !self.popover_open
            && !self.scope
            && !self.root
            && !self.host
            && self.slotted.is_none()
            && self.structural.is_empty()
            && self.states.is_empty()
            && self.pseudo.is_none()
    }

    /// (ids, classes+attrs+pseudo-classes, tags+pseudo-elements). A
    /// pseudo-ELEMENT counts like a type (Selectors 4 §17), not a class.
    /// Each `:not()`/`:is()` invocation contributes the specificity of its
    /// MOST SPECIFIC argument (not the sum over a comma list); separate
    /// invocations in one compound all add up; `:where()` contributes ZERO.
    fn spec(&self) -> (u32, u32, u32) {
        let mut s = (
            u32::from(self.id.is_some()),
            self.classes.len() as u32 + self.attrs.len() as u32 + self.pseudos,
            u32::from(matches!(&self.tag, Some(t) if t != "*")) + u32::from(self.pseudo.is_some()),
        );
        for group in &self.nots {
            if let Some(m) = group.iter().map(Compound::spec).max() {
                s = (s.0 + m.0, s.1 + m.1, s.2 + m.2);
            }
        }
        for (group, is_where) in &self.selects {
            if *is_where {
                continue; // `:where()`: always zero specificity
            }
            if let Some(m) = group.iter().map(Complex::specificity).max() {
                s = (s.0 + m.0, s.1 + m.1, s.2 + m.2);
            }
        }
        // `:has()`: its most specific argument (the anchoring `:scope` compound
        // carries zero specificity, so the complex's own specificity is it).
        for group in &self.has {
            if let Some(m) = group.iter().map(|h| h.complex.specificity()).max() {
                s = (s.0 + m.0, s.1 + m.1, s.2 + m.2);
            }
        }
        // `:nth-child(An+B of S)`: the pseudo-class (already in `pseudos`)
        // plus the specificity of the most specific S (Selectors 4 §17).
        for st in &self.structural {
            if let Structural::Nth { of: Some(sels), .. } = st
                && let Some(m) = sels.iter().map(Complex::specificity).max()
            {
                s = (s.0 + m.0, s.1 + m.1, s.2 + m.2);
            }
        }
        if let Some(inner) = &self.host_inner {
            let hs = inner.spec();
            s = (s.0 + hs.0, s.1 + hs.1, s.2 + hs.2);
        }
        if let Some(inner) = &self.slotted {
            let ss = inner.spec();
            s = (s.0 + ss.0, s.1 + ss.1, s.2 + ss.2);
        }
        s
    }
}

impl Complex {
    fn specificity(&self) -> (u32, u32, u32) {
        let mut s = (0, 0, 0);
        for (_, c) in &self.0 {
            let cs = c.spec();
            s = (s.0 + cs.0, s.1 + cs.1, s.2 + cs.2);
        }
        s
    }
}

/// Split on `sep` outside parens, brackets, and quotes — `:not(.a, .b)`
/// and `[title="x,y"]` must survive list splitting.
fn split_top_level(input: &str, sep: char) -> Vec<&str> {
    let mut out = Vec::new();
    let (mut depth, mut start) = (0i32, 0usize);
    let mut quote: Option<char> = None;
    for (i, c) in input.char_indices() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(c),
            (None, '(' | '[') => depth += 1,
            (None, ')' | ']') => depth -= 1,
            (None, c) if c == sep && depth == 0 => {
                out.push(&input[start..i]);
                start = i + c.len_utf8();
            }
            _ => {}
        }
    }
    out.push(&input[start..]);
    out
}

/// Split on TOP-LEVEL whitespace, respecting `calc()`/`var()`/`min()` parens,
/// brackets and quotes, and collapsing runs of whitespace (empty tokens
/// dropped). A naive `split_whitespace` tears a `calc(.25rem * -1)` value into
/// `calc(.25rem`, `*`, `-1)`, so a box shorthand (`margin`/`padding`/`inset`)
/// carrying a `calc()` component would parse as three sides — the `-m-1`
/// negative-margin idiom Tailwind emits.
fn split_top_level_ws(input: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    let mut start: Option<usize> = None; // Some(i) ⇒ currently inside a token
    for (i, c) in input.char_indices() {
        if quote.is_none() && depth == 0 && c.is_whitespace() {
            if let Some(s) = start.take() {
                out.push(&input[s..i]);
            }
            continue;
        }
        if start.is_none() {
            start = Some(i);
        }
        match (quote, c) {
            (Some(q), ch) if ch == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(c),
            (None, '(' | '[') => depth += 1,
            (None, ')' | ']') => depth -= 1,
            _ => {}
        }
    }
    if let Some(s) = start {
        out.push(&input[s..]);
    }
    out
}

/// External SVG sprite sheets, RAM-only and process-global (like the cookie
/// jar / connection pool). Keyed by the sprite FILE's absolute URL → its symbol
/// table (`<symbol id>` → a self-contained `<svg>` for that one symbol). The
/// `<svg><use href="sprite.svg#id"></svg>` idiom (ChatGPT, GitHub, and most
/// icon systems) keeps every icon's geometry in one shared file resvg won't
/// fetch on its own; we fetch that file ONCE during the JS subresource phase
/// (`prime_sprite_sheet`) and `rewrite_inline_svgs` inlines the referenced
/// symbol so it rasterizes like any inline vector. Parsed once per sheet; a
/// reparse (resize) or a second page on the same CDN reuses the table.
/// A sprite sheet's symbol table: `<symbol id>` → its standalone `<svg>`.
type SpriteTable = std::sync::Arc<FxHashMap<String, String>>;
static SPRITE_SHEETS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, SpriteTable>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

/// Cap on cached sprite sheets — a hostile-page lid, not a real limit (a design
/// system ships one or two sheets). Sheets can be ~600KB each.
const MAX_SPRITE_SHEETS: usize = 16;

/// Parse a fetched sprite sheet ONCE into its symbol table, keyed by absolute
/// URL. Called from the async subresource phase (`execute_js`); the sync
/// `rewrite_inline_svgs` then reads the table. Idempotent: an already-primed
/// URL is left alone (the sheet is immutable for the session).
pub fn prime_sprite_sheet(abs_url: &str, text: &str) {
    {
        let sheets = SPRITE_SHEETS.lock().unwrap();
        if sheets.contains_key(abs_url) || sheets.len() >= MAX_SPRITE_SHEETS {
            return;
        }
    }
    let table = build_sprite_symbols(text);
    let mut sheets = SPRITE_SHEETS.lock().unwrap();
    if sheets.len() < MAX_SPRITE_SHEETS {
        sheets.insert(abs_url.to_string(), std::sync::Arc::new(table));
    }
}

/// Whether a sprite sheet is already fetched+parsed (so the subresource phase
/// can skip re-downloading a ~600KB sheet across navigations/reparses).
pub fn sprite_sheet_cached(abs_url: &str) -> bool {
    SPRITE_SHEETS.lock().unwrap().contains_key(abs_url)
}

/// The self-contained `<svg>` for one symbol of a primed sprite sheet, or
/// `None` if the sheet wasn't fetched or has no such id.
fn sprite_symbol_svg(abs_url: &str, frag: &str) -> Option<String> {
    let sheets = SPRITE_SHEETS.lock().unwrap();
    sheets.get(abs_url)?.get(frag).cloned()
}

/// Whether a primed sprite sheet holds this symbol — `sprite_symbol_svg`
/// without cloning the markup (the serializer only needs the yes/no).
fn sprite_has_symbol(abs_url: &str, frag: &str) -> bool {
    let sheets = SPRITE_SHEETS.lock().unwrap();
    sheets.get(abs_url).is_some_and(|t| t.contains_key(frag))
}

/// A sprite sheet is a flat `<svg>` of `<symbol id viewBox>…</symbol>` defs.
/// Turn each into a STANDALONE `<svg viewBox>` carrying that symbol's own
/// geometry + the shape-affecting presentation attrs (`fill`/`fill-rule`/
/// `clip-rule`) — no width/height, so the replacement `<img>`'s CSS box drives
/// the used size (CSS 2.1 §10.3.2 rule 3, ratio-only). Preserve `currentColor`
/// until the referencing SVG element's computed color is available; the
/// graphical and terminal paths then make their own final paint choice.
fn build_sprite_symbols(text: &str) -> FxHashMap<String, String> {
    let dom = Dom::parse_document(text);
    let mut out = FxHashMap::default();
    for sym in dom.descendants(DOCUMENT) {
        if dom.tag_name(sym) != Some("symbol") {
            continue;
        }
        let Some(frag) = dom.attr(sym, "id").filter(|s| !s.is_empty()) else {
            continue;
        };
        // viewBox is re-emitted with the correct case regardless of how the
        // parser stored the name (`attr` matches case-insensitively).
        let vb = dom.attr(sym, "viewBox").unwrap_or("0 0 24 24").to_string();
        let mut pres = String::new();
        for k in ["fill", "fill-rule", "clip-rule"] {
            if let Some(v) = dom.attr(sym, k) {
                pres.push_str(&format!(r#" {k}="{}""#, escape_attr(v)));
            }
        }
        let mut inner = String::new();
        for c in dom.child_iter(sym) {
            inner.push_str(&dom.serialize(c));
        }
        let svg = format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="{vb}"{pres}>{inner}</svg>"#
        );
        out.insert(frag.to_string(), svg);
    }
    out
}

impl SelectorList {
    pub fn parse(input: &str) -> Option<SelectorList> {
        let mut list = Vec::new();
        for part in split_top_level(input, ',') {
            list.push(parse_complex(part.trim())?);
        }
        if list.is_empty() {
            None
        } else {
            // A selector-list can be tag-prefiltered only if EVERY branch has
            // an explicit, non-universal subject type. Otherwise an untagged
            // branch (for example `.item` or `:is(div, span)`) can match any
            // element and the conservative filter is disabled.
            let mut subject_tags = Vec::with_capacity(list.len());
            for complex in &list {
                let tag = complex
                    .0
                    .last()
                    .and_then(|(_, compound)| compound.tag.as_deref());
                let Some(tag) = tag.filter(|tag| *tag != "*") else {
                    return Some(SelectorList(list, None));
                };
                if !subject_tags.iter().any(|existing| existing == tag) {
                    subject_tags.push(tag.to_owned());
                }
            }
            Some(SelectorList(list, Some(subject_tags)))
        }
    }

    #[inline]
    fn might_match_subject_tag(&self, tag: &str) -> bool {
        self.1
            .as_ref()
            .is_none_or(|tags| tags.iter().any(|candidate| candidate == tag))
    }

    /// `parse`, memoized per thread — the JS `querySelector*`/`matches`
    /// syscall entry. Pages re-query the same selector strings constantly
    /// (every `document.body` is a `querySelector("body")`, jQuery re-runs
    /// its `.find(...)` strings per event), and a parse is pure string→AST,
    /// so the memo never invalidates. Failures are cached too (feature
    /// probes retry unsupported selectors in hot paths). Bounded by a full
    /// clear at a size lid: re-parsing is cheap, eviction bookkeeping isn't
    /// worth it.
    pub fn parse_cached(input: &str) -> Option<std::rc::Rc<SelectorList>> {
        thread_local! {
            static SELECTOR_MEMO: RefCell<FxHashMap<String, Option<std::rc::Rc<SelectorList>>>> =
                RefCell::new(FxHashMap::default());
        }
        SELECTOR_MEMO.with(|m| {
            let mut m = m.borrow_mut();
            if m.len() > 1024 {
                m.clear();
            }
            m.entry(input.to_string())
                .or_insert_with(|| SelectorList::parse(input).map(std::rc::Rc::new))
                .clone()
        })
    }
}

fn parse_complex(input: &str) -> Option<Complex> {
    let mut parts: Vec<(Combinator, Compound)> = Vec::new();
    let mut chars = input.chars().peekable();
    let mut pending = Combinator::None;
    loop {
        // Inter-compound whitespace / combinators.
        let mut saw_space = false;
        while let Some(&c) = chars.peek() {
            if c.is_ascii_whitespace() {
                saw_space = true;
                chars.next();
            } else if c == '>' {
                pending = Combinator::Child;
                chars.next();
            } else if c == '+' {
                pending = Combinator::NextSibling;
                chars.next();
            } else if c == '~' {
                pending = Combinator::SubsequentSibling;
                chars.next();
            } else {
                break;
            }
        }
        if chars.peek().is_none() {
            break;
        }
        if pending == Combinator::None && saw_space && !parts.is_empty() {
            pending = Combinator::Descendant;
        }

        let compound = parse_compound(&mut chars)?;
        if compound.is_empty() {
            return None;
        }
        parts.push((std::mem::replace(&mut pending, Combinator::None), compound));
    }
    if parts.is_empty() {
        None
    } else {
        Some(Complex(parts))
    }
}

fn parse_compound(chars: &mut std::iter::Peekable<std::str::Chars>) -> Option<Compound> {
    let mut compound = Compound::default();
    while let Some(&c) = chars.peek() {
        match c {
            '#' => {
                chars.next();
                compound.id = Some(take_name(chars)?);
            }
            '.' => {
                chars.next();
                compound.classes.push(take_name(chars)?);
            }
            '[' => {
                chars.next();
                let inner: String = chars.by_ref().take_while(|&c| c != ']').collect();
                let (name, op, value, ci) = match inner.split_once('=') {
                    Some((n, v)) => {
                        let (n, op) = match n.chars().last() {
                            Some('~') => (&n[..n.len() - 1], AttrOp::Includes),
                            Some('|') => (&n[..n.len() - 1], AttrOp::Dash),
                            Some('^') => (&n[..n.len() - 1], AttrOp::Prefix),
                            Some('$') => (&n[..n.len() - 1], AttrOp::Suffix),
                            Some('*') => (&n[..n.len() - 1], AttrOp::Substring),
                            _ => (n, AttrOp::Exact),
                        };
                        // A trailing standalone `i` makes the comparison ASCII
                        // case-insensitive; `s` forces the (default) sensitive
                        // match (Selectors 4 §6.3). A quoted value protects a
                        // literal trailing i (`[t="a i"]` has no whitespace-
                        // separated bare flag token).
                        let mut v = v.trim();
                        let mut ci = false;
                        if let Some((head, flag)) = v.rsplit_once(char::is_whitespace)
                            && !head.trim().is_empty()
                        {
                            if flag.eq_ignore_ascii_case("i") {
                                ci = true;
                                v = head.trim();
                            } else if flag.eq_ignore_ascii_case("s") {
                                v = head.trim();
                            }
                        }
                        (n, op, Some(v.trim_matches(['"', '\'']).to_string()), ci)
                    }
                    None => (inner.as_str(), AttrOp::Exact, None, false),
                };
                if name.trim().is_empty() {
                    return None;
                }
                compound.attrs.push(AttrSel {
                    name: name.trim().to_ascii_lowercase(),
                    op,
                    value,
                    ci,
                });
            }
            ':' => {
                chars.next();
                // `::foo` (double colon) marks a pseudo-element; `:before`
                // and `:after` have a legacy single-colon spelling too.
                if chars.peek() == Some(&':') {
                    chars.next();
                }
                let name = take_name(chars)?.to_ascii_lowercase();
                let mut arg = None;
                if chars.peek() == Some(&'(') {
                    chars.next();
                    let mut depth = 1u32;
                    let mut inner = String::new();
                    for c in chars.by_ref() {
                        match c {
                            '(' => depth += 1,
                            ')' => {
                                depth -= 1;
                                if depth == 0 {
                                    break;
                                }
                            }
                            _ => {}
                        }
                        inner.push(c);
                    }
                    if depth != 0 {
                        return None;
                    }
                    arg = Some(inner);
                }
                if name == "not" {
                    // Step-1 :not takes compounds (no top-level combinators) —
                    // a combinator makes the parse fail (rule ignored,
                    // fail-open) via the single-compound check below:
                    // `parse_compound` breaks at a combinator and the leftover
                    // `peek().is_some()` rejects it. We must NOT reject on a
                    // naive whitespace scan, because whitespace can live INSIDE
                    // a nested functional pseudo (`:not(:where(a, b c))`, a
                    // Tailwind-typography idiom) or an attribute value
                    // (`:not([title="a b"])`) — both valid single compounds.
                    // Specificity comes from the argument.
                    let mut group = Vec::new();
                    for part in split_top_level(&arg?, ',') {
                        let part = part.trim();
                        if part.is_empty() {
                            return None;
                        }
                        let mut inner_chars = part.chars().peekable();
                        let inner = parse_compound(&mut inner_chars)?;
                        if inner.is_empty() || inner_chars.peek().is_some() {
                            return None;
                        }
                        // A pseudo we can't evaluate would INVERT through
                        // `:not` into always-match (see `never_unknown`);
                        // fail the parse so the rule dies instead.
                        if inner.never_unknown {
                            return None;
                        }
                        group.push(inner);
                    }
                    compound.nots.push(group);
                } else if name == "is" || name == "where" || name == "matches" {
                    // `:is()`/`:where()` (Selectors 4 §4.2–4.3; `:matches` is
                    // the pre-rename legacy alias of `:is`): match ANY of a
                    // FORGIVING selector list of full complex selectors. An
                    // unparsable argument is dropped individually — never
                    // fatal to the rule (unlike a plain selector list); a
                    // pseudo-element subject is invalid inside and dropped
                    // too. Specificity is handled in `spec()` (`:is` = most
                    // specific argument, `:where` = zero) — the pseudo
                    // itself deliberately does NOT bump `pseudos`.
                    let mut group = Vec::new();
                    for part in split_top_level(&arg?, ',') {
                        let part = part.trim();
                        if part.is_empty() {
                            continue;
                        }
                        if let Some(cx) = parse_complex(part)
                            && cx.0.last().is_none_or(|(_, c)| c.pseudo.is_none())
                        {
                            group.push(cx);
                        }
                    }
                    compound.selects.push((group, name == "where"));
                } else if name == "has" {
                    // `:has(<forgiving-relative-selector-list>)` (Selectors 4
                    // §4.5): the element must have a matching element in its
                    // subtree / following-sibling forest. A forgiving list —
                    // unparsable/invalid args (pseudo-element subject, nested
                    // `:has`) drop individually; an all-invalid group matches
                    // nothing (the rule survives). Specificity is the most
                    // specific argument, added in `spec()`; `:has` does NOT
                    // bump `pseudos` (it's not `never`/`never_unknown` — real,
                    // evaluable relational matching).
                    let mut group = Vec::new();
                    for part in split_top_level(&arg?, ',') {
                        if part.trim().is_empty() {
                            continue;
                        }
                        if let Some(h) = parse_relative(part) {
                            group.push(h);
                        }
                    }
                    compound.has.push(group);
                } else if name == "slotted" {
                    // CSS Shadow 1 §3.2.4: the argument is exactly one
                    // compound selector. The pseudo-element itself is an
                    // alias for the flattened slottables, so its declarations
                    // are applied by the shadow-aware cascade rather than by
                    // ordinary in-tree selector matching.
                    let raw = arg?;
                    let mut ic = raw.trim().chars().peekable();
                    let inner = parse_compound(&mut ic)?;
                    if inner.is_empty() || ic.peek().is_some() {
                        return None;
                    }
                    compound.slotted = Some(Box::new(inner));
                    compound.pseudos += 1;
                } else if name == "before" || name == "after" {
                    // Generated-content pseudo-element: the compound still
                    // matches the element (tag/class parts), but the rule
                    // targets the element's ::before/::after box. Counted in
                    // `spec()` via `pseudo` (the TYPE bucket), not `pseudos`.
                    compound.pseudo = Some(if name == "before" {
                        PseudoEl::Before
                    } else {
                        PseudoEl::After
                    });
                } else if name == "scope" {
                    // Matches the query root (set by `query`); inert in the
                    // cascade. See `Compound::scope`.
                    compound.scope = true;
                    compound.pseudos += 1;
                } else if name == "root" {
                    compound.root = true;
                    compound.pseudos += 1;
                } else if name == "host" {
                    // `:host` / `:host(<compound>)`: styles the shadow host.
                    // Matched against the host in `cascaded`, not here.
                    compound.host = true;
                    compound.pseudos += 1;
                    if let Some(a) = &arg {
                        let mut ic = a.trim().chars().peekable();
                        let inner = parse_compound(&mut ic)?;
                        if inner.is_empty() || ic.peek().is_some() {
                            return None;
                        }
                        compound.host_inner = Some(Box::new(inner));
                    }
                } else if name == "empty" {
                    compound.structural.push(Structural::Empty);
                    compound.pseudos += 1;
                } else if let Some(simple) = structural_simple(&name) {
                    compound.structural.extend(simple);
                    compound.pseudos += 1;
                } else if let Some((of_type, from_end)) = match name.as_str() {
                    "nth-child" => Some((false, false)),
                    "nth-last-child" => Some((false, true)),
                    "nth-of-type" => Some((true, false)),
                    "nth-last-of-type" => Some((true, true)),
                    _ => None,
                } {
                    // A malformed/absent An+B fails the parse (rule ignored,
                    // fail-open) rather than silently mismatching. The
                    // `of <selector-list>` clause (Selectors 4 §5.5) applies
                    // to the child-indexed forms only; its list is
                    // NON-forgiving, so a bad member fails the parse too.
                    let raw = arg?;
                    let (nth_text, of_sel) = if of_type {
                        (raw.as_str(), None)
                    } else {
                        split_nth_of(&raw)
                    };
                    let nth = parse_nth(nth_text)?;
                    let of = match of_sel {
                        Some(sel) => Some(parse_nth_of(sel)?),
                        None => None,
                    };
                    compound.structural.push(Structural::Nth {
                        nth,
                        of_type,
                        from_end,
                        of,
                    });
                    compound.pseudos += 1;
                } else if name == "popover-open" {
                    // LIVE `:popover-open` (HTML §the popover attribute):
                    // matches while the element's popover is showing. Same
                    // shape as `:hover` — nothing open ⇒ a bare
                    // `:popover-open` rule is inert and `:not(:popover-open)`
                    // genuinely matches.
                    compound.popover_open = true;
                    compound.pseudos += 1;
                } else if name == "hover" {
                    // LIVE `:hover`: matches the chain under the terminal's
                    // pointer (`hover_chain`, moved per committed hover target
                    // by the `__dom_set_hover` syscall). No longer a
                    // never-pseudo — at rest the chain is empty, so a bare
                    // `:hover` rule is inert and `:not(:hover)` still
                    // genuinely matches, exactly as before the feature.
                    compound.hover = true;
                    compound.pseudos += 1;
                } else if let Some(state) = parse_state_pseudo(&name, arg.as_deref()) {
                    // Element-state pseudo-classes, evaluated against the
                    // arena (`matches_state`). A malformed `:lang()`/`:dir()`
                    // argument fails the parse (rule dropped, fail-open).
                    compound.states.push(state);
                    compound.pseudos += 1;
                } else {
                    // Valid CSS we can't satisfy YET: parse, count for
                    // specificity, never match. (Any of these can graduate
                    // to a real evaluation when the state exists — `:hover`,
                    // `:checked` & co. all started here.) Interaction
                    // pseudos are GENUINELY false at rest (no pointer, no
                    // focus), so a `:not(:focus)` wrapping them correctly
                    // matches; anything else unsupported is flagged so
                    // `:not` rejects it rather than inverting it into
                    // always-match.
                    compound.never = true;
                    compound.never_unknown = !matches!(
                        name.as_str(),
                        "active"
                            | "focus"
                            | "focus-within"
                            | "focus-visible"
                            | "visited"
                            | "target"
                    );
                    compound.pseudos += 1;
                }
            }
            c if c.is_ascii_whitespace() || c == '>' || c == '+' || c == '~' => break,
            _ => {
                let tag = take_name(chars)?;
                compound.tag = Some(tag.to_ascii_lowercase());
            }
        }
    }
    Some(compound)
}

/// An identifier, `*`, or tag token, with CSS ident ESCAPES decoded
/// (css-syntax §4.3.7, the same algorithm `unquote_css` uses for strings):
/// `\` + 1–6 hex digits (one optional trailing whitespace terminator) → the
/// code point; `\c` → the literal char. Tailwind-era class names lean on
/// escapes — `.md\:flex`, `.w-1\/2`, `.hover\:underline`, `.w-\[10px\]`
/// are the classes `md:flex`, `w-1/2`, … — so a parser without them drops
/// every responsive/state-variant rule on such sites.
fn take_name(chars: &mut std::iter::Peekable<std::str::Chars>) -> Option<String> {
    let mut out = String::new();
    while let Some(&c) = chars.peek() {
        if c == '\\' {
            chars.next();
            let mut hex = String::new();
            while hex.len() < 6 && chars.peek().is_some_and(char::is_ascii_hexdigit) {
                hex.push(chars.next().unwrap());
            }
            if !hex.is_empty() {
                // One whitespace may terminate the hex escape (`#\31 23`
                // is the ident `123` — that space is NOT a combinator).
                if chars.peek().is_some_and(|c| c.is_ascii_whitespace()) {
                    chars.next();
                }
                if let Some(ch) = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                    out.push(ch);
                }
            } else if let Some(lit) = chars.next() {
                out.push(lit);
            }
        } else if c.is_alphanumeric() || matches!(c, '-' | '_' | '*') {
            out.push(c);
            chars.next();
        } else {
            break;
        }
    }
    if out.is_empty() { None } else { Some(out) }
}

// ---- CSS visibility cascade (step 1) ---------------------------------
// A real mini-cascade for exactly two properties, `display` and
// `visibility`, so stylesheet-class hiding (.hidden{display:none}) and
// class-toggle re-showing (.menu.open{display:block}) work. Everything
// unparseable is IGNORED — fail-open always means "visible", never
// "hidden". `:hover`/`:focus` never match; @-blocks are skipped whole.

/// One CSS property the engine understands — the single source of truth
/// for the whole property surface. `is_tracked` (what the cascade stores)
/// and the serializer's bake list both derive from this table, so adding a
/// property is one entry here, not edits in three places. Kept deliberately
/// small: the box-layout primitives plus the visibility/animation set;
/// everything else is ignored (not stored, fail-open).
struct PropDef {
    name: &'static str,
    /// Inherited (CSS sense): when an element doesn't set this property,
    /// `computed_value` resolves it to the parent's computed value.
    /// `text-decoration` is deliberately NOT here — it is not inherited but
    /// *propagated* by painting (and accumulates), handled by
    /// `text_decoration` instead.
    inherited: bool,
    /// Baked into the element's inline `style` on serialization, so the
    /// re-parsed layout arena (which has no `<style>`) flows the property
    /// the way the engine computed it. `visibility` IS baked (Phase 2 — a
    /// `visibility:hidden` element is kept + painted blank, so the re-parse must
    /// see it; the DIRECT cascaded value is baked and re-parse inheritance
    /// reconstructs the rest, so a `visibility:visible` descendant re-clears it).
    /// `false` for properties consumed only inside the engine and never re-read
    /// verbatim: `opacity`/`animation*` (opacity is baked specially as its
    /// resolved effective value, see `write_attrs`; the animation longhands
    /// feed that resolution) and
    /// `content` (baked separately as `data-trust-before`/`data-trust-after`).
    baked: bool,
}

const fn prop(name: &'static str, inherited: bool, baked: bool) -> PropDef {
    PropDef {
        name,
        inherited,
        baked,
    }
}

/// The CSS-wide keywords (css-cascade-4 §7.3), valid as the whole value of
/// any property. `revert-layer` (css-cascade-5 §7.3.4) rolls back to the
/// previous cascade LAYER; the winner maps keep only the top declaration, so
/// it degrades to `revert` — the spec's own behavior when no lower layer
/// declares the property.
#[derive(Copy, Clone, PartialEq, Eq)]
enum WideKeyword {
    Inherit,
    Initial,
    Unset,
    Revert,
}

fn wide_keyword(v: &str) -> Option<WideKeyword> {
    let t = v.trim();
    // Fast bail on the first letter — this runs on every cascaded read.
    match t.as_bytes().first() {
        Some(b'i' | b'I' | b'u' | b'U' | b'r' | b'R') => {}
        _ => return None,
    }
    if t.eq_ignore_ascii_case("inherit") {
        Some(WideKeyword::Inherit)
    } else if t.eq_ignore_ascii_case("initial") {
        Some(WideKeyword::Initial)
    } else if t.eq_ignore_ascii_case("unset") {
        Some(WideKeyword::Unset)
    } else if t.eq_ignore_ascii_case("revert") || t.eq_ignore_ascii_case("revert-layer") {
        Some(WideKeyword::Revert)
    } else {
        None
    }
}

/// `PROPS` index for a property name, via a one-time name→index map. Replaces a
/// per-call `PROPS.iter().position()` linear scan — `computed_value` runs this
/// ~100k times in one heavy-page layout, so the scan was pure waste.
fn prop_index(name: &str) -> Option<usize> {
    static INDEX: std::sync::OnceLock<FxHashMap<&'static str, usize>> = std::sync::OnceLock::new();
    INDEX
        .get_or_init(|| PROPS.iter().enumerate().map(|(i, p)| (p.name, i)).collect())
        .get(name)
        .copied()
}

#[rustfmt::skip]
/// The inherited layout properties (the `inherited=true` rows of `PROPS`) — the
/// styling context that flows INTO a relayout boundary. `serialize_patch`
/// materializes these onto the fragment wrapper so an ancestor-less re-parse
/// resolves them identically (incremental-layout contract §4a). Keep in sync with
/// the `inherited=true` rows below. (`visibility` is inherited but a rendered
/// boundary is by definition visible, so it's a near-no-op; included for rigor.)
const INHERITED_LAYOUT_PROPS: &[&str] = &[
    "color",
    "text-align",
    "font-size",
    "font-family",
    "font-weight",
    "font-style",
    "line-height",
    "white-space",
    "white-space-collapse",
    "text-wrap",
    "text-wrap-mode",
    "text-transform",
    "letter-spacing",
    "word-spacing",
    "list-style-type",
    "list-style-image",
    "list-style-position",
    "text-indent",
    "text-decoration-color",
    "text-decoration-style",
    "visibility",
    "cursor",
    "pointer-events",
    "interactivity",
    "image-rendering",
    "caption-side",
    "overflow-wrap",
    "word-break",
    "tab-size",
];

const PROPS: &[PropDef] = &[
    //    name                    inherited  baked
    prop("display", false, true),
    prop("visibility", true, true),
    // CSS Color 4: `color` is inherited and is the `currentcolor` source for
    // borders and text decorations. Graphical paint must retain it instead of
    // falling back to the terminal theme at the end of layout.
    prop("color", true, true),
    // SVG 2 §6.6 presentation attributes participate in the CSS cascade.
    // These paint properties are consumed when an inline SVG is serialized
    // into the desktop image pipeline; they are not layout snapshot fields.
    prop("fill", true, false),
    prop("fill-opacity", true, false),
    prop("fill-rule", true, false),
    prop("stroke", true, false),
    prop("stroke-opacity", true, false),
    prop("stroke-width", true, false),
    prop("stroke-linecap", true, false),
    prop("stroke-linejoin", true, false),
    prop("stroke-miterlimit", true, false),
    prop("stroke-dasharray", true, false),
    prop("stroke-dashoffset", true, false),
    prop("clip-rule", true, false),
    prop("paint-order", true, false),
    prop("vector-effect", false, false),
    prop("shape-rendering", true, false),
    prop("stop-color", false, false),
    prop("stop-opacity", false, false),
    // CSS UI 4 §6.2/§6.3: both inherit and affect hit testing without
    // changing box generation. Bake them into live snapshots so the layout
    // arena sees the same interaction eligibility as the resident page DOM.
    prop("pointer-events", true, true),
    prop("interactivity", true, true),
    prop("opacity", false, false),
    prop("animation-name", false, false),
    prop("animation-duration", false, false),
    prop("animation-timing-function", false, false),
    prop("animation-iteration-count", false, false),
    prop("animation-direction", false, false),
    prop("animation-fill-mode", false, false),
    prop("animation-delay", false, false),
    prop("animation-play-state", false, false),
    prop("animation", false, false),
    prop("margin-top", false, true),
    prop("margin-bottom", false, true),
    prop("margin-left", false, true),
    prop("margin-right", false, true),
    prop("padding-top", false, true),
    prop("padding-bottom", false, true),
    prop("padding-left", false, true),
    prop("padding-right", false, true),
    prop("text-align", true, true),
    // CSS Writing Modes 4: direction inherits. It is also observable through
    // getComputedStyle (overlay-positioning libraries use it to mirror start/
    // end alignment), even though layout2 does not yet reorder bidi boxes.
    prop("direction", true, true),
    prop("font-size", true, true),
    prop("font-family", true, true),
    prop("font-weight", true, true),
    prop("font-style", true, true),
    prop("line-height", true, true),
    prop("white-space", true, true),
    // CSS Text 4 longhands: `white-space` is now the shorthand of
    // `white-space-collapse` × `text-wrap-mode` (`text-wrap` shorthands the
    // latter — modern Tailwind emits `text-wrap:nowrap`). All inherited.
    prop("white-space-collapse", true, true),
    prop("text-wrap", true, true),
    prop("text-wrap-mode", true, true),
    // CSS Overflow 3 §5.1 — chooses ellipsis vs plain clip at a nowrap
    // truncation. NOT inherited (applies to the clipping block itself).
    prop("text-overflow", false, true),
    // CSS Text 3 §5.2/§5.5: within-word break opportunities (`word-wrap` is
    // the legacy alias of `overflow-wrap`, normalized at shorthand expansion)
    // and §3 tab advance in preserved modes. All inherited per spec.
    prop("overflow-wrap", true, true),
    prop("word-break", true, true),
    prop("tab-size", true, true),
    prop("text-transform", true, true),
    prop("letter-spacing", true, true),
    prop("word-spacing", true, true),
    // CSS Inline 3 §4.2: inline-level boxes align against real typographic
    // baselines. The first pixel-native cut implements the interoperable CSS2
    // values in layout2 and retains unknown values as baseline.
    prop("vertical-align", false, true),
    prop("list-style-type", true, true),
    // CSS Lists 3 §3.3: list-style-image is inherited and must survive the
    // stylesheet-free live-page snapshot used by layout2.
    prop("list-style-image", true, true),
    prop("list-style-position", true, true),
    prop("text-indent", true, true),
    prop("text-decoration", false, true),
    prop("text-decoration-line", false, true),
    prop("text-decoration-color", false, true),
    prop("text-decoration-style", false, true),
    prop("text-shadow", true, true),
    prop("content", false, false),
    // CSS Box Sizing 3: whether declared width/height include border+padding.
    // The modern web's near-universal `*{box-sizing:border-box}` reset makes
    // this load-bearing for any width math (consumed by layout2's §10.3.3).
    prop("box-sizing", false, true),
    prop("width", false, true),
    prop("max-width", false, true),
    prop("min-width", false, true),
    prop("height", false, true),
    prop("min-height", false, true),
    prop("max-height", false, true),
    prop("aspect-ratio", false, true),
    prop("object-fit", false, true),
    prop("object-position", false, true),
    // CSS Images 3 §5.4: `pixelated`/`crisp-edges` ask for nearest-neighbor
    // scaling (blocky upscale — QR codes, pixel art). Inherited per spec;
    // baked so the app-side re-parse of a live snapshot keeps it.
    prop("image-rendering", true, true),
    prop("flex-wrap", false, true),
    prop("flex-flow", false, true),
    prop("flex-direction", false, true),
    prop("float", false, true),
    prop("clear", false, true),
    prop("overflow", false, true),
    prop("overflow-x", false, true),
    prop("overflow-y", false, true),
    // CSS 2.2 §11.1.2 legacy clipping. It applies only to absolutely
    // positioned boxes and clips the complete border box and descendants.
    prop("clip", false, true),
    // CSS Scroll Snap 1: a scroll container only card-SNAPS when it declares
    // `scroll-snap-type` (mandatory/proximity); otherwise it scrolls freely.
    // `scroll-snap-align` (on the items) is the snap-position alignment.
    prop("scroll-snap-type", false, true),
    prop("scroll-snap-align", false, true),
    // CSS UI 4 §5.1.1: cursor applies to all elements and is inherited. Bake
    // it into live presentation snapshots because graphical hit testing, not
    // box sizing, chooses the cursor for the topmost pointer target.
    prop("cursor", true, true),
    // CSS Backgrounds 3: the layout paints no color, but a declared background
    // is an OPAQUE FILL in the cell compositor (layout2 P4 — Appendix E paint
    // order: a modal's background erases the page cells under its rect).
    // `background` expands to these two in `expand_box_shorthand`.
    prop("background-color", false, true),
    prop("background-image", false, true),
    prop("background-repeat", false, true),
    prop("background-position", false, true),
    prop("background-size", false, true),
    prop("background-origin", false, true),
    prop("background-clip", false, true),
    prop("background-attachment", false, true),
    prop("position", false, true),
    // CSS Transforms 1: only the TRANSLATE functions are consumed (a paint
    // offset on out-of-flow composited boxes — `layout::translate_offset`);
    // scale/rotate/matrix stay unapplied (visual-only deviation). Baked so
    // the live-page re-parse keeps a JS-set slide-in offset.
    prop("transform", false, true),
    prop("transform-origin", false, true),
    // CSS Transforms 2 individual transform property (the modern
    // `translate: x y`); like `transform`, any non-none value forms a
    // stacking context and a containing block for out-of-flow descendants.
    prop("translate", false, true),
    prop("mix-blend-mode", false, true),
    prop("isolation", false, true),
    prop("filter", false, true),
    prop("box-shadow", false, true),
    prop("z-index", false, true),
    prop("top", false, true),
    prop("right", false, true),
    prop("bottom", false, true),
    prop("left", false, true),
    prop("flex-grow", false, true),
    prop("flex-shrink", false, true),
    prop("flex-basis", false, true),
    prop("flex", false, true),
    prop("gap", false, true),
    prop("column-gap", false, true),
    prop("row-gap", false, true),
    // css-multicol-1: the container count/width (§3.4) plus fill/span. Baked,
    // not inherited; the `columns` shorthand expands to count+width. Consumed by
    // layout2's multi-column slicer. `column-rule` is deliberately NOT tracked —
    // we render no color, so the rule glyph is dropped (only the gap survives).
    prop("column-count", false, true),
    prop("column-width", false, true),
    prop("column-fill", false, true),
    prop("column-span", false, true),
    prop("grid-template-columns", false, true),
    prop("grid-template-rows", false, true),
    prop("grid-auto-flow", false, true),
    prop("grid-auto-columns", false, true),
    prop("grid-auto-rows", false, true),
    prop("grid-column", false, true),
    prop("grid-row", false, true),
    // css-grid-1 placement longhands + named areas (consumed by layout2's
    // real §8 placement; the shorthands above stay for older content).
    prop("grid-column-start", false, true),
    prop("grid-column-end", false, true),
    prop("grid-row-start", false, true),
    prop("grid-row-end", false, true),
    prop("grid-area", false, true),
    prop("grid-template-areas", false, true),
    // css-align-3 self/items alignment (flex + grid item alignment).
    prop("align-self", false, true),
    prop("justify-self", false, true),
    prop("justify-items", false, true),
    prop("place-self", false, true),
    prop("place-items", false, true),
    prop("justify-content", false, true),
    prop("align-content", false, true),
    prop("align-items", false, true),
    prop("order", false, true),
    // CSS 2.1 §17: the table width algorithm (`fixed` vs auto) and caption
    // placement. `caption-side` is inherited per §17.4.1.
    prop("table-layout", false, true),
    prop("caption-side", true, true),
    prop("border-top-width", false, true),
    prop("border-right-width", false, true),
    prop("border-bottom-width", false, true),
    prop("border-left-width", false, true),
    prop("border-top-style", false, true),
    prop("border-right-style", false, true),
    prop("border-bottom-style", false, true),
    prop("border-left-style", false, true),
    prop("border-top-color", false, true),
    prop("border-right-color", false, true),
    prop("border-bottom-color", false, true),
    prop("border-left-color", false, true),
    prop("border-top-left-radius", false, true),
    prop("border-top-right-radius", false, true),
    prop("border-bottom-right-radius", false, true),
    prop("border-bottom-left-radius", false, true),
    // CSS Basic User Interface 4 §3: outlines are paint-only decorations and
    // therefore do not enter the box model. Keep the shorthand tracked for
    // @supports and expand it to these longhands before cascade resolution.
    prop("outline", false, true),
    prop("outline-width", false, true),
    prop("outline-style", false, true),
    prop("outline-color", false, true),
    prop("outline-offset", false, true),
];

/// Initial values which must be materialized at the CSSOM boundary rather
/// than represented by the engine's internal `None` sentinel. These cover the
/// complete positioned-box state read by standards-based fitting libraries.
/// Values come from CSS Positioned Layout 3, CSS Box Sizing 3, CSS2 §8, CSS
/// Writing Modes 4, and CSS Color 4.
fn cssom_initial_value(name: &str) -> Option<&'static str> {
    match name {
        "position" => Some("static"),
        "top" | "right" | "bottom" | "left" => Some("auto"),
        "max-width" | "max-height" => Some("none"),
        "box-sizing" => Some("content-box"),
        "margin-top" | "margin-right" | "margin-bottom" | "margin-left" | "padding-top"
        | "padding-right" | "padding-bottom" | "padding-left" => Some("0px"),
        "z-index" => Some("auto"),
        "direction" => Some("ltr"),
        "list-style-image" => Some("none"),
        "list-style-position" => Some("outside"),
        "list-style-type" => Some("disc"),
        "opacity" => Some("1"),
        _ => None,
    }
}

fn is_tracked(name: &str) -> bool {
    // Custom properties (`--foo`) are always stored so `var()` references can
    // resolve to their defined (cascaded, inherited) value at bake time, not
    // just the fallback. Unlike ordinary CSS property names, custom-property
    // names are case-sensitive (CSS Custom Properties §2).
    name.starts_with("--") || PROPS.iter().any(|p| p.name == name)
}

/// The HTML user-agent stylesheet's default `display` for a tag — what a
/// browser's `getComputedStyle(el).display` reports for an element with no
/// author `display`. jQuery's `.show()` reads an element's default display
/// (by computing the display of a throwaway element of the same tag) so it
/// can restore it; when that read comes back empty it falls back to a
/// temp-`<iframe>` probe (`iframe.contentWindow.document`) the prelude can't
/// satisfy, which threw and tore down jQuery's whole `.show()`/render path on
/// humblebundle.com. Reporting the UA display keeps jQuery off the iframe
/// path. This feeds `getComputedStyle` only; the layout owns the real display
/// via `computed_display` (author cascade + the layout's own tag tables).
fn ua_display(tag: &str) -> &'static str {
    match tag {
        "address" | "article" | "aside" | "blockquote" | "body" | "center" | "details"
        | "dialog" | "dir" | "div" | "dl" | "dd" | "dt" | "fieldset" | "figcaption" | "figure"
        | "footer" | "form" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "header" | "hgroup"
        | "hr" | "html" | "legend" | "listing" | "main" | "menu" | "nav" | "ol" | "optgroup"
        | "option" | "p" | "plaintext" | "pre" | "search" | "section" | "summary" | "ul"
        | "xmp" => "block",
        "li" => "list-item",
        "table" => "table",
        "thead" => "table-header-group",
        "tbody" => "table-row-group",
        "tfoot" => "table-footer-group",
        "tr" => "table-row",
        "td" | "th" => "table-cell",
        "caption" => "table-caption",
        "colgroup" => "table-column-group",
        "col" => "table-column",
        "button" | "input" | "select" | "textarea" | "meter" | "progress" | "marquee" => {
            "inline-block"
        }
        "head" | "title" | "meta" | "link" | "style" | "script" | "base" | "noscript"
        | "template" | "source" | "track" | "datalist" => "none",
        _ => "inline",
    }
}

/// Whether a CSS length is ≤ 1px — the box size of the "sr-only" visually
/// hidden clip idiom. Only unitless `0`/`1` and `px` lengths qualify; `em`,
/// `%`, `auto`, etc. are not the pattern and return `false`.
fn css_len_at_most_1px(v: &str) -> bool {
    let v = v.trim();
    let n = v.strip_suffix("px").unwrap_or(v).trim();
    n.parse::<f32>().is_ok_and(|x| x <= 1.0)
}

/// Whether an absolute length pushes a box FAR off-screen — the "shove it past
/// the corner" visually-hidden idiom (`left:-9999px`, `top:-1000px`, WordPress
/// `.screen-reader-text`, YouTube's skip-nav). Only absolute units (px/em/rem)
/// and only past a generous threshold, so legitimate small negative offsets (an
/// `-1.5rem` footer, a `-1px` overlap) and viewport-relative `%`/`vw` are never
/// caught.
fn css_len_offscreen_neg(v: &str) -> bool {
    let v = v.trim();
    let (num, mult) = if let Some(n) = v.strip_suffix("px") {
        (n, 1.0)
    } else if let Some(n) = v.strip_suffix("rem") {
        (n, 16.0)
    } else if let Some(n) = v.strip_suffix("em") {
        (n, 16.0)
    } else {
        (v, 1.0)
    };
    num.trim().parse::<f32>().is_ok_and(|x| x * mult <= -999.0)
}

/// Whether a CSS length/percentage is exactly zero (`0`, `0px`, `0%`, `0em`,
/// …) — its leading numeric part parses to 0. `auto`/empty/`calc(…)`/
/// non-numeric → false (we can't prove those zero, so we never hide on them).
fn css_len_is_zero(v: &str) -> bool {
    let num: String = v
        .trim()
        .chars()
        .take_while(|c| c.is_ascii_digit() || matches!(c, '.' | '-' | '+'))
        .collect();
    !num.is_empty() && num.parse::<f32>().map(|n| n == 0.0).unwrap_or(false)
}

/// Classify an element's OWN `font-size` declaration for the zero-size
/// (invisible-text) check. `Some(true)` = collapses text to nothing
/// (`font-size:0` in any unit); `Some(false)` = a definite non-zero size
/// (absolute px/pt/rem/vw, an absolute keyword, `calc()`, …); `None` = defer to
/// the inherited size (no declaration, a relative `em`/`%`/`ex`/`ch`/`lh` size
/// that merely scales the parent, or `inherit`/`unset`). We render every visible
/// glyph at one cell regardless of point size, so the ONE font-size that changes
/// layout is zero: `font-size:0` is the standard idiom for keeping copyable-but-
/// unseen text (Mastodon's `.invisible` spans hide a URL's scheme and tail this
/// way). A relative unit is left to the caller's inheritance so the
/// inline-block-whitespace-killer idiom (`ul{font-size:0} li{font-size:1rem}`)
/// re-shows an absolutely-reset descendant.
fn classify_font_size_zero(v: &str) -> Option<bool> {
    let v = v.trim();
    let first = *v.as_bytes().first()?;
    if !(first.is_ascii_digit() || matches!(first, b'.' | b'-' | b'+')) {
        // Keyword / function value.
        return match v.to_ascii_lowercase().as_str() {
            "inherit" | "unset" => None,
            _ => Some(false),
        };
    }
    let split = v
        .find(|c: char| c.is_ascii_alphabetic() || c == '%')
        .unwrap_or(v.len());
    let (num, unit) = v.split_at(split);
    let n = num.parse::<f32>().ok()?;
    if n == 0.0 {
        return Some(true);
    }
    match unit.trim().to_ascii_lowercase().as_str() {
        "em" | "ex" | "ch" | "lh" | "%" => None,
        _ => Some(false),
    }
}

/// The initial `font-size` (CSS `medium`): 16 CSS px in every browser.
pub(crate) const FONT_SIZE_INITIAL: f32 = 16.0;

/// Whether a `font` shorthand token is the `<font-size>` component: a
/// numeric length (`16px`, `1.2em`) or an absolute/relative size keyword.
/// (Weight numbers are matched by the shorthand's weight arm first.)
fn font_size_token(t: &str) -> bool {
    matches!(
        t,
        "xx-small"
            | "x-small"
            | "small"
            | "medium"
            | "large"
            | "x-large"
            | "xx-large"
            | "xxx-large"
            | "larger"
            | "smaller"
    ) || t
        .as_bytes()
        .first()
        .is_some_and(|b| b.is_ascii_digit() || matches!(b, b'.' | b'-'))
}

/// The font-size factor the UA stylesheet gives a tag (the HTML spec's
/// rendering section): headings in em, `<small>`/`<sub>`/`<sup>` `smaller`,
/// `<big>` `larger` (the 1.2 step browsers converged on).
fn ua_font_factor(tag: &str) -> Option<f32> {
    Some(match tag {
        "h1" => 2.0,
        "h2" => 1.5,
        "h3" => 1.17,
        "h4" => 1.0,
        "h5" => 0.83,
        "h6" => 0.67,
        "small" | "sub" | "sup" => 1.0 / 1.2,
        "big" => 1.2,
        _ => return None,
    })
}

/// A `font-size` declaration in CSS px, resolved against the inherited
/// (`parent`) and root sizes per CSS Fonts §6.1: the absolute keywords map
/// through the medium-relative table, `larger`/`smaller` step the inherited
/// size by 1.2, `em`/`%`/`ex`/`ch` multiply the inherited size, `rem` the
/// root's, and the physical units convert at CSS's fixed ratios (96px/in).
/// `None` (→ inherit) for anything unresolvable: `calc()`, a dangling
/// `var()`, negative sizes, garbage.
fn font_size_px(value: &str, parent: f32, root: f32) -> Option<f32> {
    let v = value.trim().to_ascii_lowercase();
    match v.as_str() {
        "xx-small" => return Some(FONT_SIZE_INITIAL * 3.0 / 5.0),
        "x-small" => return Some(FONT_SIZE_INITIAL * 3.0 / 4.0),
        "small" => return Some(FONT_SIZE_INITIAL * 8.0 / 9.0),
        "medium" => return Some(FONT_SIZE_INITIAL),
        "large" => return Some(FONT_SIZE_INITIAL * 6.0 / 5.0),
        "x-large" => return Some(FONT_SIZE_INITIAL * 3.0 / 2.0),
        "xx-large" => return Some(FONT_SIZE_INITIAL * 2.0),
        "xxx-large" => return Some(FONT_SIZE_INITIAL * 3.0),
        "larger" => return Some(parent * 1.2),
        "smaller" => return Some(parent / 1.2),
        "inherit" | "unset" | "revert" => return Some(parent),
        "initial" => return Some(FONT_SIZE_INITIAL),
        _ => {}
    }
    let split = v
        .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-'))
        .unwrap_or(v.len());
    let n: f32 = v[..split].parse().ok()?;
    if n < 0.0 || !n.is_finite() {
        return None;
    }
    Some(match v[split..].trim() {
        "em" => n * parent,
        "rem" => n * root,
        "%" => n / 100.0 * parent,
        // x-height / zero-advance ≈ half the em absent real font metrics.
        "ex" | "ch" => n * 0.5 * parent,
        "px" | "" => n,
        "pt" => n * 4.0 / 3.0,
        "pc" => n * 16.0,
        "in" => n * 96.0,
        "cm" => n * 96.0 / 2.54,
        "mm" => n * 96.0 / 25.4,
        "q" => n * 96.0 / 101.6,
        _ => return None,
    })
}

/// Parse a CSS `<alpha-value>`: a number, or a percentage (CSS Color 4 —
/// `opacity: 0%` is valid and must read as 0, not fail the parse and
/// default to fully opaque).
fn parse_alpha(v: &str) -> Option<f32> {
    let v = v.trim();
    match v.strip_suffix('%') {
        Some(p) => p.trim().parse::<f32>().ok().map(|n| n / 100.0),
        None => v.parse::<f32>().ok(),
    }
}

/// CSS Logical Properties → their physical equivalents. TRust renders only
/// horizontal-tb LTR (no `writing-mode`/`direction` support), so inline =
/// left/right and block = top/bottom — the mapping is exact for every page
/// we can render. `margin-inline: auto` is the modern centering idiom;
/// Mastodon-generation CSS uses the whole family.
fn logical_to_physical(prop: &str) -> Option<&'static str> {
    Some(match prop {
        "margin-inline-start" => "margin-left",
        "margin-inline-end" => "margin-right",
        "margin-block-start" => "margin-top",
        "margin-block-end" => "margin-bottom",
        "padding-inline-start" => "padding-left",
        "padding-inline-end" => "padding-right",
        "padding-block-start" => "padding-top",
        "padding-block-end" => "padding-bottom",
        "inset-inline-start" => "left",
        "inset-inline-end" => "right",
        "inset-block-start" => "top",
        "inset-block-end" => "bottom",
        "inline-size" => "width",
        "block-size" => "height",
        "min-inline-size" => "min-width",
        "min-block-size" => "min-height",
        "max-inline-size" => "max-width",
        "max-block-size" => "max-height",
        _ => return None,
    })
}

/// The two-value logical shorthands (`margin-inline: <start> <end>?`, …) →
/// their physical (left/right or top/bottom) longhand pair.
fn logical_pair(prop: &str) -> Option<(&'static str, &'static str)> {
    Some(match prop {
        "margin-inline" => ("margin-left", "margin-right"),
        "margin-block" => ("margin-top", "margin-bottom"),
        "padding-inline" => ("padding-left", "padding-right"),
        "padding-block" => ("padding-top", "padding-bottom"),
        "inset-inline" => ("left", "right"),
        "inset-block" => ("top", "bottom"),
        _ => return None,
    })
}

/// Expand a `margin`/`padding`/`border*`/`list-style` shorthand into the
/// longhands we track; pass anything else through unchanged.
fn expand_box_shorthand(prop: &str, value: &str) -> Vec<(String, String)> {
    // Logical properties resolve to their physical names first (LTR
    // horizontal-tb — see `logical_to_physical`).
    if let Some(phys) = logical_to_physical(prop) {
        return vec![(phys.to_string(), value.to_string())];
    }
    // `word-wrap` parses exactly as `overflow-wrap` (CSS Text 3 §5.5 — a
    // legacy alias, not a shorthand).
    if prop == "word-wrap" {
        return vec![("overflow-wrap".to_string(), value.to_string())];
    }
    if let Some((start, end)) = logical_pair(prop) {
        let toks: Vec<&str> = split_top_level_ws(value);
        let (a, b) = match toks.as_slice() {
            [x] => (*x, *x),
            [x, y] => (*x, *y),
            _ => return Vec::new(),
        };
        return vec![
            (start.to_string(), a.to_string()),
            (end.to_string(), b.to_string()),
        ];
    }
    if prop == "margin" || prop == "padding" {
        let Some([t, r, b, l]) = four_sides(value) else {
            return Vec::new();
        };
        return vec![
            (format!("{prop}-top"), t.to_string()),
            (format!("{prop}-right"), r.to_string()),
            (format!("{prop}-bottom"), b.to_string()),
            (format!("{prop}-left"), l.to_string()),
        ];
    }
    // CSS Backgrounds and Borders 3 §5.5: horizontal radii precede an
    // optional slash and vertical radii follow it. Keep the paired used-value
    // syntax in each corner longhand for graphical rounded geometry.
    if prop == "border-radius" {
        let (horizontal, vertical) =
            split_top_level_slash(value).map_or((value, value), |(h, v)| (h.trim(), v.trim()));
        let (Some(h), Some(v)) = (four_sides(horizontal), four_sides(vertical)) else {
            return Vec::new();
        };
        return [
            "border-top-left-radius",
            "border-top-right-radius",
            "border-bottom-right-radius",
            "border-bottom-left-radius",
        ]
        .into_iter()
        .enumerate()
        .map(|(i, name)| (name.to_string(), format!("{} {}", h[i], v[i])))
        .collect();
    }
    // `inset`: 1–4 values, top/right/bottom/left (the offset shorthand a
    // full-viewport modal often uses, `inset:0`).
    if prop == "inset" {
        let Some([t, r, b, l]) = four_sides(value) else {
            return Vec::new();
        };
        return vec![
            ("top".to_string(), t.to_string()),
            ("right".to_string(), r.to_string()),
            ("bottom".to_string(), b.to_string()),
            ("left".to_string(), l.to_string()),
        ];
    }
    // `border-width`/`border-style`/`border-color`: 1–4 values,
    // top/right/bottom/left.
    if let Some(kind) = prop
        .strip_prefix("border-")
        .filter(|k| *k == "width" || *k == "style" || *k == "color")
    {
        let Some(sides) = four_sides(value) else {
            return Vec::new();
        };
        return ["top", "right", "bottom", "left"]
            .iter()
            .zip(sides)
            .map(|(side, v)| (format!("border-{side}-{kind}"), v.to_string()))
            .collect();
    }
    // `border` / `border-{side}`: a `width || style || color` shorthand. We
    // retain all three components per side. The shorthand
    // RESETS omitted longhands (width ← `medium`, style ← `none` — so a
    // `border: 2px` has no visible style and computes a 0 used width, per
    // CSS 2.1 §8.5.4); a value where NOTHING parses (`border: var(--b)`)
    // keeps the old pass-through-nothing behavior rather than nuking.
    if prop == "border" {
        let sides: &[&str] = &["top", "right", "bottom", "left"];
        if wide_keyword(value).is_some() {
            return border_longhands(sides, Some(value), Some(value), Some(value));
        }
        let (w, s, c) = parse_border_shorthand(value);
        if w.is_none() && s.is_none() && c.is_none() {
            return Vec::new();
        }
        return border_longhands(
            sides,
            Some(w.unwrap_or("medium")),
            Some(s.unwrap_or("none")),
            Some(c.unwrap_or("currentcolor")),
        );
    }
    if let Some(side) = prop
        .strip_prefix("border-")
        .filter(|s| matches!(*s, "top" | "right" | "bottom" | "left"))
    {
        if wide_keyword(value).is_some() {
            return border_longhands(&[side], Some(value), Some(value), Some(value));
        }
        let (w, s, c) = parse_border_shorthand(value);
        if w.is_none() && s.is_none() && c.is_none() {
            return Vec::new();
        }
        return border_longhands(
            &[side],
            Some(w.unwrap_or("medium")),
            Some(s.unwrap_or("none")),
            Some(c.unwrap_or("currentcolor")),
        );
    }
    // CSS Basic User Interface 4 §3.1: `outline` has the same unordered
    // width/style/color grammar as `border`, but its omitted longhands reset
    // to the outline initials (medium/none/auto) and it also admits `auto` as
    // a style. `outline-offset` is intentionally separate and is not part of
    // this shorthand.
    if prop == "outline" {
        if wide_keyword(value).is_some() {
            return outline_longhands(Some(value), Some(value), Some(value));
        }
        let (w, s, c) = parse_outline_shorthand(value);
        if w.is_none() && s.is_none() && c.is_none() {
            return Vec::new();
        }
        return outline_longhands(
            Some(w.unwrap_or("medium")),
            Some(s.unwrap_or("none")),
            Some(c.unwrap_or("auto")),
        );
    }
    // `grid-gap`/`grid-row-gap`/`grid-column-gap`: the deprecated aliases of
    // `gap`/`row-gap`/`column-gap` (still emitted by older toolchains and
    // GitHub's Primer). Normalize to the modern names the layout reads.
    if let Some(rest) = prop.strip_prefix("grid-")
        && matches!(rest, "gap" | "row-gap" | "column-gap")
    {
        return expand_box_shorthand(rest, value);
    }
    // `gap: <row-gap> <column-gap>?` (css-align-3 §8.3) → the longhands, so
    // the cascade resolves shorthand-vs-longhand by source order.
    if prop == "gap" {
        let toks = split_top_level_ws(value);
        let (r, c) = match toks.as_slice() {
            [x] => (*x, *x),
            [x, y] => (*x, *y),
            _ => return Vec::new(),
        };
        return vec![
            ("row-gap".to_string(), r.to_string()),
            ("column-gap".to_string(), c.to_string()),
        ];
    }
    // `flex-flow: <'flex-direction'> || <'flex-wrap'>` (css-flexbox §5.3) —
    // omitted components reset to their initials.
    if prop == "flex-flow" {
        if wide_keyword(value).is_some() {
            return vec![
                ("flex-direction".to_string(), value.to_string()),
                ("flex-wrap".to_string(), value.to_string()),
            ];
        }
        let (mut dir, mut wrap) = (None, None);
        for t in value.split_whitespace() {
            match t.to_ascii_lowercase().as_str() {
                "row" | "row-reverse" | "column" | "column-reverse" => dir = Some(t),
                "wrap" | "nowrap" | "wrap-reverse" => wrap = Some(t),
                _ => return Vec::new(), // invalid token: drop whole
            }
        }
        return vec![
            (
                "flex-direction".to_string(),
                dir.unwrap_or("row").to_string(),
            ),
            (
                "flex-wrap".to_string(),
                wrap.unwrap_or("nowrap").to_string(),
            ),
        ];
    }
    // The `place-*` shorthands (css-align-3 §6.4/§7.4): first value is the
    // block/align component, the optional second the inline/justify one
    // (space-separated per spec — the old grid-side `/` split was wrong).
    if let Some((align, justify)) = match prop {
        "place-items" => Some(("align-items", "justify-items")),
        "place-self" => Some(("align-self", "justify-self")),
        "place-content" => Some(("align-content", "justify-content")),
        _ => None,
    } {
        let toks = split_top_level_ws(value);
        let (a, j) = match toks.as_slice() {
            [x] => (*x, *x),
            [x, y] => (*x, *y),
            _ => return Vec::new(),
        };
        return vec![
            (align.to_string(), a.to_string()),
            (justify.to_string(), j.to_string()),
        ];
    }
    // `columns: <'column-width'> || <'column-count'>` (css-multicol-1 §6.1) —
    // a bare integer is the count, anything else (a length) the width; the
    // shorthand resets BOTH longhands (a missing component becomes `auto`).
    if prop == "columns" {
        let (mut count, mut width) = (None, None);
        for t in value.split_whitespace() {
            if t.eq_ignore_ascii_case("auto") {
                continue;
            }
            if t.parse::<u32>().is_ok() {
                count = Some(t);
            } else {
                width = Some(t);
            }
        }
        return vec![
            (
                "column-count".to_string(),
                count.unwrap_or("auto").to_string(),
            ),
            (
                "column-width".to_string(),
                width.unwrap_or("auto").to_string(),
            ),
        ];
    }
    // `grid-template` (css-grid-1 §7.1): `none` and the CSS-wide keywords
    // reset all three longhands; `<rows> / <columns>` splits on the
    // top-level `/`; the areas form (`"a a" 1fr "b b" / 1fr 1fr`) extracts
    // the strings as `grid-template-areas`, the tokens between them as row
    // tracks. The shorthand always RESETS what it doesn't set.
    if prop == "grid-template" {
        let v = value.trim();
        if v.eq_ignore_ascii_case("none") || wide_keyword(v).is_some() {
            return vec![
                ("grid-template-rows".to_string(), v.to_string()),
                ("grid-template-columns".to_string(), v.to_string()),
                ("grid-template-areas".to_string(), v.to_string()),
            ];
        }
        let (rows_part, cols) = match split_top_level_slash(value) {
            Some((r, c)) => (r.trim().to_string(), c.trim().to_string()),
            // No `/`: only the areas form (strings, no columns) is valid.
            None if v.contains(['"', '\'']) => (v.to_string(), "none".to_string()),
            None => return Vec::new(),
        };
        // The areas form: quoted strings are area rows; what's between them
        // (minus line names in brackets) are the row track sizes.
        if rows_part.contains(['"', '\'']) {
            let mut areas = String::new();
            let mut rows = String::new();
            let mut chars = rows_part.chars().peekable();
            while let Some(c) = chars.next() {
                if c == '"' || c == '\'' {
                    let s: String = chars.by_ref().take_while(|&x| x != c).collect();
                    if !areas.is_empty() {
                        areas.push(' ');
                    }
                    areas.push_str(&format!("\"{s}\""));
                } else if c == '[' {
                    // Line names don't participate in track sizing here.
                    for x in chars.by_ref() {
                        if x == ']' {
                            break;
                        }
                    }
                } else {
                    rows.push(c);
                }
            }
            let rows = rows.split_whitespace().collect::<Vec<_>>().join(" ");
            return vec![
                ("grid-template-areas".to_string(), areas),
                (
                    "grid-template-rows".to_string(),
                    if rows.is_empty() {
                        "none".to_string()
                    } else {
                        rows
                    },
                ),
                ("grid-template-columns".to_string(), cols),
            ];
        }
        return vec![
            ("grid-template-rows".to_string(), rows_part),
            ("grid-template-columns".to_string(), cols),
            ("grid-template-areas".to_string(), "none".to_string()),
        ];
    }
    // `flex: none | auto | <grow> [<shrink>] [<basis>] | <basis>` → the three
    // longhands, so the CASCADE resolves them by source order (a `flex-grow:0`
    // BEFORE a `flex:1` must lose to the shorthand's grow:1 — manually merging
    // shorthand-then-longhand in the layout got this backwards). `flex:<n>`
    // sets basis 0 (not auto), per the spec.
    if prop == "flex" {
        let v = value.trim();
        // The CSS-wide keywords apply to every longhand of the shorthand
        // (they used to fall into the numeric arm as `1 1 inherit` garbage).
        if wide_keyword(v).is_some() {
            return vec![
                ("flex-grow".to_string(), v.to_string()),
                ("flex-shrink".to_string(), v.to_string()),
                ("flex-basis".to_string(), v.to_string()),
            ];
        }
        let (g, s, b) = match v.to_ascii_lowercase().as_str() {
            "none" => ("0", "0", "auto".to_string()),
            "auto" => ("1", "1", "auto".to_string()),
            "initial" | "" => ("0", "1", "auto".to_string()),
            _ => {
                let mut nums = Vec::new();
                let mut basis = None;
                // CSS Syntax 3 §5.5.7-§5.5.10: a function (including
                // `calc()`) is one component value even when its contents
                // contain whitespace. Flexbox §7.1 consumes component
                // values, so splitting `calc(50% - 5px)` at raw whitespace
                // corrupts the basis into three unrelated tokens.
                for t in split_top_level_ws(v) {
                    if t.parse::<f32>().is_ok() {
                        nums.push(t);
                    } else {
                        basis = Some(t.to_string());
                    }
                }
                let g = nums.first().copied().unwrap_or("1");
                let s = nums.get(1).copied().unwrap_or("1");
                // A bare number (`flex:1`) means basis 0; a bare basis
                // (`flex:30%`) keeps grow/shrink 1.
                let b =
                    basis.unwrap_or_else(|| if nums.is_empty() { "auto" } else { "0" }.to_string());
                (g, s, b)
            }
        };
        return vec![
            ("flex-grow".to_string(), g.to_string()),
            ("flex-shrink".to_string(), s.to_string()),
            ("flex-basis".to_string(), b),
        ];
    }
    // `font`: `<style> || <variant> || <weight> || <stretch> <size>
    // [/ <line-height>] <family>` (CSS Fonts §6.3) — expand the components we
    // track. The size is the first size-shaped token; everything after it is
    // the optional line-height and family. System-font keywords
    // (`caption`, `menu`, …) expand to nothing.
    if prop == "font" {
        if wide_keyword(value).is_some() {
            return vec![
                ("font-style".to_string(), value.to_string()),
                ("font-weight".to_string(), value.to_string()),
                ("font-size".to_string(), value.to_string()),
                ("line-height".to_string(), value.to_string()),
                ("font-family".to_string(), value.to_string()),
            ];
        }
        let tokens = split_top_level_ws(value);
        let (mut style, mut weight, mut size) = (None, None, None);
        let mut size_index = None;
        for (index, tok) in tokens.iter().copied().enumerate() {
            let t = tok.split('/').next().unwrap_or(tok);
            match t.to_ascii_lowercase().as_str() {
                "italic" | "oblique" => style = Some(t.to_string()),
                "bold" | "bolder" | "lighter" => weight = Some(t.to_string()),
                w if w
                    .parse::<u16>()
                    .is_ok_and(|n| (100..=900).contains(&n) && n % 100 == 0) =>
                {
                    weight = Some(t.to_string());
                }
                s if font_size_token(s) => {
                    size = Some(t.to_string());
                    size_index = Some(index);
                    break;
                }
                _ => {}
            }
        }
        // No size ⇒ not a valid `font` shorthand (a system-font keyword or
        // garbage) — drop whole, as before. With a size, the shorthand
        // RESETS the omitted longhands to `normal` (CSS Fonts §6.3).
        let Some(size) = size else {
            return Vec::new();
        };
        let index = size_index.unwrap_or(0);
        let size_token = tokens[index];
        let mut line_height = size_token
            .split_once('/')
            .map(|(_, height)| height.trim().to_string())
            .filter(|height| !height.is_empty());
        let mut family_start = index + 1;
        if line_height.is_none() && tokens.get(family_start).copied() == Some("/") {
            line_height = tokens
                .get(family_start + 1)
                .map(|height| (*height).to_string());
            family_start += 2;
        } else if line_height.is_none()
            && tokens
                .get(family_start)
                .is_some_and(|token| token.starts_with('/'))
        {
            line_height = tokens
                .get(family_start)
                .map(|height| height.trim_start_matches('/').to_string());
            family_start += 1;
        }
        let family = tokens[family_start..].join(" ");
        return vec![
            (
                "font-style".to_string(),
                style.unwrap_or_else(|| "normal".into()),
            ),
            (
                "font-weight".to_string(),
                weight.unwrap_or_else(|| "normal".into()),
            ),
            ("font-size".to_string(), size),
            (
                "line-height".to_string(),
                line_height.unwrap_or_else(|| "normal".into()),
            ),
            (
                "font-family".to_string(),
                if family.is_empty() {
                    "serif".into()
                } else {
                    family
                },
            ),
        ];
    }
    // `background`: only the color and the image longhands are consumed (the
    // layout paints no color, but a declared background is an OPAQUE FILL in
    // layout2's cell compositor). Classification is by grammar EXCLUSION —
    // CSS Backgrounds 3 §3.10's <bg-layer> idents all come from closed
    // keyword sets, so a remaining ident/function in the FINAL layer (the
    // only one that may carry a color) is the <background-color>. The
    // shorthand RESETS omitted longhands (color ← transparent, image ← none),
    // which the cascade needs to order `background:none` after a color rule.
    if prop == "background" {
        return expand_background(value);
    }
    // `list-style: <position> || <image> || <type>` (CSS Lists 3 §3.6).
    // Every omitted component resets to its initial value; in particular an
    // image from an earlier declaration must not survive a later shorthand.
    if prop == "list-style" {
        if wide_keyword(value).is_some() {
            return ["list-style-type", "list-style-image", "list-style-position"]
                .into_iter()
                .map(|name| (name.to_string(), value.to_string()))
                .collect();
        }
        let image = list_style_shorthand_image(value).unwrap_or("none");
        let position = split_top_level_ws(value)
            .into_iter()
            .find(|t| matches!(t.to_ascii_lowercase().as_str(), "inside" | "outside"))
            .unwrap_or("outside");
        let kind = list_style_shorthand_type(value).unwrap_or("disc");
        return vec![
            ("list-style-type".to_string(), kind.to_string()),
            ("list-style-image".to_string(), image.to_string()),
            ("list-style-position".to_string(), position.to_string()),
        ];
    }
    vec![(prop.to_string(), value.to_string())]
}

const PENDING_BACKGROUND_SHORTHAND: &str = "\0trust-pending-background:";

/// `background` shorthand → retained graphical longhands (CSS Backgrounds 3
/// §2.10). A declaration is expanded only after every layer parses: an invalid
/// image or component invalidates the entire declaration, so an earlier valid
/// fallback keeps winning in the cascade. For each valid layer omitted
/// components are reset to their initial values before explicit values apply.
fn expand_background(value: &str) -> Vec<(String, String)> {
    let v = value.trim();
    if v.is_empty() {
        return Vec::new();
    }
    // The raw shorthand rides along: it's untracked (sheets filter it), but
    // the inline-style path stores untracked props for getComputedStyle.
    let mut out = vec![("background".to_string(), v.to_string())];
    // CSS Variables §3: a shorthand containing var() cannot be parsed into
    // longhands until computed-value time. Preserve the complete token stream
    // on each longhand; `resolve_pending_shorthand` substitutes first and then
    // runs this parser again. This also preserves shorthand reset semantics.
    if v.contains("var(") {
        let pending = format!("{PENDING_BACKGROUND_SHORTHAND}{v}");
        for name in [
            "background-color",
            "background-image",
            "background-repeat",
            "background-position",
            "background-size",
            "background-origin",
            "background-clip",
            "background-attachment",
        ] {
            out.push((name.to_string(), pending.clone()));
        }
        return out;
    }
    // CSS-wide keywords apply to every longhand of the shorthand.
    if matches!(
        v.to_ascii_lowercase().as_str(),
        "inherit" | "initial" | "unset" | "revert" | "revert-layer"
    ) {
        for name in [
            "background-color",
            "background-image",
            "background-repeat",
            "background-position",
            "background-size",
            "background-origin",
            "background-clip",
            "background-attachment",
        ] {
            out.push((name.to_string(), v.to_string()));
        }
        return out;
    }
    let layers = split_top_level_commas(v);
    if layers.iter().any(|layer| layer.trim().is_empty()) {
        return Vec::new();
    }
    let last = layers.len() - 1;
    let mut color = None;
    let mut images = Vec::with_capacity(layers.len());
    let mut positions = Vec::with_capacity(layers.len());
    let mut sizes = Vec::with_capacity(layers.len());
    let mut repeats = Vec::with_capacity(layers.len());
    let mut origins = Vec::with_capacity(layers.len());
    let mut clips = Vec::with_capacity(layers.len());
    let mut attachments = Vec::with_capacity(layers.len());
    for (i, layer) in layers.iter().enumerate() {
        let Some(parsed) = parse_background_layer(layer, i == last) else {
            return Vec::new();
        };
        if let Some(layer_color) = parsed.color
            && color.replace(layer_color).is_some()
        {
            return Vec::new();
        }
        images.push(parsed.image.unwrap_or("none"));
        positions.push(if parsed.position.is_empty() {
            "0% 0%".to_string()
        } else {
            parsed.position.join(" ")
        });
        sizes.push(if parsed.size.is_empty() {
            "auto auto".to_string()
        } else {
            parsed.size.join(" ")
        });
        repeats.push(if parsed.repeat.is_empty() {
            "repeat".to_string()
        } else {
            parsed.repeat.join(" ")
        });
        attachments.push(parsed.attachment.unwrap_or("scroll"));
        let (origin, clip) = match parsed.boxes.as_slice() {
            [] => ("padding-box", "border-box"),
            [one] => (*one, *one),
            [origin, clip] => (*origin, *clip),
            _ => return Vec::new(),
        };
        origins.push(origin);
        clips.push(clip);
    }
    out.push((
        "background-color".to_string(),
        color.unwrap_or("transparent").to_string(),
    ));
    out.push(("background-image".to_string(), images.join(", ")));
    out.extend([
        ("background-repeat".to_string(), repeats.join(", ")),
        ("background-position".to_string(), positions.join(", ")),
        ("background-size".to_string(), sizes.join(", ")),
        ("background-origin".to_string(), origins.join(", ")),
        ("background-clip".to_string(), clips.join(", ")),
        ("background-attachment".to_string(), attachments.join(", ")),
    ]);
    out
}

#[derive(Default)]
struct BackgroundLayer<'a> {
    color: Option<&'a str>,
    image: Option<&'a str>,
    position: Vec<&'a str>,
    size: Vec<&'a str>,
    repeat: Vec<&'a str>,
    boxes: Vec<&'a str>,
    attachment: Option<&'a str>,
}

fn parse_background_layer(layer: &str, final_layer: bool) -> Option<BackgroundLayer<'_>> {
    let (before, size) = match split_top_level_slash(layer) {
        Some((before, size)) => {
            if split_top_level_slash(size).is_some() {
                return None;
            }
            (before, Some(size))
        }
        None => (layer, None),
    };
    let mut parsed = BackgroundLayer::default();
    for tok in split_top_level_ws(before) {
        let lower = tok.to_ascii_lowercase();
        if bg_image_token(&lower) {
            if parsed.image.is_some() || !valid_background_image(tok) {
                return None;
            }
            parsed.image = Some(tok);
        } else if matches!(lower.as_str(), "repeat-x" | "repeat-y") {
            if !parsed.repeat.is_empty() {
                return None;
            }
            parsed.repeat.push(tok);
        } else if matches!(lower.as_str(), "repeat" | "space" | "round" | "no-repeat") {
            if parsed.repeat.len() == 2 {
                return None;
            }
            parsed.repeat.push(tok);
        } else if matches!(lower.as_str(), "scroll" | "fixed" | "local") {
            if parsed.attachment.replace(tok).is_some() {
                return None;
            }
        } else if matches!(
            lower.as_str(),
            "border-box" | "padding-box" | "content-box" | "text"
        ) {
            if parsed.boxes.len() == 2 {
                return None;
            }
            parsed.boxes.push(tok);
        } else if lower == "none" {
            if parsed.image.replace(tok).is_some() {
                return None;
            }
        } else if background_position_token(&lower) {
            if parsed.position.len() == 4 {
                return None;
            }
            parsed.position.push(tok);
        } else if final_layer && parsed.color.is_none() && bg_color_token(&lower) {
            parsed.color = Some(tok);
        } else {
            return None;
        }
    }
    if let Some(size) = size {
        // The slash belongs to `<position> / <bg-size>`; it cannot introduce
        // a size when the position was omitted.
        if parsed.position.is_empty() {
            return None;
        }
        let tokens = split_top_level_ws(size);
        let size_len = tokens
            .iter()
            .take(2)
            .take_while(|token| background_size_token(&token.to_ascii_lowercase()))
            .count();
        parsed.size.extend_from_slice(&tokens[..size_len]);
        if parsed.size.is_empty()
            || (parsed.size.len() == 2
                && parsed.size.iter().any(|token| {
                    matches!(token.to_ascii_lowercase().as_str(), "cover" | "contain")
                }))
        {
            return None;
        }
        // Components after the size remain unordered by `||` in the shorthand
        // grammar. Position is the sole exception because the slash closes it.
        for tok in &tokens[size_len..] {
            let lower = tok.to_ascii_lowercase();
            if bg_image_token(&lower) {
                if parsed.image.is_some() || !valid_background_image(tok) {
                    return None;
                }
                parsed.image = Some(tok);
            } else if matches!(lower.as_str(), "repeat-x" | "repeat-y") {
                if !parsed.repeat.is_empty() {
                    return None;
                }
                parsed.repeat.push(tok);
            } else if matches!(lower.as_str(), "repeat" | "space" | "round" | "no-repeat") {
                if parsed.repeat.len() == 2 {
                    return None;
                }
                parsed.repeat.push(tok);
            } else if matches!(lower.as_str(), "scroll" | "fixed" | "local") {
                if parsed.attachment.replace(tok).is_some() {
                    return None;
                }
            } else if matches!(
                lower.as_str(),
                "border-box" | "padding-box" | "content-box" | "text"
            ) {
                if parsed.boxes.len() == 2 {
                    return None;
                }
                parsed.boxes.push(tok);
            } else if final_layer && parsed.color.is_none() && bg_color_token(&lower) {
                parsed.color = Some(tok);
            } else {
                return None;
            }
        }
    }
    Some(parsed)
}

fn background_position_token(token: &str) -> bool {
    matches!(token, "left" | "right" | "top" | "bottom" | "center")
        || background_length_percentage(token)
}

fn background_size_token(token: &str) -> bool {
    matches!(token, "auto" | "cover" | "contain") || background_length_percentage(token)
}

fn background_length_percentage(token: &str) -> bool {
    token.starts_with(|c: char| c.is_ascii_digit() || matches!(c, '+' | '-' | '.'))
        || ["calc(", "min(", "max(", "clamp("]
            .iter()
            .any(|function| token.starts_with(function))
}

/// Recognize standard `<image>` functions and reject legacy proprietary
/// gradient spellings. CSS Images 3 §3.1 requires keyword directions to use
/// `to <side-or-corner>`; `linear-gradient(top,...)` is therefore an invalid
/// declaration, not an image that clears a preceding solid-color fallback.
fn valid_background_image(token: &str) -> bool {
    let lower = token.to_ascii_lowercase();
    if lower.starts_with('-') || !lower.ends_with(')') {
        return false;
    }
    for name in ["linear-gradient(", "repeating-linear-gradient("] {
        if let Some(args) = lower.strip_prefix(name).and_then(|v| v.strip_suffix(')')) {
            let first = split_top_level_commas(args)
                .first()
                .copied()
                .unwrap_or("")
                .trim();
            if matches!(
                first,
                "top"
                    | "right"
                    | "bottom"
                    | "left"
                    | "top left"
                    | "left top"
                    | "top right"
                    | "right top"
                    | "bottom left"
                    | "left bottom"
                    | "bottom right"
                    | "right bottom"
            ) {
                return false;
            }
            return true;
        }
    }
    [
        "url(",
        "image(",
        "image-set(",
        "cross-fade(",
        "radial-gradient(",
        "repeating-radial-gradient(",
    ]
    .iter()
    .any(|name| lower.starts_with(name))
}

/// Split a comma-separated list at paren-depth 0 (`linear-gradient(a, b)`
/// stays one piece).
fn split_top_level_commas(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut start = 0usize;
    for (i, b) in s.bytes().enumerate() {
        match b {
            b'(' => depth += 1,
            b')' => depth -= 1,
            b',' if depth == 0 => {
                out.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&s[start..]);
    out
}

/// Split a component value into tokens at top-level whitespace and `/`
/// (the position/size separator), keeping function calls whole.
fn split_value_tokens(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut start: Option<usize> = None;
    for (i, b) in s.bytes().enumerate() {
        let boundary = depth == 0 && (b.is_ascii_whitespace() || b == b'/');
        match b {
            b'(' => depth += 1,
            b')' => depth -= 1,
            _ => {}
        }
        if boundary {
            if let Some(st) = start.take() {
                out.push(&s[st..i]);
            }
        } else if start.is_none() {
            start = Some(i);
        }
    }
    if let Some(st) = start {
        out.push(&s[st..]);
    }
    out
}

/// Whether a (lowercased) token is a `<bg-image>` value.
fn bg_image_token(t: &str) -> bool {
    t.starts_with("url(")
        || t.starts_with("image(")
        || t.starts_with("image-set(")
        || t.starts_with("-webkit-image-set(")
        || t.starts_with("cross-fade(")
        || t.contains("gradient(")
}

/// Whether a (lowercased) token can only be the `<background-color>` of a
/// `background` shorthand layer — everything the other components' closed
/// keyword/value sets do not claim (CSS Backgrounds 3 §3.10 grammar).
fn bg_color_token(t: &str) -> bool {
    const NOT_COLOR: &[&str] = &[
        // <repeat-style>, <attachment>, <box>, <position>/<bg-size> keywords.
        "repeat",
        "repeat-x",
        "repeat-y",
        "no-repeat",
        "space",
        "round",
        "scroll",
        "fixed",
        "local",
        "border-box",
        "padding-box",
        "content-box",
        "text",
        "center",
        "top",
        "bottom",
        "left",
        "right",
        "auto",
        "cover",
        "contain",
        "none",
    ];
    if t.is_empty() || NOT_COLOR.contains(&t) {
        return false;
    }
    // Lengths/percentages/numbers are position/size components.
    if t.starts_with(|c: char| c.is_ascii_digit() || c == '-' || c == '+' || c == '.') {
        return false;
    }
    if let Some(f) = t.split('(').next().filter(|_| t.contains('(')) {
        // Function colors only; var()/calc()/position functions are not
        // resolvable as a color here.
        return matches!(
            f,
            "rgb"
                | "rgba"
                | "hsl"
                | "hsla"
                | "hwb"
                | "lab"
                | "lch"
                | "oklab"
                | "oklch"
                | "color"
                | "color-mix"
                | "light-dark"
        );
    }
    // '#rrggbb', 'transparent', 'currentcolor', or a named color — the only
    // idents the grammar leaves.
    true
}

/// Split a value on the first `/` at paren-depth 0 (so a `minmax(a, b)` or
/// `repeat(2, 1fr)` track keeps its inner contents). `None` if there is no
/// top-level slash. Used for the `grid-template: rows / columns` shorthand.
fn split_top_level_slash(value: &str) -> Option<(&str, &str)> {
    let mut depth = 0i32;
    for (i, b) in value.bytes().enumerate() {
        match b {
            b'(' => depth += 1,
            b')' => depth -= 1,
            b'/' if depth == 0 => return Some((&value[..i], &value[i + 1..])),
            _ => {}
        }
    }
    None
}

/// The top/right/bottom/left values of a CSS 1–4-value box shorthand. Splits
/// on TOP-LEVEL whitespace so a `calc()`/`var()` component stays one value.
fn four_sides(value: &str) -> Option<[&str; 4]> {
    let p: Vec<&str> = split_top_level_ws(value);
    match p.as_slice() {
        [a] => Some([a, a, a, a]),
        [a, b] => Some([a, b, a, b]),
        [a, b, c] => Some([a, b, c, b]),
        [a, b, c, d] => Some([a, b, c, d]),
        _ => None,
    }
}

/// The `(width, style, color)` of a `border`/`border-<side>` shorthand.
/// Order-independent: the style keyword and a width token (`thin`/`medium`/
/// `thick` or a length) are picked out; a recognized color is retained.
fn parse_border_shorthand(value: &str) -> (Option<&str>, Option<&str>, Option<&str>) {
    const STYLES: &[&str] = &[
        "none", "hidden", "solid", "dashed", "dotted", "double", "groove", "ridge", "inset",
        "outset",
    ];
    let mut width = None;
    let mut style = None;
    let mut color = None;
    for tok in split_value_tokens(value) {
        if STYLES.contains(&tok) {
            style = Some(tok);
        } else if tok == "thin"
            || tok == "medium"
            || tok == "thick"
            || tok.starts_with(|c: char| c.is_ascii_digit() || c == '.')
        {
            width = Some(tok);
        } else if tok.eq_ignore_ascii_case("currentcolor")
            || tok.eq_ignore_ascii_case("transparent")
            || tok.parse::<svgtypes::Color>().is_ok()
            || tok.starts_with("hsl(")
            || tok.starts_with("hwb(")
            || tok.starts_with("color(")
        {
            color = Some(tok);
        }
    }
    (width, style, color)
}

fn parse_outline_shorthand(value: &str) -> (Option<&str>, Option<&str>, Option<&str>) {
    const STYLES: &[&str] = &[
        "auto", "none", "hidden", "solid", "dashed", "dotted", "double", "groove", "ridge",
        "inset", "outset",
    ];
    let mut width = None;
    let mut style = None;
    let mut color = None;
    for tok in split_value_tokens(value) {
        if STYLES.contains(&tok) {
            style = Some(tok);
        } else if tok == "thin"
            || tok == "medium"
            || tok == "thick"
            || tok.starts_with(|c: char| c.is_ascii_digit() || c == '.' || c == '-')
        {
            width = Some(tok);
        } else if tok.eq_ignore_ascii_case("currentcolor")
            || tok.eq_ignore_ascii_case("transparent")
            || tok.parse::<svgtypes::Color>().is_ok()
            || tok.starts_with("hsl(")
            || tok.starts_with("hwb(")
            || tok.starts_with("color(")
        {
            color = Some(tok);
        }
    }
    (width, style, color)
}

fn outline_longhands(w: Option<&str>, s: Option<&str>, c: Option<&str>) -> Vec<(String, String)> {
    [
        ("outline-width", w),
        ("outline-style", s),
        ("outline-color", c),
    ]
    .into_iter()
    .filter_map(|(name, value)| value.map(|value| (name.to_string(), value.to_string())))
    .collect()
}

fn border_longhands(
    sides: &[&str],
    w: Option<&str>,
    s: Option<&str>,
    c: Option<&str>,
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for side in sides {
        if let Some(w) = w {
            out.push((format!("border-{side}-width"), w.to_string()));
        }
        if let Some(s) = s {
            out.push((format!("border-{side}-style"), s.to_string()));
        }
        if let Some(c) = c {
            out.push((format!("border-{side}-color"), c.to_string()));
        }
    }
    out
}

/// The `list-style-type` keyword inside a `list-style` shorthand, if present.
fn list_style_shorthand_type(value: &str) -> Option<&str> {
    const TYPES: &[&str] = &[
        "none",
        "disc",
        "circle",
        "square",
        "decimal",
        "decimal-leading-zero",
        "lower-alpha",
        "upper-alpha",
        "lower-latin",
        "upper-latin",
        "lower-roman",
        "upper-roman",
    ];
    split_top_level_ws(value)
        .into_iter()
        .find(|t| TYPES.contains(&t.to_ascii_lowercase().as_str()))
}

/// The `<image>` component inside a `list-style` shorthand. CSS Lists permits
/// any CSS image function; the graphical marker path currently paints URL
/// images and safely ignores other image functions until their raster source
/// is available.
fn list_style_shorthand_image(value: &str) -> Option<&str> {
    split_top_level_ws(value).into_iter().find(|token| {
        let lower = token.to_ascii_lowercase();
        lower == "none"
            || lower.starts_with("url(")
            || lower.starts_with("image(")
            || lower.starts_with("cross-fade(")
            || lower.starts_with("element(")
            || lower.starts_with("linear-gradient(")
            || lower.starts_with("radial-gradient(")
            || lower.starts_with("conic-gradient(")
    })
}

/// One parsed rule, holding its tracked declarations (`(prop, (important,
/// value))`). Rules mentioning no tracked property are never stored.
struct StyleRule {
    selector: Complex,
    specificity: (u32, u32, u32),
    /// Source position across every sheet of the scope.
    order: usize,
    /// The rule's cascade-layer position (css-cascade-5 §6.4), pre-encoded
    /// for each importance (the layer order REVERSES for `!important`).
    /// See `encode_layer`; unlayered rules carry the implicit-final-layer
    /// encodings.
    layer_normal: u64,
    layer_important: u64,
    decls: Vec<(String, (bool, String))>,
}

impl StyleRule {
    /// The importance-matched cascade-layer encoding for the cascade key.
    fn layer_key(&self, important: bool) -> u64 {
        if important {
            self.layer_important
        } else {
            self.layer_normal
        }
    }
}

/// (!important, context, inline, layer, specificity, source order): the
/// cascade key; lexicographic max wins. `context` implements CSS Cascade 5
/// §6.1: outer wins for normal declarations, inner wins for important ones.
/// `layer` is the importance-adjusted cascade-layer encoding (`encode_layer`);
/// it sits AFTER context and the inline flag because encapsulation context is
/// sorted before element-attached styles, and element-attached styles before
/// layers (CSS Cascade 5 §6.1), and BEFORE specificity (layers beat
/// specificity — the point of the feature).
type CascadeKey = (bool, bool, bool, u64, (u32, u32, u32), usize);

/// Rules bucketed by tree scope: DOCUMENT for the light DOM, the shadow
/// fragment for each shadow tree. Shadow sheets never leak out;
/// document sheets never reach in.
#[derive(Default)]
struct StyleIndex {
    scopes: FxHashMap<NodeId, Vec<StyleRule>>,
    /// Per-scope rule index, keyed by each rule's rightmost-compound key
    /// (id/class/tag/universal) — the standard browser "rule hash" so an
    /// element only tests rules that could possibly match it (see
    /// `matched_rules`). Parallel to `scopes`; values index into it.
    buckets: FxHashMap<NodeId, RuleBuckets>,
    /// `(shadow-root, rule-index)` entries for `::slotted()` selectors. These
    /// rules are consulted while cascading a light-DOM element assigned to a
    /// slot in the corresponding shadow tree.
    slotted_rules: Vec<(NodeId, u32)>,
    /// The last `@keyframes` rule for each case-sensitive name. Animation
    /// declarations do not participate in the ordinary cascade; values are
    /// retained by property and sorted offset so paint can sample supported
    /// tracks without reparsing the stylesheet every frame.
    keyframes: FxHashMap<String, KeyframesRule>,
    /// Whether any rule sets `opacity` at all — lets `paint_suppressed` skip
    /// the opacity cascade entirely on the overwhelming majority of pages.
    has_opacity: bool,
    /// One probe per `:hover`-bearing compound of every rule whose
    /// applicability depends on the hover chain AND whose declarations can
    /// change the RENDER (a `PROPS`-tracked property, generated `content`, or
    /// a custom property — which can feed a tracked one via `var()`).
    /// `set_hover_chain` tests the elements whose hover state flips against
    /// these to decide whether a hover move needs a restyle at all.
    hover_probes: Vec<HoverProbe>,
    /// Render-affecting hover rules indexed by their selector subjects
    /// (rightmost compounds). `set_hover_chain` compares their old/new match
    /// state to attribute invalidation to the subjects that actually changed.
    hover_buckets: FxHashMap<NodeId, RuleBuckets>,
    /// A child-list/text mutation inside `display:none` can nevertheless
    /// change a rendered selector subject through `:has()` or through
    /// `:empty` combined with an outside combinator. Conservatively disable
    /// boxless-content suppression whenever the active sheet set contains
    /// either dependency.
    boxless_content_may_escape: bool,
}

#[derive(Clone, Debug, Default)]
struct KeyframesRule {
    properties: FxHashMap<String, Vec<KeyframeValue>>,
}

#[derive(Clone, Debug)]
struct KeyframeValue {
    offset: f32,
    value: String,
}

impl KeyframesRule {
    fn end_value(&self, property: &str) -> Option<&str> {
        self.properties
            .get(property)?
            .last()
            .map(|keyframe| keyframe.value.as_str())
    }
}

#[derive(Clone, Debug)]
pub(crate) struct CssAnimationDefinition {
    pub name: String,
    pub duration_seconds: f32,
    pub delay_seconds: f32,
    pub iteration_count: Option<f32>,
    pub direction: String,
    pub fill_mode: String,
    pub timing_function: String,
    pub running: bool,
    pub keyframes: Vec<CssAnimationKeyframe>,
}

#[derive(Clone, Debug)]
pub(crate) struct CssAnimationKeyframe {
    pub offset: f32,
    pub top: Option<String>,
    pub transform: Option<String>,
}

/// A cheap could-match test for the compound that carries a `:hover` — the
/// element that must sit ON the chain for its rule to apply. Simple keys only
/// (tag / id / first class): false positives cost one spurious re-render;
/// false negatives are forbidden (a missed restyle is silently wrong).
struct HoverProbe {
    tag: Option<String>,
    id: Option<String>,
    class: Option<String>,
    /// `:hover` nested inside `:is()`/`:where()`/`:not()`/`:host()` — the
    /// polarity/grouping analysis isn't worth it; match ANY element.
    any: bool,
}

impl HoverProbe {
    fn could_match(&self, dom: &Dom, e: NodeId) -> bool {
        if self.any {
            return true;
        }
        if let Some(t) = &self.tag
            && dom.tag_name(e) != Some(t.as_str())
        {
            return false;
        }
        if let Some(i) = &self.id
            && dom.attr(e, "id") != Some(i.as_str())
        {
            return false;
        }
        if let Some(c) = &self.class {
            let classes = dom.attr(e, "class").unwrap_or("");
            if !classes.split_ascii_whitespace().any(|t| t == c) {
                return false;
            }
        }
        true
    }
}

/// Whether `:hover` occurs anywhere INSIDE the compound's logical arguments
/// (`:is`/`:where`/`:not`/`:host(...)`) — as opposed to directly on it.
fn compound_has_nested_hover(c: &Compound) -> bool {
    c.nots
        .iter()
        .flatten()
        .any(|n| n.hover || compound_has_nested_hover(n))
        || c.selects.iter().any(|(group, _)| {
            group.iter().any(|cx| {
                cx.0.iter()
                    .any(|(_, cc)| cc.hover || compound_has_nested_hover(cc))
            })
        })
        || c.has.iter().flatten().any(|arg| {
            arg.complex
                .0
                .iter()
                .any(|(_, inner)| inner.hover || compound_has_nested_hover(inner))
        })
        || c.structural.iter().any(|structural| match structural {
            Structural::Nth { of: Some(of), .. } => of.iter().any(|complex| {
                complex
                    .0
                    .iter()
                    .any(|(_, inner)| inner.hover || compound_has_nested_hover(inner))
            }),
            _ => false,
        })
        || c.host_inner
            .as_deref()
            .is_some_and(|h| h.hover || compound_has_nested_hover(h))
}

fn rule_uses_hover(rule: &StyleRule) -> bool {
    rule.selector
        .0
        .iter()
        .any(|(_, c)| c.hover || compound_has_nested_hover(c))
}

fn rule_affects_render(rule: &StyleRule) -> bool {
    rule.decls
        .iter()
        .any(|(k, _)| k == "content" || k.starts_with("--") || is_tracked(k))
}

/// Whether changing this rule can only change pixels or pointer eligibility,
/// never box construction, intrinsic measurement, normal-flow geometry, or
/// stacking order. This deliberately small allow-list makes a hover restyle
/// eligible to start boundary selection at the selector subject itself. Any
/// unknown property, custom property, generated content, transform, opacity,
/// or z-order effect keeps the conservative `Attr` path.
fn rule_is_paint_only(rule: &StyleRule) -> bool {
    rule.decls.iter().all(|(name, _)| {
        matches!(
            name.as_str(),
            "color"
                | "visibility"
                | "cursor"
                | "pointer-events"
                | "interactivity"
                | "image-rendering"
                | "text-shadow"
                | "box-shadow"
        ) || name.starts_with("background-")
            || name.starts_with("text-decoration")
            || name.ends_with("-color")
            || name.ends_with("-radius")
    })
}

/// The hover probes of one rule: one per compound in its complex selector
/// that carries `:hover` directly (probe = that compound's simple keys), plus
/// an any-element probe if `:hover` hides inside logical pseudos.
fn hover_probes_of(rule: &StyleRule) -> Vec<HoverProbe> {
    let mut probes = Vec::new();
    for (_, c) in &rule.selector.0 {
        if c.hover {
            probes.push(HoverProbe {
                tag: c.tag.clone().filter(|t| t != "*"),
                id: c.id.clone(),
                class: c.classes.first().cloned(),
                any: false,
            });
        } else if compound_has_nested_hover(c) {
            probes.push(HoverProbe {
                tag: None,
                id: None,
                class: None,
                any: true,
            });
        }
    }
    probes
}

/// Rules of one scope, bucketed by the rightmost compound's most-selective
/// simple key. An element gathers candidates from the buckets matching its own
/// id/classes/tag plus `universal` (rules whose subject has no id/class/tag,
/// e.g. `*`, `[attr]`, pseudo-only), then full-matches only those. Each rule
/// lands in exactly one bucket, so the candidate sets are disjoint.
#[derive(Default)]
struct RuleBuckets {
    by_id: FxHashMap<String, Vec<u32>>,
    by_class: FxHashMap<String, Vec<u32>>,
    by_tag: FxHashMap<String, Vec<u32>>,
    universal: Vec<u32>,
}

impl RuleBuckets {
    fn build(rules: &[StyleRule]) -> Self {
        Self::build_where(rules, |_| true)
    }

    fn build_where(rules: &[StyleRule], keep: impl Fn(&StyleRule) -> bool) -> Self {
        let mut b = RuleBuckets::default();
        for (i, r) in rules.iter().enumerate() {
            if !keep(r) {
                continue;
            }
            let i = i as u32;
            // The subject (rightmost) compound decides the bucket; the most
            // selective key present wins (id > first class > tag).
            match r.selector.0.last().map(|(_, c)| c) {
                Some(c) if c.id.is_some() => {
                    b.by_id.entry(c.id.clone().unwrap()).or_default().push(i);
                }
                Some(c) if !c.classes.is_empty() => {
                    b.by_class.entry(c.classes[0].clone()).or_default().push(i);
                }
                Some(c) if c.tag.as_deref().is_some_and(|t| t != "*") => {
                    b.by_tag.entry(c.tag.clone().unwrap()).or_default().push(i);
                }
                _ => b.universal.push(i),
            }
        }
        b
    }

    fn candidates(&self, dom: &Dom, id: NodeId, out: &mut Vec<u32>) {
        out.extend(self.universal.iter().copied());
        if let Some(value) = dom.attr(id, "id")
            && let Some(indices) = self.by_id.get(value)
        {
            out.extend(indices.iter().copied());
        }
        if let Some(classes) = dom.attr(id, "class") {
            for class in classes.split_ascii_whitespace() {
                if let Some(indices) = self.by_class.get(class) {
                    out.extend(indices.iter().copied());
                }
            }
        }
        if let Some(tag) = dom.tag_name(id)
            && let Some(indices) = self.by_tag.get(tag)
        {
            out.extend(indices.iter().copied());
        }
        out.sort_unstable();
        out.dedup();
    }
}

/// Parse one `prop: value [!important]` declaration. CSS keywords are ASCII
/// case-insensitive, but strings and URL payloads are not: lowercasing a URL
/// here changes the resource identity on case-sensitive servers (for example
/// `/images/HeartDot.png`). Preserve those tokens while normalizing the rest.
fn parse_decl(decl: &str) -> Option<(String, String, bool)> {
    let (k, v) = decl.split_once(':')?;
    let k = k.trim();
    // CSS Custom Properties §2: custom-property names compare codepoint for
    // codepoint, and their arbitrary token streams retain author casing.
    // Ordinary property names remain ASCII case-insensitive.
    let custom = k.starts_with("--");
    let k = if custom {
        k.to_string()
    } else {
        k.to_ascii_lowercase()
    };
    let v = v.trim();
    let (v, important) = match v.rsplit_once('!') {
        Some((head, bang)) if bang.trim().eq_ignore_ascii_case("important") => (head, true),
        _ => (v, false),
    };
    let v = v.trim();
    let value = if custom || k == "content" {
        v.to_string()
    } else {
        normalize_css_value(v)
    };
    // CSSOM §6.7.1 parses a declaration value against the property's grammar
    // before it can enter a declaration block. CSS Values 4 §6 permits a
    // unitless <number> as a <length> only when it is zero. In particular,
    // `element.style.height = window.innerHeight` passes a string such as
    // "768" to CSSStyleDeclaration; that assignment is invalid and must not
    // override a valid stylesheet height. Keep this inexpensive grammar guard
    // at the declaration boundary, where both inline and sheet declarations
    // get the same fallback/cascade behavior.
    if property_rejects_unitless_nonzero_length(&k)
        && split_top_level_ws(&value)
            .into_iter()
            .any(is_bare_nonzero_css_number)
    {
        return None;
    }
    Some((k, value, important))
}

/// Properties whose bare numeric components are lengths, never numbers.
/// Functional tokens stay intact under `split_top_level_ws`, so numbers inside
/// `calc()`/color functions are left to those grammars rather than mistaken
/// for top-level lengths. Keep the corresponding CSSStyleDeclaration guard in
/// `js_platform.js` aligned with this list.
fn property_rejects_unitless_nonzero_length(property: &str) -> bool {
    matches!(
        property,
        "width"
            | "min-width"
            | "max-width"
            | "height"
            | "min-height"
            | "max-height"
            | "inline-size"
            | "min-inline-size"
            | "max-inline-size"
            | "block-size"
            | "min-block-size"
            | "max-block-size"
            | "margin"
            | "margin-top"
            | "margin-right"
            | "margin-bottom"
            | "margin-left"
            | "margin-inline"
            | "margin-block"
            | "margin-inline-start"
            | "margin-inline-end"
            | "margin-block-start"
            | "margin-block-end"
            | "padding"
            | "padding-top"
            | "padding-right"
            | "padding-bottom"
            | "padding-left"
            | "padding-inline"
            | "padding-block"
            | "padding-inline-start"
            | "padding-inline-end"
            | "padding-block-start"
            | "padding-block-end"
            | "inset"
            | "inset-inline"
            | "inset-block"
            | "inset-inline-start"
            | "inset-inline-end"
            | "inset-block-start"
            | "inset-block-end"
            | "top"
            | "right"
            | "bottom"
            | "left"
            | "gap"
            | "row-gap"
            | "column-gap"
            | "column-width"
            | "flex-basis"
            | "font-size"
            | "letter-spacing"
            | "word-spacing"
            | "text-indent"
            | "vertical-align"
            | "border-width"
            | "border-top-width"
            | "border-right-width"
            | "border-bottom-width"
            | "border-left-width"
            | "border-radius"
            | "border-top-left-radius"
            | "border-top-right-radius"
            | "border-bottom-right-radius"
            | "border-bottom-left-radius"
            | "outline-width"
            | "outline-offset"
            | "background-position"
            | "background-size"
            | "object-position"
            | "transform-origin"
            | "translate"
            | "box-shadow"
            | "text-shadow"
    )
}

fn is_bare_nonzero_css_number(value: &str) -> bool {
    let value = value.trim_matches(|c: char| c == ',' || c == '/').trim();
    !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'+' | b'-' | b'.' | b'e' | b'E'))
        && value
            .parse::<f64>()
            .is_ok_and(|number| number.is_finite() && number != 0.0)
}

fn normalize_css_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut quote = None;
    let mut chars = value.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if let Some(q) = quote {
            out.push(c);
            if c == q {
                quote = None;
            }
            continue;
        }
        if matches!(c, '\'' | '"') {
            quote = Some(c);
            out.push(c);
            continue;
        }
        // Preserve the complete url() token, including an unquoted path,
        // while keyword-normalizing the surrounding declaration.
        // `i` is a UTF-8 boundary, but `i + 4` is not necessarily one: CSS
        // values are Unicode code-point streams, and a three-byte code point
        // can straddle that byte offset. CSS Syntax tokenization must inspect
        // code points without ever slicing through their UTF-8 encoding.
        if value
            .get(i..i.saturating_add(4))
            .is_some_and(|candidate| candidate.eq_ignore_ascii_case("url("))
        {
            out.push_str(&value[i..i + 4]);
            // The iterator has consumed only the leading `u`; consume the
            // `r`, `l`, and opening parenthesis already copied above.
            chars.next();
            chars.next();
            chars.next();
            for (_, inner) in chars.by_ref() {
                out.push(inner);
                if inner == ')' {
                    break;
                }
            }
            continue;
        }
        // `var()`'s function name is ASCII-insensitive like other CSS syntax,
        // but its first argument is a case-sensitive custom-property name.
        // Normalize the function token, then copy that argument verbatim;
        // fallback tokens resume ordinary normalization in the outer loop.
        if value
            .get(i..i.saturating_add(4))
            .is_some_and(|candidate| candidate.eq_ignore_ascii_case("var("))
        {
            out.push_str("var(");
            chars.next();
            chars.next();
            chars.next();
            for (_, inner) in chars.by_ref() {
                out.push(inner);
                if matches!(inner, ',' | ')') {
                    break;
                }
            }
            continue;
        }
        out.extend(c.to_lowercase());
    }
    out
}

/// The pseudo-element a rule's subject (last compound) targets, if any.
fn rule_pseudo(rule: &StyleRule) -> Option<PseudoEl> {
    rule.selector.0.last().and_then(|(_, c)| c.pseudo)
}

/// Strip the surrounding quotes from a CSS string and decode its escapes
/// (`\HEX ` codepoints and `\c` literals). `None` if `v` isn't quoted.
fn unquote_css(v: &str) -> Option<String> {
    let quote = v.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    if v.chars().count() < 2 || !v.ends_with(quote) {
        return None;
    }
    let inner: String = {
        let mut it = v.chars();
        it.next();
        it.next_back();
        it.collect()
    };
    let mut out = String::new();
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let mut hex = String::new();
        while hex.len() < 6 && chars.peek().is_some_and(char::is_ascii_hexdigit) {
            hex.push(chars.next().unwrap());
        }
        if !hex.is_empty() {
            // CSS allows one trailing whitespace to delimit the escape.
            if chars.peek() == Some(&' ') {
                chars.next();
            }
            if let Some(ch) = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                out.push(ch);
            }
        } else if let Some(lit) = chars.next() {
            out.push(lit);
        }
    }
    Some(out)
}

fn strip_css_comments(css: &str) -> Cow<'_, str> {
    if !css.contains("/*") {
        return Cow::Borrowed(css);
    }
    let mut out = String::with_capacity(css.len());
    let mut rest = css;
    while let Some(i) = rest.find("/*") {
        out.push_str(&rest[..i]);
        out.push(' ');
        match rest[i + 2..].find("*/") {
            Some(j) => rest = &rest[i + 2 + j + 2..],
            None => return Cow::Owned(out),
        }
    }
    out.push_str(rest);
    Cow::Owned(out)
}

/// The cascade-layer name registry for ONE tree scope (css-cascade-5 §6.4:
/// "Cascade layers are scoped to their origin and context" — a shadow
/// tree's layer order is independent of the document's, exactly like our
/// per-scope rule vecs). Layers are ordered by FIRST declaration; a dotted
/// name (`a.b`) nests, so a layer's identity is its per-level
/// sibling-declaration-index path.
#[derive(Default)]
struct LayerRegistry {
    /// Fully-qualified dotted name → per-level sibling-index path.
    paths: std::collections::HashMap<String, Vec<u32>>,
    /// Next first-declaration index per parent name ("" = the root level).
    counters: std::collections::HashMap<String, u32>,
    /// Anonymous-layer uniquifier: each `@layer { … }` occurrence "gains a
    /// unique anonymous segment" (a new layer every time).
    anon: u32,
}

impl LayerRegistry {
    /// Declare (idempotently) a fully-qualified dotted layer name, creating
    /// missing ancestors, and return its path. The FIRST declaration fixes
    /// the order; later mentions return the existing path unchanged.
    fn declare(&mut self, name: &str) -> Vec<u32> {
        if let Some(p) = self.paths.get(name) {
            return p.clone();
        }
        let parent = name.rfind('.').map_or("", |i| &name[..i]);
        let mut path = if parent.is_empty() {
            Vec::new()
        } else {
            self.declare(parent)
        };
        let ctr = self.counters.entry(parent.to_string()).or_insert(0);
        path.push(*ctr);
        *ctr += 1;
        self.paths.insert(name.to_string(), path.clone());
        path
    }

    /// A fresh unique name for an anonymous `@layer { … }` block under
    /// `parent` ("" = top level). `<` can't appear in an author CSS ident,
    /// so anonymous names can never collide with declared ones.
    fn anon_name(&mut self, parent: &str) -> String {
        self.anon += 1;
        if parent.is_empty() {
            format!("<anon-{}>", self.anon)
        } else {
            format!("{parent}.<anon-{}>", self.anon)
        }
    }
}

/// Encode a cascade-layer path into ONE lexicographically-comparable u64
/// (css-cascade-5 §6.4): four 16-bit per-level components, most significant
/// first. A present component is the layer's first-declaration index among
/// its siblings; a missing level is the IMPLICIT final (sub)layer — the
/// spec puts a parent layer's direct rules "in an implicit sub-layer after
/// the explicitly nested layers", and unlayered rules (the empty path) "in
/// an implicit final layer" after everything. For NORMAL declarations the
/// LAST layer wins, so implicit levels encode 0xFFFF (max); for IMPORTANT
/// declarations the layer order REVERSES ("for important rules the
/// declaration whose cascade layer is first wins"), so every component
/// flips. Depth caps at 4 levels and width at 0xFFFE siblings — beyond
/// either the ordering degrades gracefully (real-world sheets are flat:
/// Tailwind v4 declares 4 top-level layers).
fn encode_layer(path: &[u32], important: bool) -> u64 {
    let mut key = 0u64;
    for lvl in 0..4 {
        let comp = match path.get(lvl) {
            Some(&i) => {
                let i = u64::from(i.min(0xFFFD));
                if important { 0xFFFE - i } else { i }
            }
            None => {
                if important {
                    0
                } else {
                    0xFFFF
                }
            }
        };
        key = (key << 16) | comp;
    }
    key
}

/// A syntactically-plausible `<layer-name>` (`<ident> [ '.' <ident> ]*`).
/// Loose on ident internals (unicode allowed) but strict on shape: no
/// empty segments, no whitespace. An invalid name invalidates its whole
/// `@layer` rule, per CSS error handling.
fn valid_layer_name(n: &str) -> bool {
    !n.is_empty()
        && n.split('.').all(|seg| {
            !seg.is_empty()
                && seg
                    .chars()
                    .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_'))
        })
}

/// Qualify `name` against the enclosing layer (`@layer a { @layer b {…} }`
/// "concatenates their names" → `a.b`).
fn qualify_layer(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}.{name}")
    }
}

/// Collect a sheet's tracked rules into `out`. `@keyframes` end-opacity is
/// harvested; `@media` is evaluated against `viewport` (the CSS-pixel
/// viewport) and its body spliced in when it matches (dropped otherwise);
/// `@layer` declares/enters cascade layers (`layers` + `layer`, the
/// enclosing layer's qualified name, "" = unlayered); other @-blocks are
/// skipped whole. Rules whose selectors don't parse are skipped
/// (fail-open).
#[derive(Clone, Copy)]
struct MediaEnvironment {
    viewport: (f32, f32),
    density: f32,
}

fn parse_sheet(
    css: &str,
    order: &mut usize,
    out: &mut Vec<StyleRule>,
    keyframes: &mut FxHashMap<String, KeyframesRule>,
    media: MediaEnvironment,
    layers: &mut LayerRegistry,
    layer: &str,
) {
    let css = strip_css_comments(css);
    let mut rest = css.as_ref();
    // The enclosing layer's path stamps every rule this call emits.
    // `declare` is idempotent — the layer was declared when its block was
    // entered, so this is a lookup.
    let lpath = if layer.is_empty() {
        Vec::new()
    } else {
        layers.declare(layer)
    };
    loop {
        rest = rest.trim_start();
        if rest.is_empty() {
            return;
        }
        if let Some(after) = rest.strip_prefix('@') {
            // CSS Animations 1 §3: keyframe names are case-sensitive and the
            // last rule of a given name wins. Store supported property tracks
            // in sorted offset order; duplicate offsets cascade in rule order.
            let lower = after.trim_start().to_ascii_lowercase();
            if let Some(rest_name) = lower
                .strip_prefix("keyframes")
                .or_else(|| lower.strip_prefix("-webkit-keyframes"))
                && let Some(brace_off) = after.find('{')
            {
                let name = after[after.len() - rest_name.len()..brace_off]
                    .trim()
                    .to_string();
                let (block, tail) = take_block(&after[brace_off..]);
                keyframes.insert(name, parse_keyframes_rule(block));
                rest = tail;
                continue;
            }
            // `@media <query> { ... }`: evaluate the query against the
            // viewport and splice the matching block's rules into the cascade
            // (recurse, so nested @media and normal rules both work); drop the
            // body when it doesn't match. The viewport is the shared
            // CSS-pixel initial containing block reported to page script.
            if let Some(rest_q) = lower.strip_prefix("media")
                && rest_q
                    .chars()
                    .next()
                    .is_none_or(|c| !c.is_ascii_alphanumeric() && c != '-')
                && let Some(brace_off) = after.find('{')
            {
                let query = &after[after.len() - rest_q.len()..brace_off];
                let (block, tail) = take_block(&after[brace_off..]);
                if media_query_matches_with_density(query, media.viewport, media.density) {
                    parse_sheet(block, order, out, keyframes, media, layers, layer);
                }
                rest = tail;
                continue;
            }
            // `@supports <condition> { ... }`: a CSS feature query (progressive
            // enhancement). We DO implement grid/flex/gap/aspect-ratio/etc., so
            // honor the enhanced block when we support the condition — and DROP
            // an old-browser `@supports not (display:grid)` fallback. The web's
            // dominant pattern is a flex fallback under `#x{display:flex}` plus
            // `@supports (display:grid){#x{display:grid;grid-template-columns:…}}`
            // (the IA infinite-scroller's uniform tile grid is exactly this);
            // skipping the query left us on the flex fallback. Mirrors @media.
            if let Some(rest_c) = lower.strip_prefix("supports")
                && rest_c
                    .chars()
                    .next()
                    .is_none_or(|c| !c.is_ascii_alphanumeric() && c != '-')
                && let Some(brace_off) = after.find('{')
            {
                let cond = &after[after.len() - rest_c.len()..brace_off];
                let (block, tail) = take_block(&after[brace_off..]);
                if supports_condition(cond) {
                    parse_sheet(block, order, out, keyframes, media, layers, layer);
                }
                rest = tail;
                continue;
            }
            // `@layer` (css-cascade-5 §6.4): the STATEMENT form
            // (`@layer a, b.c;`) declares layers in order without assigning
            // rules; the BLOCK form (`@layer name? { … }`) declares the
            // layer on first mention and assigns the block's rules to it —
            // the body is a full stylesheet (nested @media/@supports/@layer
            // recurse). An anonymous block is a NEW unique layer each time.
            // Before this, @layer blocks fell to the generic skip, so a
            // Tailwind-v4-era sheet (everything inside layers) contributed
            // nothing to the cascade.
            if let Some(rest_l) = lower.strip_prefix("layer")
                && rest_l
                    .chars()
                    .next()
                    .is_none_or(|c| !c.is_ascii_alphanumeric() && c != '-')
            {
                let prelude_start = after.len() - rest_l.len();
                let semi = after.find(';');
                let brace = after.find('{');
                // Statement form: the `;` comes before any `{`.
                if let Some(s) = semi
                    && brace.is_none_or(|b| s < b)
                {
                    let names: Vec<&str> =
                        after[prelude_start..s].split(',').map(str::trim).collect();
                    // Any invalid name invalidates the whole statement (CSS
                    // error handling); a valid list declares each in order.
                    if names.iter().all(|n| valid_layer_name(n)) {
                        for n in names {
                            layers.declare(&qualify_layer(layer, n));
                        }
                    }
                    rest = &after[s + 1..];
                    continue;
                }
                if let Some(b) = brace {
                    let name_txt = after[prelude_start..b].trim();
                    let (block, tail) = take_block(&after[b..]);
                    rest = tail;
                    let qualified = if name_txt.is_empty() {
                        layers.anon_name(layer)
                    } else if valid_layer_name(name_txt) {
                        qualify_layer(layer, name_txt)
                    } else {
                        continue; // malformed name: drop the block (fail-open)
                    };
                    layers.declare(&qualified);
                    parse_sheet(block, order, out, keyframes, media, layers, &qualified);
                    continue;
                }
                return; // no `;` and no `{`: malformed tail
            }
            // Other @-rules (@charset/@import end at ';'; block at-rules at
            // their balanced '}') are skipped whole.
            rest = match (after.find(';'), after.find('{')) {
                (Some(s), Some(b)) if s < b => &after[s + 1..],
                (_, Some(b)) => take_block(&after[b..]).1,
                (Some(s), None) => &after[s + 1..],
                (None, None) => return,
            };
            continue;
        }
        let Some(brace) = rest.find('{') else { return };
        let selector_text = rest[..brace].trim();
        let (block, after) = take_block(&rest[brace..]);
        rest = after;
        parse_style_rule(selector_text, block, order, out, media, &lpath);
    }
}

/// Process one style-rule body into `out`: emit its own declarations for the
/// already-concrete selector list `resolved`, then recurse into any nested
/// rules (CSS Nesting), expanding each nested selector's `&` against
/// `resolved`. A nested `@media` applies its body to `resolved` when it
/// matches the viewport. `resolved` never carries an unexpanded `&` — the
/// top-level caller passes the raw selector and `expand_nesting` resolves the
/// `&` before each recursion.
///
/// Without this, a nested rule's declarations would leak onto the parent: the
/// width-reservation/underline idiom `.tab { &::after { width:100% } }` (Steam's
/// `.supernav`, Primer, many design systems) would make `.tab` itself
/// `width:100%`, breaking horizontal nav layouts.
fn parse_style_rule(
    resolved: &str,
    block: &str,
    order: &mut usize,
    out: &mut Vec<StyleRule>,
    media: MediaEnvironment,
    layer: &[u32],
) {
    let (decl_text, nested) = split_block(block);
    let decls = collect_decls(&decl_text);
    if !decls.is_empty()
        && let Some(SelectorList(complexes, _)) = SelectorList::parse(resolved.trim())
    {
        for selector in complexes {
            out.push(StyleRule {
                specificity: selector.specificity(),
                selector,
                order: *order,
                layer_normal: encode_layer(layer, false),
                layer_important: encode_layer(layer, true),
                decls: decls.clone(),
            });
            *order += 1;
        }
    }
    for (nsel, nblock) in nested {
        // A nested grouping at-rule (CSS Nesting allows `@media`/`@supports`
        // inside a style rule). Evaluate `@media`/`@supports`; on a match apply
        // its body to the SAME parent selector. Other nested at-rules are
        // skipped whole — never leak their declarations onto the parent.
        if let Some(at) = nsel.strip_prefix('@') {
            let at = at.trim_start();
            let lower = at.to_ascii_lowercase();
            let kw_ok = |kw: &str| {
                lower.strip_prefix(kw).is_some_and(|r| {
                    r.chars()
                        .next()
                        .is_none_or(|c| !c.is_ascii_alphanumeric() && c != '-')
                })
            };
            if (kw_ok("media")
                && media_query_matches_with_density(&at[5..], media.viewport, media.density))
                || (kw_ok("supports") && supports_condition(&at[8..]))
            {
                parse_style_rule(resolved, nblock, order, out, media, layer);
            }
            continue;
        }
        let child = expand_nesting(nsel, resolved);
        parse_style_rule(&child, nblock, order, out, media, layer);
    }
}

/// Parse a declaration block's text into tracked `(prop, (important, value))`
/// pairs (later wins; never demote `!important`); shorthands are expanded.
fn collect_decls(decl_text: &str) -> Vec<(String, (bool, String))> {
    let mut decls: Vec<(String, (bool, String))> = Vec::new();
    for decl in decl_text.split(';') {
        let Some((k, v, important)) = parse_decl(decl) else {
            continue;
        };
        for (pk, pv) in expand_box_shorthand(&k, &v) {
            if !is_tracked(&pk) {
                continue;
            }
            if let Some(slot) = decls.iter_mut().find(|(n, _)| *n == pk) {
                if important >= slot.1.0 {
                    slot.1 = (important, pv);
                }
            } else {
                decls.push((pk, (important, pv)));
            }
        }
    }
    decls
}

/// Split a rule body into its declaration text and its nested rules
/// `(prelude, body)` (CSS Nesting). A top-level `{` begins a nested rule whose
/// prelude is the text back to the previous `;`/`}`; the remaining segments are
/// declarations. String/paren/bracket aware so a `;` or `{` inside `url(...)`,
/// `[attr=…]`, or a quoted value (`content:"{"`) doesn't split. The common
/// nesting-free block borrows its text unchanged.
fn split_block(block: &str) -> (Cow<'_, str>, Vec<(&str, &str)>) {
    if !block.contains('{') {
        return (Cow::Borrowed(block), Vec::new());
    }
    let bytes = block.as_bytes();
    let mut decls = String::new();
    let mut nested: Vec<(&str, &str)> = Vec::new();
    let mut seg_start = 0usize;
    let mut in_str: Option<u8> = None;
    let (mut paren, mut bracket) = (0i32, 0i32);
    let mut i = 0usize;
    while i < bytes.len() {
        let c = bytes[i];
        if let Some(q) = in_str {
            if c == b'\\' {
                i += 2;
                continue;
            }
            if c == q {
                in_str = None;
            }
            i += 1;
            continue;
        }
        match c {
            b'"' | b'\'' => in_str = Some(c),
            b'(' => paren += 1,
            b')' => paren = (paren - 1).max(0),
            b'[' => bracket += 1,
            b']' => bracket = (bracket - 1).max(0),
            b';' if paren == 0 && bracket == 0 => {
                let seg = &block[seg_start..i];
                if !seg.trim().is_empty() {
                    decls.push_str(seg);
                    decls.push(';');
                }
                seg_start = i + 1;
            }
            b'{' if paren == 0 && bracket == 0 => {
                let prelude = block[seg_start..i].trim();
                let (inner, tail) = take_block(&block[i..]);
                if !prelude.is_empty() {
                    nested.push((prelude, inner));
                }
                i += block[i..].len() - tail.len();
                seg_start = i;
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    let seg = &block[seg_start..];
    if !seg.trim().is_empty() {
        decls.push_str(seg);
        decls.push(';');
    }
    (Cow::Owned(decls), nested)
}

/// Expand a nested selector against its parent (CSS Nesting `&`). Returns a
/// concrete comma-joined selector list. Each `&` is replaced by the parent; a
/// nested selector with no `&` is a descendant (`parent nested`). When the
/// parent is itself a list, the product over (parent × nested) parts is taken
/// — equivalent to substituting `:is(parent)` for matching, without needing
/// `:is`.
fn expand_nesting(nested: &str, parent: &str) -> String {
    let parents = split_top_level(parent, ',');
    let mut out: Vec<String> = Vec::new();
    for n in split_top_level(nested, ',') {
        let n = n.trim();
        if n.is_empty() {
            continue;
        }
        for p in &parents {
            let p = p.trim();
            if n.contains('&') {
                out.push(n.replace('&', p));
            } else {
                out.push(format!("{p} {n}"));
            }
        }
    }
    out.join(", ")
}

/// Evaluate a CSS `@supports` condition — does TRust support it? Feature
/// queries gate progressively-enhanced CSS (`@supports (display:grid){…}` over a
/// flex fallback). We honor what we actually implement. Grammar (CSS
/// Conditional §): `not`, `and`, `or`, parens, `( <declaration> )` feature
/// tests, and `selector( <complex-selector> )`. An unrecognized function form
/// (`<general-enclosed>`) is treated as unsupported, so a page falls back.
fn supports_condition(cond: &str) -> bool {
    let c = cond.trim();
    if c.is_empty() {
        return false;
    }
    // `not <in-parens>`
    if let Some(rest) = c.strip_prefix("not ").or_else(|| c.strip_prefix("not(")) {
        // Re-attach the `(` we may have eaten so `supports_in_parens` sees it.
        let rest = if c.starts_with("not(") {
            &c["not".len()..]
        } else {
            rest
        };
        return !supports_in_parens(rest.trim());
    }
    // `and`/`or` chains (a chain can't mix the two without parens, per spec).
    let ands = split_supports_kw(c, "and");
    if ands.len() > 1 {
        return ands.iter().all(|p| supports_in_parens(p));
    }
    let ors = split_supports_kw(c, "or");
    if ors.len() > 1 {
        return ors.iter().any(|p| supports_in_parens(p));
    }
    supports_in_parens(c)
}

/// One `<supports-in-parens>`: `( <condition> )`, `( <declaration> )`,
/// `selector( … )`, or an unknown function form.
fn supports_in_parens(s: &str) -> bool {
    let s = s.trim();
    if let Some(inner) = s
        .strip_prefix("selector(")
        .and_then(|x| x.strip_suffix(')'))
    {
        // We support the query if our selector engine can parse the selector.
        return SelectorList::parse(inner.trim()).is_some();
    }
    if let Some(inner) = s.strip_prefix('(').and_then(|x| x.strip_suffix(')')) {
        let inner = inner.trim();
        // `( <condition> )` — a nested condition begins with `(` or `not`/has a
        // top-level and/or; otherwise it's `( <declaration> )`.
        if inner.starts_with('(')
            || inner.starts_with("not ")
            || inner.starts_with("not(")
            || split_supports_kw(inner, "and").len() > 1
            || split_supports_kw(inner, "or").len() > 1
        {
            return supports_condition(inner);
        }
        if let Some((prop, value)) = inner.split_once(':') {
            return css_supports(prop.trim(), value.trim());
        }
        return false;
    }
    false // a bare ident or unknown function form: general-enclosed → unsupported
}

/// Split a `@supports` condition on a top-level ` and `/` or ` keyword
/// (paren-depth 0), trimming each part. Returns one element when absent.
/// Byte-wise: the keyword pattern is pure ASCII, so a match position is
/// always a char boundary — a multi-byte char in the condition must never
/// be sliced into (str-indexing `cond[i..]` at every byte offset panicked
/// on non-ASCII input; the byte-slice compare can't).
fn split_supports_kw(cond: &str, kw: &str) -> Vec<String> {
    let bytes = cond.as_bytes();
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut start = 0usize;
    let mut i = 0usize;
    let pat = format!(" {kw} ");
    let pat = pat.as_bytes();
    while i < bytes.len() {
        match bytes[i] {
            b'(' => depth += 1,
            b')' => depth -= 1,
            _ => {}
        }
        if depth == 0
            && bytes[i..].len() >= pat.len()
            && bytes[i..i + pat.len()].eq_ignore_ascii_case(pat)
        {
            parts.push(cond[start..i].trim().to_string());
            i += pat.len();
            start = i;
            continue;
        }
        i += 1;
    }
    parts.push(cond[start..].trim().to_string());
    parts
}

/// Does TRust support a CSS `(prop: value)` feature declaration? `display` is
/// value-checked (the most commonly feature-queried property — we claim the box
/// types we actually lay out); every other property we TRACK counts as
/// supported (we understand and apply it), while a property we don't track —
/// the visual-only ones we deliberately skip (filter/transform/clip-path/…) —
/// is unsupported, so a page's fallback applies instead.
fn css_supports(prop: &str, value: &str) -> bool {
    let prop = prop.to_ascii_lowercase();
    let value = value.to_ascii_lowercase();
    if value.is_empty() {
        return false;
    }
    if prop == "display" {
        return matches!(
            value.as_str(),
            "grid"
                | "inline-grid"
                | "flex"
                | "inline-flex"
                | "block"
                | "inline"
                | "inline-block"
                | "none"
                | "list-item"
                | "table"
                | "inline-table"
                | "table-row"
                | "table-cell"
                | "table-row-group"
                | "table-header-group"
                | "table-footer-group"
                | "table-column"
                | "table-column-group"
                | "table-caption"
                | "contents"
                | "flow-root"
        );
    }
    // Retained for clean diagnostics/future backend work, but not painted.
    // Merely preserving a declaration must not make feature queries select a
    // path whose required visual effect TRust cannot provide.
    if prop == "filter" {
        return false;
    }
    is_tracked(&prop)
}

/// Does a CSS `@media` query list match the viewport (CSS px; `0` = unknown)?
/// A comma list is OR. Within one query, conditions join with `and`; a
/// recognized media type (`screen`/`all`) and the width/height/orientation
/// features are evaluated, `not`/`only` honored. Anything unrecognized — or a
/// width/height test with an unknown viewport — makes that query NOT match,
/// which drops its rules exactly as skipping the whole `@media` block used to.
#[cfg(test)]
fn media_query_matches(query: &str, vp: (f32, f32)) -> bool {
    media_query_matches_with_density(query, vp, 1.0)
}

fn media_query_matches_with_density(query: &str, vp: (f32, f32), density: f32) -> bool {
    let density = if density.is_finite() && density > 0.0 {
        density
    } else {
        1.0
    };
    query
        .split(',')
        .any(|q| media_query_one(&q.trim().to_ascii_lowercase(), vp, density))
}

/// One comma-separated media query (already lowercased). A leading
/// `not`/`only` is a prefix on the whole query (not an `and`-joined part);
/// the rest is a media type and/or `and`-joined `(feature: value)` conditions.
fn media_query_one(q: &str, vp: (f32, f32), density: f32) -> bool {
    let mut q = q.trim();
    let mut negate = false;
    if let Some(rest) = q.strip_prefix("not ") {
        negate = true;
        q = rest.trim();
    } else if let Some(rest) = q.strip_prefix("only ") {
        q = rest.trim();
    }
    if q.is_empty() {
        return !negate; // bare `@media { }` / `@media only` applies to all
    }
    let mut matches = true;
    for part in q.split(" and ") {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some(inner) = part.strip_prefix('(') {
            if !media_feature_matches(inner.trim_end_matches(')'), vp, density) {
                matches = false;
            }
        } else {
            // A bare token: only `screen`/`all` are our medium; any other type
            // (print/speech/tv/…) or unknown word can't match.
            match part {
                "screen" | "all" => {}
                _ => matches = false,
            }
        }
    }
    matches ^ negate
}

/// A single media condition against the viewport: the classic
/// `feature: value` form, the Media Queries L4 range form (`width >= 40em`,
/// `400px <= width < 900px`), or a boolean feature (`(hover)`).
///
/// The environment answers reflect what the terminal actually is: a
/// hover-dispatching (MQ4 §7.1: `hover`), mouse-driven (`pointer: fine`)
/// interactive browser (`display-mode: browser`, `scripting: enabled` — the
/// evaluator only runs on the JS pipeline) on a color output device
/// (`color: 8` — the terminal is truecolor even though the render style is
/// monochromatic), a CHARACTER GRID (`grid: 1` — the tty case the feature
/// was specified for), at 1 device pixel per CSS pixel (`resolution:
/// 1dppx`). The preferences: `prefers-reduced-motion: reduce` (cells can't
/// animate smoothly) and `prefers-color-scheme: dark` (the terminal
/// aesthetic).
fn media_feature_matches(inner: &str, vp: (f32, f32), density: f32) -> bool {
    let (vw, vh) = vp;
    let Some((name, value)) = inner.split_once(':') else {
        // No colon: the L4 range syntax when a comparison operator is
        // present, else the boolean-context form (MQ4 §2.4.1: false when
        // the feature's value would be zero/none).
        if inner.contains(['<', '>', '=']) {
            return media_range_matches(inner, vp);
        }
        return match inner.trim() {
            "width" => vw != 0.0,
            "height" => vh != 0.0,
            "aspect-ratio" | "orientation" => vw != 0.0 && vh != 0.0,
            "color" | "color-gamut" | "hover" | "any-hover" | "pointer" | "any-pointer"
            | "update" | "scripting" | "resolution" | "grid" => true,
            "monochrome" => false,
            _ => false,
        };
    };
    let value = value.trim();
    let ratio = || {
        (vw != 0.0 && vh != 0.0)
            .then(|| media_ratio(value).map(|r| (vw / vh, r)))
            .flatten()
    };
    let num = || value.parse::<f32>().ok();
    match name.trim() {
        "min-width" => vw != 0.0 && media_px(value).is_some_and(|n| vw >= n),
        "max-width" => vw != 0.0 && media_px(value).is_some_and(|n| vw <= n),
        "width" => vw != 0.0 && media_px(value).is_some_and(|n| vw == n),
        "min-height" => vh != 0.0 && media_px(value).is_some_and(|n| vh >= n),
        "max-height" => vh != 0.0 && media_px(value).is_some_and(|n| vh <= n),
        "height" => vh != 0.0 && media_px(value).is_some_and(|n| vh == n),
        // `device-*` (deprecated in MQ4 but still served): the terminal IS
        // the screen, so they equal the viewport.
        "min-device-width" => vw != 0.0 && media_px(value).is_some_and(|n| vw >= n),
        "max-device-width" => vw != 0.0 && media_px(value).is_some_and(|n| vw <= n),
        "device-width" => vw != 0.0 && media_px(value).is_some_and(|n| vw == n),
        "min-device-height" => vh != 0.0 && media_px(value).is_some_and(|n| vh >= n),
        "max-device-height" => vh != 0.0 && media_px(value).is_some_and(|n| vh <= n),
        "device-height" => vh != 0.0 && media_px(value).is_some_and(|n| vh == n),
        "orientation" if vw != 0.0 && vh != 0.0 => match value {
            "portrait" => vh >= vw,
            "landscape" => vw > vh,
            _ => false,
        },
        "aspect-ratio" => ratio().is_some_and(|(a, r)| (a - r).abs() < 1e-3),
        "min-aspect-ratio" => ratio().is_some_and(|(a, r)| a >= r),
        "max-aspect-ratio" => ratio().is_some_and(|(a, r)| a <= r),
        "hover" | "any-hover" => value == "hover",
        "pointer" | "any-pointer" => value == "fine",
        "prefers-color-scheme" => value == "dark",
        "prefers-reduced-motion" => value == "reduce",
        "prefers-contrast" => value == "no-preference",
        "forced-colors" => value == "none",
        "color" => num() == Some(8.0),
        "min-color" => num().is_some_and(|n| n <= 8.0),
        "max-color" => num().is_some_and(|n| n >= 8.0),
        "monochrome" | "min-monochrome" => num() == Some(0.0),
        "max-monochrome" => num().is_some_and(|n| n >= 0.0),
        "color-gamut" => value == "srgb",
        "display-mode" => value == "browser",
        "update" => value == "fast",
        "scripting" => value == "enabled",
        "grid" => matches!(value, "1"),
        "resolution" => media_dppx(value) == Some(density),
        "min-resolution" => media_dppx(value).is_some_and(|n| n <= density),
        "max-resolution" => media_dppx(value).is_some_and(|n| n >= density),
        "-webkit-device-pixel-ratio" => num() == Some(density),
        "-webkit-min-device-pixel-ratio" => num().is_some_and(|n| n <= density),
        "-webkit-max-device-pixel-ratio" => num().is_some_and(|n| n >= density),
        _ => false,
    }
}

/// A media `<ratio>`: `4/3`, `16 / 9`, or a bare number (MQ4 allows both).
fn media_ratio(value: &str) -> Option<f32> {
    if let Some((a, b)) = value.split_once('/') {
        let a: f32 = a.trim().parse().ok()?;
        let b: f32 = b.trim().parse().ok()?;
        return (b > 0.0).then(|| a / b);
    }
    value.trim().parse().ok()
}

/// A media `<resolution>` in device-pixels-per-CSS-px: `dppx`/`x`, `dpi`
/// (96/in), `dpcm` (96/2.54).
fn media_dppx(value: &str) -> Option<f32> {
    let v = value.trim();
    let split = v
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(v.len());
    let n: f32 = v[..split].parse().ok()?;
    match v[split..].trim() {
        "dppx" | "x" => Some(n),
        "dpi" => Some(n / 96.0),
        "dpcm" => Some(n / (96.0 / 2.54)),
        _ => None,
    }
}

/// The Media Queries L4 range syntax: `width >= 40em`, `width < 900px`,
/// `400px <= width <= 900px` (Tailwind v4 and modern sheets emit these).
/// Only `width`/`height` are evaluated; an unknown feature name, an unknown
/// viewport (0), or an unparsable form doesn't match — the same
/// conservative default as the colon form.
fn media_range_matches(inner: &str, vp: (f32, f32)) -> bool {
    // Split into operands and comparison operators. Operators are ASCII, so
    // the byte positions sliced at are always char boundaries.
    let bytes = inner.as_bytes();
    let (mut operands, mut ops) = (Vec::new(), Vec::new());
    let (mut start, mut i) = (0usize, 0usize);
    while i < bytes.len() {
        let len = match bytes[i] {
            b'<' | b'>' => {
                if bytes.get(i + 1) == Some(&b'=') {
                    2
                } else {
                    1
                }
            }
            b'=' => 1,
            _ => {
                i += 1;
                continue;
            }
        };
        operands.push(inner[start..i].trim());
        ops.push(&inner[i..i + len]);
        i += len;
        start = i;
    }
    operands.push(inner[start..].trim());
    let actual = |name: &str| -> Option<f32> {
        let v = match name {
            "width" => vp.0,
            "height" => vp.1,
            _ => return None,
        };
        (v != 0.0).then_some(v)
    };
    let cmp = |a: f32, op: &str, b: f32| match op {
        "<" => a < b,
        "<=" => a <= b,
        ">" => a > b,
        ">=" => a >= b,
        "=" => a == b,
        _ => false,
    };
    match operands.as_slice() {
        // `width >= 40em`
        [name, value] if actual(name).is_some() => {
            let (Some(a), Some(v)) = (actual(name), media_px(value)) else {
                return false;
            };
            cmp(a, ops[0], v)
        }
        // `400px <= width` (feature on the right: flip the comparison)
        [value, name] => {
            let (Some(a), Some(v)) = (actual(name), media_px(value)) else {
                return false;
            };
            cmp(v, ops[0], a)
        }
        // `400px <= width <= 900px`
        [lo, name, hi] => {
            let (Some(a), Some(l), Some(h)) = (actual(name), media_px(lo), media_px(hi)) else {
                return false;
            };
            cmp(l, ops[0], a) && cmp(a, ops[1], h)
        }
        _ => false,
    }
}

/// A media-feature length as CSS pixels: `px`/unitless as-is, `em`/`rem` at
/// 16px. Other units (or unparseable) → `None` (the condition won't match).
/// The icon NAME inside a token, if it carries a Font-Awesome / icon-set
/// prefix: `fa-NAME` / `fas-fa-NAME` (FA), `bi-NAME` (Bootstrap Icons),
/// `icon-NAME`. Returns the longest trailing icon name (`svg-fas-fa-ellipsis`
/// → `ellipsis`, `#fas-fa-ellipsis` → `ellipsis`). A bare `fa`/`svg-fa` (no
/// dash-name) is not a name.
fn icon_token_name(tok: &str) -> Option<&str> {
    let tok = tok.trim_start_matches('#');
    for sep in ["fa-", "bi-", "icon-"] {
        if let Some(pos) = tok.rfind(sep) {
            let name = &tok[pos + sep.len()..];
            // A real icon name is non-empty, alphanumeric/dash (drop a trailing
            // state class accidentally glued on by the rfind on the wrong sep).
            if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
                return Some(name);
            }
        }
    }
    None
}

/// The Unicode glyph for a recognized icon name (Font-Awesome vocabulary, the
/// de-facto web icon naming). Covers the common UI/nav set; an unknown name
/// returns `None` (the caller falls back to the accessible name, then a marker).
fn icon_glyph_for(name: &str) -> Option<&'static str> {
    Some(match name {
        "ellipsis" | "ellipsis-h" => "⋯",
        "ellipsis-v" | "ellipsis-vertical" => "⋮",
        "bars" | "list" | "list-ul" => "☰",
        "bell" | "bell-o" => "🔔",
        "bookmark" | "bookmark-o" => "🔖",
        "rss" | "rss-square" | "feed" => "📡",
        "cog" | "cogs" | "gear" | "gears" | "sliders" => "⚙",
        "user" | "user-circle" | "circle-user" | "user-o" => "👤",
        "users" | "user-group" | "people-group" => "👥",
        "heart" | "heart-o" => "♥",
        "comment" | "comments" | "comment-dots" | "message" | "comment-o" => "💬",
        // U+2315 has conventional search-icon form and ordinary outline-font
        // coverage. U+1F50D resolves to color-emoji fonts on common Linux
        // systems, which is not a drawable monochrome text outline.
        "search" | "magnifying-glass" => "⌕",
        "upload" | "cloud-upload" | "cloud-arrow-up" | "arrow-up-from-bracket" => "⬆",
        "download" | "cloud-download" | "cloud-arrow-down" | "arrow-down-to-bracket" => "⬇",
        "share" | "share-alt" | "share-nodes" | "arrow-up-from-square" => "↗",
        "link" | "chain" => "🔗",
        "camera" | "camera-retro" => "📷",
        "image" | "images" | "photo" | "picture-o" => "🖼",
        "eye" => "👁",
        "eye-slash" => "🙈",
        "video" | "video-camera" | "film" | "clapperboard" => "🎬",
        "play" | "circle-play" | "play-circle" => "▶",
        "pause" => "⏸",
        "times" | "xmark" | "close" | "x" | "remove" => "✕",
        "check" | "check-circle" | "circle-check" => "✓",
        "plus" | "add" => "＋",
        "minus" => "−",
        "star" | "star-o" => "★",
        "home" | "house" => "⌂",
        "envelope" | "envelope-o" | "mail" | "inbox" => "✉",
        "gear-complex" => "⚙",
        "trash" | "trash-o" | "trash-can" | "trash-alt" => "🗑",
        "edit" | "pen" | "pencil" | "pen-to-square" | "pencil-alt" => "✎",
        "lock" => "🔒",
        "unlock" | "lock-open" => "🔓",
        "flag" | "flag-o" => "⚑",
        "thumbs-up" | "thumbs-o-up" => "👍",
        "thumbs-down" | "thumbs-o-down" => "👎",
        "retweet" | "repeat" => "🔁",
        "gift" => "🎁",
        "fire" => "🔥",
        "bolt" | "flash" => "⚡",
        "globe" | "earth" => "🌐",
        "gear-six" => "⚙",
        "chevron-down" | "angle-down" | "caret-down" | "sort-down" => "▾",
        "chevron-up" | "angle-up" | "caret-up" | "sort-up" => "▴",
        "chevron-left" | "angle-left" | "caret-left" => "◂",
        "chevron-right" | "angle-right" | "caret-right" => "▸",
        "arrow-up" => "↑",
        "arrow-down" => "↓",
        "arrow-left" => "←",
        "arrow-right" => "→",
        "external-link" | "arrow-up-right-from-square" | "up-right-from-square" => "↗",
        _ => return None,
    })
}

fn media_px(value: &str) -> Option<f32> {
    let v = value.trim();
    let split = v
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(v.len());
    let n: f32 = v[..split].parse().ok()?;
    // `em`/`rem` in a media query resolve against the INITIAL font size
    // (16px), never author declarations (MQ4 §1.3); the absolute units are
    // the css-values-4 §6.1 table.
    let px = match v[split..].trim() {
        "px" | "" => n,
        "em" | "rem" => n * 16.0,
        "ex" | "ch" => n * 8.0,
        "pt" => n * 4.0 / 3.0,
        "pc" => n * 16.0,
        "in" => n * 96.0,
        "cm" => n * 96.0 / 2.54,
        "mm" => n * 96.0 / 25.4,
        "q" => n * 96.0 / 101.6,
        _ => return None,
    };
    Some(px.max(0.0))
}

/// Parse the supported declarations of an `@keyframes` rule. CSS Animations
/// 1 §3 conceptually builds an independent sorted keyframe set per property;
/// repeated selectors cascade in source order and `!important` is invalid.
fn parse_keyframes_rule(block: &str) -> KeyframesRule {
    let mut rule = KeyframesRule::default();
    let mut rest = block;
    while let Some(brace) = rest.find('{') {
        let sel = &rest[..brace];
        let (decls, tail) = take_block(&rest[brace..]);
        rest = tail;
        let offsets = sel
            .split(',')
            .filter_map(keyframe_offset)
            .collect::<Vec<_>>();
        if offsets.is_empty() {
            continue;
        }
        for decl in decls.split(';') {
            let Some((property, value, important)) = parse_decl(decl) else {
                continue;
            };
            if important || !matches!(property.as_str(), "opacity" | "top" | "transform") {
                continue;
            }
            for &offset in &offsets {
                let values = rule.properties.entry(property.clone()).or_default();
                if let Some(existing) = values.iter_mut().find(|frame| frame.offset == offset) {
                    existing.value = value.clone();
                } else {
                    values.push(KeyframeValue {
                        offset,
                        value: value.clone(),
                    });
                }
            }
        }
    }
    for values in rule.properties.values_mut() {
        values.sort_by(|left, right| left.offset.total_cmp(&right.offset));
    }
    rule
}

/// A keyframe selector offset as a 0..1 fraction (`from`=0, `to`=1, `N%`).
fn keyframe_offset(sel: &str) -> Option<f32> {
    let offset = match sel.trim() {
        "from" => Some(0.0),
        "to" => Some(1.0),
        s => s
            .strip_suffix('%')
            .and_then(|p| p.trim().parse::<f32>().ok())
            .map(|p| p / 100.0),
    }?;
    (offset.is_finite() && (0.0..=1.0).contains(&offset)).then_some(offset)
}

/// One comma-separated `animation` shorthand segment → its `(name,
/// fill-mode)`: the fill keyword and the first token that isn't a
/// time/keyword are picked out; everything else (durations, easings,
/// iteration counts) is skipped.
fn parse_animation_segment(seg: &str) -> (Option<String>, Option<String>) {
    let parsed = parse_full_animation_segment(seg);
    (
        parsed.name,
        (parsed.fill_mode != "none").then_some(parsed.fill_mode),
    )
}

#[derive(Clone, Debug)]
struct ParsedAnimationSegment {
    name: Option<String>,
    duration_seconds: f32,
    delay_seconds: f32,
    iteration_count: Option<Option<f32>>,
    direction: String,
    fill_mode: String,
    timing_function: String,
    running: bool,
}

fn parse_full_animation_segment(segment: &str) -> ParsedAnimationSegment {
    let mut parsed = ParsedAnimationSegment {
        name: None,
        duration_seconds: 0.0,
        delay_seconds: 0.0,
        iteration_count: None,
        direction: "normal".into(),
        fill_mode: "none".into(),
        timing_function: "ease".into(),
        running: true,
    };
    let mut times = 0;
    for token in segment.split_whitespace() {
        let lower = token.to_ascii_lowercase();
        if let Some(time) = parse_animation_time(&lower) {
            if times == 0 {
                parsed.duration_seconds = time;
            } else if times == 1 {
                parsed.delay_seconds = time;
            }
            times += 1;
            continue;
        }
        match lower.as_str() {
            "infinite" => parsed.iteration_count = Some(None),
            "normal" | "reverse" | "alternate" | "alternate-reverse" => parsed.direction = lower,
            "none" | "forwards" | "backwards" | "both" => {
                if lower == "none" && parsed.name.is_none() {
                    parsed.name = Some(lower);
                } else {
                    parsed.fill_mode = lower;
                }
            }
            "running" => parsed.running = true,
            "paused" => parsed.running = false,
            "linear" | "ease" | "ease-in" | "ease-out" | "ease-in-out" | "step-start"
            | "step-end" => parsed.timing_function = lower,
            _ if lower.starts_with("cubic-bezier(") || lower.starts_with("steps(") => {
                parsed.timing_function = lower
            }
            _ if parsed.iteration_count.is_none() => {
                if let Ok(count) = lower.parse::<f32>()
                    && count.is_finite()
                    && count >= 0.0
                {
                    parsed.iteration_count = Some(Some(count));
                    continue;
                }
                parsed.name.get_or_insert_with(|| token.to_string());
            }
            _ => {
                parsed.name.get_or_insert_with(|| token.to_string());
            }
        }
    }
    parsed
}

fn parse_animation_time(value: &str) -> Option<f32> {
    let value = value.trim().to_ascii_lowercase();
    let seconds = if let Some(milliseconds) = value.strip_suffix("ms") {
        milliseconds.trim().parse::<f32>().ok()? / 1000.0
    } else if let Some(seconds) = value.strip_suffix('s') {
        seconds.trim().parse::<f32>().ok()?
    } else if value == "0" {
        0.0
    } else {
        return None;
    };
    seconds.is_finite().then_some(seconds)
}

fn parse_iteration_count(value: &str) -> Option<Option<f32>> {
    let value = value.trim();
    if value.eq_ignore_ascii_case("infinite") {
        return Some(None);
    }
    let count = value.parse::<f32>().ok()?;
    (count.is_finite() && count >= 0.0).then_some(Some(count))
}

fn animation_list_value(values: &[String], index: usize) -> Option<&str> {
    (!values.is_empty())
        .then(|| &values[index % values.len()])
        .map(String::as_str)
}

/// `input` starts at '{'; return (inner text, after-the-matching-'}').
fn take_block(input: &str) -> (&str, &str) {
    let mut depth = 0i32;
    // String-aware (css-syntax §4.3.5): a `{`/`}` inside a quoted value
    // (`content: "}"`, a font-face `unicode-range` string, …) must not move
    // the brace depth, or a skipped at-rule desyncs the rest of the sheet.
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for (i, c) in input.char_indices() {
        if let Some(q) = quote {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '"' | '\'' => quote = Some(c),
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return (&input[1..i], &input[i + 1..]);
                }
            }
            _ => {}
        }
    }
    // Unbalanced sheet: everything after the brace is the block.
    (&input[1.min(input.len())..], "")
}

// ---- CSSOM: stylesheet text → a rule tree exposed to page JS ---------
//
// `parse_sheet` above is a CASCADE builder: it drops untracked properties,
// flattens `@media` against the viewport, and keeps only the data layout
// needs. CSSOM is a different view — page JS reads `<style>.sheet.cssRules`
// for raw fidelity (`selectorText`, every declaration, at-rule structure),
// e.g. feature-detection libraries and css3test's `Supports.atrule`. So
// this is a separate, lossless-ish parser whose output (compact JSON) the
// JavaScript prelude wraps as CSSStyleRule/CSSMediaRule/etc. Unknown at-rules
// are DROPPED — a real browser omits unrecognized at-rules from cssRules,
// which is exactly what at-rule feature detection relies on.

/// Whether the selector engine can parse `sel` (backs `CSS.supports(
/// "selector(…)")`). Honest: only selectors we can actually evaluate.
pub fn selector_parses(sel: &str) -> bool {
    let sel = sel.trim();
    !sel.is_empty() && SelectorList::parse(sel).is_some()
}

/// Parse a stylesheet into the CSSOM rule tree as compact JSON.
pub fn parse_cssom_json(css: &str) -> String {
    let css = strip_css_comments(css);
    cssom_rules_json(css.as_ref())
}

/// One JSON array of rules from a chunk of stylesheet text (recurses for
/// grouping at-rules like `@media`).
fn cssom_rules_json(css: &str) -> String {
    let mut out = String::from("[");
    let mut rest = css;
    let mut first = true;
    loop {
        rest = rest.trim_start();
        if rest.is_empty() {
            break;
        }
        if let Some(after) = rest.strip_prefix('@') {
            let (json, tail) = at_rule_json(after);
            rest = tail;
            if let Some(j) = json {
                push_item(&mut out, &mut first, &j);
            }
            continue;
        }
        let Some(brace) = rest.find('{') else { break };
        let sel = rest[..brace].trim().to_string();
        let (block, tail) = take_block(&rest[brace..]);
        rest = tail;
        // Keep every braced rule with a non-empty prelude: CSSOM is a text
        // view, so `selectorText` is preserved even for selectors the
        // engine can't evaluate (the cascade drops those separately).
        if sel.is_empty() {
            continue;
        }
        let item = format!(
            "{{\"t\":\"style\",\"sel\":{},\"d\":{}}}",
            json_string(&sel),
            decls_json(block)
        );
        push_item(&mut out, &mut first, &item);
    }
    out.push(']');
    out
}

/// An at-rule body (text after the `@`). Returns its JSON (None = unknown,
/// dropped) and the tail after its `;` or closing `}`.
fn at_rule_json(after: &str) -> (Option<String>, &str) {
    let name_end = after
        .find(|c: char| !c.is_ascii_alphanumeric() && c != '-')
        .unwrap_or(after.len());
    let raw_name = after[..name_end].to_ascii_lowercase();
    let name = raw_name
        .trim_start_matches("-webkit-")
        .trim_start_matches("-moz-")
        .trim_start_matches("-o-")
        .trim_start_matches("-ms-");
    let semi = after.find(';');
    let brace = after.find('{');
    let statement = match (semi, brace) {
        (Some(s), Some(b)) => s < b,
        (Some(_), None) => true,
        (None, Some(_)) => false,
        (None, None) => true,
    };
    if statement {
        let end = semi.map(|s| s + 1).unwrap_or(after.len());
        let prelude = after[name_end..semi.unwrap_or(after.len())].trim();
        return (statement_at_rule_json(name, prelude), &after[end..]);
    }
    let b = brace.unwrap();
    let prelude = after[name_end..b].trim().to_string();
    let (body, tail) = take_block(&after[b..]);
    (block_at_rule_json(name, &prelude, body), tail)
}

fn block_at_rule_json(name: &str, prelude: &str, body: &str) -> Option<String> {
    let grouping = |t: &str| {
        Some(format!(
            "{{\"t\":\"{}\",\"q\":{},\"r\":{}}}",
            t,
            json_string(prelude),
            cssom_rules_json(body)
        ))
    };
    match name {
        "media" => grouping("media"),
        "supports" => grouping("supports"),
        "container" => grouping("container"),
        "scope" => grouping("scope"),
        "layer" => grouping("layer"),
        "document" => grouping("document"),
        "keyframes" => Some(format!(
            "{{\"t\":\"keyframes\",\"name\":{},\"r\":{}}}",
            json_string(prelude),
            keyframes_rules_json(body)
        )),
        "font-face" => Some(format!(
            "{{\"t\":\"font-face\",\"d\":{}}}",
            decls_json(body)
        )),
        "page" => Some(format!(
            "{{\"t\":\"page\",\"sel\":{},\"d\":{}}}",
            json_string(prelude),
            decls_json(body)
        )),
        "counter-style" => Some(format!(
            "{{\"t\":\"counter-style\",\"name\":{},\"d\":{}}}",
            json_string(prelude),
            decls_json(body)
        )),
        "property" => Some(format!(
            "{{\"t\":\"property\",\"name\":{},\"d\":{}}}",
            json_string(prelude),
            decls_json(body)
        )),
        "font-feature-values" => Some(format!(
            "{{\"t\":\"font-feature-values\",\"name\":{},\"d\":[]}}",
            json_string(prelude)
        )),
        _ => None,
    }
}

fn statement_at_rule_json(name: &str, prelude: &str) -> Option<String> {
    match name {
        // @charset never appears in cssRules in real browsers — drop it.
        "import" => Some(format!(
            "{{\"t\":\"import\",\"q\":{}}}",
            json_string(prelude)
        )),
        "namespace" => Some(format!(
            "{{\"t\":\"namespace\",\"q\":{}}}",
            json_string(prelude)
        )),
        "layer" => Some(format!(
            "{{\"t\":\"layer\",\"q\":{},\"r\":[]}}",
            json_string(prelude)
        )),
        _ => None,
    }
}

/// `@keyframes` body: a list of keyframe rules whose "selector" is the
/// keyText (`0%`/`from`/`to`).
fn keyframes_rules_json(body: &str) -> String {
    let mut out = String::from("[");
    let mut rest = body;
    let mut first = true;
    loop {
        rest = rest.trim_start();
        if rest.is_empty() {
            break;
        }
        let Some(brace) = rest.find('{') else { break };
        let key = rest[..brace].trim().to_string();
        let (block, tail) = take_block(&rest[brace..]);
        rest = tail;
        let item = format!(
            "{{\"t\":\"keyframe\",\"key\":{},\"d\":{}}}",
            json_string(&key),
            decls_json(block)
        );
        push_item(&mut out, &mut first, &item);
    }
    out.push(']');
    out
}

/// A declaration block → JSON array of `[name, value]` pairs (raw, NOT
/// filtered by `is_tracked` — CSSOM reports what was written). Naive
/// `;`-split, matching `parse_sheet`.
fn decls_json(block: &str) -> String {
    let mut out = String::from("[");
    let mut first = true;
    for decl in block.split(';') {
        let Some((k, v, _important)) = parse_decl(decl) else {
            continue;
        };
        let item = format!("[{},{}]", json_string(&k), json_string(&v));
        push_item(&mut out, &mut first, &item);
    }
    out.push(']');
    out
}

fn push_item(out: &mut String, first: &mut bool, item: &str) {
    if !*first {
        out.push(',');
    }
    *first = false;
    out.push_str(item);
}

/// A JSON-encoded string literal (quotes + escapes).
fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

// ---- html5ever integration ------------------------------------------

struct Sink {
    dom: RefCell<Dom>,
}

impl TreeSink for Sink {
    type Handle = NodeId;
    type Output = Dom;
    type ElemName<'a> = Ref<'a, QualName>;

    fn finish(self) -> Dom {
        let mut dom = self.dom.into_inner();
        // Preserve the initial document's conservative "needs a full render"
        // state without retaining O(nodes) private parser mutation records.
        if dom.nodes.len() > 1 {
            dom.touch();
        }
        dom
    }

    fn parse_error(&self, _msg: Cow<'static, str>) {}

    fn get_document(&self) -> NodeId {
        DOCUMENT
    }

    fn elem_name<'a>(&'a self, target: &'a NodeId) -> Ref<'a, QualName> {
        Ref::map(self.dom.borrow(), |d| match &d.nodes[*target].data {
            NodeData::Element { name, .. } => name,
            _ => panic!("elem_name on a non-element"),
        })
    }

    fn create_element(&self, name: QualName, attrs: Vec<Attribute>, flags: ElementFlags) -> NodeId {
        let mut dom = self.dom.borrow_mut();
        let contents = flags.template.then(|| dom.new_node(NodeData::Fragment));
        dom.new_node(NodeData::Element {
            name,
            attrs,
            template_contents: contents,
        })
    }

    fn create_comment(&self, text: StrTendril) -> NodeId {
        self.dom.borrow_mut().create_comment(&text)
    }

    fn create_pi(&self, _target: StrTendril, _data: StrTendril) -> NodeId {
        self.dom.borrow_mut().create_comment("")
    }

    fn append(&self, parent: &NodeId, child: NodeOrText<NodeId>) {
        let mut dom = self.dom.borrow_mut();
        match child {
            NodeOrText::AppendNode(n) => dom.parser_append(*parent, n),
            NodeOrText::AppendText(t) => dom.parser_append_text(*parent, &t),
        }
    }

    fn append_based_on_parent_node(
        &self,
        element: &NodeId,
        prev_element: &NodeId,
        child: NodeOrText<NodeId>,
    ) {
        if self.dom.borrow().nodes[*element].parent.is_some() {
            self.append_before_sibling(element, child);
        } else {
            self.append(prev_element, child);
        }
    }

    fn append_doctype_to_document(
        &self,
        _name: StrTendril,
        _public_id: StrTendril,
        _system_id: StrTendril,
    ) {
        let mut dom = self.dom.borrow_mut();
        let dt = dom.new_node(NodeData::Doctype);
        dom.parser_append(DOCUMENT, dt);
    }

    fn get_template_contents(&self, target: &NodeId) -> NodeId {
        match &self.dom.borrow().nodes[*target].data {
            NodeData::Element {
                template_contents: Some(c),
                ..
            } => *c,
            _ => panic!("get_template_contents on a non-template"),
        }
    }

    fn same_node(&self, x: &NodeId, y: &NodeId) -> bool {
        x == y
    }

    fn set_quirks_mode(&self, _mode: QuirksMode) {}

    fn append_before_sibling(&self, sibling: &NodeId, new_node: NodeOrText<NodeId>) {
        let mut dom = self.dom.borrow_mut();
        let Some(parent) = dom.nodes[*sibling].parent else {
            return;
        };
        match new_node {
            NodeOrText::AppendNode(n) => dom.parser_insert_before(parent, n, *sibling),
            NodeOrText::AppendText(t) => dom.parser_insert_text_before(*sibling, &t),
        }
    }

    fn add_attrs_if_missing(&self, target: &NodeId, new_attrs: Vec<Attribute>) {
        let mut dom = self.dom.borrow_mut();
        if let NodeData::Element { attrs, .. } = &mut dom.nodes[*target].data {
            for a in new_attrs {
                if !attrs.iter().any(|e| e.name == a.name) {
                    attrs.push(a);
                }
            }
        }
    }

    fn associate_with_form(
        &self,
        _target: &NodeId,
        _form: &NodeId,
        _nodes: (&NodeId, Option<&NodeId>),
    ) {
    }

    fn remove_from_parent(&self, target: &NodeId) {
        self.dom.borrow_mut().parser_detach(*target);
    }

    fn reparent_children(&self, node: &NodeId, new_parent: &NodeId) {
        let mut dom = self.dom.borrow_mut();
        while let Some(child) = dom.nodes[*node].first_child {
            dom.parser_append(*new_parent, child);
        }
    }

    fn mark_script_already_started(&self, _node: &NodeId) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selector_parse_memo_returns_identical_parses() {
        // The JS syscall boundary parses selectors through the per-thread
        // memo; a repeat of the same string must be a cache hit (same Rc), a
        // failure must be remembered as a failure, and the memoized parse
        // must match a direct one.
        let a = SelectorList::parse_cached(".x > .y").unwrap();
        let b = SelectorList::parse_cached(".x > .y").unwrap();
        assert!(std::rc::Rc::ptr_eq(&a, &b));
        assert!(SelectorList::parse_cached("]]bad[[").is_none());
        assert!(SelectorList::parse_cached("]]bad[[").is_none());
        let dom = Dom::parse_document(r#"<body><div class="x"><p class="y">t</p></div></body>"#);
        assert_eq!(
            dom.query(DOCUMENT, &a, false),
            dom.query(DOCUMENT, &SelectorList::parse(".x > .y").unwrap(), false)
        );
        assert_eq!(
            SelectorList::parse("span, a.x").unwrap().1,
            Some(vec!["span".to_string(), "a".to_string()])
        );
        assert!(SelectorList::parse("span, .x").unwrap().1.is_none());
    }

    #[test]
    fn an_out_of_flow_textless_attr_mutation_paints_nothing() {
        // The decorative-progress-bar case (Twitch's `highlight__progress-bar`):
        // an ATTR mutation inside an absolutely-positioned, textless subtree
        // cannot change a painted cell, so `inert_positioned_attr` is true. Any
        // painting descendant (text, <img>), or an in-flow box (no positioned
        // ancestor, so a size change reflows siblings), is NOT inert.
        let dom = Dom::parse_document(
            r#"<body>
                <div id="bar" style="position:absolute"><div id="fill" style="width:50%"></div></div>
                <p id="text">hello</p>
                <div id="abstext" style="position:absolute"><span id="lbl">x</span></div>
                <div id="inflow"><span id="empty"></span></div>
                <div id="absimg" style="position:fixed"><img id="im" src="a.png"></div>
            </body>"#,
        );
        let f = |id| dom.get_by_id(id).unwrap();
        // Absolute + entirely textless/imageless → inert (the bar itself and the
        // animated fill inside it).
        assert!(
            dom.inert_positioned_attr(f("bar")),
            "abs textless box is inert"
        );
        assert!(
            dom.inert_positioned_attr(f("fill")),
            "the animated fill is inert"
        );
        // In-flow text box: no positioned ancestor → never inert.
        assert!(
            !dom.inert_positioned_attr(f("text")),
            "in-flow box is not inert"
        );
        // Absolute but contains text → it paints the text → not inert.
        assert!(
            !dom.inert_positioned_attr(f("lbl")),
            "abs box WITH text paints"
        );
        // In-flow textless box → a size change reflows siblings → not inert.
        assert!(
            !dom.inert_positioned_attr(f("empty")),
            "in-flow textless is not inert"
        );
        // Fixed but contains an <img> → the image paints → not inert.
        assert!(
            !dom.inert_positioned_attr(f("im")),
            "abs box with img paints"
        );
    }

    #[test]
    fn parse_cssom_json_preserves_rule_structure() {
        // Style rule keeps selectorText + every declaration; @media nests its
        // children; @font-face is a descriptor block; an unknown at-rule is
        // dropped (browsers omit unrecognized at-rules from cssRules).
        let json = parse_cssom_json(
            "a.x { color: red; margin: 0 } \
             @media (min-width: 1px) { p { display: block } } \
             @font-face { font-family: Z } \
             @bogusrule q { z: 1 }",
        );
        assert!(json.contains(r#""t":"style""#), "{json}");
        assert!(json.contains(r#""sel":"a.x""#), "{json}");
        assert!(json.contains(r#"["color","red"]"#), "{json}");
        assert!(json.contains(r#"["margin","0"]"#), "{json}");
        assert!(json.contains(r#""t":"media""#), "{json}");
        assert!(json.contains(r#""q":"(min-width: 1px)""#), "{json}");
        assert!(json.contains(r#""t":"font-face""#), "{json}");
        // The unknown at-rule contributes no rule.
        assert!(
            !json.contains("bogusrule"),
            "unknown at-rule dropped: {json}"
        );
        assert!(!json.contains(r#"["z","1"]"#), "{json}");
    }

    #[test]
    fn css_value_normalization_never_slices_through_unicode() {
        // CSS Syntax tokenization consumes Unicode code points, not arbitrary
        // UTF-8 byte ranges. The old URL look-ahead could end inside a
        // multi-byte code point and panic while a live page was measuring a
        // resized element; the resulting panic was misreported as a
        // ResizeObserver failure.
        let (name, value, important) =
            parse_decl("background: hello xyz” URL(Icon.PNG) !important").unwrap();
        assert_eq!(name, "background");
        assert_eq!(value, "hello xyz” URL(Icon.PNG)");
        assert!(important);
    }

    #[test]
    fn selector_parses_accepts_real_rejects_empty() {
        assert!(selector_parses("a > b.c"));
        assert!(selector_parses(":scope .tab"));
        assert!(!selector_parses(""));
        assert!(!selector_parses("   "));
    }

    /// Pre-insertion validity (WHATWG DOM §4.2.3): the host-including inclusive
    /// ancestor test that `appendChild`/`insertBefore`/`replaceChild` use to
    /// reject cycle-forming insertions. Inclusive (a node is its own), and
    /// host-including (it crosses a shadow boundary to the host's ancestors).
    #[test]
    fn host_including_inclusive_ancestor_catches_cycles() {
        let mut dom = Dom::new();
        let root = dom.create_element("div");
        let mid = dom.create_element("div");
        let leaf = dom.create_element("div");
        dom.append(root, mid);
        dom.append(mid, leaf);

        // Inclusive: a node is its own host-including inclusive ancestor.
        assert!(dom.is_host_including_inclusive_ancestor(leaf, leaf));
        // A real ancestor — appending it under `leaf` would splice a cycle.
        assert!(dom.is_host_including_inclusive_ancestor(root, leaf));
        assert!(dom.is_host_including_inclusive_ancestor(mid, leaf));
        // A descendant is NOT an ancestor of its parent — a legitimate append.
        assert!(!dom.is_host_including_inclusive_ancestor(leaf, root));
        let other = dom.create_element("span");
        assert!(!dom.is_host_including_inclusive_ancestor(other, leaf));

        // Host-including: a node inside `mid`'s shadow tree has `mid` and its
        // ancestors as host-including inclusive ancestors (the walk crosses the
        // host), so appending one of them into the shadow tree is also a cycle.
        let shadow = dom.attach_shadow(mid);
        let inner = dom.create_element("p");
        dom.append(shadow, inner);
        assert!(dom.is_host_including_inclusive_ancestor(mid, inner));
        assert!(dom.is_host_including_inclusive_ancestor(root, inner));
        assert!(!dom.is_host_including_inclusive_ancestor(inner, root));

        // A host with NO light children but a shadow tree is still an ancestor
        // of its shadow content — the O(1) leaf short-circuit must not skip it.
        let bare = dom.create_element("div");
        dom.append(root, bare);
        let bare_shadow = dom.attach_shadow(bare);
        let deep = dom.create_element("span");
        dom.append(bare_shadow, deep);
        assert!(dom.is_host_including_inclusive_ancestor(bare, deep));
    }

    #[test]
    fn flat_walk_uses_shadow_contents_and_slot_assignment() {
        // CSS Shadow 1 §4.1: after selector matching, CSS operates on the
        // flattened tree. The host's unassigned light child is absent, while
        // the assigned child occupies the slot's position in tree order.
        let mut dom = Dom::parse_document(
            "<body><x-host id=h><p id=assigned slot=main>A</p><p id=unassigned>U</p></x-host></body>",
        );
        let host = dom.get_by_id("h").unwrap();
        let assigned = dom.get_by_id("assigned").unwrap();
        let unassigned = dom.get_by_id("unassigned").unwrap();
        let root = dom.attach_shadow(host);
        let before = dom.create_element("span");
        dom.set_attr(before, "id", "before");
        dom.append(root, before);
        let slot = dom.create_element("slot");
        dom.set_attr(slot, "name", "main");
        dom.append(root, slot);
        let after = dom.create_element("span");
        dom.set_attr(after, "id", "after");
        dom.append(root, after);

        let flat = dom.flat_descendants(DOCUMENT);
        let positions = |id| flat.iter().position(|candidate| *candidate == id).unwrap();
        assert!(positions(before) < positions(assigned));
        assert!(positions(assigned) < positions(after));
        assert!(!flat.contains(&unassigned));
        assert_eq!(dom.parent_flat(assigned), Some(slot));
        assert_eq!(dom.parent_flat(slot), Some(host));
    }

    #[test]
    fn ordinary_selectors_keep_slotted_elements_in_the_light_tree() {
        // CSS Shadow 1 §3.2.1/§4.1: selectors precede flattening. The direct
        // child remains matchable through its light-DOM parent after a slot
        // acquires it; only inheritance changes to use the slot as parent.
        let mut dom = Dom::parse_document(
            "<head><style>x-search > input { width:300px; --bar:100%; }</style></head>\
             <body><x-search id=host><input id=query></x-search></body>",
        );
        let host = dom.get_by_id("host").unwrap();
        let query = dom.get_by_id("query").unwrap();
        let shadow = dom.attach_shadow(host);
        let slot = dom.create_element("slot");
        dom.append(shadow, slot);

        assert_eq!(dom.parent_flat(query), Some(slot));
        assert_eq!(dom.computed_value(query, "width").as_deref(), Some("300px"));
        assert_eq!(dom.custom_prop(query, "--bar").as_deref(), Some("100%"));
    }

    #[test]
    fn parses_and_serializes_a_document() {
        let mut dom = Dom::parse_document(
            "<html><head><title>T</title></head><body><p id=a>hi <b>there</b></p></body></html>",
        );
        let html = dom.serialize(DOCUMENT);
        assert!(html.contains("<p id=\"a\">hi <b>there</b></p>"), "{html}");
        assert_eq!(dom.epoch(), 1, "private parser work is coalesced");
        assert!(dom.take_dirty());
        assert!(dom.take_dirty_targets().is_none());
    }

    #[test]
    fn parser_tree_surgery_preserves_foster_parenting_and_adoption_agency() {
        // HTML §13.2.6.4.1 foster-parents the div before the table, while
        // §13.2.6.4.7's adoption-agency algorithm repairs the misnested
        // formatting elements. Both operations move already-linked nodes and
        // therefore exercise the parser-only unlink/insert primitives.
        let dom = Dom::parse_document(
            "<body><table id=table><div id=foster>outside</div><tr><td>cell</td></tr></table>\
             <p id=misnested><b>one<i>two</b>three</i>four</p></body>",
        );
        let foster = dom.get_by_id("foster").unwrap();
        let table = dom.get_by_id("table").unwrap();
        assert_eq!(dom.node(foster).next_sibling, Some(table));
        assert_eq!(dom.node(foster).parent, dom.node(table).parent);
        let repaired = dom.serialize_js(dom.get_by_id("misnested").unwrap());
        assert!(
            repaired.contains("<b>one<i>two</i></b><i>three</i>four"),
            "{repaired}"
        );
    }

    #[test]
    fn serializer_drops_script_noscript_template_style_and_comments() {
        let dom = Dom::parse_document(
            "<body><script>evil()</script><noscript>no js!</noscript>\
             <template><p>inert</p></template><style>p{color:red}</style>\
             <!-- c -->keep</body>",
        );
        let html = dom.serialize(DOCUMENT);
        assert!(!html.contains("evil"), "{html}");
        assert!(!html.contains("no js"), "{html}");
        assert!(!html.contains("inert"), "{html}");
        assert!(!html.contains("color:red"), "{html}");
        assert!(html.contains("keep"), "{html}");
    }

    #[test]
    fn js_serializer_preserves_template_content_but_layout_drops_it() {
        // Wiki.js (Vue 2) delivers its article inside `<template slot=contents>`
        // and recovers it by reading `#root.outerHTML`. The JS path must keep
        // the template + its content fragment as children (HTML serialization
        // standard); the layout/`Doc.raw` path keeps dropping it (inert).
        let dom = Dom::parse_document(
            r#"<body><div id="r"><template slot="contents"><p>article body</p></template></div></body>"#,
        );
        let r = dom.query(DOCUMENT, &SelectorList::parse("#r").unwrap(), true)[0];

        // JS-facing (outerHTML / serialize_js): template + content survive.
        let js = dom.serialize_js(r);
        assert!(js.contains("<template"), "outerHTML missing template: {js}");
        assert!(
            js.contains(r#"slot="contents""#),
            "outerHTML missing slot attr: {js}"
        );
        assert!(
            js.contains("article body"),
            "outerHTML missing template content: {js}"
        );

        // JS-facing innerHTML of the wrapper preserves it too.
        let inner = dom.inner_html(r);
        assert!(
            inner.contains("<template"),
            "innerHTML missing template: {inner}"
        );
        assert!(
            inner.contains("article body"),
            "innerHTML missing template content: {inner}"
        );

        // Layout path still drops the inert template content.
        let layout = dom.serialize(DOCUMENT);
        assert!(
            !layout.contains("article body"),
            "layout serializer leaked template content: {layout}"
        );
        assert!(
            !layout.contains("<template"),
            "layout serializer leaked template tag: {layout}"
        );
    }

    #[test]
    fn font_size_zero_is_baked_and_classified() {
        // The JS render path re-parses the serialized HTML with the sheets gone,
        // so a `<style>`-declared `font-size:0` must be BAKED onto the element or
        // the invisible-text hide is lost (Mastodon's `.invisible` URL spans).
        let dom = Dom::parse_document(
            "<head><style>.invisible{font-size:0}</style></head>\
             <body><span id=x class=invisible>hidden</span></body>",
        );
        let html = dom.serialize(DOCUMENT);
        assert!(
            html.contains("font-size:0"),
            "font-size not baked into render HTML: {html}"
        );
        // The classifier the layout consults resolves the same declaration.
        let x = dom.get_by_id("x").unwrap();
        assert_eq!(dom.font_size_zero(x), Some(true));
        // Unit coverage of the relative/absolute distinction.
        assert_eq!(classify_font_size_zero("0"), Some(true));
        assert_eq!(classify_font_size_zero("0px"), Some(true));
        assert_eq!(classify_font_size_zero("0%"), Some(true));
        assert_eq!(classify_font_size_zero("14px"), Some(false));
        assert_eq!(classify_font_size_zero("1rem"), Some(false));
        assert_eq!(classify_font_size_zero("medium"), Some(false));
        assert_eq!(classify_font_size_zero("calc(1em + 2px)"), Some(false));
        // Relative to the parent (scales the inherited size) / explicit inherit
        // ⇒ defer to inheritance.
        assert_eq!(classify_font_size_zero("2em"), None);
        assert_eq!(classify_font_size_zero("120%"), None);
        assert_eq!(classify_font_size_zero("inherit"), None);
    }

    #[test]
    fn zero_size_replaced_element_hidden_via_rule_and_baked() {
        // Mastodon collapses images inside `.invisible` with a RULE (a descendant
        // combinator + !important), not inline — `.invisible img{width:0!important;
        // height:0!important}`. The cascade must resolve it, is_hidden must hide the
        // img, and the JS render path must bake the zero so the re-parse hides it too.
        let dom = Dom::parse_document(
            "<head><style>.invisible img,.invisible svg\
             {width:0 !important;height:0 !important}</style></head>\
             <body><span class=invisible><img id=i src=x></span></body>",
        );
        let i = dom.get_by_id("i").unwrap();
        assert_eq!(dom.cascaded(i, "width").as_deref(), Some("0"), "rule width");
        assert_eq!(
            dom.cascaded(i, "height").as_deref(),
            Some("0"),
            "rule height"
        );
        assert!(dom.is_hidden(i), "zero-sized img not hidden");
        // The render path drops a hidden node entirely, so the re-parsed layout
        // arena never sees the collapsed img (no baked sliver to clamp to 1 cell).
        let html = dom.serialize(DOCUMENT);
        assert!(
            !html.contains("<img"),
            "zero-sized img leaked into render HTML: {html}"
        );
    }

    #[test]
    fn rewrite_inline_svg_makes_a_renderable_one_a_data_image() {
        let mut dom = Dom::parse_document(
            r##"<body>
                <a href="/x"><svg viewBox="0 0 40 40" aria-label="Web">
                    <title>Web</title><path d="M0 0h40v40H0z"/></svg></a>
                <svg viewBox="0 0 10 10"><use href="#sprite"/></svg>
                <svg style="display:none"><symbol id="s"><path d="M0 0z"/></symbol></svg>
               </body>"##,
        );
        dom.rewrite_inline_svgs(None);
        let imgs: Vec<NodeId> = dom
            .descendants(DOCUMENT)
            .filter(|&d| dom.tag_name(d) == Some("img"))
            .collect();
        // The path-bearing SVG became an <img data:…> with its <title> as alt;
        // the <use>-only and the hidden sprite-def SVG are left as <svg>.
        assert_eq!(imgs.len(), 1, "only the renderable svg is rewritten");
        let img = imgs[0];
        assert!(
            dom.attr(img, "src")
                .unwrap()
                .starts_with("data:image/svg+xml;base64,"),
            "{:?}",
            dom.attr(img, "src")
        );
        assert_eq!(dom.attr(img, "alt"), Some("Web"));
        // The data URL decodes back to SVG markup carrying the path + namespace.
        let bytes = crate::img::decode_data_url(dom.attr(img, "src").unwrap()).unwrap();
        let svg = String::from_utf8(bytes).unwrap();
        assert!(
            svg.contains("<path") && svg.contains("viewBox") && svg.contains("xmlns"),
            "{svg}"
        );
        // It stays inside the anchor, so the icon remains clickable.
        assert_eq!(dom.tag_name(dom.nodes[img].parent.unwrap()), Some("a"));
        // The two non-renderable SVGs survive untouched (glyph/text fallback).
        let svgs = dom
            .descendants(DOCUMENT)
            .filter(|&d| dom.tag_name(d) == Some("svg"))
            .count();
        assert_eq!(svgs, 2);
    }

    #[test]
    fn inline_svg_current_color_is_materialized_for_graphical_paint() {
        // CSS Color 4 defines `currentColor` from the element's computed
        // `color`; the desktop rasterizer has no DOM/CSS context once it gets
        // the image bytes, so the resource must carry that resolved value.
        let dom = Dom::parse_document(
            r#"<body style="color:rgb(4 204 116/1)"><svg id="send" viewBox="0 0 10 10">
                <path fill="currentColor" d="M0 0h10v10H0z"/>
                <g style="color:#e22"><path fill="currentColor" d="M0 0h5v10H0z"/></g>
            </svg></body>"#,
        );
        let svg = dom.get_by_id("send").unwrap();
        let (source, _) = dom
            .svg_image_data(svg, None)
            .expect("inline SVG is a graphical image");
        let markup = String::from_utf8(crate::img::decode_data_url(&source).unwrap()).unwrap();
        assert!(
            markup.contains("fill=\"#04cc74\""),
            "resolved color missing: {markup}"
        );
        assert!(
            markup.contains("fill=\"#ee2222\""),
            "descendant color missing: {markup}"
        );
        let (image, _) = crate::img::decode(&crate::img::decode_data_url(&source).unwrap())
            .expect("materialized SVG rasterizes");
        assert!(
            image.to_rgba8().pixels().any(|pixel| {
                pixel[0] < 40 && pixel[1] >= 190 && pixel[2] >= 90 && pixel[3] > 0
            })
        );
        assert!(
            image
                .to_rgba8()
                .pixels()
                .any(|pixel| { pixel[0] >= 200 && pixel[1] < 80 && pixel[2] < 80 && pixel[3] > 0 })
        );
    }

    #[test]
    fn inline_svg_materializes_stylesheet_paint_and_presentation_vars() {
        // SVG 2 §6.6: a presentation attribute is an author declaration at
        // specificity zero, and a matching stylesheet declaration overrides
        // it. CSS Variables §3 substitution occurs before the isolated image
        // parser consumes either form.
        let dom = Dom::parse_document(
            r#"<head><style>
                 svg { --selected:#dbe0ff; color:#999 }
                 .icon { fill:currentColor; width:10px }
                 .override { fill:#123456 }
               </style></head><body>
               <svg id="paint" viewBox="0 0 20 10">
                 <rect class="icon" width="10" height="10"/>
                 <rect class="override" x="10" width="10" height="10"
                       style="opacity:1"
                       fill="var(--selected, #f00)"/>
               </svg></body>"#,
        );
        let svg = dom.get_by_id("paint").unwrap();
        let (source, _) = dom.svg_image_data(svg, None).expect("paintable SVG");
        let markup = String::from_utf8(crate::img::decode_data_url(&source).unwrap()).unwrap();
        assert!(markup.contains("fill:#999999"), "{markup}");
        assert!(markup.contains("fill:#123456"), "{markup}");
        assert!(
            !markup.contains("var("),
            "unresolved presentation var: {markup}"
        );
        let (image, _) = crate::img::decode(&crate::img::decode_data_url(&source).unwrap())
            .expect("materialized stylesheet paint rasterizes");
        let rgba = image.to_rgba8();
        assert!(
            rgba.pixels()
                .any(|p| p[0] == 0x99 && p[1] == 0x99 && p[2] == 0x99)
        );
        assert!(
            rgba.pixels()
                .any(|p| p[0] == 0x12 && p[1] == 0x34 && p[2] == 0x56)
        );
    }

    #[test]
    fn same_tree_svg_use_resolves_a_symbol_outside_the_outer_svg() {
        // SVG 2 §5.6: a same-tree fragment creates a read-only instance of
        // the referenced element. Icon sheets commonly place hidden symbols
        // in one SVG and paint them from a later `<svg><use href=#...>`.
        let dom = Dom::parse_document(
            r##"<body><svg style="display:none"><symbol id="search" viewBox="0 0 24 24"><path d="M1 1h22v22H1z"/></symbol></svg>
                 <button><svg id="icon" width="24" height="24"><use href="#search"/></svg></button></body>"##,
        );
        let icon = dom.get_by_id("icon").unwrap();
        let (source, _) = dom
            .svg_image_data(icon, None)
            .expect("local use is paintable");
        let markup = String::from_utf8(crate::img::decode_data_url(&source).unwrap()).unwrap();
        assert!(markup.contains("<defs>"), "{markup}");
        assert!(markup.contains("M1 1h22v22H1z"), "{markup}");
        let (raster, _) = crate::img::decode(&crate::img::decode_data_url(&source).unwrap())
            .expect("resolved use rasterizes");
        assert!(raster.to_rgba8().pixels().any(|pixel| pixel[3] > 0));
    }

    #[test]
    fn external_sprite_use_rewrites_to_a_data_image() {
        // The `<svg><use href="sprite.svg#id"></svg>` idiom (chatgpt.com's nav
        // icons, GitHub, most icon systems): the subresource phase primes the
        // sheet, then `rewrite_inline_svgs` inlines the referenced symbol as a
        // data:image the rasterizer renders. Unique URL so it can't race other
        // tests on the process-global sheet cache.
        let base = url::Url::parse("https://sprite-test.example/app/").unwrap();
        prime_sprite_sheet(
            "https://sprite-test.example/assets/sprite.svg",
            r#"<svg xmlns="http://www.w3.org/2000/svg">
                 <symbol id="pencil" viewBox="0 0 20 20"><path d="M2 2h16v16H2z"/></symbol>
                 <symbol id="other" viewBox="0 0 24 24"><path d="M0 0h24v24H0z"/></symbol>
               </svg>"#,
        );
        let mut dom = Dom::parse_document(
            r#"<body><button><svg width="20" height="20" class="icon">
                 <use href="/assets/sprite.svg#pencil"/></svg></button></body>"#,
        );
        dom.rewrite_inline_svgs(Some(&base));
        let img = dom
            .descendants(DOCUMENT)
            .find(|&n| dom.tag_name(n) == Some("img"))
            .expect("the sprite <use> becomes an <img>");
        let decoded =
            String::from_utf8(crate::img::decode_data_url(dom.attr(img, "src").unwrap()).unwrap())
                .unwrap();
        assert!(
            decoded.contains(r#"viewBox="0 0 20 20""#),
            "carries the referenced symbol's viewBox: {decoded}"
        );
        assert!(
            decoded.contains("M2 2h16v16H2z"),
            "carries the pencil geometry: {decoded}"
        );
        assert!(
            !decoded.contains("M0 0h24v24"),
            "not the OTHER symbol's geometry: {decoded}"
        );
        // The standalone svg actually RASTERIZES to painted pixels (a default-
        // fill path paints black → coverage for the silhouette tint). An empty
        // or malformed svg would decode to nothing and the icon would vanish.
        let bytes = crate::img::decode_data_url(dom.attr(img, "src").unwrap()).unwrap();
        let (raster, _) = crate::img::decode(&bytes).expect("symbol svg rasterizes");
        assert!(
            raster.to_rgba8().pixels().any(|p| p[3] > 0),
            "the rasterized icon has painted pixels"
        );
        // The size attr rides onto the <img> so the icon is sized like the page.
        assert_eq!(dom.attr(img, "width"), Some("20"));
        // No raw <svg>/<use> left in the tree.
        assert!(
            !dom.descendants(DOCUMENT)
                .any(|n| matches!(dom.tag_name(n), Some("svg" | "use")))
        );
    }

    #[test]
    fn unfetched_sprite_use_is_left_untouched() {
        // A sheet that was never fetched → the svg stays an svg (renders
        // nothing), never a broken empty <img>. Same graceful fallback as
        // before sprite support existed.
        let base = url::Url::parse("https://sprite-miss.example/").unwrap();
        let mut dom = Dom::parse_document(
            r#"<body><svg width="20" height="20"><use href="/never-fetched.svg#x"/></svg></body>"#,
        );
        dom.rewrite_inline_svgs(Some(&base));
        assert!(
            dom.descendants(DOCUMENT)
                .any(|n| dom.tag_name(n) == Some("svg")),
            "the svg is left in place"
        );
        assert!(
            !dom.descendants(DOCUMENT)
                .any(|n| dom.tag_name(n) == Some("img")),
            "no empty <img> is produced"
        );
    }

    #[test]
    fn rewrite_inline_svg_carries_the_elements_css_and_attr_size() {
        // The replacement <img> must keep the SVG element's box so layout sizes
        // the vector the way the page does — the cascaded CSS size (`style`)
        // over presentation attrs over the intrinsic. archive.org's logo carries
        // only `style="width:2.7rem;height:3rem"`; its media icons carry both a
        // `width="40"` attr and a winning `style="width:4rem"`.
        let mut dom = Dom::parse_document(
            r##"<body>
                <svg class="logo" viewBox="0 0 27 30" style="width:2.7rem;height:3rem">
                    <path d="M0 0h27v30H0z"/></svg>
                <svg width="40" height="40" viewBox="0 0 40 40" style="width:4rem;height:4rem">
                    <path d="M0 0h40v40H0z"/></svg>
               </body>"##,
        );
        dom.rewrite_inline_svgs(None);
        let imgs: Vec<NodeId> = dom
            .descendants(DOCUMENT)
            .filter(|&d| dom.tag_name(d) == Some("img"))
            .collect();
        assert_eq!(imgs.len(), 2);
        // The style-only logo carries its CSS size (no width/height attr).
        assert_eq!(dom.attr(imgs[0], "style"), Some("width:2.7rem;height:3rem"));
        assert_eq!(dom.attr(imgs[0], "width"), None);
        // The icon carries BOTH; CSS wins in layout, but the attr is preserved
        // as the presentation-hint fallback.
        assert_eq!(dom.attr(imgs[1], "style"), Some("width:4rem;height:4rem"));
        assert_eq!(dom.attr(imgs[1], "width"), Some("40"));
        assert_eq!(dom.attr(imgs[1], "height"), Some("40"));
    }

    #[test]
    fn css_cascade_hides_and_reshows() {
        // Stylesheet-class hiding, and the part a one-way hide-list
        // would get wrong: a MORE SPECIFIC rule re-showing.
        let dom = Dom::parse_document(
            "<head><style>
                .hidden { display: none }
                .menu { display: none }
                .menu.open { display: block }
             </style></head>
             <body><p class=hidden>secret</p>
             <div class=menu>shut menu</div>
             <div class='menu open'>open menu</div></body>",
        );
        let html = dom.serialize(DOCUMENT);
        assert!(!html.contains("secret"), "{html}");
        assert!(!html.contains("shut menu"), "{html}");
        assert!(html.contains("open menu"), "{html}");
    }

    #[test]
    fn css_opacity_suppresses_paint_but_keeps_the_box() {
        // The W3C/Bootstrap slideshow idiom: every slide is opacity:0, and the
        // active one is revealed by a fade-in whose end state (fill-mode
        // forwards) is opacity:1. `opacity:0` does NOT collapse the box (CSS
        // separates box generation from painting) — it is `paint_suppressed`
        // (laid out, painted blank), never `is_hidden`. The animation reveal and
        // the merely-faded (0.5) case are honored, so `paint_suppressed` marks
        // exactly the inactive slides — no slideshow-specific code.
        let dom = Dom::parse_document(
            "<head><style>
                @keyframes fade-in { from { opacity: 0 } to { opacity: 1 } }
                @keyframes fade-out { from { opacity: 1 } to { opacity: 0 } }
                .slide { opacity: 0 }
                .slide.active { animation-name: fade-in; animation-fill-mode: forwards }
                .slide.leaving { animation-name: fade-out; animation-fill-mode: forwards }
                .faded { opacity: 0.5 }
             </style></head>
             <body>
               <div id=active class='slide active'>shown slide</div>
               <div id=hidden class='slide'>hidden slide</div>
               <div id=leaving class='slide leaving'>leaving slide</div>
               <div id=faded class='faded'>still visible</div>
             </body>",
        );
        let g = |i| dom.get_by_id(i).unwrap();
        // Never `is_hidden` — opacity generates a box.
        for id in ["active", "hidden", "leaving", "faded"] {
            assert!(!dom.is_hidden(g(id)), "opacity never hides: {id}");
        }
        // Paint suppressed = effectively invisible: the plain opacity:0 slide
        // and the fade-out (ends opacity:0); NOT the fade-in (ends opacity:1)
        // nor the merely-faded 0.5.
        assert!(
            !dom.paint_suppressed(g("active")),
            "fade-in ends opacity:1 → painted"
        );
        assert!(
            dom.paint_suppressed(g("hidden")),
            "opacity:0 slide painted blank"
        );
        assert!(
            dom.paint_suppressed(g("leaving")),
            "fade-out ends opacity:0 → painted blank"
        );
        assert!(
            !dom.paint_suppressed(g("faded")),
            "merely-faded (0.5) painted normally"
        );
        // All four survive serialization — a paint-suppressed box is still laid
        // out (its subtree reserves space and reports its measured geometry).
        let html = dom.serialize(DOCUMENT);
        for t in [
            "shown slide",
            "hidden slide",
            "leaving slide",
            "still visible",
        ] {
            assert!(
                html.contains(t),
                "opacity:0 content kept for layout: {html}"
            );
        }
    }

    #[test]
    fn css_visibility_is_paint_suppression_inherited_and_re_clearable() {
        // Phase 2: `visibility:hidden` is NOT `is_hidden` (it keeps its box) — it
        // is `visibility_hidden` (painted blank). It INHERITS (a plain child of a
        // hidden element is hidden) but is RE-CLEARABLE (`visibility:visible` on a
        // descendant re-shows it). All are KEPT by the serializer, with the
        // suppression baked so the re-parsed layout sees it.
        let dom = Dom::parse_document(
            "<head><style>
                .hide { visibility: hidden }
                .show { visibility: visible }
             </style></head>
             <body>
               <div id=root class=hide>ROOTHIDDEN
                 <span id=child>CHILDINHERITS</span>
                 <span id=reshow class=show>RESHOWN</span>
               </div>
               <p id=normal>NORMALVIS</p>
             </body>",
        );
        let g = |i| dom.get_by_id(i).unwrap();
        // Never `is_hidden` — visibility generates a box.
        for id in ["root", "child", "reshow"] {
            assert!(
                !dom.is_hidden(g(id)),
                "visibility never removes the box: {id}"
            );
        }
        assert!(dom.visibility_hidden(g("root")), "the hidden element");
        assert!(
            dom.visibility_hidden(g("child")),
            "a plain child INHERITS visibility:hidden"
        );
        assert!(
            !dom.visibility_hidden(g("reshow")),
            "visibility:visible RE-CLEARS on a descendant"
        );
        assert!(
            !dom.visibility_hidden(g("normal")),
            "unrelated content visible"
        );
        // Kept + baked so the JS-pipeline re-parse paints it blank.
        let html = dom.serialize(DOCUMENT);
        assert!(html.contains("ROOTHIDDEN"), "hidden content kept: {html}");
        assert!(
            html.contains("visibility:hidden"),
            "suppression baked: {html}"
        );
        assert!(
            html.contains("visibility:visible"),
            "the re-clear baked: {html}"
        );
    }

    #[test]
    fn css_cascade_inline_and_important_precedence() {
        // Inline style beats sheet rules — except !important.
        let dom = Dom::parse_document(
            "<head><style>
                #a { display: none }
                #b { display: none !important }
             </style></head>
             <body><p id=a style='display:block'>inline wins</p>
             <p id=b style='display:block'>important wins</p></body>",
        );
        let html = dom.serialize(DOCUMENT);
        assert!(html.contains("inline wins"), "{html}");
        assert!(!html.contains("important wins"), "{html}");
    }

    #[test]
    fn css_cascade_fails_open() {
        // :hover can't be true here; @media blocks are skipped whole; a
        // selector list with an unparseable member (`:nth-child()` — an
        // empty An+B) dies entirely (the spec's rule, and it fails toward
        // VISIBLE).
        let dom = Dom::parse_document(
            "<head><style>
                .x:hover { display: none }
                @media (max-width: 600px) { .x { display: none } }
                :nth-child(), .y { display: none }
                .z { display: none }
             </style></head>
             <body><p class=x>pointer</p><p class=y>comma survivor</p>\
             <p class=z>plain hide</p></body>",
        );
        let html = dom.serialize(DOCUMENT);
        assert!(html.contains("pointer"), "{html}");
        assert!(html.contains("comma survivor"), "{html}");
        assert!(!html.contains("plain hide"), "{html}");
    }

    #[test]
    fn css_not_and_attr_operators_match() {
        let dom = Dom::parse_document(
            "<head><style>
                li:not(.keep) { display: none }
                [data-state^=clos] { visibility: hidden }
             </style></head>
             <body><ul><li class=keep>kept</li><li>dropped</li></ul>
             <div data-state=closed>shut</div><div data-state=open>still open</div></body>",
        );
        let html = dom.serialize(DOCUMENT);
        assert!(html.contains("kept"), "{html}");
        assert!(!html.contains("dropped"), "display:none is dropped: {html}");
        // `visibility:hidden` is paint suppression (Phase 2): the matched box is
        // KEPT for layout (painted blank) and carries the baked suppression, so
        // the `[data-state^=clos]` selector match shows up as a baked visibility.
        let shut = dom
            .descendants(DOCUMENT)
            .into_iter()
            .find(|&n| dom.attr(n, "data-state") == Some("closed"))
            .unwrap();
        assert!(
            dom.visibility_hidden(shut),
            "the `^=` attr selector matched → visibility:hidden: {html}"
        );
        assert!(
            html.contains("visibility:hidden"),
            "suppression baked: {html}"
        );
        assert!(html.contains("still open"), "{html}");
    }

    #[test]
    fn css_shadow_scope_is_isolated() {
        // Shadow sheets hide shadow content but never leak into the
        // document; document sheets never reach into shadow trees.
        let mut dom = Dom::parse_document(
            "<head><style>.doc-hidden{display:none}</style></head>
             <body><div id=host></div><p class=sec>light sec stays</p>
             <p class=doc-hidden>doc target</p></body>",
        );
        let host = dom.get_by_id("host").unwrap();
        let root = dom.attach_shadow(host);
        let style = dom.create_element("style");
        let css = dom.create_text(".sec { display: none }");
        dom.append(style, css);
        dom.append(root, style);
        let hidden_span = dom.create_element("span");
        dom.set_attr(hidden_span, "class", "sec");
        let t1 = dom.create_text("shadow secret");
        dom.append(hidden_span, t1);
        dom.append(root, hidden_span);
        let shown_span = dom.create_element("span");
        dom.set_attr(shown_span, "class", "doc-hidden");
        let t2 = dom.create_text("shadow shown");
        dom.append(shown_span, t2);
        dom.append(root, shown_span);
        let html = dom.serialize(DOCUMENT);
        assert!(!html.contains("shadow secret"), "{html}");
        assert!(html.contains("light sec stays"), "{html}");
        assert!(!html.contains("doc target"), "{html}");
        assert!(html.contains("shadow shown"), "{html}");
    }

    #[test]
    fn font_px_composes_the_cascade_numerically() {
        // Computed font-size is NUMERIC composition (CSS Fonts §6.1), not
        // string inheritance: % and em multiply the parent's computed size,
        // rem multiplies the root's, keywords map through the medium table,
        // and headings get the UA factor. The root here is the Twitch idiom
        // `html{font-size:62.5%}` = 10px — the rem basis that a fixed 16px
        // inflated 1.6× (the hero-band bug).
        let dom = Dom::parse_document(
            r##"<html style="font-size:62.5%"><body>
              <div id=a style="font-size:1.5em">
                <p id=b style="font-size:150%"><span id=c style="font-size:2rem">x</span></p>
              </div>
              <h2 id=d>h</h2>
              <div id=e style="font-size:x-large">k</div>
              <div id=f>plain</div>
            </body></html>"##,
        );
        let root = dom.document_element().unwrap();
        assert_eq!(dom.font_px(root), 10.0, "62.5% of the 16px initial");
        assert_eq!(dom.root_font_px(), 10.0);
        let a = dom.get_by_id("a").unwrap();
        assert_eq!(dom.font_px(a), 15.0, "1.5em of the inherited 10px");
        let b = dom.get_by_id("b").unwrap();
        assert_eq!(dom.font_px(b), 22.5, "150% of the parent's 15px");
        let c = dom.get_by_id("c").unwrap();
        assert_eq!(dom.font_px(c), 20.0, "2rem = 2 × the 10px root, not 32px");
        let d = dom.get_by_id("d").unwrap();
        assert_eq!(
            dom.font_px(d),
            15.0,
            "h2 = the UA 1.5em of the inherited 10px"
        );
        let e = dom.get_by_id("e").unwrap();
        assert_eq!(dom.font_px(e), 24.0, "x-large = 3/2 of medium, absolute");
        let f = dom.get_by_id("f").unwrap();
        assert_eq!(dom.font_px(f), 10.0, "no declaration inherits the number");
    }

    #[test]
    fn the_font_shorthand_expands_its_tracked_components() {
        // CSS Fonts §6.3: `font: <style>||<weight> <size>[/<line-height>]
        // <family>` — the tracked longhands come out of the shorthand; the
        // size stops the scan (everything after is line-height/family).
        let dom = Dom::parse_document(
            r##"<body><div id=a style="font: italic bold 14px/1.4 sans-serif">x</div>
            <div id=b style="font: 62.5% Arial, sans-serif">y</div></body>"##,
        );
        let a = dom.get_by_id("a").unwrap();
        assert_eq!(dom.computed_value(a, "font-size").as_deref(), Some("14px"));
        assert_eq!(
            dom.computed_value(a, "font-weight").as_deref(),
            Some("bold")
        );
        assert_eq!(
            dom.computed_value(a, "font-style").as_deref(),
            Some("italic")
        );
        assert_eq!(dom.font_px(a), 14.0);
        let b = dom.get_by_id("b").unwrap();
        assert_eq!(dom.computed_value(b, "font-size").as_deref(), Some("62.5%"));
        assert_eq!(dom.font_px(b), 10.0, "62.5% of the inherited 16px");
    }

    #[test]
    fn font_px_follows_mutations_across_epochs() {
        // The per-epoch memo must not serve stale sizes after a style write
        // (the live page mutates `el.style.fontSize`).
        let mut dom = Dom::parse_document(r##"<body><div id=a>x</div></body>"##);
        let a = dom.get_by_id("a").unwrap();
        assert_eq!(dom.font_px(a), 16.0);
        dom.set_attr(a, "style", "font-size: 62.5%");
        assert_eq!(dom.font_px(a), 10.0, "the memo refreshed with the epoch");
    }

    #[test]
    fn host_pseudo_styles_the_shadow_host() {
        // CSS Scoping §3.3: a shadow root's OWN sheet styles its host element
        // through `:host` / `:host(<compound>)`. The host lives in the parent
        // tree, so these are matched specially — not via in-scope selectors.
        // This is how a Lit component's `:host{display:block}` reaches the
        // custom element (archive.org's `home-page-hero-block-icon-bar`).
        let mut dom = Dom::parse_document("<body><my-bar id=host></my-bar><div id=o></div></body>");
        let host = dom.get_by_id("host").unwrap();
        let root = dom.attach_shadow(host);
        let style = dom.create_element("style");
        let css =
            dom.create_text(":host{display:block} :host(.wide){max-width:44rem} .in{display:none}");
        dom.append(style, css);
        dom.append(root, style);
        let inner = dom.create_element("span");
        dom.set_attr(inner, "class", "in");
        dom.append(root, inner);

        // `:host` styles the host element.
        assert_eq!(
            dom.computed_style(host, "display").as_deref(),
            Some("block")
        );
        // `:host(.wide)` applies only when the host matches the argument.
        assert_eq!(dom.computed_style(host, "max-width"), None);
        dom.set_attr(host, "class", "wide");
        assert_eq!(
            dom.computed_style(host, "max-width").as_deref(),
            Some("44rem")
        );
        // `:host` never leaks onto a sibling in the parent tree...
        let other = dom.get_by_id("o").unwrap();
        assert_eq!(dom.computed_style(other, "display"), None);
        // ...and a normal selector in the shadow sheet still styles shadow content.
        assert_eq!(
            dom.computed_style(inner, "display").as_deref(),
            Some("none")
        );
    }

    #[test]
    fn slotted_shadow_rules_style_flattened_assigned_elements() {
        // CSS Shadow 1 §3.2.4: ::slotted() is an alias for the flattened
        // elements assigned to its originating slot. It must not style
        // fallback content, an unassigned sibling, or a deeper descendant.
        let mut dom = Dom::parse_document(
            "<body><x-strip><ul id=list><li id=one>one</li></ul></x-strip><p id=outside>outside</p></body>",
        );
        let host = dom
            .descendants(DOCUMENT)
            .find(|&id| dom.tag_name(id) == Some("x-strip"))
            .unwrap();
        let list = dom.get_by_id("list").unwrap();
        let outside = dom.get_by_id("outside").unwrap();
        let shadow = dom.attach_shadow(host);
        let style = dom.create_element("style");
        let css = dom.create_text(
            "::slotted(:not([slot])){display:grid;grid-auto-flow:column} ::slotted(ul){height:100%}",
        );
        dom.append(style, css);
        dom.append(shadow, style);
        let slot = dom.create_element("slot");
        dom.append(shadow, slot);

        assert_eq!(dom.computed_style(list, "display").as_deref(), Some("grid"));
        assert_eq!(
            dom.computed_style(list, "grid-auto-flow").as_deref(),
            Some("column")
        );
        assert_eq!(dom.computed_style(outside, "display"), None);
    }

    #[test]
    fn shadow_host_cascade_orders_encapsulation_context_before_specificity() {
        // CSS Cascade 5 §6.1: for a host receiving declarations from its
        // outer and inner tree contexts, normal declarations from the OUTER
        // context win, while important declarations from the INNER context
        // win. Archive.org relies on this ordering: an outer component fixes
        // <onboarding-tile> at 60px while the tile's own :host default says
        // height:100%.
        let mut dom = Dom::parse_document("<body><outer-box id=outer></outer-box></body>");
        let outer = dom.get_by_id("outer").unwrap();
        let outer_root = dom.attach_shadow(outer);
        let outer_style = dom.create_element("style");
        let outer_css = dom.create_text("inner-tile { height:60px; width:60px !important; }");
        dom.append(outer_style, outer_css);
        dom.append(outer_root, outer_style);
        let tile = dom.create_element("inner-tile");
        dom.append(outer_root, tile);

        let tile_root = dom.attach_shadow(tile);
        let tile_style = dom.create_element("style");
        let tile_css = dom.create_text(":host { height:100%; width:20px !important; }");
        dom.append(tile_style, tile_css);
        dom.append(tile_root, tile_style);

        assert_eq!(
            dom.computed_value(tile, "height").as_deref(),
            Some("60px"),
            "normal outer-context declaration wins before specificity/source order"
        );
        assert_eq!(
            dom.computed_value(tile, "width").as_deref(),
            Some("20px"),
            "important inner-context declaration wins"
        );
    }

    #[test]
    fn css_cascade_follows_mutations() {
        // The cached index rebuilds on the mutation epoch: class
        // toggles genuinely show and re-hide.
        let mut dom = Dom::parse_document(
            "<head><style>.menu{display:none}.menu.open{display:block}</style></head>
             <body><div id=m class=menu>payload</div></body>",
        );
        assert!(!dom.serialize(DOCUMENT).contains("payload"));
        let m = dom.get_by_id("m").unwrap();
        dom.set_attr(m, "class", "menu open");
        assert!(dom.serialize(DOCUMENT).contains("payload"));
        dom.set_attr(m, "class", "menu");
        assert!(!dom.serialize(DOCUMENT).contains("payload"));
    }

    #[test]
    fn external_sheets_join_the_cascade() {
        let mut dom = Dom::parse_document(
            "<head><link rel=stylesheet href='/a.css'></head>
             <body><p class=x>linked hide</p></body>",
        );
        assert_eq!(dom.stylesheet_links(), vec![String::from("/a.css")]);
        dom.attach_external_sheets(&[(String::from("/a.css"), String::from(".x{display:none}"))]);
        assert!(!dom.serialize(DOCUMENT).contains("linked hide"));
    }

    #[test]
    fn alternate_and_disabled_stylesheets_are_skipped() {
        // Only applied stylesheets feed the cascade and the fetch list: an
        // `alternate` stylesheet (user-selectable, off by default) and a
        // `disabled` one don't apply (HTML §4.6.7), so we neither fetch nor
        // attach them — they must not crowd real sheets out of the fetch cap.
        let dom = Dom::parse_document(
            "<head>\
             <link rel=stylesheet href='/main.css'>\
             <link rel='alternate stylesheet' href='/theme-dark.css'>\
             <link rel=stylesheet href='/late.css' disabled>\
             </head><body></body>",
        );
        assert_eq!(dom.stylesheet_links(), vec![String::from("/main.css")]);
    }

    #[test]
    fn hidden_pseudo_element_generates_no_content() {
        // The width-reservation idiom: a hidden bold copy of a tab label via
        // `::before{content:attr(data-content);visibility:hidden}`. Its content
        // must NOT render (else the label doubles — GitHub's "CodeCode"). A
        // visible `::before` still renders.
        let dom = Dom::parse_document(
            "<head><style>\
             .tab::before{content:attr(data-content);visibility:hidden}\
             .tag::before{content:\"#\"}\
             </style></head>\
             <body><span class=tab data-content=Code>Code</span>\
             <span class=tag>topic</span></body>",
        );
        let tab = dom
            .descendants(DOCUMENT)
            .into_iter()
            .find(|&i| dom.attr(i, "class") == Some("tab"))
            .unwrap();
        let tag = dom
            .descendants(DOCUMENT)
            .into_iter()
            .find(|&i| dom.attr(i, "class") == Some("tag"))
            .unwrap();
        assert_eq!(
            dom.pseudo_content(tab, PseudoEl::Before),
            None,
            "hidden ::before renders nothing"
        );
        assert_eq!(
            dom.pseudo_content(tag, PseudoEl::Before).as_deref(),
            Some("#"),
            "visible ::before still renders"
        );
    }

    #[test]
    fn css_nesting_keeps_nested_declarations_off_the_parent() {
        // CSS Nesting (2023): `.supernav { &::after { display:block; width:100% } }`
        // is Steam's nav-underline idiom (Primer and many design systems too).
        // The `&::after` declarations must target the ::after box, NOT leak onto
        // `.supernav` itself — leaking `width:100%` onto a floated nav item makes
        // every item fill the line and stack vertically. Likewise a plain nested
        // rule resolves to a descendant selector.
        let dom = Dom::parse_document(
            "<head><style>\
             .supernav{float:left}\
             .supernav{ &::after{ content:\"\"; display:block; width:100% } }\
             .card{ color:x; & .title{ font-weight:bold } }\
             </style></head>\
             <body>\
             <a class=supernav>STORE</a>\
             <div class=card><span class=title>Hi</span></div></body>",
        );
        let by = |cls: &str| {
            dom.descendants(DOCUMENT)
                .into_iter()
                .find(|&i| dom.attr(i, "class") == Some(cls))
                .unwrap()
        };
        let nav = by("supernav");
        // The nested `&::after` decls did NOT contaminate the element itself.
        assert_eq!(
            dom.computed_value(nav, "width"),
            None,
            "nested ::after width:100% must not apply to .supernav"
        );
        assert_ne!(
            dom.computed_value(nav, "display").as_deref(),
            Some("block"),
            "nested ::after display:block must not apply to .supernav"
        );
        assert_eq!(
            dom.computed_value(nav, "float").as_deref(),
            Some("left"),
            "the parent's own float survives"
        );
        // The decls landed on the ::after box instead.
        assert_eq!(
            dom.pseudo_style(nav, PseudoEl::After, "width").as_deref(),
            Some("100%"),
            "nested ::after width:100% reaches the pseudo box"
        );
        // A bare nested rule (`& .title`) resolves to a descendant.
        let title = by("title");
        assert_eq!(
            dom.computed_value(title, "font-weight").as_deref(),
            Some("bold"),
            "`.card & .title` applies to the descendant"
        );
    }

    #[test]
    fn computed_value_inherits_only_inherited_properties() {
        // An inherited property flows to a descendant that doesn't set it; a
        // non-inherited one stays put. This is the single inheritance
        // authority the layout and getComputedStyle both read through.
        let dom = Dom::parse_document(
            "<head><style>#outer{text-align:center;margin-left:4px}</style></head>
             <body><div id=outer><p id=inner>x</p></div></body>",
        );
        let inner = dom.get_by_id("inner").unwrap();
        assert_eq!(
            dom.computed_value(inner, "text-align").as_deref(),
            Some("center"),
            "text-align inherits"
        );
        assert_eq!(
            dom.computed_value(inner, "margin-left"),
            None,
            "margin-left does not inherit"
        );
    }

    #[test]
    fn box_shorthand_keeps_calc_components_whole() {
        // `-m-1` (Tailwind's negative margin) computes to `margin: calc(.25rem
        // * -1)`. A naive whitespace split tore that into THREE sides
        // (top=`calc(.25rem`, right=`*`, bottom=`-1)`) — the exact corruption
        // seen baked onto chatgpt.com's icons. The paren-aware split keeps the
        // `calc()` whole and applies it to all four sides.
        assert_eq!(
            expand_box_shorthand("margin", "calc(.25rem * -1)"),
            vec![
                ("margin-top".to_string(), "calc(.25rem * -1)".to_string()),
                ("margin-right".to_string(), "calc(.25rem * -1)".to_string()),
                ("margin-bottom".to_string(), "calc(.25rem * -1)".to_string()),
                ("margin-left".to_string(), "calc(.25rem * -1)".to_string()),
            ]
        );
        // Two-value shorthand, calc vertical + plain-length horizontal.
        assert_eq!(
            expand_box_shorthand("padding", "calc(1rem + 2px) 0"),
            vec![
                ("padding-top".to_string(), "calc(1rem + 2px)".to_string()),
                ("padding-right".to_string(), "0".to_string()),
                ("padding-bottom".to_string(), "calc(1rem + 2px)".to_string()),
                ("padding-left".to_string(), "0".to_string()),
            ]
        );
        // A logical pair (`margin-inline`) with a calc component: two whole values.
        assert_eq!(
            expand_box_shorthand("margin-inline", "calc(2px + 1em) auto"),
            vec![
                ("margin-left".to_string(), "calc(2px + 1em)".to_string()),
                ("margin-right".to_string(), "auto".to_string()),
            ]
        );
    }

    #[test]
    fn flex_shorthand_keeps_calc_basis_whole() {
        // Flexbox §7.1 consumes the nested function as one <flex-basis>
        // component value; CSS Syntax §5.5.7 makes its internal whitespace
        // part of that function rather than shorthand separators.
        let dom = Dom::parse_document(
            r#"<body><div id=item style="flex:1 1 calc(50% - 5px)">x</div></body>"#,
        );
        let item = dom.get_by_id("item").unwrap();
        assert_eq!(dom.computed_value(item, "flex-grow").as_deref(), Some("1"));
        assert_eq!(
            dom.computed_value(item, "flex-shrink").as_deref(),
            Some("1")
        );
        assert_eq!(
            dom.computed_value(item, "flex-basis").as_deref(),
            Some("calc(50% - 5px)")
        );
    }

    #[test]
    fn computed_value_applies_and_inherits_ua_defaults() {
        // `<b>` is bold via the UA default layer; a nested span inherits it;
        // an explicit normal weight wins over the inherited bold.
        let dom = Dom::parse_document(
            "<body><b id=b>bold <span id=s>still</span>\
             <span id=n style='font-weight:normal'>not</span></b></body>",
        );
        let b = dom.get_by_id("b").unwrap();
        let s = dom.get_by_id("s").unwrap();
        let n = dom.get_by_id("n").unwrap();
        assert_eq!(
            dom.computed_value(b, "font-weight").as_deref(),
            Some("bold"),
            "UA default"
        );
        assert_eq!(
            dom.computed_value(s, "font-weight").as_deref(),
            Some("bold"),
            "inherited from <b>"
        );
        assert_eq!(
            dom.computed_value(n, "font-weight").as_deref(),
            Some("normal"),
            "own value beats inherited UA default"
        );
    }

    #[test]
    fn text_decoration_accumulates_without_none_inhibiting_ancestor_lines() {
        // CSS Text Decoration 3 §2.1: lines accumulate across nesting, while
        // `none` establishes no new line and cannot cancel a propagated line.
        // An author declaration can still suppress the UA-origin line that the
        // same semantic element would otherwise establish.
        let dom = Dom::parse_document(
            "<body><u id=u>under <s id=s>both</s>\
             <span id=none style='text-decoration:none'>still underlined</span></u>\
             <u id=own-none style='text-decoration:none'>not underlined</u></body>",
        );
        let u = dom.get_by_id("u").unwrap();
        let s = dom.get_by_id("s").unwrap();
        let none = dom.get_by_id("none").unwrap();
        let own_none = dom.get_by_id("own-none").unwrap();
        assert_eq!(dom.text_decoration(u), (true, false), "<u> underlines");
        assert_eq!(
            dom.text_decoration(s),
            (true, true),
            "<s> inside <u> adds strike, keeps underline"
        );
        assert_eq!(
            dom.text_decoration(none),
            (true, false),
            "none cannot inhibit the ancestor's propagated underline"
        );
        assert_eq!(
            dom.text_decoration(own_none),
            (false, false),
            "author none suppresses this element's UA-origin underline"
        );
    }

    #[test]
    fn relayout_boundary_finds_the_enclosing_scroll_container() {
        // incremental-layout contract §4b: a mutation maps to the nearest scroll
        // container (the size-contained relayout boundary).
        let dom = Dom::parse_document(
            r#"<body><div id=chrome>x</div>
               <div id=chat style="overflow-y:scroll;height:100px"><div id=msg>hi</div></div></body>"#,
        );
        let chat = dom.get_by_id("chat").unwrap();
        let msg = dom.get_by_id("msg").unwrap();
        let chrome = dom.get_by_id("chrome").unwrap();
        // The app confirmed #chat is a live clipped region.
        let live: std::collections::HashSet<NodeId> = [chat].into_iter().collect();
        let none: std::collections::HashSet<NodeId> = Default::default();
        // Content mutation inside the region → the region.
        assert_eq!(
            dom.relayout_boundary(msg, DirtyKind::Content, &live),
            Some(chat)
        );
        // Content mutation ON the region (appending into it) is contained → itself.
        assert_eq!(
            dom.relayout_boundary(chat, DirtyKind::Content, &live),
            Some(chat)
        );
        // An ATTRIBUTE change on the region itself may move its box → look
        // STRICTLY above it (none here → full relayout).
        assert_eq!(dom.relayout_boundary(chat, DirtyKind::Attr, &live), None);
        // Page chrome (no region ancestor) → no boundary.
        assert_eq!(
            dom.relayout_boundary(chrome, DirtyKind::Content, &live),
            None
        );
        // Not a CONFIRMED live region (content fits / no app signal yet) → no
        // patch boundary; the change takes the full path, never a failed patch.
        assert_eq!(dom.relayout_boundary(msg, DirtyKind::Content, &none), None);
    }

    #[test]
    fn independent_formatting_context_matches_the_spec_triggers() {
        // incremental-layout contract §13a: the boundary set is exactly the boxes
        // that establish an independent formatting context (CSS2 §9.4.1 BFC + CSS
        // Display + Flexbox/Grid §3 + Containment L2). A plain in-flow block is
        // NOT one (its inside can affect its outside), so it is never a boundary.
        let dom = Dom::parse_document(
            r#"<body>
              <div id=plain>x</div>
              <div id=scroll style="overflow-y:auto">x</div>
              <div id=hidden style="overflow:hidden">x</div>
              <div id=flowroot style="display:flow-root">x</div>
              <span id=ib style="display:inline-block">x</span>
              <div id=flex style="display:flex"><div id=item>x</div></div>
              <div id=grid style="display:grid"><div id=gitem>x</div></div>
              <div id=abs style="position:absolute">x</div>
              <div id=flt style="float:left">x</div>
              <div id=contain style="contain:layout">x</div>
              <table><tr><td id=cell>x</td></tr></table>
            </body>"#,
        );
        let ifc =
            |id: &str| dom.establishes_independent_formatting_context(dom.get_by_id(id).unwrap());
        // A normal in-flow block is NOT an independent formatting context.
        assert!(!ifc("plain"), "a plain block is not a boundary");
        // The spec triggers all are.
        for id in [
            "scroll", "hidden", "flowroot", "ib", "flex", "grid", "abs", "flt", "contain",
        ] {
            assert!(
                ifc(id),
                "{id} establishes an independent formatting context"
            );
        }
        // A flex/grid ITEM establishes one for its contents (Flexbox §3).
        assert!(ifc("item"), "a flex item is a boundary");
        assert!(ifc("gitem"), "a grid item is a boundary");
        // A bare table cell (UA default display:table-cell) is one too.
        assert!(ifc("cell"), "a table cell is a boundary");
    }

    #[test]
    fn general_boundary_walks_to_the_nearest_formatting_context_root() {
        // The general relayout boundary (incremental-layout design §13c step 4 target) is the nearest
        // independent-formatting-context ancestor — NOT keyed on an app-confirmed
        // region. Here a mutation deep inside a plain wrapper resolves up to the
        // enclosing `overflow:auto` card, skipping the in-flow `<p>` wrapper.
        let dom = Dom::parse_document(
            r#"<body>
              <div id=page>
                <div id=card style="overflow-y:auto;height:80px">
                  <p id=wrap><span id=leaf>hi</span></p>
                </div>
              </div>
            </body>"#,
        );
        let card = dom.get_by_id("card").unwrap();
        let leaf = dom.get_by_id("leaf").unwrap();
        let page = dom.get_by_id("page").unwrap();
        // A content change at the leaf maps up to the card (the nearest BFC).
        assert_eq!(
            dom.relayout_boundary_general(leaf, DirtyKind::Content),
            Some(card)
        );
        // A content change ON the card is contained → the card itself.
        assert_eq!(
            dom.relayout_boundary_general(card, DirtyKind::Content),
            Some(card)
        );
        // An ATTRIBUTE change on the card may move ITS box → look strictly above;
        // the only formatting-context ancestor here is none (plain `#page`/body)
        // → no general boundary (the page reflows).
        assert_eq!(dom.relayout_boundary_general(card, DirtyKind::Attr), None);
        // A plain wrapper with no formatting-context ancestor → no boundary.
        assert_eq!(
            dom.relayout_boundary_general(page, DirtyKind::Content),
            None
        );
    }

    #[test]
    fn dirty_targets_record_node_and_kind_then_force_full_on_a_global_change() {
        let mut dom = Dom::parse_document(r#"<body><div id=box><span id=s>a</span></div></body>"#);
        let box_id = dom.get_by_id("box").unwrap();
        let s = dom.get_by_id("s").unwrap();
        let _ = dom.take_dirty_targets(); // drain parse-time mutations
        // An attribute change records (element, Attr).
        dom.set_attr(s, "class", "hot");
        assert_eq!(dom.take_dirty_targets(), Some(vec![(s, DirtyKind::Attr)]));
        // Appending a child records the PARENT as Content (the fresh child's own
        // orphan-detach records nothing).
        let p = dom.create_element("p");
        dom.append(box_id, p);
        assert_eq!(
            dom.take_dirty_targets(),
            Some(vec![(box_id, DirtyKind::Content)])
        );
        // A global (unattributed) stylesheet change forces a full relayout.
        dom.set_adopted_styles(DOCUMENT, "div{font-weight:bold}");
        assert_eq!(dom.take_dirty_targets(), None);
    }

    #[test]
    fn geometry_dirty_log_retains_nested_document_scope_across_render_drains() {
        // HTML §7.3.1.3 gives an iframe's content navigable a distinct active Document even
        // though TRust stores its nodes below the iframe in one arena. The geometry queue must
        // survive the frontend dirty-target drain and retain enough scope to distinguish a child
        // Document mutation from a mutation of the embedding element in the container Document.
        let mut dom = Dom::parse_document("<body id=outer></body>");
        let outer = dom.get_by_id("outer").unwrap();
        let frame = dom.create_element("iframe");
        let html = dom.create_element("html");
        let body = dom.create_element("body");
        let child = dom.create_element("p");
        dom.append(outer, frame);
        dom.append(frame, html);
        dom.append(html, body);
        dom.append(body, child);
        let _ = dom.take_dirty_targets();
        let _ = dom.take_geometry_dirty_targets();

        dom.set_attr(child, "class", "updated");
        let _ = dom.take_dirty_targets();
        let child_changes = dom
            .take_geometry_dirty_targets()
            .expect("child mutation remains attributed");
        assert_eq!(child_changes, vec![(child, DirtyKind::Attr)]);
        assert_eq!(dom.frame_owner(child), Some(frame));

        dom.set_attr(frame, "width", "410");
        let container_changes = dom
            .take_geometry_dirty_targets()
            .expect("container mutation remains attributed");
        assert_eq!(container_changes, vec![(frame, DirtyKind::Attr)]);
        assert_eq!(dom.frame_owner(frame), None);
    }

    #[test]
    fn boxless_subtrees_suppress_only_render_notifications_they_cannot_affect() {
        // CSS Display 3 §2/§2.5: display:none omits the entire subtree from the
        // box tree. DOM §4.3 still observes these mutations; this predicate is
        // consulted only after script and observer delivery, at render emit.
        let dom = Dom::parse_document(
            r#"<html><head id=head><title id=title>x</title></head><body>
               <div id=none style="display:none"><span id=child>x</span></div>
               <div id=contents style="display:contents"><span id=shown>x</span></div>
               <div id=visibility style="visibility:hidden"><span id=painted>x</span></div>
               <p id=visible>x</p></body></html>"#,
        );
        let head = dom.get_by_id("head").unwrap();
        let title = dom.get_by_id("title").unwrap();
        let none = dom.get_by_id("none").unwrap();
        let child = dom.get_by_id("child").unwrap();
        let shown = dom.get_by_id("shown").unwrap();
        let painted = dom.get_by_id("painted").unwrap();
        let visible = dom.get_by_id("visible").unwrap();

        assert!(
            dom.dirty_target_can_render(head, DirtyKind::Content),
            "UA-hidden metadata can affect styles and document state"
        );
        assert!(dom.dirty_target_can_render(title, DirtyKind::Attr));
        assert!(!dom.dirty_target_can_render(none, DirtyKind::Content));
        assert!(!dom.dirty_target_can_render(child, DirtyKind::Content));
        assert!(
            dom.dirty_target_can_render(child, DirtyKind::Attr),
            "attribute changes remain conservative"
        );
        assert!(
            dom.dirty_target_can_render(none, DirtyKind::Attr),
            "an attribute on the omitted element may reveal it"
        );
        assert!(
            dom.dirty_target_can_render(shown, DirtyKind::Content),
            "display:contents hoists descendants rather than omitting them"
        );
        assert!(
            dom.dirty_target_can_render(painted, DirtyKind::Content),
            "visibility:hidden retains boxes and can be cleared by a descendant"
        );
        assert!(dom.dirty_target_can_render(visible, DirtyKind::Content));
    }

    #[test]
    fn relational_selectors_disable_boxless_content_suppression() {
        // Selectors 4 §4.5/§14.2: content below a boxless element can still
        // change an outside subject through :has() or :empty + a combinator.
        let dom = Dom::parse_document(
            r#"<head><style>
               body:has(#hidden .new) #a { color:red }
               #hidden:empty + #b { color:blue }
               </style></head><body>
               <div id=hidden style="display:none"></div><p id=b>b</p><p id=a>a</p>
               </body>"#,
        );
        let hidden = dom.get_by_id("hidden").unwrap();
        assert!(dom.dirty_target_can_render(hidden, DirtyKind::Content));
    }

    #[test]
    fn detached_shadow_and_style_construction_keeps_concrete_dirty_targets() {
        // DOM §4.2.2: a detached custom-element work tree is not connected,
        // so attaching its shadow root and building scoped styles cannot
        // invalidate the rendered Document yet. Keep concrete targets for the
        // actor's connectedness filter. Inserting the finished styled subtree
        // into the live body must then become a global style invalidation.
        let mut dom = Dom::parse_document("<body><x-live>light</x-live></body>");
        let body = dom
            .descendants(DOCUMENT)
            .find(|&id| dom.tag_name(id) == Some("body"))
            .unwrap();
        let live = dom
            .descendants(DOCUMENT)
            .find(|&id| dom.tag_name(id) == Some("x-live"))
            .unwrap();
        let _ = dom.take_dirty_targets();

        dom.attach_shadow(live);
        assert_eq!(
            dom.take_dirty_targets(),
            Some(vec![(live, DirtyKind::Content)]),
            "a connected host is a scoped content mutation, not unattributed"
        );

        let detached = dom.create_element("x-work");
        let root = dom.attach_shadow(detached);
        let style = dom.create_element("style");
        dom.append(root, style);
        dom.set_text(style, ":host{display:block}");
        let targets = dom
            .take_dirty_targets()
            .expect("detached style construction remains attributed");
        assert!(!targets.is_empty());
        assert!(targets.iter().all(|(id, _)| !dom.is_connected(*id)));

        dom.append(body, detached);
        assert_eq!(
            dom.take_dirty_targets(),
            None,
            "insertion connects the scoped sheet and can restyle live content"
        );
    }

    #[test]
    fn computed_value_memo_follows_mutations() {
        // The memo is epoch-keyed: changing an ancestor's class re-resolves an
        // inherited value rather than serving a stale cache hit.
        let mut dom = Dom::parse_document(
            "<head><style>.up{text-transform:uppercase}</style></head>
             <body><div id=o><span id=i>x</span></div></body>",
        );
        let i = dom.get_by_id("i").unwrap();
        let o = dom.get_by_id("o").unwrap();
        assert_eq!(dom.computed_value(i, "text-transform"), None);
        dom.set_attr(o, "class", "up");
        assert_eq!(
            dom.computed_value(i, "text-transform").as_deref(),
            Some("uppercase"),
            "mutation invalidates the inherited-value memo"
        );
    }

    #[test]
    fn rule_hash_buckets_resolve_the_same_cascade_as_a_full_scan() {
        // The rule-hash (rightmost-key buckets + per-element match memo) must
        // pick exactly the rules a full scan would. Exercises every bucket and
        // the cases a naive bucketing would get wrong: a multi-class subject
        // where the element has only one of the classes (must NOT match), a
        // universal/attribute subject (always tested), an id subject, a tag
        // subject, and specificity ordering across buckets.
        let dom = Dom::parse_document(
            "<head><style>\
               div { letter-spacing: 1px }\
               .box { letter-spacing: 2px }\
               .box.active { letter-spacing: 3px }\
               [data-on] { text-indent: 9px }\
               #hero { letter-spacing: 5px }\
             </style></head>\
             <body>\
               <div id=hero class='box active' data-on>h</div>\
               <div id=plain class='box'>p</div>\
               <span id=s class='active'>s</span>\
             </body>",
        );
        let hero = dom.get_by_id("hero").unwrap();
        let plain = dom.get_by_id("plain").unwrap();
        let s = dom.get_by_id("s").unwrap();
        // hero matches div/.box/.box.active/[data-on]/#hero; #hero wins
        // letter-spacing on specificity, and the attribute rule still applies.
        assert_eq!(
            dom.computed_style(hero, "letter-spacing").as_deref(),
            Some("5px")
        );
        assert_eq!(
            dom.computed_style(hero, "text-indent").as_deref(),
            Some("9px")
        );
        // plain has .box but NOT .active, so `.box.active` must not win.
        assert_eq!(
            dom.computed_style(plain, "letter-spacing").as_deref(),
            Some("2px")
        );
        // <span> has .active but lacks .box, so `.box.active` must not match it
        // even though it shares the bucket key (`box`) is irrelevant — `active`
        // is the bucket key and the second class is verified.
        assert_eq!(dom.computed_style(s, "letter-spacing"), None);
    }

    #[test]
    fn matched_rules_memo_follows_mutations() {
        // The per-element match memo is epoch-keyed: toggling a class must
        // re-match (the element gains the `.active` rule), not serve a stale
        // matched-rule list.
        let mut dom = Dom::parse_document(
            "<head><style>.active{letter-spacing:3px}</style></head>\
             <body><div id=d>x</div></body>",
        );
        let d = dom.get_by_id("d").unwrap();
        assert_eq!(dom.computed_style(d, "letter-spacing"), None);
        dom.set_attr(d, "class", "active");
        assert_eq!(
            dom.computed_style(d, "letter-spacing").as_deref(),
            Some("3px"),
            "mutation invalidates the matched-rules memo"
        );
    }

    #[test]
    fn media_queries_evaluate_against_the_viewport() {
        let vp = (800.0, 600.0); // 800x600 CSS px
        assert!(media_query_matches("(min-width: 768px)", vp));
        assert!(!media_query_matches("(min-width: 1000px)", vp));
        assert!(media_query_matches("(max-width: 1000px)", vp));
        assert!(media_query_matches("screen and (min-width: 640px)", vp));
        assert!(!media_query_matches("print", vp), "wrong medium");
        assert!(
            media_query_matches("print, (min-width: 640px)", vp),
            "comma is OR"
        );
        assert!(media_query_matches("(orientation: landscape)", vp));
        assert!(!media_query_matches("(orientation: portrait)", vp));
        assert!(media_query_matches("(min-width: 40em)", vp), "40em = 640px");
        assert!(media_query_matches("not (min-width: 1000px)", vp), "not");
        // The environment features answer what the terminal actually is:
        // hover-dispatching, mouse-driven, dark, motion-reduced, 1dppx, a
        // character grid, a color device.
        assert!(media_query_matches("(hover: hover)", vp));
        assert!(!media_query_matches("(hover: none)", vp));
        assert!(media_query_matches("(any-pointer: fine)", vp));
        assert!(!media_query_matches("(pointer: coarse)", vp));
        assert!(media_query_matches("(prefers-color-scheme: dark)", vp));
        assert!(!media_query_matches("(prefers-color-scheme: light)", vp));
        assert!(media_query_matches("(prefers-reduced-motion: reduce)", vp));
        assert!(media_query_matches("(resolution: 1dppx)", vp));
        assert!(media_query_matches("(min-resolution: 96dpi)", vp));
        assert!(!media_query_matches("(min-resolution: 2x)", vp));
        assert!(media_query_matches_with_density(
            "(resolution: 2dppx)",
            vp,
            2.0
        ));
        assert!(media_query_matches_with_density(
            "(-webkit-min-device-pixel-ratio: 2)",
            vp,
            2.0
        ));
        assert!(media_query_matches("(grid: 1)", vp), "we ARE a tty grid");
        assert!(media_query_matches("(color)", vp));
        assert!(!media_query_matches("(monochrome)", vp));
        assert!(media_query_matches("(update: fast)", vp));
        assert!(media_query_matches("(scripting: enabled)", vp));
        assert!(media_query_matches("(display-mode: browser)", vp));
        // aspect-ratio: 800x600 = 4/3.
        assert!(media_query_matches("(aspect-ratio: 4/3)", vp));
        assert!(media_query_matches("(min-aspect-ratio: 1/1)", vp));
        assert!(!media_query_matches("(min-aspect-ratio: 16/9)", vp));
        assert!(media_query_matches("(max-aspect-ratio: 16/9)", vp));
        // Boolean-context features (MQ4 §2.4.1).
        assert!(media_query_matches("(hover)", vp));
        assert!(media_query_matches("(width)", vp));
        assert!(!media_query_matches("(width)", (0.0, 0.0)));
        // The full absolute-unit table in media lengths: 8in = 768px.
        assert!(media_query_matches("(min-width: 8in)", vp));
        assert!(!media_query_matches("(min-width: 9in)", vp));
        // Media Queries use CSS-pixel lengths, not integer framebuffer pixels.
        let fractional = (799.5, 600.25);
        assert!(media_query_matches("(width: 799.5px)", fractional));
        assert!(!media_query_matches("(min-width: 800px)", fractional));
        // Unknown feature, or an unknown viewport, conservatively don't match
        // (so the rules are dropped, exactly as skipping @media used to).
        assert!(!media_query_matches("(bleeding-edge-feature: on)", vp));
        assert!(!media_query_matches("(min-width: 768px)", (0.0, 0.0)));
    }

    #[test]
    fn supports_conditions_evaluate_what_we_implement() {
        // Feature tests we implement.
        assert!(supports_condition("(display: grid)"));
        assert!(supports_condition("(display:flex)"));
        assert!(supports_condition("(gap: 1rem)"));
        assert!(supports_condition("(aspect-ratio: 1 / 1)"));
        // A box type we don't lay out, and visual-only properties we don't
        // track, are unsupported → the page's fallback applies.
        assert!(!supports_condition("(display: ruby)"));
        assert!(!supports_condition("(filter: blur(1px))"));
        assert!(!supports_condition("(backdrop-filter: blur(1px))"));
        // not / and / or / nesting.
        assert!(!supports_condition("not (display: grid)"));
        assert!(supports_condition("not (filter: blur(1px))"));
        assert!(supports_condition("(display: grid) and (gap: 1rem)"));
        assert!(!supports_condition(
            "(display: grid) and (filter: blur(1px))"
        ));
        assert!(supports_condition("(filter: blur(1px)) or (display: grid)"));
        assert!(supports_condition("((display: grid))"));
        assert!(supports_condition("selector(.a)"));
    }

    #[test]
    fn supports_feature_queries_gate_their_rules() {
        // We implement grid, so `@supports (display:grid)` applies (hiding
        // `.grid-only`); the old-browser `@supports not (display:grid)` fallback
        // is dropped (`.no-grid` stays); a property we don't implement
        // (`@supports (filter:…)`) is dropped (`.fancy` stays). This is the
        // progressive-enhancement pattern (the IA infinite-scroller serves a
        // flex fallback + `@supports (display:grid)` uniform-track grid).
        let dom = Dom::parse_document(
            "<head><style>
                @supports (display: grid) { .grid-only { display: none } }
                @supports not (display: grid) { .no-grid { display: none } }
                @supports (filter: blur(1px)) { .fancy { display: none } }
                @supports (display: grid) and (gap: 1rem) { .both { display: none } }
             </style></head>
             <body>
               <p class=grid-only>grid gone</p>
               <p class=no-grid>nogrid kept</p>
               <p class=fancy>fancy kept</p>
               <p class=both>both gone</p>
             </body>",
        );
        let html = dom.serialize(DOCUMENT);
        assert!(
            !html.contains("grid gone"),
            "@supports(grid) applies: {html}"
        );
        assert!(html.contains("nogrid kept"), "not(grid) dropped: {html}");
        assert!(
            html.contains("fancy kept"),
            "@supports(filter) dropped: {html}"
        );
        assert!(!html.contains("both gone"), "grid and gap applies: {html}");
    }

    #[test]
    fn mutation_appends_inserts_and_detaches() {
        let mut dom = Dom::parse_document("<body><div id=root></div></body>");
        let root = dom.get_by_id("root").unwrap();
        let a = dom.create_element("p");
        let at = dom.create_text("first");
        dom.append(a, at);
        dom.append(root, a);
        let b = dom.create_element("p");
        dom.insert_before(root, b, Some(a));
        assert_eq!(dom.children(root), vec![b, a]);
        dom.detach(b);
        assert_eq!(dom.children(root), vec![a]);
        assert_eq!(dom.text_content(root), "first");
        let html = dom.serialize(DOCUMENT);
        assert!(
            html.contains("<div id=\"root\"><p>first</p></div>"),
            "{html}"
        );
    }

    #[test]
    fn attributes_set_get_remove() {
        let mut dom = Dom::parse_document("<body><a id=x href='/y'>l</a></body>");
        let a = dom.get_by_id("x").unwrap();
        assert_eq!(dom.attr(a, "href"), Some("/y"));
        assert_eq!(dom.attr(a, "HREF"), Some("/y"));
        dom.set_attr(a, "class", "big");
        assert_eq!(dom.attr(a, "class"), Some("big"));
        dom.remove_attr(a, "href");
        assert_eq!(dom.attr(a, "href"), None);
    }

    #[test]
    fn text_escaping_round_trips() {
        let mut dom = Dom::parse_document("<body><p id=t></p></body>");
        let p = dom.get_by_id("t").unwrap();
        dom.set_text(p, "a < b & \"c\"");
        let html = dom.serialize(DOCUMENT);
        assert!(html.contains("a &lt; b &amp; \"c\""), "{html}");
        // And the parser reads it back to the same text.
        let dom2 = Dom::parse_document(&html);
        let p2 = dom2.get_by_id("t").unwrap();
        assert_eq!(dom2.text_content(p2), "a < b & \"c\"");
    }

    #[test]
    fn fragment_parse_transplants_nodes() {
        let mut dom = Dom::parse_document("<body><div id=host></div></body>");
        let host = dom.get_by_id("host").unwrap();
        let nodes = dom.parse_fragment_into("div", "<p class=x>one</p>two");
        for n in &nodes {
            dom.append(host, *n);
        }
        assert_eq!(dom.text_content(host), "onetwo");
        let html = dom.serialize(DOCUMENT);
        assert!(html.contains("<p class=\"x\">one</p>two"), "{html}");
    }

    #[test]
    fn replace_all_batches_bookkeeping_and_preserves_detached_subtrees() {
        // HTML §8.5.4 parses the fragment before DOM §4.2.3 replace-all.
        // Constructing the detached result must not dirty the live tree; the
        // completed replacement is one attributed content mutation.
        let mut dom = Dom::parse_document(
            "<body><div id=host><section id=old><b>kept</b></section></div></body>",
        );
        let host = dom.get_by_id("host").unwrap();
        let old = dom.get_by_id("old").unwrap();
        let old_child = dom.node(old).first_child.unwrap();
        let _ = dom.take_dirty_targets();
        let before_parse = dom.epoch();

        let nodes =
            dom.parse_fragment_into("div", "<p id=first>one</p><p id=second><i>two</i></p>");
        assert_eq!(
            dom.epoch(),
            before_parse,
            "detached parsing is unobservable"
        );

        dom.replace_all_children(host, nodes);
        assert_eq!(dom.epoch(), before_parse + 1);
        assert_eq!(
            dom.take_dirty_targets(),
            Some(vec![(host, DirtyKind::Content)])
        );
        assert_eq!(dom.node(old).parent, None);
        assert_eq!(dom.node(old).first_child, Some(old_child));
        assert_eq!(dom.node(old_child).parent, Some(old));
        assert_eq!(dom.text_content(old), "kept");
        assert_eq!(dom.text_content(host), "onetwo");
        let children = dom.children(host);
        assert_eq!(dom.attr(children[0], "id"), Some("first"));
        assert_eq!(dom.attr(children[1], "id"), Some("second"));
        assert!(
            children
                .iter()
                .all(|&child| dom.owner_document(child) == Some(DOCUMENT))
        );
    }

    #[test]
    fn replace_all_invalidates_a_changed_sheet_set_once() {
        let mut dom =
            Dom::parse_document("<head id=host><style>.old{color:red}</style></head><body></body>");
        let host = dom.get_by_id("host").unwrap();
        let _ = dom.take_dirty_targets();
        let before_epoch = dom.epoch();
        let before_style_epoch = dom.style_epoch;
        let nodes = dom.parse_fragment_into("head", "<style>.new{color:blue}</style>");

        dom.replace_all_children(host, nodes);

        assert_eq!(dom.epoch(), before_epoch + 1);
        assert_eq!(dom.style_epoch, before_style_epoch + 1);
        assert!(dom.take_dirty_targets().is_none());
    }

    #[test]
    fn install_frame_document_parses_replaces_and_absolutizes() {
        let mut dom = Dom::parse_document("<body><iframe></iframe></body>");
        let frame = dom
            .descendants(DOCUMENT)
            .into_iter()
            .find(|&n| dom.tag_name(n) == Some("iframe"))
            .unwrap();
        // A FULL document is parsed and installed as the frame's content.
        dom.install_frame_document(
            frame,
            "<!DOCTYPE html><html><head><title>FRAME TITLE</title></head>\
             <body><p>HELLO FRAME</p><a href=\"deep.html\">go</a></body></html>",
            "http://h.test/dir/page.html",
        )
        .unwrap();
        // Serializing the iframe node flattens it into a chrome-less block.
        let html = dom.serialize(frame);
        assert!(html.contains("data-trust-frame"), "{html}");
        assert!(html.contains("HELLO FRAME"), "{html}");
        // The relative link resolved against the FRAME's base, not the parent.
        assert!(html.contains("http://h.test/dir/deep.html"), "{html}");
        // Head content (title) stays out of the inline body flow.
        assert!(
            !html.contains("FRAME TITLE"),
            "head leaked into flow: {html}"
        );
        // A re-navigation REPLACES the prior content navigable.
        dom.install_frame_document(frame, "<body><p>SECOND</p></body>", "http://h.test/")
            .unwrap();
        let html2 = dom.serialize(frame);
        assert!(html2.contains("SECOND"), "{html2}");
        assert!(
            !html2.contains("HELLO FRAME"),
            "stale content kept: {html2}"
        );
    }

    #[test]
    fn frame_snapshot_keeps_outer_position_and_child_body_formatting_context() {
        // HTML §4.8.5 keeps the iframe viewport separate from its content
        // navigable. CSS Display 3 §2 and Flexbox §3 require the child BODY's
        // own display type to continue governing its children. This is the
        // stylesheet-neutral form of the SCM-player + Burgeritchi regression:
        // the fixed frame used to enter parent flow, while BODY's flex box was
        // discarded and its centered shell became a left-aligned block.
        let mut dom = Dom::parse_document(
            r#"<body style="margin:0"><iframe id=frame
               style="position:fixed;inset:0;width:100%;height:100%;z-index:99"></iframe>
               <p id=after style="margin:0">after</p></body>"#,
        );
        let frame = dom.get_by_id("frame").unwrap();
        dom.install_frame_document(
            frame,
            r#"<html style="font-family:serif"><head><style>
               body{display:flex;flex-direction:column;align-items:center;
                    min-height:100vh;margin:0}
               </style></head><body><main id=shell style="width:400px">centered</main></body></html>"#,
            "https://frame.test/",
        )
        .unwrap();

        let html = dom.serialize_live(DOCUMENT, &std::collections::HashSet::new());
        assert!(html.contains("data-trust-frame"), "{html}");
        assert!(html.contains("data-trust-frame-body"), "{html}");

        let snapshot = Dom::parse_document(&html);
        let outer = snapshot
            .descendants(DOCUMENT)
            .find(|&id| snapshot.attr(id, "data-trust-frame").is_some())
            .expect("serialized iframe viewport");
        let body = snapshot
            .child_iter(outer)
            .find(|&id| snapshot.attr(id, "data-trust-frame-body").is_some())
            .expect("serialized child body formatting box");
        assert_eq!(
            snapshot
                .computed_value_resolved(outer, "position")
                .as_deref(),
            Some("fixed")
        );
        assert_eq!(snapshot.effective_display(body).as_deref(), Some("flex"));
        assert_eq!(
            snapshot
                .computed_value_resolved(body, "flex-direction")
                .as_deref(),
            Some("column")
        );
        assert_eq!(
            snapshot
                .computed_value_resolved(body, "align-items")
                .as_deref(),
            Some("center")
        );
        assert_eq!(
            snapshot
                .computed_value_resolved(body, "font-family")
                .as_deref(),
            Some("serif"),
            "inherited child-document style must not leak or disappear at the flattening boundary"
        );
    }

    #[test]
    fn unitless_nonzero_length_declaration_is_invalid_and_does_not_win_cascade() {
        // CSS Values 4 §6 makes only zero a unit-optional <length>; CSSOM
        // §6.7.1 therefore drops `height:518` before cascade resolution. SCM
        // Music Player assigns `iframe.style.height = window.innerHeight`
        // without "px". Browsers retain the valid stylesheet's 100% height;
        // accepting the number as px froze TRust's outer page frame at the
        // actor's early viewport measurement and clipped the lower document.
        let dom = Dom::parse_document(
            r#"<style>#frame{height:100%;margin-left:7px}</style><body>
               <iframe id=frame style="height:518;margin:2 3px;width:0"></iframe>
               </body>"#,
        );
        let frame = dom.get_by_id("frame").unwrap();
        assert_eq!(
            dom.computed_value_resolved(frame, "height").as_deref(),
            Some("100%"),
            "invalid inline height must not mask the valid stylesheet declaration"
        );
        assert_eq!(
            dom.computed_value_resolved(frame, "margin-left").as_deref(),
            Some("7px"),
            "one unitless nonzero component invalidates the whole shorthand"
        );
        assert_eq!(
            dom.computed_value_resolved(frame, "width").as_deref(),
            Some("0"),
            "unitless zero remains a valid length"
        );
    }

    #[test]
    fn unrealized_iframe_snapshot_keeps_replaced_element_footprint() {
        let dom = Dom::parse_document(
            r#"<body><iframe id=frame width=420 height=180></iframe><span>after</span></body>"#,
        );
        let frame = dom.get_by_id("frame").unwrap();
        let html = dom.serialize(frame);
        assert!(html.contains("data-trust-frame"), "{html}");
        assert!(html.contains("width:420px"), "{html}");
        assert!(html.contains("height:180px"), "{html}");
    }

    #[test]
    fn iframe_document_has_an_independent_author_style_scope() {
        let mut dom = Dom::parse_document(
            "<style>p{color:red} iframe{overflow:scroll;color:green}</style>\
             <body><p id=parent>parent</p><iframe id=frame></iframe></body>",
        );
        let frame = dom.get_by_id("frame").unwrap();
        dom.install_frame_document(
            frame,
            "<html><head><style>p{color:blue} iframe{overflow:hidden}</style></head>\
             <body><p id=child>child</p></body></html>",
            "https://child.test/",
        )
        .unwrap();
        let parent = dom.get_by_id("parent").unwrap();
        let child = dom.get_by_id("child").unwrap();
        let child_html = dom
            .child_iter(frame)
            .find(|&node| dom.tag_name(node) == Some("html"))
            .unwrap();

        assert_eq!(
            dom.computed_value_resolved(parent, "color").as_deref(),
            Some("red")
        );
        assert_eq!(
            dom.computed_value_resolved(child, "color").as_deref(),
            Some("blue")
        );
        assert_eq!(
            dom.computed_value_resolved(frame, "overflow").as_deref(),
            Some("scroll"),
            "a child sheet cannot restyle its embedding iframe"
        );
        assert_ne!(
            dom.computed_value_resolved(child_html, "color").as_deref(),
            Some("green"),
            "inherited values do not cross the Document boundary"
        );
        assert!(
            !dom.matches(child, &SelectorList::parse("iframe p").unwrap()),
            "selector ancestry stops at the child document element"
        );
    }

    #[test]
    fn selectors_match_the_workhorse_grammar() {
        let dom = Dom::parse_document(
            "<body><div class='a b'><p id=p1 class=x>1</p><span data-k='v'>2</span></div>\
             <div><p class=x>3</p></div></body>",
        );
        let q = |s: &str| {
            let sel = SelectorList::parse(s).unwrap();
            dom.query(DOCUMENT, &sel, false).len()
        };
        assert_eq!(q("p"), 2);
        assert_eq!(q(".x"), 2);
        assert_eq!(q("#p1"), 1);
        assert_eq!(q("div.a.b p.x"), 1);
        assert_eq!(q("div > p"), 2);
        assert_eq!(q("body > p"), 0);
        assert_eq!(q("[data-k]"), 1);
        assert_eq!(q("[data-k=v]"), 1);
        assert_eq!(q("[data-k=w]"), 0);
        assert_eq!(q("p, span"), 3);
        assert_eq!(q("*"), 8); // html, head, body, div, p, span, div, p
    }

    #[test]
    fn scope_pseudo_matches_the_query_root() {
        // jQuery rewrites a context-rooted comma `.find()` to
        // `:scope X, :scope Y`. `:scope` must resolve to the element the query
        // is rooted on, or the query returns nothing — the SL Marketplace
        // tab-deselection bug (`removeClass` over `:scope .tab-header,…`).
        let dom = Dom::parse_document(
            "<body><div id=box><span class=a>1</span><span class=b>2</span>\
             <span class=a>3</span></div><span class=a>outside</span></body>",
        );
        let box_id = dom.get_by_id("box").unwrap();
        let q = |root: NodeId, s: &str| {
            let sel = SelectorList::parse(s).unwrap();
            dom.query(root, &sel, false).len()
        };
        // Rooted at #box, `:scope .a` finds the two inside, not the outsider.
        assert_eq!(q(box_id, ":scope .a"), 2, ":scope roots at #box");
        // The exact jQuery shape: a comma list of :scope-prefixed selectors.
        assert_eq!(
            q(box_id, ":scope .a, :scope .b"),
            3,
            "comma :scope list ORs"
        );
        // Inert in the cascade / scopeless match (no query root → never).
        let b = dom.query(box_id, &SelectorList::parse(".b").unwrap(), true)[0];
        assert!(!dom.matches(b, &SelectorList::parse(":scope").unwrap()));
    }

    #[test]
    fn sibling_combinators_match() {
        let dom = Dom::parse_document(
            "<body><ul><li class=a>1</li><li class=b>2</li><li class=c>3</li></ul></body>",
        );
        let q = |s: &str| {
            dom.query(DOCUMENT, &SelectorList::parse(s).unwrap(), false)
                .len()
        };
        // `.a + li` = the li immediately after .a (just one).
        assert_eq!(q(".a + li"), 1, "next-sibling matches one");
        // `.a ~ li` = every following li sibling (two).
        assert_eq!(q(".a ~ li"), 2, "subsequent-sibling matches all following");
        // `.c + li` = nothing follows .c.
        assert_eq!(q(".c + li"), 0, "no sibling after last");
    }

    #[test]
    fn structural_pseudo_classes_match() {
        let dom = Dom::parse_document(
            "<body><ul id=list>\
             <li>1</li><li>2</li><li>3</li><li>4</li><li>5</li>\
             </ul><div id=empty></div><div id=ws>   </div><div id=full>x</div></body>",
        );
        let root = DOCUMENT;
        let q = |s: &str| {
            dom.query(root, &SelectorList::parse(s).unwrap(), false)
                .len()
        };
        assert_eq!(q("li:first-child"), 1);
        assert_eq!(q("li:last-child"), 1);
        assert_eq!(q("li:only-child"), 0, "5 li children: none is only-child");
        assert_eq!(q("li:nth-child(2)"), 1);
        assert_eq!(q("li:nth-child(odd)"), 3, "1,3,5");
        assert_eq!(q("li:nth-child(even)"), 2, "2,4");
        assert_eq!(q("li:nth-child(2n+1)"), 3, "same as odd");
        assert_eq!(q("li:nth-last-child(1)"), 1, "== last-child");
        // :empty — whitespace-only counts as empty (Selectors-4); text doesn't.
        assert_eq!(q("#empty:empty"), 1);
        assert_eq!(q("#ws:empty"), 1, "whitespace-only is empty");
        assert_eq!(q("#full:empty"), 0, "text content is not empty");
    }

    #[test]
    fn of_type_pseudo_classes_match() {
        let dom = Dom::parse_document(
            "<body id=b><h1>t</h1><p>a</p><p>b</p><span>s</span><p>c</p></body>",
        );
        let b = dom.get_by_id("b").unwrap();
        let q = |s: &str| dom.query(b, &SelectorList::parse(s).unwrap(), false).len();
        assert_eq!(q("p:first-of-type"), 1, "first p");
        assert_eq!(q("p:last-of-type"), 1, "last p");
        assert_eq!(q("h1:only-of-type"), 1, "the lone h1");
        assert_eq!(q("p:only-of-type"), 0, "three p's");
        assert_eq!(q("p:nth-of-type(2)"), 1, "second p");
    }

    #[test]
    fn a_scroll_container_bakes_its_node_id_and_scroll_top_in_css_pixels() {
        // The live serializer marks a vertical scroll container with a stable
        // node id AND the page's current scrollTop signal in CSS pixels. A
        // terminal adapter may quantize it later; DOM state never stores rows.
        let mut dom = Dom::parse_document(
            "<body><div id=s style='overflow-y:auto;height:96px'><p>x</p></div></body>",
        );
        let s = dom.get_by_id("s").unwrap();
        assert!(
            dom.is_scroll_container(s),
            "overflow-y:auto is a scroll container"
        );
        // The app pushed the clip box; the page's setter clamped + stored the
        // position (here we drive the syscalls directly).
        dom.set_scroll_geom(s, 160.0, 100.0);
        dom.set_scroll_pos(s, 320.0, 0.0, true);
        let html = dom.serialize_live(DOCUMENT, &std::collections::HashSet::new());
        assert!(
            html.contains("data-trust-node="),
            "the scroll container carries an actor node id: {html}"
        );
        assert!(
            html.contains("data-trust-scroll-top=\"320\""),
            "the scrollTop signal is baked in CSS pixels: {html}"
        );
    }

    #[test]
    fn a_plain_block_bakes_no_scroll_signal() {
        let dom = Dom::parse_document("<body><div id=p><p>x</p></div></body>");
        let p = dom.get_by_id("p").unwrap();
        assert!(!dom.is_scroll_container(p), "a plain div is not a scroller");
        let html = dom.serialize_live(DOCUMENT, &std::collections::HashSet::new());
        assert!(
            !html.contains("data-trust-scroll-top"),
            "no scroll signal on a non-scroll-container: {html}"
        );
    }

    #[test]
    fn a_horizontal_scroll_container_bakes_its_actor_and_scroll_left() {
        let mut dom = Dom::parse_document(
            "<body><div id=s style='overflow-x:auto;width:96px'><p>x</p></div></body>",
        );
        let s = dom.get_by_id("s").unwrap();
        assert!(dom.is_hscroll_container(s));
        dom.set_scroll_pos(s, 0.0, 40.5, true);
        let html = dom.serialize_live(DOCUMENT, &std::collections::HashSet::new());
        assert!(
            html.contains(&format!("data-trust-node=\"{s}\"")),
            "horizontal scroller carries its actor id: {html}"
        );
        assert!(
            html.contains("data-trust-scroll-left=\"40.5\""),
            "scrollLeft survives the live snapshot in CSS pixels: {html}"
        );
    }

    #[test]
    fn scroll_writes_are_recorded_on_both_axes_without_region_geometry() {
        // CSSOM View scroll state belongs to every scrolling box. Graphical
        // horizontal scrollers do not receive the terminal's RegionGeom
        // message, so a changed scrollLeft must be recorded before any pushed
        // clip geometry exists. scrollHeight (`which=2`) is deliberately NOT
        // stored — it reads the fresh fragment scrolling area.
        let mut dom = Dom::parse_document("<body><div id=s style='overflow-y:auto'></div></body>");
        let s = dom.get_by_id("s").unwrap();
        assert!(dom.set_scroll_pos(s, 0.0, 50.0, true));
        assert_eq!(
            dom.take_scroll_changes(),
            vec![(s, 0.0, 50.0)],
            "horizontal scrolling is observable without RegionGeom"
        );
        dom.set_scroll_geom(s, 100.0, 80.0);
        assert_eq!(dom.scroll_metric(s, 4), Some(100.0), "clientHeight stored");
        assert_eq!(dom.scroll_metric(s, 5), Some(80.0), "clientWidth stored");
        assert_eq!(
            dom.scroll_metric(s, 2),
            None,
            "scrollHeight is read from the rect, never stored"
        );
        assert!(dom.set_scroll_pos(s, 70.0, 50.0, true));
        assert_eq!(
            dom.take_scroll_changes(),
            vec![(s, 70.0, 50.0)],
            "vertical scrolling remains observable after geometry is pushed"
        );
        assert!(!dom.set_scroll_pos(s, 70.0, 50.0, true));
        assert!(
            dom.take_scroll_changes().is_empty(),
            "a no-op queues nothing"
        );
    }

    #[test]
    fn scripts_are_collected_in_document_order() {
        let dom = Dom::parse_document(
            "<head><script src='/a.js'></script></head>\
             <body><script>inline()</script><script type='module'>mod()</script></body>",
        );
        let scripts = dom.scripts();
        assert_eq!(scripts.len(), 3);
        assert_eq!(scripts[0].0.as_deref(), Some("/a.js"));
        assert_eq!(scripts[1].1, "inline()");
        assert_eq!(scripts[2].2.as_deref(), Some("module"));
    }

    #[test]
    fn dirty_bit_tracks_mutations_and_skips_idempotent_writes() {
        let mut dom = Dom::parse_document("<body><p id=a>x</p></body>");
        assert!(dom.take_dirty()); // parsing itself mutates
        assert!(!dom.take_dirty()); // and the take resets
        let a = dom.get_by_id("a").unwrap();
        dom.set_attr(a, "class", "y");
        assert!(dom.take_dirty());
        // Idempotent writes are free: no dirty, no redraw downstream.
        dom.set_attr(a, "class", "y");
        assert!(!dom.take_dirty());
        dom.set_text(a, "x");
        assert!(!dom.take_dirty());
        dom.set_text(a, "z");
        assert!(dom.take_dirty());
        let _ = dom.text_content(a); // reads stay clean
        let _ = dom.serialize(DOCUMENT);
        assert!(!dom.take_dirty());
    }

    #[test]
    fn serialize_live_marks_buttons_and_live_anchors() {
        let dom = Dom::parse_document(
            "<body><button id=b>Push</button>\
             <button id=icon aria-label=search></button>\
             <button id=opts><svg class=\"svg-fa svg-fas-fa-ellipsis\"><use href=\"#fas-fa-ellipsis\"></use></svg></button>\
             <span id=dot></span>\
             <a id=plain href='/normal'>plain</a>\
             <a id=hot href='/hot'>hot</a></body>",
        );
        let b = dom.get_by_id("b").unwrap();
        let icon = dom.get_by_id("icon").unwrap();
        let opts = dom.get_by_id("opts").unwrap();
        let dot = dom.get_by_id("dot").unwrap();
        let hot = dom.get_by_id("hot").unwrap();
        let clickable = std::collections::HashSet::from([b, icon, opts, dot, hot]);
        let html = dom.serialize_live(DOCUMENT, &clickable);
        // A native button remains the authored box.  Inventing an anchor
        // parent would change which box is the flex/grid item (CSS Flexbox
        // §4) and therefore its used geometry.  The private marker carries
        // activation semantics without changing the tree.
        assert!(
            html.contains(&format!(
                "<button id=\"b\" data-trust-click=\"x-trust-js:{b}:\" data-trust-node=\"{b}\">Push</button>"
            )),
            "{html}"
        );
        assert!(
            html.contains(&format!(
                "data-trust-click=\"x-trust-js:{icon}:\" data-trust-node=\"{icon}\">"
            )),
            "search pictogram belongs to the button content box: {html}"
        );
        assert!(html.contains(">⌕</span></button>"), "{html}");
        assert!(
            !html.contains("[search]"),
            "accessible name is metadata: {html}"
        );
        // An icon-only button renders the icon GLYPH as its handle (the
        // dominant web icon idiom) — the comment's ⋯ menu — not "·"/"[button]".
        // (An icon-only ANCHOR `<a><svg></a>` is glyphed by the layout instead,
        // see `icon_only_label`, since anchors aren't wrapped.)
        assert!(html.contains('⋯'), "ellipsis icon glyph: {html}");
        // An unnamed icon-only clickable (a CSS-drawn dot — no text, glyph, or
        // accessible name) gets NO marker: its meaning lived only in CSS, which
        // a text reader can't convey, so we emit an empty wrapper rather than
        // litter a `·` per anonymous control (Steam's carousel pagination dots
        // are ~12 such `<div>`s each). Still wrapped (so it stays a clickable),
        // just with nothing to show — no debris, no stolen selection stop.
        assert!(!html.contains('·'), "no anonymous-clickable marker: {html}");
        assert!(!html.contains("[button]"), "{html}");
        assert!(
            html.contains(&format!("x-trust-js:{dot}:")),
            "anonymous dot stays a clickable wrapper: {html}"
        );
        // The live anchor's href is rewritten with the original kept;
        // the plain one is untouched (the zero-overhead path).
        assert!(
            html.contains(&format!("href=\"x-trust-js:{hot}:/hot\"")),
            "{html}"
        );
        assert!(html.contains("href=\"/normal\""), "{html}");
    }

    #[test]
    fn serialize_live_skips_the_text_handle_when_the_icon_paints() {
        // The icon-only fallback exists because an icon-only clickable used
        // to render EMPTY. A subtree whose icon actually paints — a visible
        // <img>, an <svg> with inline geometry, or a sprite <use> that
        // resolves against the primed sheet table — needs no injected handle:
        // the icon is the visible content, and doubling it grew ChatGPT's
        // composer a "[Start dictation]" label beside the rendered mic icon.
        let mut dom = Dom::parse_document(
            "<body>\
             <button id=inline aria-label='Play'><svg viewBox='0 0 24 24'><path d='M0 0h24v24z'></path></svg></button>\
             <button id=pic aria-label='Send'><img src='/send.png'></button>\
             <button id=sprite aria-label='Start dictation'><svg><use href='/s.svg#mic'></use></svg></button>\
             <button id=cold aria-label='Search'><svg><use href='/cold.svg#glass'></use></svg></button>\
             </body>",
        );
        dom.set_doc_url(url::Url::parse("https://sprite-label-suppress.example/page").ok());
        prime_sprite_sheet(
            "https://sprite-label-suppress.example/s.svg",
            "<svg><symbol id='mic' viewBox='0 0 24 24'><path d='M0 0h24v24z'/></symbol></svg>",
        );
        let ids: Vec<_> = ["inline", "pic", "sprite", "cold"]
            .iter()
            .map(|i| dom.get_by_id(i).unwrap())
            .collect();
        let clickable: std::collections::HashSet<_> = ids.iter().copied().collect();
        let html = dom.serialize_live(DOCUMENT, &clickable);
        assert!(
            !html.contains("[Play]"),
            "inline-geometry icon must not double its name: {html}"
        );
        assert!(
            !html.contains("[Send]"),
            "img icon must not double its name: {html}"
        );
        assert!(
            !html.contains("[Start dictation]"),
            "resolved sprite icon must not double its name: {html}"
        );
        // An unresolved sprite with a recognized action receives a compact
        // UA pictogram inside the button. Its accessible name is not painted.
        assert!(
            html.contains("⌕") && !html.contains("[Search]"),
            "cold search control gets only a pictogram: {html}"
        );
        // All four retain direct activation markers either way.
        for id in ids {
            assert!(
                html.contains(&format!("data-trust-node=\"{id}\"")),
                "{html}"
            );
        }
    }

    #[test]
    fn serialize_live_does_not_duplicate_native_control_labels() {
        // HTML form controls already have a visible widget representation in
        // the layout contract.  Their aria-label/title/value remains metadata
        // for the accessibility tree; it must not also become a wrapper's
        // bracketed text handle (HTML §4.10; AccName §4).
        let dom = Dom::parse_document(
            "<body><form><input id=q title='Google Suche' name=q>\
             <input id=submit type=submit value='Google Suche'></form></body>",
        );
        let submit = dom.get_by_id("submit").unwrap();
        let clickable = std::collections::HashSet::from([submit]);
        let html = dom.serialize_live(DOCUMENT, &clickable);
        assert!(
            html.contains(&format!("x-trust-js:{submit}:")),
            "submit control remains live: {html}"
        );
        assert!(
            !html.contains("[Google Suche]"),
            "native submit's value is not painted a second time: {html}"
        );
        assert!(
            !html.contains("[Google Suche ]"),
            "native text control's title is not painted as a wrapper label: {html}"
        );
    }

    #[test]
    fn serialize_live_drops_a_clipped_icon_controls_accessible_name() {
        // `aria-label` is an accessible name, not generated visual content,
        // regardless of clipping. Neither control may acquire bracketed text.
        let dom = Dom::parse_document(
            "<body>\
             <button id=reply aria-label='Click to reply to @user' style='width:3.2rem;height:3.2rem;overflow:hidden'></button>\
             <button id=menu aria-label='Open menu'></button></body>",
        );
        let reply = dom.get_by_id("reply").unwrap();
        let menu = dom.get_by_id("menu").unwrap();
        let clickable = std::collections::HashSet::from([reply, menu]);
        let html = dom.serialize_live(DOCUMENT, &clickable);
        assert!(
            !html.contains("[Click to reply"),
            "a clipped accessible name is not surfaced as a label: {html}"
        );
        assert!(
            !html.contains("[Open menu]"),
            "an unclipped accessible name is metadata too: {html}"
        );
    }

    #[test]
    fn serialize_live_drops_a_full_bleed_overlay_scrims_handle() {
        // A content-less full-area positioned overlay (Twitch's `<button
        // aria-label="Play" style="position:absolute;width:100%;height:100%">`
        // click-to-play scrim) paints nothing in a browser — the live serializer
        // must not give it a bracketed handle, which floated "[Play]" over the
        // player. A normal icon control also keeps its name as metadata only.
        let dom = Dom::parse_document(
            "<body>\
             <button id=scrim aria-label='Play' style='position:absolute;width:100%;height:100%'></button>\
             <button id=menu aria-label='Open menu'></button></body>",
        );
        let scrim = dom.get_by_id("scrim").unwrap();
        let menu = dom.get_by_id("menu").unwrap();
        let clickable = std::collections::HashSet::from([scrim, menu]);
        let html = dom.serialize_live(DOCUMENT, &clickable);
        assert!(
            !html.contains("[Play]"),
            "a full-bleed scrim is not given a handle label: {html}"
        );
        assert!(
            !html.contains("[Open menu]"),
            "an ordinary icon control does not paint its accessible name: {html}"
        );
    }

    #[test]
    fn clickable_inside_an_anchor_is_not_wrapped_in_a_nested_anchor() {
        // archive.org tiles: an info <button> nested inside the tile's own
        // <a aria-label="…">. Wrapping the button in its own x-trust-js <a>
        // makes an <a>-in-<a>; when the app re-parses this serialized output
        // for layout, html5ever's adoption agency SPLITS the outer anchor
        // into empty fragments that still carry aria-label — leaking the
        // title as two extra link lines. The nested clickable must stay
        // UN-wrapped (it inherits the surrounding anchor's link).
        let dom = Dom::parse_document(
            "<body><a id=tile href='/details/x' aria-label='Tile Title'>\
               <button id=info aria-label='info'>i</button>\
               <h3>Tile Title</h3>\
             </a></body>",
        );
        let tile = dom.get_by_id("tile").unwrap();
        let info = dom.get_by_id("info").unwrap();
        let clickable = std::collections::HashSet::from([tile, info]);
        let html = dom.serialize_live(DOCUMENT, &clickable);
        // Exactly one anchor in the output: the tile. The nested button got
        // no wrapper marker.
        assert_eq!(html.matches("<a ").count(), 1, "one anchor only: {html}");
        assert!(
            !html.contains(&format!("x-trust-js:{info}:")),
            "info button not wrapped in a nested anchor: {html}"
        );
        // The tile anchor still routes through the actor (href rewritten).
        assert!(
            html.contains(&format!("x-trust-js:{tile}:/details/x")),
            "{html}"
        );
        // The decisive check: re-parsing the serialized output keeps the
        // anchor INTACT — no adoption-agency split — so its aria-label never
        // leaks as duplicate text.
        let reparsed = Dom::parse_document(&html);
        let anchors = reparsed
            .descendants(DOCUMENT)
            .filter(|&d| reparsed.tag_name(d) == Some("a"))
            .count();
        assert_eq!(anchors, 1, "anchor survives re-parse un-split: {html}");
    }

    #[test]
    fn ordinary_live_anchor_retains_actor_for_html_default_activation() {
        let dom = Dom::parse_document(
            r#"<iframe name="content"></iframe><a href="inside.html" target="content">open</a>"#,
        );
        let html = dom.serialize_live(DOCUMENT, &std::collections::HashSet::new());
        assert!(html.contains("href=\"inside.html\""), "{html}");
        assert!(html.contains("target=\"content\""), "{html}");
        assert!(html.contains("data-trust-click=\"x-trust-js:"), "{html}");
    }

    #[test]
    fn shadow_trees_flatten_with_slot_projection() {
        let mut dom = Dom::parse_document(
            "<body><my-card><span slot=title>Hello</span>plain text</my-card></body>",
        );
        let host = dom
            .descendants(DOCUMENT)
            .into_iter()
            .find(|&d| dom.tag_name(d) == Some("my-card"))
            .unwrap();
        let root = dom.attach_shadow(host);
        let nodes = dom.parse_fragment_into(
            "div",
            "<h2><slot name=title>untitled</slot></h2><p>body: <slot></slot></p><slot name=missing>fallback</slot>",
        );
        for n in nodes {
            dom.append(root, n);
        }
        let html = dom.serialize(DOCUMENT);
        // Shadow content replaces light children; slots project.
        assert!(
            html.contains("<h2><span slot=\"title\">Hello</span></h2>"),
            "{html}"
        );
        assert!(html.contains("body: plain text"), "{html}");
        // Unassigned slot falls back to its own content.
        assert!(html.contains("fallback"), "{html}");
        // The light children don't ALSO render outside their slots.
        assert_eq!(html.matches("Hello").count(), 1, "{html}");
    }

    #[test]
    fn custom_properties_resolve_through_the_cascade() {
        // A custom property defined on an ancestor inherits to a descendant and
        // resolves in its `var()` reference to the DEFINED value (not just the
        // fallback) — the lever for sites whose cell sizing rides custom props
        // (archive.org's `--infinitescrollercellminwidth`). Resolved at bake,
        // while the stylesheets are still present.
        let dom = Dom::parse_document(
            "<body><div id=root style=\"--cell: 12rem\">\
             <p id=c style=\"min-width: var(--cell, 16rem)\">x</p></div></body>",
        );
        let c = dom.get_by_id("c").unwrap();
        let html = dom.serialize(c);
        // The resolved value is baked; it's appended after the original so the
        // re-parsed inline cascade (later-wins) uses 12rem, not the fallback.
        assert!(
            html.contains("min-width:12rem"),
            "defined --cell wins: {html}"
        );

        // A class-defined custom property (in a dropped stylesheet) resolves too.
        let dom = Dom::parse_document(
            "<body><div class=scope><p id=c style=\"min-width: var(--cell, 16rem)\">x</p></div>\
             <style>.scope{--cell:10rem}</style></body>",
        );
        let c = dom.get_by_id("c").unwrap();
        assert!(
            dom.serialize(c).contains("min-width:10rem"),
            "stylesheet-defined --cell resolves"
        );

        // Defined on `:root` — the conventional home for custom properties.
        let dom = Dom::parse_document(
            "<html><head><style>:root{--cell:8rem}</style></head>\
             <body><p id=c style=\"min-width: var(--cell, 16rem)\">x</p></body></html>",
        );
        let c = dom.get_by_id("c").unwrap();
        assert!(
            dom.serialize(c).contains("min-width:8rem"),
            ":root-defined --cell resolves"
        );
    }

    #[test]
    fn inherited_custom_property_cache_is_case_sensitive_and_epoch_scoped() {
        // CSS Custom Properties §2: unregistered custom properties inherit,
        // their names compare codepoint-for-codepoint, and a mutation must be
        // visible at the next computed-value read. The deep chain is also the
        // Speedometer complex-DOM shape which makes repeated uncached ancestor
        // walks dominate a forced layout.
        let depth = 128;
        let mut html =
            String::from("<div id=root style='--Base:red;--Tone:VAR(--Base);--tone:blue'>");
        for _ in 0..depth {
            html.push_str("<div>");
        }
        html.push_str(
            "<span id=leaf style='color:var(--Tone);background-color:var(--tone)'>x</span>",
        );
        for _ in 0..=depth {
            html.push_str("</div>");
        }
        let mut dom = Dom::parse_document(&html);
        let root = dom.get_by_id("root").unwrap();
        let leaf = dom.get_by_id("leaf").unwrap();

        assert_eq!(
            dom.computed_value_resolved(leaf, "color").as_deref(),
            Some("red")
        );
        assert_eq!(
            dom.computed_value_resolved(leaf, "background-color")
                .as_deref(),
            Some("blue")
        );
        assert!(
            dom.custom_prop_cache.borrow().1.len() >= depth,
            "the first inherited lookup memoizes the ancestor path"
        );

        dom.set_attr(
            root,
            "style",
            "--Base:green;--Tone:VAR(--Base);--tone:purple",
        );
        assert_eq!(
            dom.computed_value_resolved(leaf, "color").as_deref(),
            Some("green"),
            "a new DOM epoch cannot reuse the old inherited value"
        );
        assert_eq!(
            dom.computed_value_resolved(leaf, "background-color")
                .as_deref(),
            Some("purple")
        );
    }

    #[test]
    fn custom_property_display_controls_box_generation() {
        // CSS Variables L1 §3: `display:var(--state)` is substituted at
        // computed-value time. This is the visibility pattern used by
        // Stack Exchange's `.s-popover` (`--_po-d:none`, changed to `block`
        // by `.is-visible`); a closed popover must not leak its tooltip box
        // into the document while its visible counterpart still renders.
        let dom = Dom::parse_document(
            r#"<head><style>
                .s-popover { --_state: none; display: var(--_state) }
                .s-popover.is-visible { --_state: block }
            </style></head>
            <body><div id=closed class=s-popover>closed tooltip</div>
            <div id=open class="s-popover is-visible">open popover</div></body>"#,
        );
        let closed = dom.get_by_id("closed").unwrap();
        let open = dom.get_by_id("open").unwrap();
        assert_eq!(dom.computed_display(closed).as_deref(), Some("none"));
        assert!(
            dom.is_hidden(closed),
            "the substituted display:none omits the box"
        );
        assert_eq!(dom.computed_display(open).as_deref(), Some("block"));
        assert!(!dom.is_hidden(open), "the class override restores the box");
    }

    #[test]
    fn background_shorthand_waits_for_custom_property_substitution() {
        // CSS Variables §3: when a shorthand contains var(), its longhands
        // receive a pending-substitution value. Parsing the shorthand before
        // substitution would mistake this declaration for an omitted color and
        // reset it to transparent (archive.org's white logo on a white nav).
        let mut dom = Dom::parse_document("<body><ia-nav id=host></ia-nav></body>");
        let host = dom.get_by_id("host").unwrap();
        let root = dom.attach_shadow(host);
        let style = dom.create_element("style");
        let css = dom.create_text(
            ":host{--grey13:#222;--primaryNavBg:var(--grey13)}\
             nav{background:var(--primaryNavBg)}",
        );
        dom.append(style, css);
        dom.append(root, style);
        let nav = dom.create_element("nav");
        dom.append(root, nav);

        assert_eq!(
            dom.computed_style(nav, "background-color").as_deref(),
            Some("#222")
        );
        assert_eq!(
            dom.computed_value_resolved(nav, "background-color")
                .as_deref(),
            Some("#222")
        );
        let html = dom.serialize(host);
        assert!(html.contains("background-color:#222"), "{html}");
        assert!(!html.contains("background-color:transparent"), "{html}");
    }

    #[test]
    fn invalid_legacy_gradient_keeps_valid_background_fallback() {
        // CSS Images 3 §3.1 accepts `to bottom`, not the pre-standard
        // unprefixed `top` direction. CSS Syntax/Cascade discard each invalid
        // declaration as a unit, leaving the earlier solid fallback intact.
        let dom = Dom::parse_document(
            "<style>#x{background:#5e95a1;\
             background:-moz-linear-gradient(top,#fff,#000);\
             background:linear-gradient(top,#fff,#000)}</style>\
             <div id=x></div>",
        );
        let x = dom.get_by_id("x").unwrap();
        assert_eq!(
            dom.computed_value_resolved(x, "background-color")
                .as_deref(),
            Some("#5e95a1")
        );
        assert_eq!(
            dom.computed_value_resolved(x, "background-image")
                .as_deref(),
            Some("none")
        );
    }

    #[test]
    fn background_shorthand_retains_position_repeat_and_size() {
        // CSS Backgrounds 3 §2.10: explicit layer values replace their
        // initial values while omitted longhands reset normally.
        let dom = Dom::parse_document(
            "<div id=x style='background:url(hero.png) 95% bottom / 734px auto no-repeat'></div>",
        );
        let x = dom.get_by_id("x").unwrap();
        assert_eq!(
            dom.computed_value_resolved(x, "background-position")
                .as_deref(),
            Some("95% bottom")
        );
        assert_eq!(
            dom.computed_value_resolved(x, "background-size").as_deref(),
            Some("734px auto")
        );
        assert_eq!(
            dom.computed_value_resolved(x, "background-repeat")
                .as_deref(),
            Some("no-repeat")
        );
    }

    #[test]
    fn pseudo_content_resolves_custom_property_and_visual_alt_split() {
        // CSS Variables 1 §3 + CSS Content 3 §1.2. Font icon libraries
        // commonly keep the glyph in a custom property and mark it decorative
        // with empty alternative text after `/`.
        let dom = Dom::parse_document(
            r#"<style>#x{--icon:"\f004"}#x::before{content:var(--icon)/""}</style>
               <i id=x></i>"#,
        );
        let x = dom.get_by_id("x").unwrap();
        assert_eq!(
            dom.pseudo_content(x, PseudoEl::Before).as_deref(),
            Some("\u{f004}")
        );
    }

    #[test]
    fn custom_property_falls_back_when_undefined() {
        let dom = Dom::parse_document(
            "<body><p id=c style=\"min-width: var(--cell, 16rem)\">x</p></body>",
        );
        let c = dom.get_by_id("c").unwrap();
        assert!(
            dom.serialize(c).contains("min-width:16rem"),
            "undefined --cell uses the fallback"
        );
    }

    #[test]
    fn cyclic_custom_property_is_invalid_at_computed_value_time() {
        // CSS Variables L1 §3 "Resolving Dependency Cycles": a custom property
        // that references itself (Vector-2022 ships
        // `--font-size-medium: var(--font-size-medium, 1rem)`) is invalid at
        // computed-value time. WITHOUT cycle detection this recurses until the
        // 64MB `trust-page` stack aborts (the telewiki.miraheze.org/wiki/Users
        // crash). Strict spec: the cyclic property is the guaranteed-invalid
        // value, so a *downstream* reference WITH a fallback uses its own
        // fallback — and the cyclic property's own fallback is NOT consulted.
        let dom = Dom::parse_document(
            "<body><div id=root style=\"--cell: var(--cell, 9rem)\">\
             <p id=c style=\"min-width: var(--cell, 16rem)\">x</p></div></body>",
        );
        let c = dom.get_by_id("c").unwrap();
        let html = dom.serialize(c); // must terminate, not stack-overflow
        assert!(
            html.contains("min-width:16rem"),
            "self-cyclic --cell is invalid → downstream fallback (16rem), not its own (9rem): {html}"
        );

        // A mutual cycle (`--a` ⇄ `--b`) is the same: both invalid, so the
        // reference's own fallback wins.
        let dom = Dom::parse_document(
            "<head><style>:root{--a:var(--b);--b:var(--a)}</style></head>\
             <body><p id=c style=\"min-width: var(--a, 5rem)\">x</p></body>",
        );
        let c = dom.get_by_id("c").unwrap();
        assert!(
            dom.serialize(c).contains("min-width:5rem"),
            "mutually cyclic --a/--b are invalid → the reference's fallback (5rem) is used"
        );

        // A non-cyclic chain still resolves fully (regression guard: the
        // resolution stack must not flag a legitimate A→B→literal as a cycle).
        let dom = Dom::parse_document(
            "<head><style>:root{--a:var(--b);--b:7rem}</style></head>\
             <body><p id=c style=\"min-width: var(--a, 5rem)\">x</p></body>",
        );
        let c = dom.get_by_id("c").unwrap();
        assert!(
            dom.serialize(c).contains("min-width:7rem"),
            "an acyclic --a→--b→7rem chain resolves to 7rem"
        );
    }

    #[test]
    fn serialize_bakes_computed_display_into_style() {
        let dom = Dom::parse_document(
            "<html><head><style>li{display:inline}</style></head>\
             <body><ul><li>x</li></ul></body></html>",
        );
        let li = dom
            .descendants(DOCUMENT)
            .into_iter()
            .find(|&id| dom.tag_name(id) == Some("li"))
            .unwrap();
        assert_eq!(dom.computed_display(li).as_deref(), Some("inline"));
        // The serialized HTML carries the computed display so a re-parse
        // (the layout arena) flows it the same way.
        let html = dom.serialize(DOCUMENT);
        assert!(html.contains("display:inline"), "baked display: {html}");
        // Merges into an existing inline style rather than dropping it.
        let dom = Dom::parse_document(
            r#"<body><p style="color:red" class="x">y</p><style>.x{display:inline}</style></body>"#,
        );
        let p = dom
            .descendants(DOCUMENT)
            .into_iter()
            .find(|&id| dom.tag_name(id) == Some("p"))
            .unwrap();
        let html = dom.serialize(p);
        assert!(html.contains("color:red"), "keeps original style: {html}");
        assert!(html.contains("display:inline"), "adds display: {html}");

        // Box properties (margin shorthand → longhands) bake too, so a
        // living page's CSS spacing reaches the re-parsed layout arena.
        let dom =
            Dom::parse_document("<body><p class=x>y</p><style>.x{margin:1em 0}</style></body>");
        let p = dom
            .descendants(DOCUMENT)
            .into_iter()
            .find(|&id| dom.tag_name(id) == Some("p"))
            .unwrap();
        assert_eq!(dom.computed_style(p, "margin-top").as_deref(), Some("1em"));
        let html = dom.serialize(p);
        assert!(html.contains("margin-top:1em"), "bakes margin: {html}");
    }

    #[test]
    fn closed_dialog_is_hidden_open_one_renders() {
        // UA default `dialog:not([open]){display:none}`: a closed dialog's
        // content must not render (modal text otherwise bleeds into the
        // page), an open one does, and an author `display` rule wins.
        let dom = Dom::parse_document(
            "<body><dialog id=a>shut</dialog><dialog id=b open>shown</dialog></body>",
        );
        let a = dom.get_by_id("a").unwrap();
        let b = dom.get_by_id("b").unwrap();
        assert!(dom.is_hidden(a), "closed dialog hidden");
        assert!(!dom.is_hidden(b), "open dialog renders");
        // Serialization drops the hidden one, keeps the open one.
        let html = dom.serialize(DOCUMENT);
        assert!(!html.contains("shut"), "closed dialog dropped: {html}");
        assert!(html.contains("shown"), "open dialog kept: {html}");
        // An author rule setting the dialog's display overrides the UA
        // default — a closed dialog forced visible renders.
        let dom = Dom::parse_document(
            "<body><dialog id=c>forced</dialog><style>#c{display:block}</style></body>",
        );
        let c = dom.get_by_id("c").unwrap();
        assert!(
            !dom.is_hidden(c),
            "author display:block beats the UA default"
        );
    }

    #[test]
    fn visually_hidden_sr_only_is_dropped() {
        // The universal screen-reader-only idiom (1px clipped absolutely
        // positioned box) carries text invisible to sighted users — both the
        // class form (Bootstrap/Tailwind `.sr-only`) and the inline form
        // (archive.org's `aria-describedby` targets) must be hidden + dropped,
        // while a normal sibling renders.
        let dom = Dom::parse_document(
            "<body>\
             <span id=a class=sr>screen reader only</span>\
             <span id=b style=\"position:absolute;overflow:hidden;width:1px;height:1px\">inline hidden</span>\
             <span id=c>visible</span>\
             <style>.sr{position:absolute;overflow:hidden;width:1px;height:1px;clip:rect(0,0,0,0)}</style>\
             </body>",
        );
        let a = dom.get_by_id("a").unwrap();
        let b = dom.get_by_id("b").unwrap();
        let c = dom.get_by_id("c").unwrap();
        assert!(dom.is_hidden(a), "class .sr-only hidden");
        assert!(dom.is_hidden(b), "inline sr-only hidden");
        assert!(!dom.is_hidden(c), "normal content visible");
        let html = dom.serialize(DOCUMENT);
        assert!(
            !html.contains("screen reader only"),
            "class sr dropped: {html}"
        );
        assert!(!html.contains("inline hidden"), "inline sr dropped: {html}");
        assert!(html.contains("visible"), "normal kept: {html}");
        // A wider absolutely-positioned overflow-hidden box is NOT sr-only.
        let dom2 = Dom::parse_document(
            "<body><div id=d style=\"position:absolute;overflow:hidden;width:20em\">real</div></body>",
        );
        let d = dom2.get_by_id("d").unwrap();
        assert!(!dom2.is_hidden(d), "a real clipped box is not sr-only");
    }

    #[test]
    fn opposing_negative_insets_with_auto_margins_keep_centered_images() {
        // CSS Position 3 §5.1: opposing negative insets with auto margins
        // resolve through the positioning constraint and center the box. This
        // is the image-centering pattern used by Amazon's gateway cards; a
        // one-sided negative inset remains an off-screen accessibility box.
        let dom = Dom::parse_document(
            "<body>\
             <div id=card style=\"position:relative;width:320px;height:180px\">\
               <img id=centered src=card.jpg style=\"position:absolute;left:-9999px;right:-9999px;margin:auto;height:100%\">\
             </div>\
             <span id=hidden style=\"position:absolute;left:-9999px\">screen reader only</span>\
             </body>",
        );
        let centered = dom.get_by_id("centered").unwrap();
        let hidden = dom.get_by_id("hidden").unwrap();
        assert!(!dom.is_hidden(centered), "centered image was hidden");
        assert!(dom.is_hidden(hidden), "one-sided off-screen text was kept");
        assert!(
            dom.serialize(DOCUMENT).contains("card.jpg"),
            "centered image was dropped from the live tree"
        );
    }

    #[test]
    fn zero_axis_overflow_hidden_box_is_hidden_but_padding_ratio_box_renders() {
        // A box collapsed to zero on an axis with `overflow:hidden` on that
        // axis clips ALL its content — Steam's `.menu_takeover_background`
        // preload copy of the banner (`height:0;overflow:hidden`) drew a
        // full-width 1-row sliver. Hide it (and its image child).
        let dom = Dom::parse_document(
            "<body>\
             <div id=a style=\"height:0;overflow:hidden\"><img src=banner.jpg></div>\
             <div id=b style=\"max-height:0;overflow:hidden\">collapsed drawer</div>\
             <div id=c style=\"width:0;overflow-x:hidden\">narrow</div>\
             <div id=d style=\"height:0;overflow:hidden;padding-bottom:56.25%\"><img id=di src=tile.jpg></div>\
             <div id=e style=\"height:0\">no clip, not hidden</div>\
             <div id=f style=\"height:0;overflow:auto\">scrollable, not hidden</div>\
             </body>",
        );
        let g = |i| dom.get_by_id(i).unwrap();
        assert!(dom.is_hidden(g("a")), "height:0 + overflow:hidden hidden");
        assert!(
            dom.is_hidden(g("b")),
            "max-height:0 + overflow:hidden hidden"
        );
        assert!(dom.is_hidden(g("c")), "width:0 + overflow-x:hidden hidden");
        // The responsive-image intrinsic-ratio box (padding reserves height)
        // is NOT empty — its absolutely-positioned child fills the padding box.
        assert!(
            !dom.is_hidden(g("d")),
            "padding-bottom ratio box renders (responsive image idiom)"
        );
        assert!(!dom.is_hidden(g("di")), "the ratio box's image renders");
        assert!(
            !dom.is_hidden(g("e")),
            "height:0 with visible overflow is not hidden"
        );
        assert!(
            !dom.is_hidden(g("f")),
            "height:0 with overflow:auto is not hidden"
        );
        let html = dom.serialize(DOCUMENT);
        assert!(
            !html.contains("banner.jpg"),
            "hidden banner dropped: {html}"
        );
        assert!(html.contains("tile.jpg"), "ratio-box image kept: {html}");
    }

    #[test]
    fn content_mutations_retain_the_parsed_style_index() {
        // The style-epoch split: ordinary content mutations (text, attrs,
        // appends of ordinary nodes) must NOT rebuild the parsed style index
        // (`Rc` identity proves retention) — while per-element matching still
        // follows the mutation (the class toggle below styles correctly
        // against the RETAINED index).
        let mut dom = Dom::parse_document(
            "<head><style>.hot{letter-spacing:3px}</style></head>\
             <body><p id=t>x</p><div id=box></div></body>",
        );
        let t = dom.get_by_id("t").unwrap();
        let box_id = dom.get_by_id("box").unwrap();
        let idx0 = dom.style_index();
        // Text mutation.
        dom.set_text(t, "tick");
        assert!(
            std::rc::Rc::ptr_eq(&idx0, &dom.style_index()),
            "a text edit must not re-parse the sheets"
        );
        // Ordinary attribute mutation — and the cascade still follows it.
        dom.set_attr(t, "class", "hot");
        assert!(
            std::rc::Rc::ptr_eq(&idx0, &dom.style_index()),
            "an attr change must not re-parse the sheets"
        );
        assert_eq!(
            dom.computed_style(t, "letter-spacing").as_deref(),
            Some("3px"),
            "matching re-runs against the retained index"
        );
        // Appending an ordinary subtree.
        let d = dom.create_element("div");
        let s = dom.create_element("span");
        dom.append(d, s);
        dom.append(box_id, d);
        assert!(
            std::rc::Rc::ptr_eq(&idx0, &dom.style_index()),
            "an ordinary subtree attach must not re-parse the sheets"
        );
        // Detaching it again.
        dom.detach(d);
        assert!(
            std::rc::Rc::ptr_eq(&idx0, &dom.style_index()),
            "an ordinary detach must not re-parse the sheets"
        );
    }

    #[test]
    fn style_mutations_rebuild_the_index() {
        // The standards' sheet-(re)creation triggers (HTML §4.2.6 for
        // <style> text/tree changes, <link> attribute changes, plus adopted
        // sheets and the viewport) must each invalidate the parsed index —
        // and the new rules must actually apply.
        let mut dom = Dom::parse_document(
            "<head><style id=sh>.a{letter-spacing:1px}</style></head>\
             <body><p id=t class='a b c'>x</p></body>",
        );
        let t = dom.get_by_id("t").unwrap();
        let sheet = dom.get_by_id("sh").unwrap();
        let fresh = |dom: &Dom, prev: &std::rc::Rc<StyleIndex>| {
            let now = dom.style_index();
            !std::rc::Rc::ptr_eq(prev, &now)
        };
        // 1. Editing the <style> element's text.
        let i = dom.style_index();
        dom.set_text(sheet, ".a{letter-spacing:2px}");
        assert!(fresh(&dom, &i), "style text edit rebuilds");
        assert_eq!(
            dom.computed_style(t, "letter-spacing").as_deref(),
            Some("2px")
        );
        // 2. A script-created <style> appended to the tree.
        let i = dom.style_index();
        let st = dom.create_element("style");
        let css = dom.create_text(".b{text-indent:4px}");
        dom.append(st, css);
        let head = dom
            .descendants(DOCUMENT)
            .into_iter()
            .find(|&n| dom.tag_name(n) == Some("head"))
            .unwrap();
        dom.append(head, st);
        assert!(fresh(&dom, &i), "appending a style element rebuilds");
        assert_eq!(dom.computed_style(t, "text-indent").as_deref(), Some("4px"));
        // 3. A subtree attach whose NESTED content carries a <style>.
        let i = dom.style_index();
        let wrap = dom.create_element("div");
        let inner = dom.create_element("style");
        let css2 = dom.create_text(".c{text-transform:uppercase}");
        dom.append(inner, css2);
        dom.append(wrap, inner);
        let body = dom
            .descendants(DOCUMENT)
            .into_iter()
            .find(|&n| dom.tag_name(n) == Some("body"))
            .unwrap();
        dom.append(body, wrap);
        assert!(fresh(&dom, &i), "a nested-style subtree attach rebuilds");
        assert_eq!(
            dom.computed_value(t, "text-transform").as_deref(),
            Some("uppercase")
        );
        // 4. Detaching a style element removes its rules.
        let i = dom.style_index();
        dom.detach(st);
        assert!(fresh(&dom, &i), "detaching a style element rebuilds");
        assert_eq!(dom.computed_style(t, "text-indent"), None);
        // 5. Adopted sheets.
        let i = dom.style_index();
        dom.set_adopted_styles(DOCUMENT, ".a{margin-top:9px}");
        assert!(fresh(&dom, &i), "adoptedStyleSheets rebuilds");
        assert_eq!(dom.computed_style(t, "margin-top").as_deref(), Some("9px"));
        // 6. Viewport change (@media re-evaluation).
        let i = dom.style_index();
        dom.set_viewport_px(800.0, 600.0);
        assert!(fresh(&dom, &i), "viewport change rebuilds");
        // 7. An attribute change on a sheet-bearing element.
        let i = dom.style_index();
        dom.set_attr(sheet, "media", "screen");
        assert!(fresh(&dom, &i), "style/link attr change rebuilds");
    }

    /// The style-epoch split's honest A/B, one binary (release:
    /// `cargo test --release style_epoch_bench -- --ignored --nocapture`).
    /// Loop A mutates CONTENT then reads styles — the index is retained, so
    /// each cycle pays only re-matching. Loop B touches the SHEET each cycle
    /// — forcing the full re-parse + bucket rebuild that, before the split,
    /// EVERY mutation paid. B−A ≈ the per-mutate-read-cycle saving.
    #[test]
    #[ignore]
    fn style_epoch_bench() {
        let mut css = String::new();
        for i in 0..4000 {
            css.push_str(&format!(
                ".c{i}{{letter-spacing:{}px;margin-top:{}px;text-indent:{}px}}\n",
                i % 9,
                i % 5,
                i % 3
            ));
        }
        let mut html = format!("<head><style id=sh>{css}</style></head><body>");
        for i in 0..300 {
            html.push_str(&format!("<p id=n{i} class='c{}'>x</p>", (i * 13) % 4000));
        }
        html.push_str("</body>");
        let mut dom = Dom::parse_document(&html);
        let ids: Vec<NodeId> = (0..20)
            .map(|i| dom.get_by_id(&format!("n{i}")).unwrap())
            .collect();
        let sheet = dom.get_by_id("sh").unwrap();
        let read = |dom: &Dom| {
            for &id in &ids {
                for p in ["letter-spacing", "margin-top", "text-indent", "display"] {
                    let _ = dom.computed_style(id, p);
                }
                let _ = dom.is_hidden(id);
            }
        };
        let _ = dom.style_index(); // warm
        let n = 200;
        let t = std::time::Instant::now();
        for i in 0..n {
            dom.set_text(ids[0], &format!("tick {i}")); // content churn
            read(&dom);
        }
        let content = t.elapsed();
        let t = std::time::Instant::now();
        for i in 0..n {
            dom.set_attr(sheet, "data-i", &format!("{i}")); // sheet churn
            read(&dom);
        }
        let style = t.elapsed();
        println!(
            "content churn (index retained): {content:?} ({:?}/cycle)\n\
             sheet churn (index rebuilt):    {style:?} ({:?}/cycle)\n\
             saved per mutate-then-read cycle: {:?}",
            content / n,
            style / n,
            (style.saturating_sub(content)) / n
        );
    }

    #[test]
    fn winner_map_keeps_untracked_inline_props_and_pseudo_buckets() {
        // The per-element winner map must preserve two easy-to-lose
        // behaviors: (1) UNTRACKED properties declared INLINE stay readable
        // (getComputedStyle of `background` — sheets filter untracked props
        // at parse, inline must not); (2) a rule targeting ::before must not
        // leak its declarations onto the element bucket, nor vice versa.
        let dom = Dom::parse_document(
            "<head><style>\
             .x::before{content:\"*\";text-indent:7px}\
             .x{letter-spacing:2px}\
             </style></head>\
             <body><p id=t class=x style='background:red;letter-spacing:3px'>y</p></body>",
        );
        let t = dom.get_by_id("t").unwrap();
        assert_eq!(
            dom.computed_value(t, "background").as_deref(),
            Some("red"),
            "untracked inline property readable (getComputedStyle path)"
        );
        assert_eq!(
            dom.computed_style(t, "letter-spacing").as_deref(),
            Some("3px"),
            "inline beats the sheet rule"
        );
        assert_eq!(
            dom.computed_style(t, "text-indent"),
            None,
            "::before declarations don't leak onto the element"
        );
        assert_eq!(
            dom.pseudo_style(t, PseudoEl::Before, "text-indent")
                .as_deref(),
            Some("7px"),
            "the pseudo bucket holds its own winners"
        );
        assert_eq!(
            dom.pseudo_content(t, PseudoEl::Before).as_deref(),
            Some("*")
        );
        assert_eq!(
            dom.pseudo_style(t, PseudoEl::Before, "letter-spacing"),
            None,
            "element declarations don't leak onto the pseudo"
        );
    }

    /// Cascade-read cost on the SERIALIZE path (release:
    /// `cargo test --release cascade_winner_bench -- --ignored --nocapture`).
    /// Each iteration bumps the epoch (one text mutation) then fully
    /// serializes — `write_attrs` reads every baked property per element, so
    /// this measures per-element cascade cost end to end. Compare before/
    /// after the per-element winner map.
    #[test]
    #[ignore]
    fn cascade_winner_bench() {
        let mut css = String::new();
        for i in 0..4000 {
            css.push_str(&format!(
                ".c{i}{{letter-spacing:{}px;margin-top:{}px;text-indent:{}px;padding-left:{}px}}\n",
                i % 9,
                i % 5,
                i % 3,
                i % 7
            ));
        }
        // Give every element multiple matched rules + an inline style (the
        // real-page shape: utility classes + a style attribute).
        let mut html = format!("<head><style>{css}</style></head><body>");
        for i in 0..400 {
            html.push_str(&format!(
                "<p id=n{i} class='c{} c{} c{}' style='margin-bottom:2px'>x</p>",
                (i * 13) % 4000,
                (i * 7 + 1) % 4000,
                (i * 3 + 2) % 4000
            ));
        }
        html.push_str("</body>");
        let mut dom = Dom::parse_document(&html);
        let n0 = dom.get_by_id("n0").unwrap();
        let _ = dom.serialize(DOCUMENT); // warm
        let n = 50u32;
        let t = std::time::Instant::now();
        for i in 0..n {
            dom.set_text(n0, &format!("tick {i}")); // epoch bump per frame
            let _ = dom.serialize(DOCUMENT);
        }
        let total = t.elapsed();
        println!(
            "serialize after mutation: {total:?} ({:?}/serialize)",
            total / n
        );
    }

    #[test]
    fn descendants_iterator_walks_document_order() {
        // The lazy pointer-walk must produce the exact pre-order document
        // order the old materialized walk did — including climbing out of
        // deep branches to an ancestor's next sibling, and staying INSIDE
        // the subtree when the walk is rooted below the document.
        let dom = Dom::parse_document(
            "<body><div id=a><p id=b><b id=c>x</b></p><p id=d>y</p></div><span id=e>z</span></body>",
        );
        let tags: Vec<&str> = dom
            .descendants(DOCUMENT)
            .filter_map(|d| dom.tag_name(d))
            .collect();
        assert_eq!(tags, ["html", "head", "body", "div", "p", "b", "p", "span"]);
        // Rooted at #a: only its subtree, never the following <span>.
        let a = dom.get_by_id("a").unwrap();
        let sub: Vec<&str> = dom.descendants(a).filter_map(|d| dom.tag_name(d)).collect();
        assert_eq!(sub, ["p", "b", "p"]);
        // A leaf element yields no element descendants.
        let c = dom.get_by_id("c").unwrap();
        assert_eq!(
            dom.descendants(c)
                .filter(|&d| dom.tag_name(d).is_some())
                .count(),
            0
        );
    }

    /// Traversal cost (release:
    /// `cargo test --release traversal_bench -- --ignored --nocapture`).
    /// getElementById near the front and the back of a wide document,
    /// querySelector first-match, textContent, and a full serialize —
    /// the paths that used to materialize whole-subtree Vecs (plus a
    /// per-node child Vec) before matching/serializing anything.
    #[test]
    #[ignore]
    fn traversal_bench() {
        let mut html = String::from("<body>");
        html.push_str("<p id=front class=hit>first</p>");
        for i in 0..5000 {
            html.push_str(&format!(
                "<div class=row><p>row {i}</p><span>cell</span></div>"
            ));
        }
        html.push_str("<p id=deep class=hit2>last</p></body>");
        let dom = Dom::parse_document(&html);
        let n = 2000u32;
        let t = std::time::Instant::now();
        for _ in 0..n {
            let _ = dom.get_by_id("front");
        }
        let front = t.elapsed();
        let t = std::time::Instant::now();
        for _ in 0..n {
            let _ = dom.get_by_id("deep");
        }
        let deep = t.elapsed();
        let sel_front = SelectorList::parse(".hit").unwrap();
        let sel_deep = SelectorList::parse(".hit2").unwrap();
        let t = std::time::Instant::now();
        for _ in 0..n {
            let _ = dom.query(DOCUMENT, &sel_front, true);
        }
        let q_front = t.elapsed();
        let t = std::time::Instant::now();
        for _ in 0..n {
            let _ = dom.query(DOCUMENT, &sel_deep, true);
        }
        let q_deep = t.elapsed();
        let t = std::time::Instant::now();
        for _ in 0..50 {
            let _ = dom.text_content(DOCUMENT);
        }
        let text = t.elapsed();
        let t = std::time::Instant::now();
        for _ in 0..50 {
            let _ = dom.serialize(DOCUMENT);
        }
        let ser = t.elapsed();
        println!(
            "getElementById front: {:?}/call  deep: {:?}/call\n\
             querySelector  front: {:?}/call  deep: {:?}/call\n\
             textContent(doc): {:?}/call  serialize(doc): {:?}/call",
            front / n,
            deep / n,
            q_front / n,
            q_deep / n,
            text / 50,
            ser / 50
        );
    }

    #[test]
    fn clone_subtree_is_deep_and_detached() {
        let mut dom = Dom::parse_document("<body><div id=d><p>x</p></div></body>");
        let d = dom.get_by_id("d").unwrap();
        let copy = dom.clone_subtree(d, true);
        assert!(dom.node(copy).parent.is_none());
        assert_eq!(dom.text_content(copy), "x");
    }

    #[test]
    fn insert_before_self_is_an_in_place_no_op() {
        // WHATWG DOM §4.2.4 pre-insert: inserting a node before ITSELF is
        // legal (the reference becomes its next sibling — an in-place move).
        // This used to splice the node's sibling pointers to itself; every
        // later sibling walk (children/serialize) then never terminated.
        let mut dom =
            Dom::parse_document("<body><div id=r><p id=a>1</p><p id=b>2</p></div></body>");
        let r = dom.get_by_id("r").unwrap();
        let a = dom.get_by_id("a").unwrap();
        let b = dom.get_by_id("b").unwrap();
        dom.insert_before(r, a, Some(a));
        assert_eq!(dom.children(r), vec![a, b], "in-place, order kept");
        assert_ne!(dom.node(a).next_sibling, Some(a), "no self-loop");
        // At the END of the child list (no next sibling → plain re-append).
        dom.insert_before(r, b, Some(b));
        assert_eq!(dom.children(r), vec![a, b]);
        // A sibling walk terminates with both children intact.
        let html = dom.serialize(r);
        assert!(html.contains('1') && html.contains('2'), "{html}");
    }

    #[test]
    fn supports_condition_survives_non_ascii() {
        // A multi-byte char at paren depth 0 used to panic the byte-wise
        // ` and `/` or ` scanner (str slicing at a non-char-boundary).
        assert!(!supports_condition("(font-family: x) and 微软"));
        assert!(supports_condition(
            "(font-family: 微软雅黑) or (display: grid)"
        ));
    }

    #[test]
    fn remove_attr_of_an_absent_attribute_stays_clean() {
        // Idempotent removes are free, like set_attr's idempotent writes: a
        // redundant removeAttribute must not dirty the page or invalidate the
        // per-epoch cascade caches.
        let mut dom = Dom::parse_document("<body><p id=a class=x>t</p></body>");
        let a = dom.get_by_id("a").unwrap();
        let _ = dom.take_dirty();
        dom.remove_attr(a, "nope");
        assert!(!dom.take_dirty(), "removing a missing attribute is free");
        dom.remove_attr(a, "class");
        assert!(dom.take_dirty(), "a real removal still dirties");
        assert_eq!(dom.attr(a, "class"), None);
    }

    #[test]
    fn padding_right_is_tracked_and_baked() {
        // padding-right was missing from PROPS (top/bottom/left were there),
        // so sheet-declared right padding was dropped from the cascade and
        // never baked for the re-parsed layout arena.
        let dom = Dom::parse_document(
            "<head><style>#p{padding-right:2em}#q{padding:1em}</style></head>\
             <body><div id=p>x</div><div id=q>y</div></body>",
        );
        let p = dom.get_by_id("p").unwrap();
        let q = dom.get_by_id("q").unwrap();
        assert_eq!(
            dom.computed_style(p, "padding-right").as_deref(),
            Some("2em")
        );
        assert_eq!(
            dom.computed_style(q, "padding-right").as_deref(),
            Some("1em"),
            "the padding shorthand expands to the right longhand"
        );
        assert!(
            dom.serialize(DOCUMENT).contains("padding-right:2em"),
            "baked for the re-parse"
        );
    }

    #[test]
    fn set_attr_preserves_case_on_foreign_elements() {
        // DOM setAttribute folds the name to lowercase only for HTML-namespace
        // elements; SVG attributes are case-sensitive (viewBox). Folding
        // unconditionally created a duplicate lowercase attr and left reads on
        // the stale original — a D3-style setAttribute("viewBox") never took.
        let mut dom = Dom::parse_document(
            r#"<body><svg id=s viewBox="0 0 10 10"><path d="M0 0z"/></svg><div id=d></div></body>"#,
        );
        let s = dom.get_by_id("s").unwrap();
        dom.set_attr(s, "viewBox", "0 0 40 40");
        assert_eq!(
            dom.attr(s, "viewBox"),
            Some("0 0 40 40"),
            "updated in place"
        );
        let viewboxes = dom
            .attr_names(s)
            .iter()
            .filter(|n| n.eq_ignore_ascii_case("viewbox"))
            .count();
        assert_eq!(viewboxes, 1, "no duplicate lowercase attr");
        // HTML elements still fold per the spec.
        let d = dom.get_by_id("d").unwrap();
        dom.set_attr(d, "CLASS", "x");
        assert_eq!(dom.attr(d, "class"), Some("x"));
        assert!(dom.attr_names(d).contains(&"class".to_string()));
    }

    #[test]
    fn attr_selector_case_flags_match() {
        // Selectors 4 `[attr=value i]` (and the no-op `s`). The flag used to
        // be glued onto the value, so such selectors never matched.
        let dom = Dom::parse_document(
            "<body><a id=x href='FILE.PDF'>d</a><a id=y href='file.txt'>t</a></body>",
        );
        let q = |s: &str| {
            dom.query(DOCUMENT, &SelectorList::parse(s).unwrap(), false)
                .len()
        };
        assert_eq!(q("[href$='.pdf' i]"), 1, "i flag: case-insensitive suffix");
        assert_eq!(q("[href$='.pdf']"), 0, "no flag: case-sensitive");
        assert_eq!(q("[href$='.PDF' s]"), 1, "s flag: explicit sensitive");
        // Case-insensitive FILE prefix matches BOTH file.txt and FILE.PDF —
        // the flag parses on an unquoted value too.
        assert_eq!(q("[href^=FILE i]"), 2, "unquoted value with flag");
        assert_eq!(q("[href^=FILE]"), 1, "sensitive prefix matches only one");
        assert_eq!(q("[href='file.pdf' i]"), 1, "i flag: exact");
    }

    #[test]
    fn percentage_opacity_suppresses_paint() {
        // CSS Color 4 <alpha-value>: `opacity: 0%` is valid; the plain-number
        // parser used to fail on it and default to fully opaque.
        let dom = Dom::parse_document(
            "<head><style>.z{opacity:0%}.h{opacity:50%}</style></head>\
             <body><div id=z class=z>a</div><div id=h class=h>b</div>\
             <div style=\"opacity:.4\"><span id=i style=\"opacity:inherit\">c</span></div></body>",
        );
        assert!(
            dom.paint_suppressed(dom.get_by_id("z").unwrap()),
            "0% is invisible"
        );
        assert!(
            !dom.paint_suppressed(dom.get_by_id("h").unwrap()),
            "50% still paints"
        );
        assert!(
            (dom.effective_opacity(dom.get_by_id("i").unwrap()) - 0.4).abs() < f32::EPSILON,
            "the CSS-wide inherit keyword uses the parent's computed alpha"
        );
    }

    #[test]
    fn shadow_host_opacity_resolves_vars_and_survives_live_serialization() {
        // CSS Custom Properties 2 §6 substitutes var() at computed-value
        // time, including fallbacks. CSS Color 4 §3.3 then retains the
        // resulting alpha for group compositing. Live serialization must carry
        // that computed value across the stylesheet-less frontend boundary.
        let mut dom = Dom::parse_document(
            "<body><overlay-backdrop id=b class=opened></overlay-backdrop></body>",
        );
        let backdrop = dom.get_by_id("b").unwrap();
        let root = dom.attach_shadow(backdrop);
        let style = dom.create_element("style");
        let css = dom.create_text(
            ":host { opacity: 0 } \
             :host(.opened) { opacity: var(--overlay-backdrop-opacity, .6) }",
        );
        dom.append(style, css);
        dom.append(root, style);

        assert_eq!(
            dom.computed_style(backdrop, "opacity").as_deref(),
            Some("var(--overlay-backdrop-opacity, .6)"),
            "the cascade retains the custom-property token stream"
        );
        assert!((dom.effective_opacity(backdrop) - 0.6).abs() < f32::EPSILON);

        let html = dom.serialize_live(DOCUMENT, &std::collections::HashSet::new());
        assert!(html.contains("opacity:0.6;"), "{html}");
        let reparsed = Dom::parse_document(&html);
        let reparsed_backdrop = reparsed.get_by_id("b").unwrap();
        assert!(
            (reparsed.effective_opacity(reparsed_backdrop) - 0.6).abs() < f32::EPSILON,
            "the native frontend receives the same compositing alpha: {html}"
        );
    }

    #[test]
    fn ua_display_covers_menu_and_form_internals() {
        // Tags that are block in every browser's UA sheet but fell to the
        // generic `inline` default.
        for tag in ["menu", "option", "optgroup", "legend", "search", "dir"] {
            assert_eq!(ua_display(tag), "block", "{tag}");
        }
    }

    #[test]
    fn comma_separated_animations_still_reveal_the_active_slide() {
        // css-animations-1: `animation` and its longhands are COMMA lists.
        // `animation: fade-in 1s forwards, pulse 2s infinite` used to glom
        // `forwards,pulse` into one whitespace token and lose the name.
        let dom = Dom::parse_document(
            "<head><style>
                @keyframes fade-in { to { opacity: 1 } }
                .s { opacity: 0 }
                .s.active { animation: fade-in 1s forwards, pulse 2s infinite }
                .l2 { opacity: 0; animation-name: pulse, fade-in; animation-fill-mode: none, forwards }
             </style></head>
             <body><div id=a class='s active'>x</div>
             <div id=plain class=s>y</div>
             <div id=l2 class=l2>z</div></body>",
        );
        assert!(
            !dom.paint_suppressed(dom.get_by_id("a").unwrap()),
            "shorthand comma list: fade-in forwards ends visible"
        );
        assert!(
            dom.paint_suppressed(dom.get_by_id("plain").unwrap()),
            "inactive slide stays suppressed"
        );
        assert!(
            !dom.paint_suppressed(dom.get_by_id("l2").unwrap()),
            "longhand comma lists pair by index"
        );
    }

    #[test]
    fn animation_longhand_lists_retain_independent_keyframe_tracks() {
        let dom = Dom::parse_document(
            "<style>
               @keyframes fall { from { top:-10% } to { top:100% } }
               @keyframes shake { 0%,100% { transform:translateX(0) }
                                  50% { transform:translateX(80px) } }
               #snow { animation-name:fall,shake;
                       animation-duration:10s,3s;
                       animation-timing-function:linear,ease-in-out;
                       animation-iteration-count:infinite,infinite;
                       animation-delay:6s,.5s }
             </style><div id=snow>x</div>",
        );
        let animations = dom.css_animation_definitions(dom.get_by_id("snow").unwrap());
        assert_eq!(animations.len(), 2);
        assert_eq!(animations[0].name, "fall");
        assert_eq!(animations[0].duration_seconds, 10.0);
        assert_eq!(animations[0].delay_seconds, 6.0);
        assert_eq!(animations[0].iteration_count, None);
        assert_eq!(animations[0].timing_function, "linear");
        assert_eq!(animations[0].keyframes.len(), 2);
        assert_eq!(animations[0].keyframes[1].top.as_deref(), Some("100%"));
        assert_eq!(animations[1].name, "shake");
        assert_eq!(animations[1].duration_seconds, 3.0);
        assert_eq!(animations[1].delay_seconds, 0.5);
        assert_eq!(animations[1].keyframes.len(), 3);
        assert_eq!(
            animations[1].keyframes[1].transform.as_deref(),
            Some("translatex(80px)")
        );
    }

    #[test]
    fn cursor_is_inherited_as_required_by_css_ui() {
        let dom = Dom::parse_document(
            "<style>#parent{cursor:url(pointer.cur) 2 3, pointer}</style>\
             <div id=parent><span id=child>child</span></div>",
        );
        assert_eq!(
            dom.computed_value_resolved(dom.get_by_id("child").unwrap(), "cursor")
                .as_deref(),
            Some("url(pointer.cur) 2 3, pointer")
        );
    }

    #[test]
    fn specificity_follows_selectors_4() {
        let spec = |s: &str| parse_complex(s).unwrap().specificity();
        // `:not()` takes its MOST SPECIFIC argument, not the sum.
        assert_eq!(spec(":not(.a, .b)"), (0, 1, 0));
        assert_eq!(spec(":not(#a, .b)"), (1, 0, 0));
        // Two separate `:not()`s still both count.
        assert_eq!(spec(":not(.a):not(.b)"), (0, 2, 0));
        // A pseudo-ELEMENT counts like a type, not a class.
        assert_eq!(spec("p::before"), (0, 0, 2));
        assert_eq!(spec(".x::after"), (0, 1, 1));
        // And the observable consequence: a later equal-specificity rule wins
        // where the old argument-summing put the :not rule ahead.
        let dom = Dom::parse_document(
            "<head><style>p:not(.q, .r){letter-spacing:1px} p.z{letter-spacing:2px}</style></head>\
             <body><p id=t class=z>x</p></body>",
        );
        assert_eq!(
            dom.computed_style(dom.get_by_id("t").unwrap(), "letter-spacing")
                .as_deref(),
            Some("2px"),
            "(0,1,1) ties → source order decides"
        );
    }

    #[test]
    fn content_concatenates_strings_and_attr() {
        // CSS2 §12.2: `content` is a concatenation of components. The old
        // single-component reader mangled `"(" attr(x) ")"`.
        let dom = Dom::parse_document(
            "<head><style>\
             .p::before{content:\"(\" attr(data-n) \")\"}\
             .c::before{content:counter(x)}\
             .ab::before{content:\"a\" \"b\"}\
             </style></head>\
             <body><span class=p data-n=42>x</span><span class=c>y</span>\
             <span class=ab>z</span></body>",
        );
        let by = |cls: &str| {
            dom.descendants(DOCUMENT)
                .into_iter()
                .find(|&i| dom.attr(i, "class") == Some(cls))
                .unwrap()
        };
        assert_eq!(
            dom.pseudo_content(by("p"), PseudoEl::Before).as_deref(),
            Some("(42)")
        );
        assert_eq!(
            dom.pseudo_content(by("c"), PseudoEl::Before),
            None,
            "counter() unsupported → whole value dropped"
        );
        assert_eq!(
            dom.pseudo_content(by("ab"), PseudoEl::Before).as_deref(),
            Some("ab")
        );
    }

    #[test]
    fn empty_generated_content_and_its_box_style_survive_serialization() {
        // The native layout arena re-parses a stylesheet-free live snapshot.
        // Preserve both halves of CSS Pseudo 4 §4.1 box generation: the
        // empty (but non-`none`) content list and the pseudo's own declarations.
        let dom = Dom::parse_document(
            r#"<head><style>
               #thumb::before{content:"";display:block;width:100%;padding-top:56.25%}
               </style></head><body><div id=thumb></div></body>"#,
        );
        let thumb = dom.get_by_id("thumb").unwrap();
        assert_eq!(
            dom.pseudo_content(thumb, PseudoEl::Before).as_deref(),
            Some("")
        );
        let html = dom.serialize(DOCUMENT);
        assert!(html.contains("data-trust-before=\"\""), "{html}");
        assert!(
            html.contains("data-trust-before-style=\"")
                && html.contains("display:block;")
                && html.contains("padding-top:56.25%;")
                && html.contains("width:100%;"),
            "{html}"
        );
    }

    #[test]
    fn logical_properties_map_to_physical() {
        // CSS Logical Properties: we render only horizontal-tb LTR, so
        // inline = left/right and block = top/bottom, exactly.
        let dom = Dom::parse_document(
            "<head><style>#m{margin-inline:auto}#p{padding-block:1em 2em}\
             #w{inline-size:50%;max-inline-size:40rem}#s{margin-inline-start:1em}</style></head>\
             <body><div id=m></div><div id=p></div><div id=w></div><div id=s></div></body>",
        );
        let g = |i: &str, p: &str| dom.computed_style(dom.get_by_id(i).unwrap(), p);
        assert_eq!(g("m", "margin-left").as_deref(), Some("auto"));
        assert_eq!(g("m", "margin-right").as_deref(), Some("auto"));
        assert_eq!(g("p", "padding-top").as_deref(), Some("1em"));
        assert_eq!(g("p", "padding-bottom").as_deref(), Some("2em"));
        assert_eq!(g("w", "width").as_deref(), Some("50%"));
        assert_eq!(g("w", "max-width").as_deref(), Some("40rem"));
        assert_eq!(g("s", "margin-left").as_deref(), Some("1em"));
    }

    #[test]
    fn media_query_range_syntax_evaluates() {
        // Media Queries L4 range form (Tailwind v4 emits these).
        let vp = (800.0, 600.0);
        assert!(media_query_matches("(width >= 40em)", vp), "640px <= 800");
        assert!(!media_query_matches("(width >= 1000px)", vp));
        assert!(media_query_matches("(width <= 1000px)", vp));
        assert!(media_query_matches("(400px <= width <= 900px)", vp));
        assert!(!media_query_matches("(400px <= width < 800px)", vp));
        assert!(media_query_matches("(height > 500px)", vp));
        assert!(media_query_matches("screen and (width < 1000px)", vp));
        assert!(
            !media_query_matches("(width >= 40em)", (0.0, 0.0)),
            "unknown viewport conservatively fails"
        );
    }

    #[test]
    fn layer_rules_apply_and_unlayered_beats_layered() {
        // css-cascade-5 §6.4: @layer bodies used to be skipped whole (a
        // Tailwind-v4-era sheet contributed NOTHING). Layered rules now
        // join the cascade; unlayered rules form the implicit FINAL layer,
        // so for normal declarations they beat any layered rule REGARDLESS
        // of specificity — the whole point of the feature.
        let dom = Dom::parse_document(
            "<head><style>
                @layer base { p { display: none } #t { letter-spacing: 9px } }
                p.up { letter-spacing: 1px }
             </style></head>
             <body><p id=t class=up>x</p></body>",
        );
        let t = dom.get_by_id("t").unwrap();
        assert!(dom.is_hidden(t), "a layered rule applies at all");
        assert_eq!(
            dom.computed_style(t, "letter-spacing").as_deref(),
            Some("1px"),
            "unlayered (0,1,0) beats layered (1,0,0): layers outrank specificity"
        );
    }

    #[test]
    fn layer_order_is_first_declaration_not_source_position() {
        // `@layer b, a;` fixes the order (b first, a second) regardless of
        // where the blocks appear; for normal declarations the LATER layer
        // wins even though its block comes first in the source.
        let dom = Dom::parse_document(
            "<head><style>
                @layer b, a;
                @layer a { .x { letter-spacing: 1px } }
                @layer b { .x { letter-spacing: 2px } }
             </style></head>
             <body><p id=t class=x>x</p></body>",
        );
        assert_eq!(
            dom.computed_style(dom.get_by_id("t").unwrap(), "letter-spacing")
                .as_deref(),
            Some("1px"),
            "layer a is later in declaration order → wins for normal"
        );
    }

    #[test]
    fn important_reverses_the_layer_order() {
        // "for important rules the declaration whose cascade layer is
        // first wins" — and the implicit unlayered layer is LAST, so
        // layered !important beats unlayered !important.
        let dom = Dom::parse_document(
            "<head><style>
                @layer a { .x { letter-spacing: 1px !important } }
                @layer b { .x { letter-spacing: 2px !important } }
                .x { letter-spacing: 3px !important }
                @layer a { .y { text-indent: 5px !important } }
                .y { text-indent: 7px }
             </style></head>
             <body><p id=t class=x>x</p><p id=u class=y>y</p></body>",
        );
        assert_eq!(
            dom.computed_style(dom.get_by_id("t").unwrap(), "letter-spacing")
                .as_deref(),
            Some("1px"),
            "earliest layer wins among important; unlayered important loses"
        );
        assert_eq!(
            dom.computed_style(dom.get_by_id("u").unwrap(), "text-indent")
                .as_deref(),
            Some("5px"),
            "importance still beats layering (important layered > normal unlayered)"
        );
    }

    #[test]
    fn nested_layers_concatenate_and_parent_direct_rules_win_normal() {
        // `@layer a { @layer b {…} }` ≡ `@layer a.b` ("nesting concatenates
        // their names"); a parent's DIRECT rules form an implicit final
        // sublayer AFTER explicit sublayers, so they win for normal.
        let dom = Dom::parse_document(
            "<head><style>
                @layer a {
                    @layer b { .x { letter-spacing: 1px } }
                    .x { letter-spacing: 2px }
                }
                @layer a.b { .y { text-indent: 4px } }
             </style></head>
             <body><p id=t class='x y'>x</p></body>",
        );
        let t = dom.get_by_id("t").unwrap();
        assert_eq!(
            dom.computed_style(t, "letter-spacing").as_deref(),
            Some("2px"),
            "parent-direct beats sublayer for normal declarations"
        );
        assert_eq!(
            dom.computed_style(t, "text-indent").as_deref(),
            Some("4px"),
            "the dotted form reaches the same nested layer"
        );
    }

    #[test]
    fn layers_compose_with_media_and_anonymous_blocks() {
        let mut dom = Dom::parse_document(
            "<head><style>
                @layer a { @media (min-width: 500px) { .m { display: none } } }
                @media (min-width: 500px) { @layer a { .n { display: none } } }
                @layer { .anon { letter-spacing: 1px } }
                @layer { .anon { letter-spacing: 2px } }
             </style></head>
             <body><p id=m class=m>m</p><p id=n class=n>n</p>
             <p id=o class=anon>o</p></body>",
        );
        dom.set_viewport_px(800.0, 600.0);
        assert!(
            dom.is_hidden(dom.get_by_id("m").unwrap()),
            "@media in @layer"
        );
        assert!(
            dom.is_hidden(dom.get_by_id("n").unwrap()),
            "@layer in @media"
        );
        // Each anonymous block is a NEW layer; the second is later → wins.
        assert_eq!(
            dom.computed_style(dom.get_by_id("o").unwrap(), "letter-spacing")
                .as_deref(),
            Some("2px"),
            "anonymous layers are distinct, later one wins"
        );
    }

    #[test]
    fn layer_names_are_scoped_per_tree() {
        // "Cascade layers are scoped to their origin and context": a shadow
        // tree's layer named `a` is independent of the document's `a` — the
        // shadow sheet's own declaration order governs inside the shadow.
        let mut dom = Dom::parse_document(
            "<head><style>@layer z, a; @layer a { .s { letter-spacing: 1px } }</style></head>
             <body><div id=host></div></body>",
        );
        let host = dom.get_by_id("host").unwrap();
        let root = dom.attach_shadow(host);
        let style = dom.create_element("style");
        let css = dom.create_text(
            "@layer a { .s { letter-spacing: 3px } } @layer z { .s { letter-spacing: 4px } }",
        );
        dom.append(style, css);
        dom.append(root, style);
        let span = dom.create_element("span");
        dom.set_attr(span, "class", "s");
        dom.append(root, span);
        // In the SHADOW scope, a is declared first, z second → z wins.
        assert_eq!(
            dom.computed_style(span, "letter-spacing").as_deref(),
            Some("4px"),
            "the shadow scope has its own layer order (z declared after a)"
        );
    }

    #[test]
    fn tailwind_shaped_layer_statement_then_blocks() {
        // The Tailwind v4 output shape: one statement declaring the order,
        // then blocks appending to each layer. utilities (declared last)
        // beats base for normal declarations, wherever the blocks sit.
        let dom = Dom::parse_document(
            "<head><style>
                @layer theme, base, components, utilities;
                @layer utilities { .u { letter-spacing: 2px } }
                @layer base { .u { letter-spacing: 1px } p { display: block } }
             </style></head>
             <body><p id=t class=u>x</p></body>",
        );
        let t = dom.get_by_id("t").unwrap();
        assert_eq!(
            dom.computed_style(t, "letter-spacing").as_deref(),
            Some("2px"),
            "utilities beats base by declared order"
        );
        assert_eq!(
            dom.computed_style(t, "display").as_deref(),
            Some("block"),
            "base-layer rules apply"
        );
    }

    #[test]
    fn selector_ident_escapes_decode_and_match() {
        // css-syntax §4.3.7 ident escapes — the Tailwind class idiom
        // (`.md\:flex` is the class `md:flex`). These rules used to fail the
        // parse entirely, dropping every responsive/state-variant rule.
        let dom = Dom::parse_document(
            "<head><style>\
             .md\\:flex { display: none }\
             .w-1\\/2 { letter-spacing: 1px }\
             .w-\\[10px\\] { letter-spacing: 2px }\
             </style></head>\
             <body><p id=a class='md:flex'>x</p>\
             <p id=b class='w-1/2'>y</p>\
             <p id=c class='w-[10px]'>z</p></body>",
        );
        assert!(
            dom.is_hidden(dom.get_by_id("a").unwrap()),
            "escaped-colon class rule applies"
        );
        assert_eq!(
            dom.computed_style(dom.get_by_id("b").unwrap(), "letter-spacing")
                .as_deref(),
            Some("1px"),
            "escaped slash"
        );
        assert_eq!(
            dom.computed_style(dom.get_by_id("c").unwrap(), "letter-spacing")
                .as_deref(),
            Some("2px"),
            "escaped brackets (arbitrary-value classes)"
        );
        // Hex escape with its whitespace terminator: `#\31 23` is id "123"
        // (that space is the escape terminator, not a combinator).
        let dom2 = Dom::parse_document("<body><p id='123'>q</p></body>");
        let sel = SelectorList::parse("#\\31 23").unwrap();
        assert_eq!(dom2.query(DOCUMENT, &sel, false).len(), 1, "hex escape");
    }

    #[test]
    fn is_and_where_match_any_argument_forgivingly() {
        // Selectors 4 §4.2–4.3: `:is()` matches any argument; arguments are
        // full COMPLEX selectors; the list is FORGIVING (an unparsable
        // argument drops individually, never killing the rule); `:matches`
        // is the pre-rename legacy alias.
        let dom = Dom::parse_document(
            "<head><style>\
             :is(.a, .b) { display: none }\
             .wrap :is(.deep, .other) { letter-spacing: 1px }\
             :is(:bogus!, .c) { letter-spacing: 3px }\
             :matches(.legacy) { letter-spacing: 4px }\
             .q:is(!!) { display: none }\
             </style></head>\
             <body><p id=a class=a>a</p><p id=b class=b>b</p>\
             <p id=c class=c>c</p>\
             <div class=wrap><span id=d class=deep>d</span></div>\
             <span id=e class=deep>outside</span>\
             <p id=f class=legacy>f</p><p id=q class=q>q</p></body>",
        );
        let g = |i: &str| dom.get_by_id(i).unwrap();
        assert!(dom.is_hidden(g("a")), ":is matches first arg");
        assert!(dom.is_hidden(g("b")), ":is matches second arg");
        assert!(!dom.is_hidden(g("c")), ".c is not in the display rule");
        assert_eq!(
            dom.computed_style(g("d"), "letter-spacing").as_deref(),
            Some("1px"),
            ":is under a descendant combinator"
        );
        assert_eq!(
            dom.computed_style(g("e"), "letter-spacing"),
            None,
            "same class outside .wrap does not match"
        );
        assert_eq!(
            dom.computed_style(g("c"), "letter-spacing").as_deref(),
            Some("3px"),
            "forgiving: the bad argument drops, .c still matches"
        );
        assert_eq!(
            dom.computed_style(g("f"), "letter-spacing").as_deref(),
            Some("4px"),
            "legacy :matches alias"
        );
        // An all-invalid group matches nothing but leaves the rule (and the
        // element) alone.
        assert!(
            !dom.is_hidden(g("q")),
            "empty forgiving list matches nothing"
        );
        // querySelector shares the engine.
        let sel = SelectorList::parse(":is(.a, .b)").unwrap();
        assert_eq!(dom.query(DOCUMENT, &sel, false).len(), 2);
    }

    #[test]
    fn is_takes_max_argument_specificity_where_takes_zero() {
        let spec = |s: &str| parse_complex(s).unwrap().specificity();
        assert_eq!(spec(":is(.a, #b)"), (1, 0, 0), ":is = most specific arg");
        assert_eq!(spec(":where(.a, #b)"), (0, 0, 0), ":where = zero");
        assert_eq!(spec("div:is(.a)"), (0, 1, 1));
        assert_eq!(spec(":is(.a .b.c)"), (0, 3, 0), "complex arg sums");
        // Observable: `p:where(#t)` (0,0,1) loses to an EARLIER `.z` (0,1,0)
        // — with :is (1,0,1) it would win. Both prove the wiring.
        let dom = Dom::parse_document(
            "<head><style>\
             .z { letter-spacing: 2px }\
             p:where(#t) { letter-spacing: 1px }\
             </style></head>\
             <body><p id=t class=z>x</p></body>",
        );
        assert_eq!(
            dom.computed_style(dom.get_by_id("t").unwrap(), "letter-spacing")
                .as_deref(),
            Some("2px"),
            ":where contributes no specificity, so .z wins"
        );
        let dom = Dom::parse_document(
            "<head><style>\
             .z { letter-spacing: 2px }\
             p:is(#t) { letter-spacing: 1px }\
             </style></head>\
             <body><p id=t class=z>x</p></body>",
        );
        assert_eq!(
            dom.computed_style(dom.get_by_id("t").unwrap(), "letter-spacing")
                .as_deref(),
            Some("1px"),
            ":is carries the #id specificity and wins"
        );
    }

    #[test]
    fn live_hover_matches_the_chain_and_not_hover_inverts() {
        // `:hover` matches the committed pointer chain (target + composed
        // ancestors) — empty at rest, so a bare `:hover` rule stays inert and
        // `:not(:hover)` keeps matching, exactly as before the feature.
        let mut dom = Dom::parse_document(
            "<head><style>\
             .row:hover{letter-spacing:2px}\
             .row:not(:hover){text-indent:1em}\
             .menu:hover .drop{display:none}\
             </style></head>\
             <body><div id=m class=menu><p id=r class=row>x</p>\
             <p id=s class=row>y</p><p id=d class=drop>z</p></div></body>",
        );
        let r = dom.get_by_id("r").unwrap();
        let s = dom.get_by_id("s").unwrap();
        let d = dom.get_by_id("d").unwrap();
        assert_eq!(
            dom.computed_style(r, "letter-spacing"),
            None,
            ":hover inert at rest"
        );
        assert_eq!(
            dom.computed_style(r, "text-indent").as_deref(),
            Some("1em"),
            ":not(:hover) true at rest"
        );
        assert!(!dom.is_hidden(d), "descendant rule inert at rest");

        // Hover r: the chain is r + ancestors (m, body, html) — so the
        // `.menu:hover .drop` descendant rule fires too; the sibling `s`
        // (not on the chain) stays at rest.
        let affected = dom.set_hover_chain(Some(r));
        assert!(affected, "display/letter-spacing rules affect the render");
        assert_eq!(
            dom.computed_style(r, "letter-spacing").as_deref(),
            Some("2px"),
            ":hover matches the chain"
        );
        assert_eq!(
            dom.computed_style(r, "text-indent"),
            None,
            ":not(:hover) inverts on the chain"
        );
        assert_eq!(
            dom.computed_style(s, "letter-spacing"),
            None,
            "the sibling is not hovered"
        );
        assert!(dom.is_hidden(d), ".menu:hover .drop applies (CSS dropdown)");

        // Clearing restores rest state.
        assert!(dom.set_hover_chain(None), "clearing restyles back");
        assert_eq!(dom.computed_style(r, "letter-spacing"), None);
        assert!(!dom.is_hidden(d));
    }

    #[test]
    fn hover_affected_check_includes_graphical_color_rules() {
        // Color is real graphical paint, so a hover that changes it must
        // invalidate retained page paint even though it does not change
        // geometry. A display-changing rule on the same page still dirties and
        // additionally changes box generation.
        let mut dom = Dom::parse_document(
            "<head><style>\
             a:hover{color:red}\
             .card:hover{display:none}\
             </style></head>\
             <body><a id=l href=x>link</a><div id=c class=card>card</div></body>",
        );
        let l = dom.get_by_id("l").unwrap();
        let c = dom.get_by_id("c").unwrap();
        // Hovering the link changes a retained foreground brush.
        let _ = dom.take_dirty();
        assert!(
            dom.set_hover_chain(Some(l)),
            "graphical color hover must request repaint"
        );
        assert!(dom.take_dirty(), "paint restyle marks the page dirty");
        assert_eq!(dom.computed_value(l, "color").as_deref(), Some("red"));
        // Hovering the card: `.card:hover{display:none}` is tracked → affected
        // + dirty, and the element actually hides.
        assert!(
            dom.set_hover_chain(Some(c)),
            "a display-flipping hover rule affects the render"
        );
        assert!(dom.take_dirty(), "affected hover move marks the page dirty");
        assert!(dom.is_hidden(c));
    }

    #[test]
    fn hover_bakes_external_style_color_for_graphical_snapshot() {
        // Ruby's House uses an external sheet with an inherited body color,
        // a link color, and a paint-only a:hover color.  A native presentation
        // DOM has no live stylesheet actor, so the live serializer must bake
        // the winning color for both states into the snapshot.
        let mut dom = Dom::parse_document(
            "<head><link rel=stylesheet href=style.css></head>\
             <body><ul><li><a id=l href=/post>post</a></li></ul></body>",
        );
        let link = dom.get_by_id("l").unwrap();
        dom.attach_external_sheets(&[(
            String::from("style.css"),
            String::from(
                "body{color:black}a{color:#FF6666;font-weight:bold}a:hover{color:#FF8888}",
            ),
        )]);
        let idle = dom.serialize_live(DOCUMENT, &std::collections::HashSet::new());
        assert!(idle.contains("color:#ff6666"), "idle link color: {idle}");
        assert!(dom.set_hover_chain(Some(link)));
        let hovered = dom.serialize_live(DOCUMENT, &std::collections::HashSet::new());
        assert!(
            hovered.contains("color:#ff8888"),
            "hovered link color: {hovered}"
        );
        assert!(
            !hovered.contains("color:#ff6666"),
            "stale link color: {hovered}"
        );
    }

    #[test]
    fn hover_invalidation_names_changed_selector_subjects() {
        let mut dom = Dom::parse_document(
            "<head><style>\
             .row:hover{letter-spacing:2px}\
             .menu:hover .drop{display:none}\
             </style></head>\
             <body><section id=m class=menu><p id=r class=row>x</p>\
             <p id=s class=row>y</p><p id=d class=drop>z</p></section></body>",
        );
        let m = dom.get_by_id("m").unwrap();
        let r = dom.get_by_id("r").unwrap();
        let s = dom.get_by_id("s").unwrap();
        let d = dom.get_by_id("d").unwrap();
        let _ = dom.take_dirty();
        let _ = dom.take_dirty_targets();

        assert!(dom.set_hover_chain(Some(r)));
        let dirty = dom.take_dirty_targets().expect("hover stays attributed");
        let nodes: FxHashSet<_> = dirty.into_iter().map(|(node, _)| node).collect();
        assert_eq!(nodes, FxHashSet::from_iter([r, d]));
        assert!(
            !nodes.contains(&m),
            "the :hover carrier is not the rule subject"
        );

        // Moving between children leaves the ancestor-hovered dropdown rule
        // matched. Only the two `.row:hover` subjects change applicability.
        assert!(dom.set_hover_chain(Some(s)));
        let dirty = dom.take_dirty_targets().expect("hover stays attributed");
        let nodes: FxHashSet<_> = dirty.into_iter().map(|(node, _)| node).collect();
        assert_eq!(nodes, FxHashSet::from_iter([r, s]));
        assert!(!nodes.contains(&d));
    }

    #[test]
    fn hover_invalidation_distinguishes_paint_from_layout_changes() {
        let mut dom = Dom::parse_document(
            "<head><style>\
             #carrier:hover + #paint{background-color:red;color:white}\
             #carrier:hover ~ #layout{display:none}\
             </style></head><body><div id=carrier>x</div>\
             <section id=paint>paint</section><section id=layout>layout</section></body>",
        );
        let carrier = dom.get_by_id("carrier").unwrap();
        let paint = dom.get_by_id("paint").unwrap();
        let layout = dom.get_by_id("layout").unwrap();
        let _ = dom.take_dirty();
        let _ = dom.take_dirty_targets();

        assert!(dom.set_hover_chain(Some(carrier)));
        let dirty: FxHashMap<_, _> = dom
            .take_dirty_targets()
            .expect("hover stays attributed")
            .into_iter()
            .collect();
        assert_eq!(dirty.get(&paint), Some(&DirtyKind::Paint));
        assert_eq!(dirty.get(&layout), Some(&DirtyKind::Attr));
    }

    #[test]
    fn hover_invalidation_tracks_relational_and_logical_selectors() {
        let mut dom = Dom::parse_document(
            "<head><style>\
             li:has(a:hover) .flyout{display:block}\
             button:not(:hover){color:gray}\
             .missing .carrier:hover .never{display:block}\
             </style></head>\
             <body><li><a id=link href=x>link</a><span id=f class=flyout>x</span></li>\
             <button id=b>button</button><div id=c class=carrier><i class=never>x</i></div>\
             </body>",
        );
        let link = dom.get_by_id("link").unwrap();
        let flyout = dom.get_by_id("f").unwrap();
        let button = dom.get_by_id("b").unwrap();
        let carrier = dom.get_by_id("c").unwrap();
        let _ = dom.take_dirty();
        let _ = dom.take_dirty_targets();

        assert!(dom.set_hover_chain(Some(link)));
        let dirty = dom
            .take_dirty_targets()
            .expect(":has hover stays attributed");
        assert!(dirty.iter().any(|(node, _)| *node == flyout));

        assert!(dom.set_hover_chain(Some(button)));
        let dirty = dom
            .take_dirty_targets()
            .expect(":not hover stays attributed");
        assert!(dirty.iter().any(|(node, _)| *node == button));

        // A carrier probe alone is insufficient: the complete selector never
        // matches, so moving onto it must not schedule any rendering work.
        assert!(dom.set_hover_chain(None));
        let _ = dom.take_dirty();
        let _ = dom.take_dirty_targets();
        assert!(!dom.set_hover_chain(Some(carrier)));
        assert!(!dom.take_dirty());
        assert_eq!(dom.take_dirty_targets(), Some(Vec::new()));
    }

    #[test]
    fn logical_hover_marks_every_possible_designated_element() {
        let dom = Dom::parse_document(
            "<head><style>:is(:hover){color:red}</style></head>\
             <body><p id=plain>target</p></body>",
        );
        let plain = dom.get_by_id("plain").unwrap();
        assert!(
            dom.hover_css_candidates().contains(&plain),
            "a logical :hover cannot omit an otherwise-unremarkable hit target"
        );
    }

    #[test]
    fn hover_follows_flat_tree_slot_ancestry() {
        let mut dom = Dom::parse_document(
            "<head><style>#host:hover{color:red}</style></head>\
             <body><x-box id=host><span id=light>slotted</span></x-box></body>",
        );
        let host = dom.get_by_id("host").unwrap();
        let light = dom.get_by_id("light").unwrap();
        let shadow = dom.attach_shadow(host);
        let style = dom.create_element("style");
        let css = dom.create_text("slot:hover{background:blue}");
        dom.append(style, css);
        dom.append(shadow, style);
        let slot = dom.create_element("slot");
        dom.append(shadow, slot);
        let _ = dom.take_dirty();
        let _ = dom.take_dirty_targets();

        assert!(dom.set_hover_chain(Some(light)));
        let dirty: FxHashSet<_> = dom
            .take_dirty_targets()
            .unwrap()
            .into_iter()
            .map(|(node, _)| node)
            .collect();
        assert!(
            dirty.contains(&slot),
            "the assigned slot is a flat ancestor"
        );
        assert!(dirty.contains(&host), "the shadow host is a flat ancestor");
    }

    #[test]
    fn hover_adds_labeled_control_without_its_ancestors() {
        let mut dom = Dom::parse_document(
            "<head><style>label:hover{color:red}#b:hover{color:red}#c:hover{color:red}</style></head>\
             <body><p><label for=c id=l><input id=a></label>\
             <span id=b><input id=c></span></p></body>",
        );
        let a = dom.get_by_id("a").unwrap();
        let b = dom.get_by_id("b").unwrap();
        let c = dom.get_by_id("c").unwrap();
        let label = dom.get_by_id("l").unwrap();
        let _ = dom.take_dirty();
        let _ = dom.take_dirty_targets();

        assert!(dom.set_hover_chain(Some(a)));
        let dirty: FxHashSet<_> = dom
            .take_dirty_targets()
            .unwrap()
            .into_iter()
            .map(|(node, _)| node)
            .collect();
        assert!(dirty.contains(&label));
        assert!(dirty.contains(&c), "HTML's labeled control also matches");
        assert!(
            !dirty.contains(&b),
            "the control is not designated, so its ancestors do not match"
        );
    }

    #[test]
    fn unsupported_pseudo_inside_not_kills_the_rule_instead_of_matching_all() {
        // `:not(:hover)` is genuinely TRUE at rest, but an UNEVALUABLE pseudo
        // (`:defined`, which we can't satisfy) must not invert into
        // always-match — that turned a targeted hide rule into
        // hide-everything. It dies instead. (`:has()`/`:lang()` are now real
        // — see `not_has_matches_elements_*` / `lang_pseudo_matches_*`.)
        let dom = Dom::parse_document(
            "<head><style>\
             .x:not(:defined){display:none}\
             .y:not(:hover){letter-spacing:2px}\
             </style></head>\
             <body><p id=a class=x>kept</p><p id=b class=y>styled</p></body>",
        );
        assert!(
            !dom.is_hidden(dom.get_by_id("a").unwrap()),
            ":defined inside :not drops the rule (fail-open)"
        );
        assert_eq!(
            dom.computed_style(dom.get_by_id("b").unwrap(), "letter-spacing")
                .as_deref(),
            Some("2px"),
            ":not(:hover) still applies at rest"
        );
    }

    #[test]
    fn has_matches_descendant_child_and_sibling_relatives() {
        // `:has()` relative selectors: bare descendant, `>` child, `+` next
        // sibling, `~` following sibling. Each rule hides only the elements
        // that satisfy the relation.
        let dom = Dom::parse_document(
            "<head><style>\
             .card:has(img){display:none}\
             .row:has(> .lead){display:none}\
             .a:has(+ .b){display:none}\
             .h:has(~ .footer){display:none}\
             </style></head>\
             <body>\
             <div id=c1 class=card><p><img src=x></p></div>\
             <div id=c2 class=card><p>no image</p></div>\
             <div id=r1 class=row><span class=lead>x</span></div>\
             <div id=r2 class=row><span><span class=lead>deep</span></span></div>\
             <div id=a1 class=a></div><div class=b></div>\
             <div id=a2 class=a></div><div class=c></div>\
             <div id=h1 class=h></div><div></div><div class=footer></div>\
             <div id=h2 class=h></div><div></div>\
             </body>",
        );
        let hidden = |id: &str| dom.is_hidden(dom.get_by_id(id).unwrap());
        assert!(hidden("c1"), "descendant :has(img) — a deep <img> counts");
        assert!(
            !hidden("c2"),
            ":has(img) does NOT match without a descendant img"
        );
        assert!(hidden("r1"), ":has(> .lead) — a direct child matches");
        assert!(!hidden("r2"), ":has(> .lead) — a GRANDCHILD .lead does not");
        assert!(
            hidden("a1"),
            ":has(+ .b) — the immediate next sibling matches"
        );
        assert!(!hidden("a2"), ":has(+ .b) — a non-.b next sibling does not");
        assert!(hidden("h1"), ":has(~ .footer) — a later sibling matches");
        assert!(!hidden("h2"), ":has(~ .footer) — none following does not");
    }

    #[test]
    fn not_has_matches_elements_without_the_descendant() {
        // Real `:has()` inside `:not()`: `.x:not(:has(img))` hides `.x` WITHOUT
        // an <img> descendant and leaves `.x` WITH one alone. This is exactly
        // the shape of chatgpt.com's `not-has-focus-visible:sr-only`
        // skip-to-content link, which `:has()` support finally hides.
        let dom = Dom::parse_document(
            "<head><style>.x:not(:has(img)){display:none}</style></head>\
             <body>\
             <p id=a class=x>no image</p>\
             <p id=b class=x>has image <img src=x></p>\
             </body>",
        );
        assert!(
            dom.is_hidden(dom.get_by_id("a").unwrap()),
            ".x with no <img> descendant is hidden"
        );
        assert!(
            !dom.is_hidden(dom.get_by_id("b").unwrap()),
            ".x WITH an <img> descendant is kept"
        );
    }

    #[test]
    fn has_specificity_and_forgiving_and_nesting() {
        // Specificity: `:has()` contributes its most specific argument
        // (Selectors 4 §17), like `:is()` — the anchoring `:scope` adds zero.
        let spec = |s: &str| parse_complex(s).unwrap().specificity();
        assert_eq!(
            spec("a:has(.b)"),
            (0, 1, 1),
            ":has(.b) = one class + the tag"
        );
        assert_eq!(
            spec("a:has(> #b)"),
            (1, 0, 1),
            ":has(> #id) = one id + the tag"
        );
        // Forgiving list: an invalid arg drops, a valid one survives.
        assert_eq!(
            spec(":has(.ok, ::before)"),
            (0, 1, 0),
            "pseudo-element arg dropped"
        );
        // Nested :has is invalid → that argument drops (forgiving), so a lone
        // `:has(:has(...))` matches nothing but the rule still parses.
        assert!(
            parse_complex(":has(:has(.x))").is_some(),
            ":has(:has()) parses (the inner arg is dropped, not fatal)"
        );
    }

    #[test]
    fn not_allows_whitespace_nested_in_a_functional_pseudo() {
        // `:not()` takes a single compound (no TOP-LEVEL combinator), but the
        // arg may nest whitespace inside a functional pseudo or an attribute
        // value — those are still one compound and must parse. The old naive
        // "reject any whitespace in the arg" guard dropped these whole rules.
        // This is the @tailwindcss/typography `prose` code-block idiom that
        // gives every `<pre>` its horizontal scroll region (HuggingFace model
        // cards, and every Tailwind-prose site): the `:where(... , ... *)`
        // list carries an inner descendant combinator.
        let hf = ".prose :where(pre):not(:where([class~=not-prose],[class~=not-prose] *))";
        let dom = Dom::parse_document(&format!(
            "<head><style>{hf}{{overflow-x:auto}}</style></head>\
             <body><div class=prose><div><pre id=t><code>x</code></pre></div>\
             <pre id=np class=not-prose>y</pre></div></body>"
        ));
        assert_eq!(
            dom.computed_value(dom.get_by_id("t").unwrap(), "overflow-x")
                .as_deref(),
            Some("auto"),
            "prose <pre> gets its scroll region (whitespace nested in :where(:not(...)))"
        );
        assert_eq!(
            dom.computed_value(dom.get_by_id("np").unwrap(), "overflow-x"),
            None,
            "a .not-prose <pre> is still excluded by the :not()"
        );

        // A genuine TOP-LEVEL combinator inside :not() must still fail the
        // whole rule (we don't support complex-selector :not) — fail-open.
        let dom2 = Dom::parse_document(
            "<head><style>p:not(.a .b){overflow-x:auto}</style></head><body><p id=t>x</p></body>",
        );
        assert_eq!(
            dom2.computed_value(dom2.get_by_id("t").unwrap(), "overflow-x"),
            None,
            ":not() with a real combinator still drops the rule"
        );

        // Whitespace inside an attribute-value :not() arg is also one compound.
        let dom3 = Dom::parse_document(
            "<head><style>p:not([title=\"a b\"]){overflow-x:auto}</style></head>\
             <body><p id=t title=\"c\">x</p><p id=x title=\"a b\">y</p></body>",
        );
        assert_eq!(
            dom3.computed_value(dom3.get_by_id("t").unwrap(), "overflow-x")
                .as_deref(),
            Some("auto"),
            ":not([title=\"a b\"]) matches an element WITHOUT that title"
        );
        assert_eq!(
            dom3.computed_value(dom3.get_by_id("x").unwrap(), "overflow-x"),
            None,
            "and excludes the element WITH that title"
        );
    }

    #[test]
    fn css_wide_keywords_resolve_in_the_cascade() {
        let dom = Dom::parse_document(
            "<head><style>\
             .w{width:200px} .wi{width:inherit} .wu{width:unset}\
             b.norm{font-weight:initial} b.rev{font-weight:revert}\
             .ta{text-align:center} .tai{text-align:initial} .tau{text-align:unset}\
             </style></head>\
             <body>\
             <div class=w><p id=wi class=wi>x</p><p id=wu class=wu>x</p></div>\
             <b id=norm class=norm>x</b><b id=rev class=rev>x</b>\
             <div class=ta><p id=tai class=tai>x</p><p id=tau class=tau>x</p>\
             <p id=plain>x</p></div>\
             </body>",
        );
        let id = |s: &str| dom.get_by_id(s).unwrap();
        assert_eq!(
            dom.computed_value(id("wi"), "width").as_deref(),
            Some("200px"),
            "inherit on a NON-inherited property takes the parent's computed value"
        );
        assert_eq!(
            dom.computed_value(id("wu"), "width"),
            None,
            "unset on a non-inherited property = initial"
        );
        assert_eq!(
            dom.computed_value(id("norm"), "font-weight"),
            None,
            "initial beats the UA origin (<b> is not bold under font-weight:initial)"
        );
        assert_eq!(
            dom.computed_value(id("rev"), "font-weight").as_deref(),
            Some("bold"),
            "revert rolls the author origin back to the UA origin"
        );
        assert_eq!(
            dom.computed_value(id("tai"), "text-align"),
            None,
            "initial on an INHERITED property stops inheritance"
        );
        assert_eq!(
            dom.computed_value(id("tau"), "text-align").as_deref(),
            Some("center"),
            "unset on an inherited property inherits"
        );
        assert_eq!(
            dom.computed_value(id("plain"), "text-align").as_deref(),
            Some("center"),
            "plain inheritance is unchanged"
        );
    }

    #[test]
    fn form_state_pseudo_classes_match_the_arena() {
        let dom = Dom::parse_document(
            "<head><style>\
             input:checked{display:none}\
             option:checked{display:none}\
             button:disabled{display:none}\
             button:enabled{letter-spacing:1px}\
             input:required{letter-spacing:2px}\
             input:optional{letter-spacing:3px}\
             input:read-only{letter-spacing:4px}\
             .ed:read-write{letter-spacing:5px}\
             input:placeholder-shown{letter-spacing:6px}\
             progress:indeterminate{display:none}\
             a:any-link{letter-spacing:7px}\
             a:link{letter-spacing:8px}\
             </style></head>\
             <body>\
             <input id=c1 type=checkbox checked><input id=c2 type=checkbox>\
             <select><option id=o1 selected>a</option><option id=o2>b</option></select>\
             <button id=b1 disabled>x</button><button id=b2>x</button>\
             <fieldset disabled><legend><button id=inlegend>x</button></legend>\
             <button id=fdis>x</button></fieldset>\
             <input id=rq type=text required>\
             <input id=ro type=text readonly>\
             <div id=ed class=ed contenteditable>x</div><div id=ned class=ed>x</div>\
             <input id=ph type=text placeholder=hi>\
             <input id=ph2 type=text placeholder=hi value=v>\
             <progress id=p1></progress><progress id=p2 value=3 max=10></progress>\
             <a id=l1 href=/x>x</a><a id=l2>x</a>\
             </body>",
        );
        let id = |s: &str| dom.get_by_id(s).unwrap();
        let hidden = |s: &str| dom.is_hidden(id(s));
        let ls = |s: &str| dom.computed_style(id(s), "letter-spacing");
        assert!(hidden("c1"), ":checked matches a checked checkbox");
        assert!(!hidden("c2"), "and not an unchecked one");
        assert!(hidden("o1"), ":checked matches a selected <option>");
        assert!(!hidden("o2"));
        assert!(hidden("b1"), ":disabled via the attribute");
        assert_eq!(ls("b2").as_deref(), Some("1px"), ":enabled");
        assert!(hidden("fdis"), "a disabled <fieldset> disables descendants");
        assert!(
            !hidden("inlegend"),
            "…but not controls in its first <legend>"
        );
        assert_eq!(ls("rq").as_deref(), Some("2px"), ":required");
        // `ro` is optional (3px) AND read-only (4px) — source order wins.
        assert_eq!(ls("ro").as_deref(), Some("4px"), ":read-only");
        assert_eq!(
            ls("ed").as_deref(),
            Some("5px"),
            "contenteditable = :read-write"
        );
        assert_eq!(ls("ned"), None, "a plain div is :read-only");
        assert_eq!(ls("ph").as_deref(), Some("6px"), ":placeholder-shown");
        assert_eq!(
            ls("ph2").as_deref(),
            Some("3px"),
            "a value hides the placeholder (falls to :optional)"
        );
        assert!(hidden("p1"), "<progress> without value is :indeterminate");
        assert!(!hidden("p2"));
        assert_eq!(
            ls("l1").as_deref(),
            Some("8px"),
            ":link/:any-link on a[href]"
        );
        assert_eq!(ls("l2"), None, "an anchor without href is no link");
    }

    #[test]
    fn checked_sibling_toggle_drives_a_menu() {
        // The pure-CSS hamburger idiom: a checkbox toggles a sibling menu.
        let dom = Dom::parse_document(
            "<head><style>#t:checked ~ #menu{display:none}</style></head>\
             <body><input id=t type=checkbox checked><nav id=menu>items</nav></body>",
        );
        assert!(dom.is_hidden(dom.get_by_id("menu").unwrap()));
        let dom2 = Dom::parse_document(
            "<head><style>#t:checked ~ #menu{display:none}</style></head>\
             <body><input id=t type=checkbox><nav id=menu>items</nav></body>",
        );
        assert!(
            !dom2.is_hidden(dom2.get_by_id("menu").unwrap()),
            ":not-yet-checked toggle leaves the menu visible"
        );
        // And `:not(:checked)` composes (checked left the never bucket).
        let dom3 = Dom::parse_document(
            "<head><style>#t:not(:checked) ~ #menu{display:none}</style></head>\
             <body><input id=t type=checkbox><nav id=menu>items</nav></body>",
        );
        assert!(dom3.is_hidden(dom3.get_by_id("menu").unwrap()));
    }

    #[test]
    fn radio_group_indeterminate_scans_the_group() {
        let dom = Dom::parse_document(
            "<head><style>input:indeterminate{display:none}</style></head>\
             <body>\
             <form><input id=r1 type=radio name=g><input id=r2 type=radio name=g></form>\
             <form><input id=r3 type=radio name=g checked>\
             <input id=r4 type=radio name=g></form>\
             </body>",
        );
        let hidden = |s: &str| dom.is_hidden(dom.get_by_id(s).unwrap());
        assert!(hidden("r1"), "no checked radio in the group");
        assert!(hidden("r2"));
        assert!(!hidden("r3"), "a checked group is determinate");
        assert!(
            !hidden("r4"),
            "grouping is per form owner — the sibling form's radios don't leak"
        );
    }

    #[test]
    fn lang_and_dir_pseudos_match_ancestors() {
        let dom = Dom::parse_document(
            "<head><style>\
             :lang(en){letter-spacing:1px}\
             :lang(fr){letter-spacing:2px}\
             :dir(rtl){display:none}\
             </style></head>\
             <body>\
             <div lang=en-US><p id=en>x</p></div>\
             <div lang=fr><p id=fr>x</p></div>\
             <div dir=rtl><p id=rtl>x</p></div>\
             <p id=plain>x</p>\
             </body>",
        );
        let id = |s: &str| dom.get_by_id(s).unwrap();
        let ls = |s: &str| dom.computed_style(id(s), "letter-spacing");
        assert_eq!(ls("en").as_deref(), Some("1px"), ":lang(en) matches en-US");
        assert_eq!(ls("fr").as_deref(), Some("2px"));
        assert!(dom.is_hidden(id("rtl")), ":dir(rtl) via the dir attribute");
        assert_eq!(
            ls("plain"),
            None,
            "no inherited lang → :lang matches nothing"
        );
        assert!(!dom.is_hidden(id("plain")), "the default direction is ltr");
    }

    #[test]
    fn nth_child_of_selector_counts_matching_siblings() {
        let dom = Dom::parse_document(
            "<head><style>li:nth-child(2 of .x){display:none}</style></head>\
             <body><ul>\
             <li id=a class=x>1</li><li id=b>skip</li>\
             <li id=c class=x>2</li><li id=d class=x>3</li>\
             </ul></body>",
        );
        let hidden = |s: &str| dom.is_hidden(dom.get_by_id(s).unwrap());
        assert!(!hidden("a"), "first .x");
        assert!(!hidden("b"), "a non-matching sibling has no ordinal");
        assert!(hidden("c"), "the SECOND .x (third child) matches");
        assert!(!hidden("d"));
        // odd of S counts within the filtered list.
        let dom2 = Dom::parse_document(
            "<head><style>li:nth-child(odd of .x){letter-spacing:1px}</style></head>\
             <body><ul>\
             <li id=a class=x>1</li><li id=b>skip</li>\
             <li id=c class=x>2</li><li id=d class=x>3</li>\
             </ul></body>",
        );
        let ls = |s: &str| dom2.computed_style(dom2.get_by_id(s).unwrap(), "letter-spacing");
        assert_eq!(ls("a").as_deref(), Some("1px"));
        assert_eq!(ls("c"), None);
        assert_eq!(ls("d").as_deref(), Some("1px"));
        assert_eq!(ls("b"), None);
    }

    #[test]
    fn unknown_at_rule_skip_is_string_aware() {
        // A `}` inside a quoted value of a SKIPPED at-rule must not close the
        // block early and desync the rest of the sheet.
        let dom = Dom::parse_document(
            "<head><style>\
             @font-face{font-family:x;descriptor:\"}\"}\
             p{letter-spacing:1px}\
             </style></head><body><p id=t>x</p></body>",
        );
        assert_eq!(
            dom.computed_style(dom.get_by_id("t").unwrap(), "letter-spacing")
                .as_deref(),
            Some("1px"),
            "the rule after the skipped at-rule survives"
        );
    }

    #[test]
    fn table_and_alignment_props_are_tracked_from_sheets() {
        // These were read by the layout but missing from PROPS, so their
        // STYLESHEET declarations were silently dropped (inline worked).
        let dom = Dom::parse_document(
            "<head><style>\
             .g{align-content:center}\
             table{table-layout:fixed;caption-side:bottom}\
             </style></head>\
             <body><div id=g class=g>x</div>\
             <table id=t><caption id=cap>c</caption><tr><td>x</td></tr></table></body>",
        );
        let id = |s: &str| dom.get_by_id(s).unwrap();
        assert_eq!(
            dom.computed_value(id("g"), "align-content").as_deref(),
            Some("center")
        );
        assert_eq!(
            dom.computed_value(id("t"), "table-layout").as_deref(),
            Some("fixed")
        );
        assert_eq!(
            dom.computed_value(id("cap"), "caption-side").as_deref(),
            Some("bottom"),
            "caption-side inherits from the table to the caption"
        );
    }

    #[test]
    fn list_style_image_is_inherited_and_shorthand_preserves_image() {
        let dom = Dom::parse_document(
            "<head><style>ul{list-style-image:url('/images/HeartDot.png')}\
             .shorthand{list-style:url('/images/HeartDot.png') inside}</style></head>\
             <body><ul id=u><li id=li>heart</li></ul>\
             <ul id=s class=shorthand><li id=sl>heart</li></ul></body>",
        );
        let id = |s: &str| dom.get_by_id(s).unwrap();
        assert_eq!(
            dom.computed_value(id("li"), "list-style-image").as_deref(),
            Some("url('/images/HeartDot.png')"),
            "list-style-image is inherited by list items"
        );
        assert_eq!(
            dom.computed_value(id("sl"), "list-style-image").as_deref(),
            Some("url('/images/HeartDot.png')"),
            "the list-style shorthand retains its image component"
        );
        assert_eq!(
            dom.computed_value(id("sl"), "list-style-position")
                .as_deref(),
            Some("inside")
        );
    }

    #[test]
    fn shorthand_resets_and_wide_keywords_expand() {
        let dom = Dom::parse_document(
            "<head><style>\
             .b{border:2px}\
             .fp{flex-grow:7} .f{flex:inherit}\
             .fw{font:italic 12px serif}\
             .gt{grid-template-columns:1fr;grid-template:none}\
             .gap{column-gap:4px;gap:8px}\
             .ff{flex-direction:column;flex-flow:row wrap}\
             .pi{place-items:center start}\
             .gta{grid-template:\"a a\" 20px \"b b\" 20px / 1fr 2fr}\
             </style></head>\
             <body><div id=b class=b>x</div>\
             <div class=fp><div id=f class=f>x</div></div>\
             <div id=fw class=fw>x</div>\
             <div id=gt class=gt>x</div>\
             <div id=gap class=gap>x</div>\
             <div id=ff class=ff>x</div>\
             <div id=pi class=pi>x</div>\
             <div id=gta class=gta>x</div>\
             </body>",
        );
        let id = |s: &str| dom.get_by_id(s).unwrap();
        let cv = |s: &str, p: &str| dom.computed_value(id(s), p);
        // `border: 2px` resets the omitted style to none (invisible, and a
        // 0 used width per §8.5.3).
        assert_eq!(cv("b", "border-top-width").as_deref(), Some("2px"));
        assert_eq!(cv("b", "border-top-style").as_deref(), Some("none"));
        // `flex: inherit` propagates the keyword to all three longhands.
        assert_eq!(cv("f", "flex-grow").as_deref(), Some("7"));
        // `font` resets omitted weight to normal.
        assert_eq!(cv("fw", "font-style").as_deref(), Some("italic"));
        assert_eq!(cv("fw", "font-weight").as_deref(), Some("normal"));
        // `grid-template: none` resets the earlier columns.
        assert_eq!(cv("gt", "grid-template-columns").as_deref(), Some("none"));
        // `gap` expands, so the later shorthand beats the earlier longhand.
        assert_eq!(cv("gap", "column-gap").as_deref(), Some("8px"));
        assert_eq!(cv("gap", "row-gap").as_deref(), Some("8px"));
        // `flex-flow` expands likewise.
        assert_eq!(cv("ff", "flex-direction").as_deref(), Some("row"));
        assert_eq!(cv("ff", "flex-wrap").as_deref(), Some("wrap"));
        // `place-items` is space-separated align/justify.
        assert_eq!(cv("pi", "align-items").as_deref(), Some("center"));
        assert_eq!(cv("pi", "justify-items").as_deref(), Some("start"));
        // The `grid-template` areas form splits into all three longhands.
        assert_eq!(
            cv("gta", "grid-template-areas").as_deref(),
            Some("\"a a\" \"b b\"")
        );
        assert_eq!(
            cv("gta", "grid-template-rows").as_deref(),
            Some("20px 20px")
        );
        assert_eq!(
            cv("gta", "grid-template-columns").as_deref(),
            Some("1fr 2fr")
        );
    }
}
