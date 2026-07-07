//! The live, scriptable document: an arena DOM built straight from
//! html5ever, mutated by page JavaScript (through js.rs), then either
//! laid out into rows directly (layout.rs) or serialized back to HTML
//! for the app to re-parse and lay out.
//!
//! Deliberately NOT rcdom: a mutable DOM can't live with rcdom's
//! Node::drop force-clearing children (see CLAUDE.md), and an arena of
//! indices gives JS a flat, GC-free handle type — wrappers hold a
//! `NodeId`, the whole arena drops with the page.

use std::borrow::Cow;
use std::cell::{Ref, RefCell};

use rustc_hash::{FxHashMap, FxHashSet};

use html5ever::interface::{ElementFlags, NodeOrText, QuirksMode, TreeSink};
use html5ever::tendril::{StrTendril, TendrilSink};
use html5ever::{Attribute, ParseOpts, QualName, ns};

pub type NodeId = usize;

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

pub struct Node {
    pub parent: Option<NodeId>,
    pub first_child: Option<NodeId>,
    pub last_child: Option<NodeId>,
    pub prev_sibling: Option<NodeId>,
    pub next_sibling: Option<NodeId>,
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
    /// `computed_value` invocations (inheritance/UA-default resolution).
    pub computed_value_calls: u64,
    /// `flow_element` entries — total element-flow visits this layout (a value
    /// far above the node count means subtrees are re-flowed, e.g. measurement).
    pub flow_visits: u64,
    /// `measure_width` entries — intrinsic-sizing passes that re-descend subtrees.
    pub measure_calls: u64,
    /// `cascaded` invocations (post-winner-map: each is a hash lookup).
    pub cascaded_calls: u64,
    /// Cumulative time building per-element cascade winner maps (one build
    /// per element per epoch — the inline-style parse + matched-decl scan).
    pub cascaded_us: u64,
}

impl CascDiag {
    const ZERO: Self = CascDiag {
        style_index_us: 0,
        style_index_builds: 0,
        rules: 0,
        computed_value_calls: 0,
        flow_visits: 0,
        measure_calls: 0,
        cascaded_calls: 0,
        cascaded_us: 0,
    };
}

/// Layout-side counters (called from `layout.rs`); no-op when diag is off.
pub fn casc_note_flow_visit() {
    casc_bump(|d| d.flow_visits += 1);
}
pub fn casc_note_measure() {
    casc_bump(|d| d.measure_calls += 1);
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
    /// The CSS-pixel viewport (`cols*cell_px`, `rows*cell_px`) used to
    /// evaluate `@media` queries when the cascade is built; `(0, 0)` = unknown
    /// (width/height queries then conservatively don't match, as if skipped).
    /// Set by `execute_js` from `PageEnv`.
    viewport_px: (u32, u32),
    /// Per-element inner-scroll state (CSSOM View `element.scrollTop`, Phase 3).
    /// Keyed by node; absent = never scrolled / not a measured scroll box.
    scroll_state: FxHashMap<NodeId, ScrollBox>,
    /// Page-initiated scroll writes `(node, top, left)` (px) since the last
    /// drain — delivered to the app as `PageEvt::Scrolled` so a pure scroll (no
    /// DOM mutation) re-windows a region WITHOUT a full re-parse/relayout.
    scroll_changes: Vec<(NodeId, f64, f64)>,
    /// Terminal cell size in px (`PageEnv.cell_px`), for the px↔row conversion
    /// when baking `data-trust-scroll-top`. Default 8×16 (the nominal).
    cell_px: (u16, u16),
    /// Incremental layout (INCREMENTAL_LAYOUT_PLAN.md): the element nodes mutated
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
    /// Elements holding hover-type listeners (computed by the actor's
    /// `hover_set` before each serialize): the serializer bakes
    /// `data-trust-hover` on them so the app can resolve a hovered cell back
    /// to an actor node. Deliberately a DEDICATED attribute — `data-trust-node`
    /// is load-bearing for incremental-layout boundary sparsity and
    /// scroll-region correlation, so hover hosts must not carry it.
    hover_hosts: std::collections::HashSet<NodeId>,
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
}

/// The kind of DOM mutation, for incremental-layout boundary mapping. See
/// `Dom::relayout_boundary`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DirtyKind {
    /// A childList or text-content change: the content INSIDE a node changed.
    Content,
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
            style_epoch: 0,
            adopted_styles: FxHashMap::default(),
            external_sheets: FxHashMap::default(),
            style_cache: RefCell::new(None),
            computed_cache: RefCell::new((u64::MAX, FxHashMap::default())),
            matched_cache: RefCell::new(NodeCache::default()),
            cascaded_cache: RefCell::new(NodeCache::default()),
            hidden_cache: RefCell::new(NodeCache::default()),
            font_cache: RefCell::new(NodeCache::default()),
            viewport_px: (0, 0),
            scroll_state: FxHashMap::default(),
            scroll_changes: Vec::new(),
            cell_px: (8, 16),
            dirty_nodes: Vec::new(),
            dirty_attributed: true,
            hover_hosts: std::collections::HashSet::new(),
            hover_chain: FxHashSet::default(),
            popover_open: FxHashSet::default(),
        };
        dom.new_node(NodeData::Document);
        dom
    }

    /// Replace the hover-host set (elements holding hover-type listeners) the
    /// serializer marks with `data-trust-hover`. Refreshed by the actor
    /// wherever the clickable set is refreshed — a pure marking input, so it
    /// deliberately does NOT touch the dirty bit or the epoch.
    pub fn set_hover_hosts(&mut self, hosts: std::collections::HashSet<NodeId>) {
        self.hover_hosts = hosts;
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

    /// The elements a render-affecting `:hover` rule could match (the style
    /// index's non-any probes) — pure-CSS hover targets like `.menu` of
    /// `.menu:hover .drop{display:block}`. They carry no listener, so the
    /// listener registry can't name them; the serializer still needs to mark
    /// them (`data-trust-hover`) or the app can never resolve a pointer cell
    /// to them and the chain never moves there. Any-element probes (`:hover`
    /// nested in logical pseudos) are skipped — marking everything is
    /// marking nothing.
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
        let probes: Vec<&HoverProbe> = idx.hover_probes.iter().filter(|p| !p.any).collect();
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

    /// Move the live `:hover` chain to `target` + its composed ancestors
    /// (`None`/stale target clears it). Returns whether the move can change
    /// the RENDER: some element whose hover state flips is a candidate for a
    /// hover-dependent rule with render-affecting declarations. Affected ⇒
    /// bump the epoch (the per-element match/cascade memos invalidate lazily)
    /// and mark dirty-UNATTRIBUTED via `touch` — a chain change can restyle
    /// arbitrary descendants (`.menu:hover .dropdown`), so no incremental
    /// patch is sound. A no-op or non-affecting move (color-only `:hover`
    /// rules — the common web) costs no epoch, no dirty, no relayout.
    /// Set/clear an element's popover SHOWING state (HTML §the popover
    /// attribute). Bumps the main epoch on change — the UA hide rule and
    /// `:popover-open` both read this, so hidden/match memos must refresh —
    /// and marks the document dirty (an open/closed popover renders
    /// differently by definition). Never touches `style_epoch`: the sheet set
    /// is unchanged.
    pub fn set_popover_open(&mut self, id: NodeId, open: bool) {
        let changed = if open {
            self.popover_open.insert(id)
        } else {
            self.popover_open.remove(&id)
        };
        if changed {
            self.touch();
        }
    }

    pub fn set_hover_chain(&mut self, target: Option<NodeId>) -> bool {
        let mut chain: FxHashSet<NodeId> = FxHashSet::default();
        let mut cur = target.filter(|&t| self.is_valid(t));
        while let Some(c) = cur {
            chain.insert(c);
            cur = self.parent_composed(c);
        }
        if chain == self.hover_chain {
            return false;
        }
        let affected = {
            let idx = self.style_index();
            !idx.hover_probes.is_empty()
                && chain
                    .symmetric_difference(&self.hover_chain)
                    .any(|&e| idx.hover_probes.iter().any(|p| p.could_match(self, e)))
        };
        self.hover_chain = chain;
        if affected {
            self.touch();
        }
        affected
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
    /// current shape (the geometry box map in `js.rs`, like the cascade
    /// caches here) keys on this and rebuilds when it advances.
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

    /// An UNATTRIBUTED mutation — one we can't pin to a single element (a global
    /// stylesheet/viewport change). Forces the next render to a full relayout
    /// (no incremental patch), since it may have changed anything.
    fn touch(&mut self) {
        self.mark();
        self.dirty_attributed = false;
    }

    /// An attribute change on `id` (its own styling/box may have changed).
    fn touch_attr(&mut self, id: NodeId) {
        self.mark();
        self.dirty_nodes.push((id, DirtyKind::Attr));
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

    /// Bump the style epoch when a tree mutation involving `child` (being
    /// appended/inserted under, or detached from, `parent`) can change the
    /// sheet set: the node is — or its subtree contains — a `<style>`/
    /// `<link>` element, or it's a text node directly under a `<style>`
    /// (HTML §4.2.6: the style element's sheet re-creates when its child
    /// nodes change or it enters/leaves the document). The dominant append
    /// (a fresh leaf node) pays one tag check; only subtree attaches walk,
    /// early-exiting on the first sheet element found.
    fn note_tree_style_mutation(&mut self, parent: Option<NodeId>, child: NodeId) {
        let styled = match &self.nodes[child].data {
            NodeData::Text(_) => parent.is_some_and(|p| self.tag_name(p) == Some("style")),
            NodeData::Element { .. } | NodeData::Fragment => self.subtree_has_style(child),
            _ => false,
        };
        if styled {
            self.touch_style();
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

    /// The nearest LIVE scroll-region ancestor (the Tier-1 relayout boundary,
    /// INCREMENTAL_LAYOUT_PLAN.md §4b) a mutation at `node` is confined to —
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
            DirtyKind::Content => Some(node),
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
    /// (INCREMENTAL_LAYOUT_PLAN.md §13a): CSS2 §9.4.1 block-formatting-context
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
    /// relayout boundary (INCREMENTAL_LAYOUT_PLAN.md §13a). Unlike
    /// `relayout_boundary` (which only finds an app-confirmed live scroll
    /// region), this returns ANY independent-formatting-context ancestor, the
    /// boundary whose interior the app will re-lay and splice once the general
    /// `Doc.rows` splice lands (plan §13c step 4). Until then it drives only the
    /// diagnostic (`confined_boundaries`) so a live page reveals which boundaries
    /// the splice must handle. An `Attr` change may move the node's OWN box, so we
    /// start the walk at its parent; a `Content` change is contained, so `node`
    /// itself may be the boundary.
    pub fn relayout_boundary_general(&self, node: NodeId, kind: DirtyKind) -> Option<NodeId> {
        let mut cur = match kind {
            DirtyKind::Content => Some(node),
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
    /// (INCREMENTAL_LAYOUT_PLAN.md §14). A mutation is contained by EVERY IFC
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
            DirtyKind::Content => Some(node),
            DirtyKind::Attr => self.parent_composed(node),
        };
        while let Some(c) = cur {
            if cached.contains(&c) && self.establishes_independent_formatting_context(c) {
                return Some(c);
            }
            cur = self.parent_composed(c);
        }
        None
    }

    /// A content hash of `id`'s subtree (tag names, attributes, and text, in
    /// document order) — the cache key for incremental region layout
    /// (INCREMENTAL_LAYOUT_PLAN.md §14, per-child memoization). Two subtrees that
    /// serialize identically hash identically, so an UNCHANGED chat message reuses
    /// its laid rows across the per-message re-parse while a new/edited one is a
    /// cache miss and re-laid. Over-conservative on purpose: it covers EVERY
    /// attribute (one the layout ignores still busts the key), so a hit can never
    /// reuse rows for layout-different content — a miss only costs a re-lay. Walks
    /// the same `first_child`/`next_sibling` order the block flow lays children.
    pub fn subtree_layout_hash(&self, id: NodeId) -> u64 {
        use std::hash::Hasher;
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.hash_subtree(id, &mut h);
        h.finish()
    }

    fn hash_subtree(&self, id: NodeId, h: &mut impl std::hash::Hasher) {
        use std::hash::Hash;
        match &self.nodes[id].data {
            NodeData::Text(t) => {
                h.write_u8(0);
                t.hash(h);
            }
            NodeData::Element { name, attrs, .. } => {
                h.write_u8(1);
                (*name.local).hash(h);
                for a in attrs {
                    (*a.name.local).hash(h);
                    (*a.value).hash(h);
                }
                h.write_u8(2); // open-children delimiter
                let mut c = self.nodes[id].first_child;
                while let Some(cid) = c {
                    self.hash_subtree(cid, h);
                    c = self.nodes[cid].next_sibling;
                }
                h.write_u8(3); // close-children delimiter
            }
            // Comment/doctype/fragment/document: structural marker only (they
            // contribute no laid content).
            _ => h.write_u8(4),
        }
    }

    /// Serialize a relayout boundary's subtree as a self-contained fragment for
    /// an incremental patch (INCREMENTAL_LAYOUT_PLAN.md §4a). The boundary is
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

    /// Set the CSS-pixel viewport (`cols*cell_px`, `rows*cell_px`) that
    /// `@media` queries evaluate against. Invalidates the cascade cache when
    /// it changes so breakpoint-gated rules re-resolve.
    pub fn set_viewport_px(&mut self, width: u32, height: u32) {
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
        media_query_matches(query, self.viewport_px)
    }

    /// Set the terminal cell pixel size (`PageEnv.cell_px`) — used to convert the
    /// px `scrollTop` to rows when the live serializer bakes
    /// `data-trust-scroll-top`. No `touch()`: it only affects the next bake.
    pub fn set_cell_px(&mut self, w: u16, h: u16) {
        self.cell_px = (w.max(1), h.max(1));
    }

    /// Read a scroll metric (CSSOM View, px). `which`: 0=scrollTop, 1=scrollLeft,
    /// 4=clientHeight, 5=clientWidth. Position (0/1) defaults to 0; the clip box
    /// (4/5) is `None` until the app has pushed it (`set_scroll_geom`), so the
    /// getter falls back to the element's rect. `scrollHeight`/`scrollWidth`
    /// (2/3) are deliberately ALWAYS `None` here — they read the actor's fresh
    /// `__dom_rect` (current content) instead, never a lagging pushed value.
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

    /// Set a scroll position (px). The caller (the `scrollTop` setter) has
    /// already clamped to `[0, scrollHeight − clientHeight]` per CSSOM View.
    /// `record` (a page-JS write) queues a `Scrolled` delivery so the app
    /// re-windows the region cheaply; the wheel write-back passes `record=false`
    /// (the app already moved its own voffset). NEVER sets the dirty bit — a
    /// scroll paints no content of its own; the position rides the next serialize
    /// (baked) and the `Scrolled` channel.
    pub fn set_scroll_pos(&mut self, id: NodeId, top: f64, left: f64, record: bool) {
        let sb = self.scroll_state.entry(id).or_default();
        let changed = sb.top != top || sb.left != left;
        sb.top = top;
        sb.left = left;
        // Only an element the app has MEASURED as a scroll region (geometry
        // present) can move a window — record just those for the cheap path.
        if record && changed && sb.client_h.is_some() {
            self.scroll_changes.push((id, top, left));
        }
    }

    /// Store the app-measured CLIP box (px) for a scroll region — the viewport
    /// height/width the `clientHeight`/`clientWidth` getters report. (Pure
    /// measurement backing: no dirty, no scroll record. `scrollHeight` is NOT
    /// stored — it reads the fresh `__dom_rect`; see `ScrollBox`.)
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
            data,
        });
        self.nodes.len() - 1
    }

    pub fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id]
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
        self.touch_content(Some(parent));
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
        self.touch_content(Some(parent));
    }

    /// Append text, merging into a trailing text node like a parser would.
    pub fn append_text(&mut self, parent: NodeId, text: &str) {
        if let Some(last) = self.nodes[parent].last_child
            && let NodeData::Text(existing) = &mut self.nodes[last].data
        {
            existing.push_str(text);
            if self.tag_name(parent) == Some("style") {
                self.touch_style(); // the sheet's text grew
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
                self.touch_style();
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
                    self.touch_style();
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
        // UA rule `[popover]:not(:popover-open) { display:none }` (HTML §the
        // popover attribute): a popover renders only while SHOWN
        // (`showPopover()` / a `popovertarget` button). Same origin ordering
        // as the dialog rule above — an author `display` (a tooltip lib's
        // inline `display:flex`) wins over the UA sheet.
        if !self.popover_open.contains(&id)
            && self.attr(id, "popover").is_some()
            && self.cascaded(id, "display").is_none()
        {
            return true;
        }
        // `display:none` generates NO box (the element and subtree occupy no
        // space). `visibility:hidden` is NOT here — like `opacity:0` it is
        // paint suppression (laid out, painted blank), routed through
        // `visibility_hidden`/`Ctx.invisible`, so a `visibility:hidden` element
        // keeps its box (CSS2 §11.2) and a `visibility:visible` descendant of it
        // is still painted.
        if self.cascaded(id, "display").as_deref() == Some("none") {
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
        ) && (self
            .cascaded(id, "left")
            .as_deref()
            .is_some_and(css_len_offscreen_neg)
            || self
                .cascaded(id, "top")
                .as_deref()
                .is_some_and(css_len_offscreen_neg))
        {
            return true;
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

    /// Whether the element's own PAINT is suppressed by `opacity:0` (effective
    /// opacity below `OPACITY_HIDDEN`). Unlike `is_hidden` (box generation),
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
            && self.effective_opacity(id) < OPACITY_HIDDEN
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
            Some("hidden" | "collapse")
        )
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
    fn effective_opacity(&self, id: NodeId) -> f32 {
        let base = self
            .cascaded(id, "opacity")
            .as_deref()
            .and_then(parse_alpha)
            .unwrap_or(1.0);
        // Only a near-invisible base is worth the animation lookup; a normally
        // opaque (or merely faded) element shows as-is.
        if base >= OPACITY_HIDDEN {
            return base;
        }
        for (name, fill) in self.animations_of(id) {
            if matches!(fill.as_deref(), Some("forwards" | "both"))
                && let Some(&end) = self.style_index().keyframes.get(&name)
            {
                return end;
            }
        }
        base
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

    /// The cascaded `display` value for an element (the mini-cascade
    /// winner), or `None` when no rule sets it. `hidden` attribute counts
    /// as `display:none`. Drives block/inline flow in the layout pass and
    /// is baked into the serialized HTML so the re-parsed layout arena
    /// sees the same computed display the engine did.
    pub fn computed_display(&self, id: NodeId) -> Option<String> {
        if self.attr(id, "hidden").is_some() {
            return Some("none".to_string());
        }
        self.cascaded(id, "display")
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

    /// Whether the subtree rooted at `root` (inclusive) paints any cell in our
    /// renderer: any non-whitespace text, any replaced/control/marker element,
    /// any `::before`/`::after` generated content, or (borders on) a drawn
    /// border. We render no color/background, so a plain container with none of
    /// those paints nothing. Early-exits on the first painting node. Generic
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
        let borders = crate::layout::borders_enabled();
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
                    if borders && self.has_drawn_border(id) {
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
                .and_then(|v| crate::layout::css_length_px(v, crate::layout::Units::of(self, id)))
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
        let u = crate::layout::Units::of(self, id);
        let Some(width_px) = self
            .computed_value_resolved(id, "width")
            .and_then(|v| crate::layout::css_length_px(&v, u))
        else {
            return false;
        };
        // A cell ≈ one unit of display width.
        crate::layout::display_width(label) as f32 > width_px / u.cell_w
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
    /// specified value (author cascade, else the UA default). Memoized per
    /// epoch because the layout reads it per element. getComputedStyle and
    /// the layout's inherited-text reads both go through here, so a property
    /// inherits everywhere by being marked `inherited` once.
    pub fn computed_value(&self, id: NodeId, name: &str) -> Option<String> {
        casc_bump(|d| d.computed_value_calls += 1);
        let Some(idx) = prop_index(name) else {
            // Untracked: no UA default, no inheritance — author cascade.
            return self.cascaded(id, name);
        };
        if !PROPS[idx].inherited {
            return self.specified(id, name);
        }
        if let Some(hit) = self.computed_cache_get(id, idx) {
            return hit;
        }
        let v = self.specified(id, name).or_else(|| {
            self.nodes[id]
                .parent
                .and_then(|p| self.computed_value(p, name))
        });
        self.computed_cache_put(id, idx, v.clone());
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
        let parent_px = match self.nodes[id].parent {
            Some(p) if p != DOCUMENT => self.font_px(p),
            _ => FONT_SIZE_INITIAL,
        };
        // `rem` on the root element itself resolves against the initial
        // value (a self-reference otherwise, per CSS Values §6.2.1).
        let root_px = if Some(id) == self.document_element() {
            FONT_SIZE_INITIAL
        } else {
            self.root_font_px()
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

    /// The specified value: the author cascade, or the UA default for the
    /// element's tag. (Before inheritance — `computed_value` adds that.)
    fn specified(&self, id: NodeId, name: &str) -> Option<String> {
        self.cascaded(id, name)
            .or_else(|| self.ua_default(id, name))
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
    /// `text-decoration` is not inherited but PROPAGATED — the lines paint
    /// across descendant boxes and accumulate — so this walks ancestors→self:
    /// each `<u>/<ins>` adds underline, each `<s>/<strike>/<del>` adds
    /// line-through, an author `text-decoration(-line)` adds its named lines,
    /// and `none` clears both from that point down. Replaces the layout's
    /// emphasis threading for the two decoration flags.
    pub fn text_decoration(&self, id: NodeId) -> (bool, bool) {
        let mut chain = vec![id];
        while let Some(&c) = chain.last() {
            match self.nodes[c].parent {
                Some(p) => chain.push(p),
                None => break,
            }
        }
        let (mut underline, mut strike) = (false, false);
        for &e in chain.iter().rev() {
            match self.tag_name(e) {
                Some("u" | "ins") => underline = true,
                Some("s" | "strike" | "del") => strike = true,
                _ => {}
            }
            if let Some(v) = self
                .cascaded(e, "text-decoration-line")
                .or_else(|| self.cascaded(e, "text-decoration"))
            {
                if v.split_whitespace().any(|t| t == "none") {
                    underline = false;
                    strike = false;
                } else {
                    underline |= v.contains("underline");
                    strike |= v.contains("line-through");
                }
            }
        }
        (underline, strike)
    }

    /// The author-cascade winner for one property on the element itself:
    /// one hash lookup into the element's per-epoch winner maps. Inline
    /// styles beat tree rules, `!important`/layers/specificity/source order
    /// resolved by `CascadeKey` when the maps are built.
    fn cascaded(&self, id: NodeId, prop: &str) -> Option<String> {
        casc_bump(|d| d.cascaded_calls += 1);
        self.cascaded_maps(id).elem.get(prop).cloned()
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
                    // component, so the (unlayered) encoding is inert.
                    // (Inline styles can't target a pseudo-element.)
                    consider_into(
                        &mut elem,
                        &pk,
                        (
                            important,
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
                        (*imp, false, r.layer_key(*imp), r.specificity, r.order),
                        v,
                    );
                }
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
                        (*imp, false, r.layer_key(*imp), r.specificity, r.order),
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
        if let Some(v) = self.cascaded(id, name) {
            return Some(v);
        }
        self.parent_composed(id)
            .and_then(|p| self.custom_prop(p, name))
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
        if !value.contains("var(") {
            return Some(value.to_owned());
        }
        let mut out = String::new();
        let mut rest = value;
        let mut guard = 0;
        while let Some(pos) = rest.find("var(") {
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
                VarResult::Undefined => match fallback {
                    Some(f) => out.push_str(&self.substitute_vars(id, f, active)?),
                    None => return None,
                },
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
        self.parse_content_value(id, &raw)
    }

    /// The cascade-winning value of `prop` on `id`'s `::before`/`::after`
    /// pseudo-element, or `None` if no matching rule sets it. One hash
    /// lookup into the pseudo's bucket of the element's winner maps.
    pub fn pseudo_style(&self, id: NodeId, which: PseudoEl, prop: &str) -> Option<String> {
        self.cascaded_maps(id).pseudo(which).get(prop).cloned()
    }

    /// Whether `id` carries the clearfix idiom — a `::before`/`::after`
    /// pseudo-element that `clear`s floats (`.clearfix`, Bootstrap's `.row`,
    /// `.group`, …). Such a block CONTAINS its descendant floats (the universal
    /// pre-flexbox containment pattern: `::after{content:"";clear:both}`), so
    /// the layout treats it as a block formatting context. Without it a float
    /// grid leaks past its row and the next section paints on top of it.
    pub fn has_clearing_pseudo(&self, id: NodeId) -> bool {
        // The baked marker (set by the serializer when the real CSS was still
        // in scope) — the layout re-parses without the `::after{clear}` rule.
        if self.attr(id, "data-trust-clearfix").is_some() {
            return true;
        }
        [PseudoEl::Before, PseudoEl::After].into_iter().any(|p| {
            self.pseudo_style(id, p, "clear").is_some_and(|v| {
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
        let v = raw.trim();
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
        (!out.is_empty()).then_some(out)
    }

    /// The root of a node's tree: DOCUMENT for the light DOM, the
    /// shadow fragment for shadow content. An element consults only its
    /// own scope's sheets (selector matching can't cross the boundary
    /// either — ancestor walks stop at fragment roots).
    fn tree_scope(&self, id: NodeId) -> NodeId {
        let mut cur = id;
        while let Some(p) = self.nodes[cur].parent {
            cur = p;
        }
        cur
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
                self.viewport_px,
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
                self.viewport_px,
                layer_regs.entry(*scope).or_default(),
                "",
            );
        }
        index.has_opacity = index
            .scopes
            .values()
            .flatten()
            .any(|r| r.decls.iter().any(|(k, _)| k == "opacity"));
        // The hover probes: only rules that could change what we PAINT under a
        // moved hover chain. Untracked-only declarations (color — nothing the
        // terminal renders) build no probe, so `set_hover_chain`
        // short-circuits to "unaffected" on the common color-only web.
        // Backgrounds are tracked for layout2's cell compositor (opaque
        // fills), but under the flow engine they paint nothing — a hover
        // background must not cost a relayout there. Collapses to plain
        // `is_tracked` when layout2 becomes the engine (P9).
        index.hover_probes = index
            .scopes
            .values()
            .flatten()
            .filter(|r| {
                r.decls.iter().any(|(k, _)| {
                    if k == "background-color" || k == "background-image" {
                        return crate::layout2::enabled();
                    }
                    k == "content" || k.starts_with("--") || is_tracked(k)
                })
            })
            .flat_map(hover_probes_of)
            .collect();
        // Build the rightmost-key buckets so `matched_rules` tests only
        // candidate rules per element instead of the whole scope.
        index.buckets = index
            .scopes
            .iter()
            .map(|(scope, rules)| (*scope, RuleBuckets::build(rules)))
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
        let index = self.style_index();
        let scope = self.tree_scope(id);
        let matched = match (index.scopes.get(&scope), index.buckets.get(&scope)) {
            (Some(rules), Some(b)) => {
                let mut out: Vec<u32> = Vec::new();
                let test = |dom: &Dom, ri: u32, out: &mut Vec<u32>| {
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
        self.touch_style();
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
                self.touch_style();
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
        self.touch_style();
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
        self.shadow_roots.insert(host, root);
        self.shadow_hosts.insert(root, host);
        self.touch();
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
    /// incremental layout IGNORES it (INCREMENTAL_LAYOUT_PLAN.md): the insertion
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
    /// hoists a `<slot>` transparently at the element level, this projects one
    /// level of slots directly so classification sees the assigned nodes.
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
                let assigned = self.slot_assigned_nodes(c);
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
    /// `FrameDocument`). The serializers flow this body inline so the frame's
    /// content lays out as a normal block instead of the RAWTEXT the HTML
    /// parser otherwise makes of `<iframe>` children. `None` for an empty or
    /// cross-origin (never-loaded) frame.
    pub fn frame_body(&self, id: NodeId) -> Option<NodeId> {
        let html = self
            .child_iter(id)
            .find(|&c| self.tag_name(c) == Some("html"))?;
        self.child_iter(html)
            .find(|&c| self.tag_name(c) == Some("body"))
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

    /// Whether the subtree under `id` (inclusive for a text node) contains
    /// any non-whitespace text — the allocation-free, early-exiting form of
    /// `!text_content(id).trim().is_empty()`, which built the whole
    /// concatenated string only to test it (the live serializer runs this
    /// per clickable element).
    fn subtree_has_text(&self, id: NodeId) -> bool {
        let non_ws =
            |d: &NodeData| matches!(d, NodeData::Text(t) if !t.chars().all(char::is_whitespace));
        non_ws(&self.nodes[id].data) || self.descendants(id).any(|d| non_ws(&self.nodes[d].data))
    }

    /// The terminal glyph for an icon element/subtree — the dominant web icon
    /// idiom is a Font-Awesome-style `<svg class="...fa-NAME"><use href=
    /// "#...fa-NAME"></svg>` (also `icon-NAME`/`bi-NAME`). We don't rasterize
    /// SVG (an icon-sized raster is an unreadable smear in a terminal); instead
    /// we recognize the icon by NAME and render its Unicode glyph. Scans `id`
    /// and its descendants for the first recognizable name. `None` when nothing
    /// matches (a non-icon `<svg>` — a D3 chart, a logo — stays unrendered).
    pub fn icon_glyph(&self, id: NodeId) -> Option<&'static str> {
        for n in std::iter::once(id).chain(self.descendants(id)) {
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
                if parent.is_some_and(|p| self.tag_name(p) == Some("style")) {
                    self.touch_style(); // the sheet's text changed in place
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
                self.append(frag, cc);
            }
        }
        for c in other.child_iter(id) {
            let cc = self.transplant(other, c);
            self.append(copy, cc);
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

    /// Serialize for JS consumption (`outerHTML`). Identical to `serialize`
    /// except `<template>` elements serialize WITH their content fragment as
    /// children — the HTML serialization standard ("if the node is a template
    /// element, serialize its template contents"). Frameworks that recover
    /// in-DOM template/slot markup by reading `outerHTML` (Vue 2's DOM-template
    /// compiler reads `el.outerHTML`) need the template content present; the
    /// layout/`Doc.raw` `serialize` keeps dropping it (inert, not laid out).
    pub fn serialize_js(&self, root: NodeId) -> String {
        let mut out = String::new();
        self.serialize_node_inner(root, None, true, &mut out);
        out
    }

    /// JS-facing `innerHTML`: preserves `<template>` content (single caller is
    /// `sys_inner_html`). See `serialize_js`.
    pub fn inner_html(&self, id: NodeId) -> String {
        let mut out = String::new();
        for c in self.child_iter(self.content_target(id)) {
            self.serialize_node_inner(c, None, true, &mut out);
        }
        out
    }

    /// Replace each renderable inline `<svg>` with an `<img>` whose `src` is the
    /// SVG as a `data:` URL, so the existing image pipeline decodes, sizes,
    /// caches, and silhouette-tints it — a vector icon/logo becomes a rendered
    /// glyph rather than its accessible-name text. SVG colors are NOT honored
    /// (same call as dropping HTML/CSS color); the recolor happens at encode.
    /// Non-renderable SVG (a `<use>`-only sprite instance, or a hidden
    /// `<symbol>`/`<defs>` container) is left untouched, keeping the existing
    /// icon-glyph / accessible-name fallback. Runs once per DOM build, before
    /// image-URL collection and layout. This is the first slice of inline-SVG
    /// support: a static snapshot of self-contained vector markup.
    pub fn rewrite_inline_svgs(&mut self) {
        // Materialize the candidate list first: the loop MUTATES the tree
        // (insert/detach), which can't overlap the lazy descendants walk.
        let svgs: Vec<NodeId> = self
            .descendants(DOCUMENT)
            .filter(|&id| self.tag_name(id) == Some("svg"))
            .collect();
        for id in svgs {
            if self.ancestor_is_svg(id) || !self.svg_is_renderable(id) {
                continue;
            }
            let Some(parent) = self.nodes[id].parent else {
                continue;
            };
            let mut svg = self.serialize(id);
            // resvg needs the namespace; an inline <svg> in HTML may omit it.
            if !svg.contains("xmlns") {
                svg = svg.replacen("<svg", r#"<svg xmlns="http://www.w3.org/2000/svg""#, 1);
            }
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

    /// `keep_template`: when true, `<template>` elements serialize WITH their
    /// content fragment as children (the JS/`outerHTML` path — see
    /// `serialize_js`); when false they are dropped entirely (the layout/
    /// `Doc.raw` path, where template content is inert and must not be flowed).
    fn serialize_node_inner(
        &self,
        id: NodeId,
        host: Option<NodeId>,
        keep_template: bool,
        out: &mut String,
    ) {
        match &self.nodes[id].data {
            NodeData::Document | NodeData::Fragment => {
                for c in self.child_iter(id) {
                    self.serialize_node_inner(c, host, keep_template, out);
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
            NodeData::Text(t) => out.push_str(&escape_text(t)),
            NodeData::Element { name, attrs, .. } => {
                let tag: &str = &name.local;
                // A `<template>` is dropped from the layout serializer (inert),
                // but the JS path serializes it WITH its content fragment as
                // children — handled before the is_hidden gate because a
                // template is UA `display:none` yet a browser always serializes
                // its contents.
                if tag == "template" {
                    if keep_template {
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
                if matches!(tag, "script" | "noscript" | "style") || self.is_hidden(id) {
                    return;
                }
                // An iframe/frame with realized same-origin content: flow the
                // nested document's body inline as a block, so the re-parse
                // lays it out normally instead of as the RAWTEXT the HTML
                // parser makes of <iframe> children. Empty/cross-origin frames
                // emit nothing (unchanged).
                if matches!(tag, "iframe" | "frame") {
                    if let Some(body) = self.frame_body(id) {
                        let mut kids = self.child_iter(body).peekable();
                        if kids.peek().is_some() {
                            out.push_str("<div data-trust-frame=\"\">");
                            for c in kids {
                                self.serialize_node_inner(c, host, keep_template, out);
                            }
                            out.push_str("</div>");
                        }
                    }
                    return;
                }
                // <slot> inside a shadow tree: project the host's light
                // children (or the slot's own fallback content).
                if tag == "slot"
                    && let Some(h) = host
                {
                    let assigned = self.slot_assigned(h, self.attr(id, "name"));
                    if assigned.is_empty() {
                        for c in self.child_iter(id) {
                            self.serialize_node_inner(c, host, keep_template, out);
                        }
                    } else {
                        for c in assigned {
                            self.serialize_node_inner(c, None, keep_template, out);
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
                if let Some(root) = self.shadow_root(id) {
                    for c in self.child_iter(root) {
                        self.serialize_node_inner(c, Some(id), keep_template, out);
                    }
                } else {
                    for c in self.child_iter(id) {
                        self.serialize_node_inner(c, host, keep_template, out);
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
        // iframe/frame nested-document content flows inline as a block (see the
        // static serializer + `frame_body`): scripted/`srcdoc` frame content
        // renders, RAWTEXT re-parse is avoided, empty/cross-origin frames emit
        // nothing.
        if matches!(tag, "iframe" | "frame") {
            if let Some(body) = self.frame_body(id) {
                let mut kids = self.child_iter(body).peekable();
                if kids.peek().is_some() {
                    out.push_str("<div data-trust-frame=\"\">");
                    for c in kids {
                        self.serialize_live_node(c, host, clickable, in_anchor, out);
                    }
                    out.push_str("</div>");
                }
            }
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
                for c in assigned {
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
        let wrap = is_click && !is_anchor && !in_anchor && !self.is_contenteditable_host(id);
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
            // debris in a group.)
            if !self.subtree_has_text(id) {
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
        // The app re-parses this serialized HTML into a fresh layout DOM, so
        // form controls AND vertical scroll containers need an explicit pointer
        // back to the resident page actor's original node ids (form values /
        // the region's scroll position round-trip by it).
        let is_scroll = self.is_scroll_container(id);
        // Bake the actor node id on every element the app re-correlates after a
        // re-parse: form controls + vertical scroll containers (values / region
        // scroll round-trip by it) AND every independent-formatting-context
        // boundary (INCREMENTAL_LAYOUT_PLAN.md §14 step 3). An IFC boundary is a
        // box whose interior can't reflow anything outside it, so the app can
        // re-lay ONLY that subtree and splice it back — but only if it can map the
        // patched fragment to the cached box by this id. IFC roots are SPARSE (not
        // every block), so the HTML/parse bloat stays bounded (§3).
        if matches!(tag, "form" | "input" | "button" | "select" | "textarea")
            || self.is_contenteditable_host(id)
            || is_scroll
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
        // A scroll container's current `scrollTop` SIGNAL (CSSOM View) rides the
        // HTML in ROWS so `flow_region` can re-seed the region's voffset — the
        // page's own `element.scrollTop` write (a chat pinning to the bottom)
        // survives the re-parse, exactly like a baked form value. (The box
        // GEOMETRY round-trips separately via a `PageCmd`; only the position is
        // baked, since the app already measures the geometry.)
        if is_scroll && let Some(sb) = self.scroll_state.get(&id) {
            let rows = (sb.top / f64::from(self.cell_px.1.max(1))).round();
            if rows >= 1.0 {
                out.push_str(&format!(" data-trust-scroll-top=\"{}\"", rows as i64));
            }
        }
        out.push('>');
        if !VOID_ELEMENTS.contains(&tag) {
            if let Some(root) = self.shadow_root(id) {
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
        // Bake the cascaded box/layout properties (the engine has the
        // sheets; the re-parsed layout arena doesn't) into the element's
        // inline style. `display:none` is dropped outright (never baked, see the
        // skip below); `visibility:hidden` IS kept + baked now (paint
        // suppression, Phase 2) so the re-parse paints it blank.
        let mut bake = String::new();
        for prop in PROPS.iter().filter(|p| p.baked).map(|p| p.name) {
            if let Some(v) = self.cascaded(id, prop) {
                if prop == "display" && v == "none" {
                    continue;
                }
                // Resolve `var(--x, …)` to the defined custom-property value
                // now, while the stylesheets (and so the `--x` definitions) are
                // still here — the re-parsed layout arena has neither.
                let v = self.resolve_vars(id, &v);
                // An undefined `var()` with no fallback resolves to nothing —
                // don't bake an empty declaration.
                if v.trim().is_empty() {
                    continue;
                }
                bake.push_str(prop);
                bake.push(':');
                bake.push_str(&escape_attr(&v));
                bake.push(';');
            }
        }
        // `opacity` is not a normally-baked property — a terminal has no alpha
        // compositing, so a merely-faded element (`opacity:0.5`) renders solid
        // and its raw value is irrelevant. But a PAINT-SUPPRESSED element
        // (effective opacity ~0) must survive the re-parse AS suppressed: the JS
        // pipeline re-parses this HTML with no `<style>`, so bake the resolved
        // suppression as `opacity:0`. The animation reveal is already folded into
        // `paint_suppressed` (an `animation-fill-mode:forwards` slide ending
        // opacity:1 bakes nothing and stays painted), so this can't misfire on
        // the active slide. The layout reads it back through
        // `paint_suppressed`/`Ctx.invisible` to paint the box blank while still
        // reserving its geometry (React virtualized-list placeholders).
        if self.paint_suppressed(id) {
            bake.push_str("opacity:0;");
        }
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
                out.push_str(&bake);
                style_done = true;
            } else if name == "style" {
                style_done = true;
            }
            out.push('"');
        }
        if !bake.is_empty() && !style_done {
            out.push_str(" style=\"");
            out.push_str(&bake);
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
    /// source, type-attr) — the execution schedule for js.rs.
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
            Combinator::Child => self.nodes[id]
                .parent
                .is_some_and(|p| self.matches_complex(p, rest, scope)),
            Combinator::Descendant | Combinator::None => {
                let mut up = self.nodes[id].parent;
                while let Some(a) = up {
                    if self.matches_complex(a, rest, scope) {
                        return true;
                    }
                    up = self.nodes[a].parent;
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
    /// last). `None` if it has no parent or isn't an element. One sibling
    /// pass, no Vec: count qualifying siblings and note our own ordinal.
    fn nth_position(&self, id: NodeId, of_type: bool, from_end: bool) -> Option<i32> {
        let parent = self.nodes[id].parent?;
        let my_tag = self.tag_name(id)?;
        let mut count = 0i32;
        let mut ordinal = None;
        let mut child = self.nodes[parent].first_child;
        while let Some(c) = child {
            if let Some(t) = self.tag_name(c)
                && (!of_type || t == my_tag)
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

    fn matches_structural(&self, id: NodeId, st: &Structural) -> bool {
        match st {
            Structural::Empty => self.is_element_empty(id),
            Structural::Nth {
                nth,
                of_type,
                from_end,
            } => self
                .nth_position(id, *of_type, *from_end)
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
        if c.popover_open && !self.popover_open.contains(&id) {
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
            .all(|st| self.matches_structural(id, st))
        {
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
        c.nots
            .iter()
            .flatten()
            .all(|n| !self.matches_compound(id, n, scope))
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
pub struct SelectorList(Vec<Complex>);

struct Complex(Vec<(Combinator, Compound)>);

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

/// A structural pseudo-class: a positional/childless test that depends on
/// the element's siblings, not its own attributes.
enum Structural {
    /// `:empty` — no element or non-empty text children.
    Empty,
    /// `:nth-child(An+B)` and its variants. `of_type` counts only same-tag
    /// siblings; `from_end` counts position from the last sibling.
    /// (`:first-child` = `nth(1)`, `:last-child` = `nth(1)` from end, etc.)
    Nth {
        nth: Nth,
        of_type: bool,
        from_end: bool,
    },
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
    };
    let last = |of_type| Structural::Nth {
        nth: Nth { a: 0, b: 1 },
        of_type,
        from_end: true,
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
            && !self.never
            && !self.hover
            && !self.popover_open
            && !self.scope
            && !self.root
            && !self.host
            && self.structural.is_empty()
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
        if let Some(inner) = &self.host_inner {
            let hs = inner.spec();
            s = (s.0 + hs.0, s.1 + hs.1, s.2 + hs.2);
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

impl SelectorList {
    pub fn parse(input: &str) -> Option<SelectorList> {
        let mut list = Vec::new();
        for part in split_top_level(input, ',') {
            list.push(parse_complex(part.trim())?);
        }
        if list.is_empty() {
            None
        } else {
            Some(SelectorList(list))
        }
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
                    // Step-1 :not takes compounds (no combinators) —
                    // anything fancier fails the parse (rule ignored,
                    // fail-open). Specificity comes from the argument.
                    let mut group = Vec::new();
                    for part in split_top_level(&arg?, ',') {
                        let part = part.trim();
                        if part.is_empty() || part.contains(char::is_whitespace) {
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
                    // fail-open) rather than silently mismatching.
                    let nth = parse_nth(&arg?)?;
                    compound.structural.push(Structural::Nth {
                        nth,
                        of_type,
                        from_end,
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
                } else {
                    // Valid CSS we can never satisfy: parse, count for
                    // specificity, never match. Interaction pseudos are
                    // GENUINELY false at rest (no pointer, no focus), so a
                    // `:not(:focus)` wrapping them correctly matches;
                    // anything else unsupported is flagged so `:not` rejects
                    // it rather than inverting it into always-match.
                    compound.never = true;
                    compound.never_unknown = !matches!(
                        name.as_str(),
                        "active"
                            | "focus"
                            | "focus-within"
                            | "focus-visible"
                            | "visited"
                            | "target"
                            | "checked"
                            | "disabled"
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
    /// verbatim: `opacity`/`animation*` (opacity is baked SPECIALLY — the
    /// resolved paint-suppression as `opacity:0`, see `write_attrs` — not its raw
    /// cascaded value; the animation longhands feed that resolution) and
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
/// resolves them identically (INCREMENTAL_LAYOUT_PLAN.md §4a). Keep in sync with
/// the `inherited=true` rows below. (`visibility` is inherited but a rendered
/// boundary is by definition visible, so it's a near-no-op; included for rigor.)
const INHERITED_LAYOUT_PROPS: &[&str] = &[
    "text-align",
    "font-size",
    "font-weight",
    "font-style",
    "white-space",
    "white-space-collapse",
    "text-wrap",
    "text-wrap-mode",
    "text-transform",
    "letter-spacing",
    "list-style-type",
    "list-style-position",
    "text-indent",
    "visibility",
];

const PROPS: &[PropDef] = &[
    //    name                    inherited  baked
    prop("display", false, true),
    prop("visibility", true, true),
    prop("opacity", false, false),
    prop("animation-name", false, false),
    prop("animation-fill-mode", false, false),
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
    prop("font-size", true, true),
    prop("font-weight", true, true),
    prop("font-style", true, true),
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
    prop("text-transform", true, true),
    prop("letter-spacing", true, true),
    prop("list-style-type", true, true),
    prop("list-style-position", true, true),
    prop("text-indent", true, true),
    prop("text-decoration", false, true),
    prop("text-decoration-line", false, true),
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
    // CSS Scroll Snap 1: a scroll container only card-SNAPS when it declares
    // `scroll-snap-type` (mandatory/proximity); otherwise it scrolls freely.
    // `scroll-snap-align` (on the items) is the snap-position alignment.
    prop("scroll-snap-type", false, true),
    prop("scroll-snap-align", false, true),
    prop("cursor", false, false),
    // CSS Backgrounds 3: the layout paints no color, but a declared background
    // is an OPAQUE FILL in the cell compositor (layout2 P4 — Appendix E paint
    // order: a modal's background erases the page cells under its rect).
    // `background` expands to these two in `expand_box_shorthand`.
    prop("background-color", false, true),
    prop("background-image", false, true),
    prop("position", false, true),
    // CSS Transforms 1: only the TRANSLATE functions are consumed (a paint
    // offset on out-of-flow composited boxes — `layout::translate_offset`);
    // scale/rotate/matrix stay unapplied (visual-only deviation). Baked so
    // the live-page re-parse keeps a JS-set slide-in offset.
    prop("transform", false, true),
    // CSS Transforms 2 individual transform property (the modern
    // `translate: x y`); like `transform`, any non-none value forms a
    // stacking context and a containing block for out-of-flow descendants.
    prop("translate", false, true),
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
    prop("align-items", false, true),
    prop("order", false, true),
    prop("border-top-width", false, true),
    prop("border-right-width", false, true),
    prop("border-bottom-width", false, true),
    prop("border-left-width", false, true),
    prop("border-top-style", false, true),
    prop("border-right-style", false, true),
    prop("border-bottom-style", false, true),
    prop("border-left-style", false, true),
];

fn is_tracked(name: &str) -> bool {
    // Custom properties (`--foo`) are always stored so `var()` references can
    // resolve to their defined (cascaded, inherited) value at bake time, not
    // just the fallback. They inherit and are case-folded like everything else.
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
        "button" | "input" | "select" | "textarea" | "meter" | "progress" => "inline-block",
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

/// Below this effective opacity an element's paint is suppressed (laid out but
/// painted blank — `paint_suppressed`). Keeps merely-faded content (e.g.
/// `opacity:0.5`) painted normally.
const OPACITY_HIDDEN: f32 = 0.05;

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
    if let Some((start, end)) = logical_pair(prop) {
        let toks: Vec<&str> = value.split_whitespace().collect();
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
    // `border-width`/`border-style`: 1–4 values, top/right/bottom/left.
    if let Some(kind) = prop
        .strip_prefix("border-")
        .filter(|k| *k == "width" || *k == "style")
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
    // keep the width and style (color is ignored), per side.
    if prop == "border" {
        let (w, s) = parse_border_shorthand(value);
        return border_longhands(&["top", "right", "bottom", "left"], w, s);
    }
    if let Some(side) = prop
        .strip_prefix("border-")
        .filter(|s| matches!(*s, "top" | "right" | "bottom" | "left"))
    {
        let (w, s) = parse_border_shorthand(value);
        return border_longhands(&[side], w, s);
    }
    // `grid-gap`/`grid-row-gap`/`grid-column-gap`: the deprecated aliases of
    // `gap`/`row-gap`/`column-gap` (still emitted by older toolchains and
    // GitHub's Primer). Normalize to the modern names the layout reads.
    if let Some(rest) = prop.strip_prefix("grid-")
        && matches!(rest, "gap" | "row-gap" | "column-gap")
    {
        return vec![(rest.to_string(), value.to_string())];
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
    // `grid-template: <rows> / <columns>` (the area form is ignored — we don't
    // place by name). Split on the top-level `/` into the two track lists.
    if prop == "grid-template" {
        if let Some((rows, cols)) = split_top_level_slash(value) {
            return vec![
                ("grid-template-rows".to_string(), rows.trim().to_string()),
                ("grid-template-columns".to_string(), cols.trim().to_string()),
            ];
        }
        return Vec::new();
    }
    // `flex: none | auto | <grow> [<shrink>] [<basis>] | <basis>` → the three
    // longhands, so the CASCADE resolves them by source order (a `flex-grow:0`
    // BEFORE a `flex:1` must lose to the shorthand's grow:1 — manually merging
    // shorthand-then-longhand in the layout got this backwards). `flex:<n>`
    // sets basis 0 (not auto), per the spec.
    if prop == "flex" {
        let v = value.trim();
        let (g, s, b) = match v.to_ascii_lowercase().as_str() {
            "none" => ("0", "0", "auto".to_string()),
            "auto" => ("1", "1", "auto".to_string()),
            "initial" | "" => ("0", "1", "auto".to_string()),
            _ => {
                let mut nums = Vec::new();
                let mut basis = None;
                for t in v.split_whitespace() {
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
    // line-height/family (untracked, ignored). System-font keywords
    // (`caption`, `menu`, …) expand to nothing.
    if prop == "font" {
        let mut out = Vec::new();
        for tok in value.split_whitespace() {
            let t = tok.split('/').next().unwrap_or(tok);
            match t.to_ascii_lowercase().as_str() {
                "italic" | "oblique" => out.push(("font-style".to_string(), t.to_string())),
                "bold" | "bolder" | "lighter" => {
                    out.push(("font-weight".to_string(), t.to_string()));
                }
                w if w
                    .parse::<u16>()
                    .is_ok_and(|n| (100..=900).contains(&n) && n % 100 == 0) =>
                {
                    out.push(("font-weight".to_string(), t.to_string()));
                }
                s if font_size_token(s) => {
                    out.push(("font-size".to_string(), t.to_string()));
                    break;
                }
                _ => {}
            }
        }
        return out;
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
    // `list-style: <type> || <position> || <image>` — we track the type and
    // position keywords (a bare `none` counts as the type, per the shorthand
    // grammar; the image and any URL are ignored).
    if prop == "list-style" {
        let mut out = Vec::new();
        if let Some(t) = list_style_shorthand_type(value) {
            out.push(("list-style-type".to_string(), t.to_string()));
        }
        if let Some(p) = value
            .split_whitespace()
            .find(|t| matches!(*t, "inside" | "outside"))
        {
            out.push(("list-style-position".to_string(), p.to_string()));
        }
        return out;
    }
    vec![(prop.to_string(), value.to_string())]
}

/// `background` shorthand → the two tracked longhands (see the call site).
fn expand_background(value: &str) -> Vec<(String, String)> {
    let v = value.trim();
    if v.is_empty() {
        return Vec::new();
    }
    // The raw shorthand rides along: it's untracked (sheets filter it), but
    // the inline-style path stores untracked props for getComputedStyle.
    let mut out = vec![("background".to_string(), v.to_string())];
    // CSS-wide keywords apply to every longhand of the shorthand.
    if matches!(
        v.to_ascii_lowercase().as_str(),
        "inherit" | "initial" | "unset" | "revert" | "revert-layer"
    ) {
        out.push(("background-color".to_string(), v.to_string()));
        out.push(("background-image".to_string(), v.to_string()));
        return out;
    }
    let mut color: Option<&str> = None;
    let mut image: Option<&str> = None;
    let layers = split_top_level_commas(v);
    let last = layers.len() - 1;
    for (i, layer) in layers.iter().enumerate() {
        for tok in split_value_tokens(layer) {
            let t = tok.to_ascii_lowercase();
            if image.is_none() && bg_image_token(&t) {
                image = Some(tok);
            } else if i == last && color.is_none() && bg_color_token(&t) {
                color = Some(tok);
            }
        }
    }
    out.push((
        "background-color".to_string(),
        color.unwrap_or("transparent").to_string(),
    ));
    out.push((
        "background-image".to_string(),
        image.unwrap_or("none").to_string(),
    ));
    out
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

/// The top/right/bottom/left values of a CSS 1–4-value box shorthand.
fn four_sides(value: &str) -> Option<[&str; 4]> {
    let p: Vec<&str> = value.split_whitespace().collect();
    match p.as_slice() {
        [a] => Some([a, a, a, a]),
        [a, b] => Some([a, b, a, b]),
        [a, b, c] => Some([a, b, c, b]),
        [a, b, c, d] => Some([a, b, c, d]),
        _ => None,
    }
}

/// The `(width, style)` of a `border`/`border-<side>` shorthand (color
/// dropped). Order-independent: the style keyword and a width token (`thin`/
/// `medium`/`thick` or a length) are picked out; anything else is the color.
fn parse_border_shorthand(value: &str) -> (Option<&str>, Option<&str>) {
    const STYLES: &[&str] = &[
        "none", "hidden", "solid", "dashed", "dotted", "double", "groove", "ridge", "inset",
        "outset",
    ];
    let mut width = None;
    let mut style = None;
    for tok in value.split_whitespace() {
        if STYLES.contains(&tok) {
            style = Some(tok);
        } else if tok == "thin"
            || tok == "medium"
            || tok == "thick"
            || tok.starts_with(|c: char| c.is_ascii_digit() || c == '.')
        {
            width = Some(tok);
        }
    }
    (width, style)
}

fn border_longhands(sides: &[&str], w: Option<&str>, s: Option<&str>) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for side in sides {
        if let Some(w) = w {
            out.push((format!("border-{side}-width"), w.to_string()));
        }
        if let Some(s) = s {
            out.push((format!("border-{side}-style"), s.to_string()));
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
    value.split_whitespace().find(|t| TYPES.contains(t))
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

/// (!important, inline, layer, specificity, source order): the cascade key;
/// lexicographic max wins. `layer` is the importance-adjusted cascade-layer
/// encoding (`encode_layer`); it sits AFTER the inline flag because
/// element-attached styles outrank layers (css-cascade-5 §6.1 sorts
/// "Element-Attached Styles" before "Layers"), and BEFORE specificity
/// (layers beat specificity — the point of the feature).
type CascadeKey = (bool, bool, u64, (u32, u32, u32), usize);

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
    /// `@keyframes <name>` → the animation's END opacity (the `to`/`100%`
    /// keyframe), for honoring an `animation-fill-mode:forwards` reveal/hide.
    /// Only opacity is extracted (the one keyframe property visibility needs).
    keyframes: FxHashMap<String, f32>,
    /// Whether any rule sets `opacity` at all — lets `paint_suppressed` skip
    /// the opacity cascade entirely on the overwhelming majority of pages.
    has_opacity: bool,
    /// One probe per `:hover`-bearing compound of every rule whose
    /// applicability depends on the hover chain AND whose declarations can
    /// change the RENDER (a `PROPS`-tracked property, generated `content`, or
    /// a custom property — which can feed a tracked one via `var()`).
    /// `set_hover_chain` tests the elements whose hover state flips against
    /// these to decide whether a hover move needs a restyle at all. Color-only
    /// `:hover` rules (the overwhelming majority of the web) build NO probes,
    /// so hovering across such pages is free.
    hover_probes: Vec<HoverProbe>,
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
        || c.host_inner
            .as_deref()
            .is_some_and(|h| h.hover || compound_has_nested_hover(h))
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
        let mut b = RuleBuckets::default();
        for (i, r) in rules.iter().enumerate() {
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
}

/// Parse one `prop: value [!important]` declaration. The value is
/// lowercased (keyword props), EXCEPT `content`, whose text is
/// case-significant (`content:"Read more"`).
fn parse_decl(decl: &str) -> Option<(String, String, bool)> {
    let (k, v) = decl.split_once(':')?;
    let k = k.trim().to_ascii_lowercase();
    let v = v.trim();
    let (v, important) = match v.rsplit_once('!') {
        Some((head, bang)) if bang.trim().eq_ignore_ascii_case("important") => (head, true),
        _ => (v, false),
    };
    let v = v.trim();
    let value = if k == "content" {
        v.to_string()
    } else {
        v.to_ascii_lowercase()
    };
    Some((k, value, important))
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
fn parse_sheet(
    css: &str,
    order: &mut usize,
    out: &mut Vec<StyleRule>,
    keyframes: &mut FxHashMap<String, f32>,
    viewport: (u32, u32),
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
            // `@keyframes <name> { ... }` (and the -webkit- prefix): we read
            // only the END opacity, to honor an animation that reveals/hides
            // an element via `animation-fill-mode:forwards` (slideshow fades).
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
                if let Some(end) = keyframes_end_opacity(block) {
                    keyframes.insert(name, end);
                }
                rest = tail;
                continue;
            }
            // `@media <query> { ... }`: evaluate the query against the
            // viewport and splice the matching block's rules into the cascade
            // (recurse, so nested @media and normal rules both work); drop the
            // body when it doesn't match. The viewport is what `execute_js`
            // reports (`cols*cell_px`).
            if let Some(rest_q) = lower.strip_prefix("media")
                && rest_q
                    .chars()
                    .next()
                    .is_none_or(|c| !c.is_ascii_alphanumeric() && c != '-')
                && let Some(brace_off) = after.find('{')
            {
                let query = &after[after.len() - rest_q.len()..brace_off];
                let (block, tail) = take_block(&after[brace_off..]);
                if media_query_matches(query, viewport) {
                    parse_sheet(block, order, out, keyframes, viewport, layers, layer);
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
                    parse_sheet(block, order, out, keyframes, viewport, layers, layer);
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
                    parse_sheet(block, order, out, keyframes, viewport, layers, &qualified);
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
        parse_style_rule(selector_text, block, order, out, viewport, &lpath);
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
    viewport: (u32, u32),
    layer: &[u32],
) {
    let (decl_text, nested) = split_block(block);
    let decls = collect_decls(&decl_text);
    if !decls.is_empty()
        && let Some(SelectorList(complexes)) = SelectorList::parse(resolved.trim())
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
            if (kw_ok("media") && media_query_matches(&at[5..], viewport))
                || (kw_ok("supports") && supports_condition(&at[8..]))
            {
                parse_style_rule(resolved, nblock, order, out, viewport, layer);
            }
            continue;
        }
        let child = expand_nesting(nsel, resolved);
        parse_style_rule(&child, nblock, order, out, viewport, layer);
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
    is_tracked(&prop)
}

/// Does a CSS `@media` query list match the viewport (CSS px; `0` = unknown)?
/// A comma list is OR. Within one query, conditions join with `and`; a
/// recognized media type (`screen`/`all`) and the width/height/orientation
/// features are evaluated, `not`/`only` honored. Anything unrecognized — or a
/// width/height test with an unknown viewport — makes that query NOT match,
/// which drops its rules exactly as skipping the whole `@media` block used to.
fn media_query_matches(query: &str, vp: (u32, u32)) -> bool {
    query
        .split(',')
        .any(|q| media_query_one(&q.trim().to_ascii_lowercase(), vp))
}

/// One comma-separated media query (already lowercased). A leading
/// `not`/`only` is a prefix on the whole query (not an `and`-joined part);
/// the rest is a media type and/or `and`-joined `(feature: value)` conditions.
fn media_query_one(q: &str, vp: (u32, u32)) -> bool {
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
            if !media_feature_matches(inner.trim_end_matches(')'), vp) {
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
/// `feature: value` form, or the Media Queries L4 range form
/// (`width >= 40em`, `400px <= width < 900px`).
fn media_feature_matches(inner: &str, vp: (u32, u32)) -> bool {
    let (vw, vh) = vp;
    let Some((name, value)) = inner.split_once(':') else {
        // No colon: try the L4 range syntax; a boolean feature (`(color)`)
        // or anything unrecognized still doesn't match.
        return media_range_matches(inner, vp);
    };
    let value = value.trim();
    match name.trim() {
        "min-width" => vw != 0 && media_px(value).is_some_and(|n| vw >= n),
        "max-width" => vw != 0 && media_px(value).is_some_and(|n| vw <= n),
        "width" => vw != 0 && media_px(value).is_some_and(|n| vw == n),
        "min-height" => vh != 0 && media_px(value).is_some_and(|n| vh >= n),
        "max-height" => vh != 0 && media_px(value).is_some_and(|n| vh <= n),
        "height" => vh != 0 && media_px(value).is_some_and(|n| vh == n),
        "orientation" if vw != 0 && vh != 0 => match value {
            "portrait" => vh >= vw,
            "landscape" => vw > vh,
            _ => false,
        },
        _ => false,
    }
}

/// The Media Queries L4 range syntax: `width >= 40em`, `width < 900px`,
/// `400px <= width <= 900px` (Tailwind v4 and modern sheets emit these).
/// Only `width`/`height` are evaluated; an unknown feature name, an unknown
/// viewport (0), or an unparsable form doesn't match — the same
/// conservative default as the colon form.
fn media_range_matches(inner: &str, vp: (u32, u32)) -> bool {
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
    let actual = |name: &str| -> Option<u32> {
        let v = match name {
            "width" => vp.0,
            "height" => vp.1,
            _ => return None,
        };
        (v != 0).then_some(v)
    };
    let cmp = |a: u32, op: &str, b: u32| match op {
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
        "search" | "magnifying-glass" => "🔍",
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

fn media_px(value: &str) -> Option<u32> {
    let v = value.trim();
    let split = v
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(v.len());
    let n: f32 = v[..split].parse().ok()?;
    let px = match v[split..].trim() {
        "px" | "" => n,
        "em" | "rem" => n * 16.0,
        _ => return None,
    };
    Some(px.round().max(0.0) as u32)
}

/// The opacity at an `@keyframes` animation's END — the value at the highest
/// keyframe offset (`to`/`100%`). `None` if no keyframe sets opacity. Only
/// the END matters: with `animation-fill-mode:forwards` that's the resting
/// state, so a fade-in resolves to its `to{opacity:1}` (visible) and a
/// fade-out to `to{opacity:0}` (hidden).
fn keyframes_end_opacity(block: &str) -> Option<f32> {
    let mut best: Option<(f32, f32)> = None; // (offset, opacity)
    let mut rest = block;
    while let Some(brace) = rest.find('{') {
        let sel = &rest[..brace];
        let (decls, tail) = take_block(&rest[brace..]);
        rest = tail;
        let offset = sel
            .split(',')
            .filter_map(keyframe_offset)
            .fold(f32::MIN, f32::max);
        if offset == f32::MIN {
            continue;
        }
        for decl in decls.split(';') {
            if let Some((k, v, _)) = parse_decl(decl)
                && k == "opacity"
                && let Some(o) = parse_alpha(&v)
                && best.is_none_or(|(bo, _)| offset >= bo)
            {
                best = Some((offset, o));
            }
        }
    }
    best.map(|(_, o)| o)
}

/// A keyframe selector offset as a 0..1 fraction (`from`=0, `to`=1, `N%`).
fn keyframe_offset(sel: &str) -> Option<f32> {
    match sel.trim() {
        "from" => Some(0.0),
        "to" => Some(1.0),
        s => s
            .strip_suffix('%')
            .and_then(|p| p.trim().parse::<f32>().ok())
            .map(|p| p / 100.0),
    }
}

/// One comma-separated `animation` shorthand segment → its `(name,
/// fill-mode)`: the fill keyword and the first token that isn't a
/// time/keyword are picked out; everything else (durations, easings,
/// iteration counts) is skipped.
fn parse_animation_segment(seg: &str) -> (Option<String>, Option<String>) {
    let mut name = None;
    let mut fill = None;
    for tok in seg.split_whitespace() {
        match tok {
            "forwards" | "backwards" | "both" => {
                fill.get_or_insert_with(|| tok.to_string());
            }
            _ if is_anim_keyword_or_time(tok) => {}
            _ => {
                name.get_or_insert_with(|| tok.to_string());
            }
        }
    }
    (name, fill)
}

/// Whether an `animation` shorthand token is a non-name part (a time, a
/// timing function, an iteration count, a direction/fill/play keyword) — so
/// the remaining token can be taken as the `animation-name`.
fn is_anim_keyword_or_time(tok: &str) -> bool {
    const KW: &[&str] = &[
        "none",
        "normal",
        "reverse",
        "alternate",
        "alternate-reverse",
        "infinite",
        "running",
        "paused",
        "linear",
        "ease",
        "ease-in",
        "ease-out",
        "ease-in-out",
        "step-start",
        "step-end",
    ];
    if KW.contains(&tok) {
        return true;
    }
    let num = tok
        .strip_suffix("ms")
        .or_else(|| tok.strip_suffix('s'))
        .unwrap_or(tok);
    num.parse::<f32>().is_ok()
        || tok.parse::<f32>().is_ok()
        || tok.starts_with("cubic-bezier")
        || tok.starts_with("steps")
}

/// `input` starts at '{'; return (inner text, after-the-matching-'}').
fn take_block(input: &str) -> (&str, &str) {
    let mut depth = 0i32;
    for (i, c) in input.char_indices() {
        match c {
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
// js.rs prelude wraps as CSSStyleRule/CSSMediaRule/etc. Unknown at-rules
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
        self.dom.into_inner()
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
            NodeOrText::AppendNode(n) => dom.append(*parent, n),
            NodeOrText::AppendText(t) => dom.append_text(*parent, &t),
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
        dom.append(DOCUMENT, dt);
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
            NodeOrText::AppendNode(n) => dom.insert_before(parent, n, Some(*sibling)),
            NodeOrText::AppendText(t) => {
                // Merge into the preceding text node when there is one.
                if let Some(prev) = dom.nodes[*sibling].prev_sibling
                    && let NodeData::Text(existing) = &mut dom.nodes[prev].data
                {
                    existing.push_str(&t);
                    return;
                }
                let tn = dom.create_text(&t);
                dom.insert_before(parent, tn, Some(*sibling));
            }
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
        self.dom.borrow_mut().detach(*target);
    }

    fn reparent_children(&self, node: &NodeId, new_parent: &NodeId) {
        let mut dom = self.dom.borrow_mut();
        for c in dom.children(*node) {
            dom.append(*new_parent, c);
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
    fn parses_and_serializes_a_document() {
        let dom = Dom::parse_document(
            "<html><head><title>T</title></head><body><p id=a>hi <b>there</b></p></body></html>",
        );
        let html = dom.serialize(DOCUMENT);
        assert!(html.contains("<p id=\"a\">hi <b>there</b></p>"), "{html}");
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
        dom.rewrite_inline_svgs();
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
        dom.rewrite_inline_svgs();
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
    fn text_decoration_accumulates_and_resets() {
        // Underline + line-through accumulate across nesting (each box adds
        // its line); `text-decoration:none` clears both from there down.
        let dom = Dom::parse_document(
            "<body><u id=u>under <s id=s>both</s>\
             <span id=clear style='text-decoration:none'>neither</span></u></body>",
        );
        let u = dom.get_by_id("u").unwrap();
        let s = dom.get_by_id("s").unwrap();
        let clear = dom.get_by_id("clear").unwrap();
        assert_eq!(dom.text_decoration(u), (true, false), "<u> underlines");
        assert_eq!(
            dom.text_decoration(s),
            (true, true),
            "<s> inside <u> adds strike, keeps underline"
        );
        assert_eq!(
            dom.text_decoration(clear),
            (false, false),
            "text-decoration:none clears both"
        );
    }

    #[test]
    fn relayout_boundary_finds_the_enclosing_scroll_container() {
        // INCREMENTAL_LAYOUT_PLAN.md §4b: a mutation maps to the nearest scroll
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
        // INCREMENTAL_LAYOUT_PLAN.md §13a: the boundary set is exactly the boxes
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
        // The general relayout boundary (plan §13c step 4 target) is the nearest
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
        let vp = (800, 600); // 800x600 CSS px
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
        // Unknown feature, or an unknown viewport, conservatively don't match
        // (so the rules are dropped, exactly as skipping @media used to).
        assert!(!media_query_matches("(hover: hover)", vp));
        assert!(!media_query_matches("(min-width: 768px)", (0, 0)));
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
    fn a_scroll_container_bakes_its_node_id_and_scroll_top_in_rows() {
        // The live serializer marks a vertical scroll container with a stable
        // node id AND the page's current scrollTop SIGNAL in rows, so the app's
        // `flow_region` can re-seed the region's voffset across the re-parse.
        let mut dom = Dom::parse_document(
            "<body><div id=s style='overflow-y:auto;height:96px'><p>x</p></div></body>",
        );
        dom.set_cell_px(8, 16);
        let s = dom.get_by_id("s").unwrap();
        assert!(
            dom.is_scroll_container(s),
            "overflow-y:auto is a scroll container"
        );
        // The app pushed the clip box; the page's setter clamped + stored the
        // position (here we drive the syscalls directly).
        dom.set_scroll_geom(s, 160.0, 100.0);
        dom.set_scroll_pos(s, 320.0, 0.0, true); // 320px / 16px = 20 rows
        let html = dom.serialize_live(DOCUMENT, &std::collections::HashSet::new());
        assert!(
            html.contains("data-trust-node="),
            "the scroll container carries an actor node id: {html}"
        );
        assert!(
            html.contains("data-trust-scroll-top=\"20\""),
            "the scrollTop signal is baked in rows: {html}"
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
    fn set_scroll_geom_stores_the_clip_box_and_gates_scroll_records() {
        // The app pushes the CLIP box; a page scrollTop write is recorded for the
        // cheap `Scrolled` channel only once geometry is known (the app has
        // measured this as a region). scrollHeight (`which=2`) is deliberately NOT
        // stored — it reads the fresh `__dom_rect`.
        let mut dom = Dom::parse_document("<body><div id=s style='overflow-y:auto'></div></body>");
        let s = dom.get_by_id("s").unwrap();
        // No geometry yet ⇒ a scroll write isn't recorded.
        dom.set_scroll_pos(s, 50.0, 0.0, true);
        assert!(
            dom.take_scroll_changes().is_empty(),
            "no record before the app measured the region"
        );
        dom.set_scroll_geom(s, 100.0, 80.0);
        assert_eq!(dom.scroll_metric(s, 4), Some(100.0), "clientHeight stored");
        assert_eq!(dom.scroll_metric(s, 5), Some(80.0), "clientWidth stored");
        assert_eq!(
            dom.scroll_metric(s, 2),
            None,
            "scrollHeight is read from the rect, never stored"
        );
        dom.set_scroll_pos(s, 70.0, 0.0, true);
        assert_eq!(
            dom.take_scroll_changes(),
            vec![(s, 70.0, 0.0)],
            "a scroll write records once the region is measured"
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
             <button id=icon aria-label=menu></button>\
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
        // Buttons wrapped; icon-only ones get a readable label.
        assert!(
            html.contains(&format!(
                "<a href=\"x-trust-js:{b}:\"><button id=\"b\" data-trust-node=\"{b}\">Push</button></a>"
            )),
            "{html}"
        );
        assert!(html.contains("[menu]"), "{html}");
        // A wrapped icon-only button renders the icon GLYPH as its handle (the
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
    fn serialize_live_drops_a_clipped_icon_controls_accessible_name() {
        // A control the author CLIPPED to an icon-sized box (`width` under
        // `overflow:hidden`) never paints its accessible name — only the icon.
        // The live serializer must not bracket the `aria-label` of such a
        // control: Twitch's per-message reply button (`width:3.2rem;
        // overflow:hidden`) spammed every chat line with "[Click to reply to
        // @user]". An UN-clipped icon control still surfaces its name.
        let dom = Dom::parse_document(
            "<body>\
             <button id=reply aria-label='Click to reply to @user' style='width:3.2rem;height:3.2rem;overflow:hidden'></button>\
             <button id=menu aria-label='Open menu'></button></body>",
        );
        let reply = dom.get_by_id("reply").unwrap();
        let menu = dom.get_by_id("menu").unwrap();
        let clickable = std::collections::HashSet::from([reply, menu]);
        let html = dom.serialize_live(DOCUMENT, &clickable);
        // The PAINTED form is the bracketed handle; the raw name still appears in
        // the preserved `aria-label` attribute, so assert on the bracket.
        assert!(
            !html.contains("[Click to reply"),
            "a clipped accessible name is not surfaced as a label: {html}"
        );
        assert!(
            html.contains("[Open menu]"),
            "an un-clipped icon control keeps its accessible name: {html}"
        );
    }

    #[test]
    fn serialize_live_drops_a_full_bleed_overlay_scrims_handle() {
        // A content-less full-area positioned overlay (Twitch's `<button
        // aria-label="Play" style="position:absolute;width:100%;height:100%">`
        // click-to-play scrim) paints nothing in a browser — the live serializer
        // must not give it a bracketed handle, which floated "[Play]" over the
        // player. A normal icon control keeps its name.
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
            html.contains("[Open menu]"),
            "an ordinary icon control keeps its handle: {html}"
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
        dom.set_viewport_px(800, 600);
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
             <body><div id=z class=z>a</div><div id=h class=h>b</div></body>",
        );
        assert!(
            dom.paint_suppressed(dom.get_by_id("z").unwrap()),
            "0% is invisible"
        );
        assert!(
            !dom.paint_suppressed(dom.get_by_id("h").unwrap()),
            "50% still paints"
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
        let vp = (800, 600);
        assert!(media_query_matches("(width >= 40em)", vp), "640px <= 800");
        assert!(!media_query_matches("(width >= 1000px)", vp));
        assert!(media_query_matches("(width <= 1000px)", vp));
        assert!(media_query_matches("(400px <= width <= 900px)", vp));
        assert!(!media_query_matches("(400px <= width < 800px)", vp));
        assert!(media_query_matches("(height > 500px)", vp));
        assert!(media_query_matches("screen and (width < 1000px)", vp));
        assert!(
            !media_query_matches("(width >= 40em)", (0, 0)),
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
        dom.set_viewport_px(800, 600);
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
    fn hover_affected_check_skips_color_only_rules() {
        // The efficiency answer (her call to include CSS :hover): a page whose
        // hover rules touch only UNRENDERED properties (color — not in the PROPS
        // registry) reports "unaffected" on every chain move: no epoch bump, no
        // dirty, no relayout. A tracked-property rule on the SAME page still
        // trips the probe only when a candidate element's state flips.
        // (`background` is render-affecting under layout2 — the cell compositor
        // paints opaque fills — so it is NOT a color-only property there.)
        let mut dom = Dom::parse_document(
            "<head><style>\
             a:hover{color:red}\
             .card:hover{display:none}\
             </style></head>\
             <body><a id=l href=x>link</a><div id=c class=card>card</div></body>",
        );
        let l = dom.get_by_id("l").unwrap();
        let c = dom.get_by_id("c").unwrap();
        // Hovering the link: the only rule whose probe matches (`a:hover`) is
        // color-only, so it built NO probe — unaffected, and nothing dirtied.
        let _ = dom.take_dirty();
        assert!(
            !dom.set_hover_chain(Some(l)),
            "color-only hover rules must not cost a re-render"
        );
        assert!(!dom.take_dirty(), "no dirty bit for an unrendered restyle");
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
    fn unsupported_pseudo_inside_not_kills_the_rule_instead_of_matching_all() {
        // `:not(:hover)` is genuinely TRUE at rest, but `:not(:has(img))`
        // must not invert an unsupported pseudo into always-match — that
        // turned a targeted hide rule into hide-everything. It dies instead.
        let dom = Dom::parse_document(
            "<head><style>\
             .x:not(:has(img)){display:none}\
             .y:not(:hover){letter-spacing:2px}\
             </style></head>\
             <body><p id=a class=x>kept</p><p id=b class=y>styled</p></body>",
        );
        assert!(
            !dom.is_hidden(dom.get_by_id("a").unwrap()),
            ":has inside :not drops the rule (fail-open)"
        );
        assert_eq!(
            dom.computed_style(dom.get_by_id("b").unwrap(), "letter-spacing")
                .as_deref(),
            Some("2px"),
            ":not(:hover) still applies at rest"
        );
    }
}
