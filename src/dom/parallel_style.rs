//! Parallel style computation ("phase C" of a style pass).
//!
//! An element's computed values depend on its parent's (CSS Cascade 5
//! #inheriting), so unlike selector matching they cannot all be computed
//! independently. They can be computed by runs: a participant that claims a
//! contiguous run of elements in tree order matches them (as `parallel_match`
//! does), then computes their values and typed records with the page thread's
//! own `ComputeView` code over a `Worker` backend, which keeps its own memo
//! storage and computes any ancestor outside the run itself. Duplicated work
//! is about one ancestor chain per run.
//!
//! The pass runs before a frame's style consumers: the transition update
//! (CSS Transitions 1 #starting reads computed values of every rendered
//! element) and box-tree construction (the typed records). Participants hand
//! their results over as owned data: rule lists, cascades and computed rows
//! interned per participant, records as plain values. No `Rc` crosses a
//! thread. While other runs are still being computed, the page thread wraps
//! finished results into its caches, exactly where the lazy path would have
//! put them, so invalidation and later lazy reads are unchanged.
//!
//! Pages whose cascade needs layout results or page-thread state the worker
//! does not model stay on the lazy path: container queries and units,
//! `revert-layer`, `::slotted()`, registered custom properties and running
//! transitions. Animated elements and their subtrees are not computed in the
//! pass: their rows hold animation-origin values and are private (CSS
//! Animations 1 #animations). A worker that meets any such state anyway
//! stops returning results, so those elements stay lazy too.
//! `TRUST_STYLE_VERIFY=1` recomputes every result on the lazy path and
//! reports differences.

use super::parallel_match::{
    Chunk, Participant, Published, ScopeRules, Stream, Walk, participants_for, verify_enabled, work,
};
use super::*;
use crate::layout2::value::Vp;
use crate::layout2::{BoxStyle, InlineStyle, Units};
use std::rc::Rc;
use std::time::{Duration, Instant};

/// Fewer stale elements than this are left to the lazy path.
const MIN_ELEMENTS: usize = 128;
/// Nor is a pass worth starting for less estimated work.
const MIN_WORK: Duration = Duration::from_micros(300);
/// The per-element cost assumed before a document's first pass is measured.
const FIRST_ELEMENT_COST: Duration = Duration::from_micros(4);
/// Elements per claimed run: long enough that a run's ancestor chain is a
/// small part of its work, short enough to balance participants.
const RUN: usize = 64;

/// When to attempt a pass, per arena.
#[derive(Default)]
pub(super) struct Trigger {
    /// The revision (style value epoch, page font epoch) last considered.
    attempted: Cell<Option<(u64, u64)>>,
    /// The last pass's work per element.
    measured: Cell<Option<Duration>>,
    #[cfg(test)]
    pub(super) passes: Cell<u32>,
}

/// The page thread's state that participants read. Everything here is
/// `Sync`; the tree itself is reached only through the shared view.
struct Inputs<'a> {
    view: style_view::SharedView<'a>,
    index: &'a StyleIndex,
    scopes: &'a [ScopeRules<'a>],
    style_value_epoch: u64,
    style_epoch: u64,
    epoch: u64,
    viewport_px: (f32, f32),
    device_pixel_ratio: f32,
    doc_url: Option<&'a url::Url>,
    render_clickables: &'a std::collections::HashSet<NodeId>,
    render_live: bool,
    /// The top-level document's base for presentational hints; nested
    /// documents' hints poison a worker.
    document_base: html_hints::DocumentBase,
    /// Elements whose rows hold animation-origin values.
    animated: &'a FxHashSet<NodeId>,
    vp: Vp,
    base: &'a url::Url,
}

/// One element's results as owned data. Indices refer to tables that the
/// participant extends in run order (`Run::new_*`).
struct Computed {
    cascade: Option<u32>,
    row: Option<u32>,
    node: Option<computed_cache::NodeExport>,
    font_px: Option<f32>,
    units: Option<(u64, Units)>,
    generation: Option<BoxGeneration>,
    decoration: Option<(bool, bool)>,
    custom: Option<FxHashMap<String, Option<String>>>,
}

/// One claimed run's results.
struct Run {
    matched: Chunk,
    new_cascades: Vec<CascadedMaps>,
    new_rows: Vec<computed_cache::RowExport>,
    /// Per element of the run; empty when the participant stopped computing
    /// (its elements' values stay lazy, their rule lists are still valid).
    computed: Vec<Computed>,
    busy: Duration,
}

// Results cross from participants to the page thread; nothing in them may
// be `Rc` or otherwise bound to the thread that built them.
const _: () = {
    const fn send<T: Send>() {}
    send::<Run>();
    const fn sync<T: Sync>() {}
    sync::<Inputs<'static>>();
};

/// A participant's style backend: the shared inputs plus memo storage of
/// its own, which lives as long as the participant's part in one pass.
struct Worker<'i, 'a> {
    inputs: &'i Inputs<'a>,
    matched: RefCell<NodeCache<Rc<Vec<u32>>>>,
    classes: RefCell<FxHashMap<NodeId, class_tokens::ClassTokens>>,
    computed_cache: RefCell<ComputedCache>,
    cascaded_cache: RefCell<NodeCache<Rc<CascadedMaps>>>,
    custom_prop_cache: RefCell<CustomPropCache>,
    style_sharing: RefCell<style_sharing::State>,
    hidden_cache: RefCell<NodeCache<BoxGeneration>>,
    font_cache: RefCell<NodeCache<f32>>,
    font_units_cache: RefCell<NodeCache<(u64, Units)>>,
    decoration_cache: RefCell<NodeCache<(bool, bool)>>,
    /// Registered custom properties keep the pass off, so only the
    /// resolution state (`var()` cycle detection) is used.
    properties: properties::State,
    page_support: color_scheme::PageSupport,
    /// Set when computation needed state the worker does not have.
    poisoned: Cell<bool>,
}

impl<'i, 'a> Worker<'i, 'a> {
    fn new(inputs: &'i Inputs<'a>) -> Self {
        Worker {
            inputs,
            matched: Default::default(),
            classes: Default::default(),
            computed_cache: Default::default(),
            cascaded_cache: Default::default(),
            custom_prop_cache: Default::default(),
            style_sharing: Default::default(),
            hidden_cache: Default::default(),
            font_cache: Default::default(),
            font_units_cache: Default::default(),
            decoration_cache: Default::default(),
            properties: Default::default(),
            page_support: Default::default(),
            poisoned: Cell::new(false),
        }
    }

    fn poison(&self) {
        self.poisoned.set(true);
    }
}

impl<'a> StyleBackend for Worker<'_, 'a> {
    type Index = &'a StyleIndex;

    #[inline]
    fn style_view(&self) -> StyleView<'_> {
        *self.inputs.view.view()
    }
    #[inline]
    fn style_index(&self) -> Self::Index {
        self.inputs.index
    }
    #[inline]
    fn flush_style_invalidations(&self) {}
    fn matched_rules(&self, id: NodeId) -> Rc<Vec<u32>> {
        let epoch = self.inputs.style_value_epoch;
        if let Some(hit) = self.matched.borrow().get(id, epoch) {
            return hit.clone();
        }
        // As `Dom::matched_rules` does on a selector-cache miss. Container
        // queries keep the pass off, so the matched rules are the selectors'.
        let view = self.style_view();
        let index = self.inputs.index;
        let scope = view.tree_scope(id);
        let mut out = Vec::new();
        if let (Some(rules), Some(buckets)) = (index.scopes.get(&scope), index.buckets.get(&scope))
            && view.nodes.tag_name(id).is_some()
        {
            let mut ancestors = None;
            view.match_rules(
                id,
                buckets,
                |ri| &rules[ri as usize].selector,
                view.state.shadow_hosts.get(&scope).copied(),
                ClassMemo::Local(&self.classes),
                |required| {
                    ancestors
                        .get_or_insert_with(|| rule_index::Ancestors::of(&view, id))
                        .may_match(&view, required)
                },
                &mut Vec::new(),
                &mut out,
            );
        }
        let rules = Rc::new(out);
        self.matched.borrow_mut().put(id, epoch, rules.clone());
        rules
    }
    #[inline]
    fn style_value_epoch(&self) -> u64 {
        self.inputs.style_value_epoch
    }
    #[inline]
    fn style_epoch(&self) -> u64 {
        self.inputs.style_epoch
    }
    #[inline]
    fn epoch(&self) -> u64 {
        self.inputs.epoch
    }
    #[inline]
    fn viewport_px(&self) -> (f32, f32) {
        self.inputs.viewport_px
    }
    #[inline]
    fn device_pixel_ratio(&self) -> f32 {
        self.inputs.device_pixel_ratio
    }
    #[inline]
    fn doc_url(&self) -> Option<&url::Url> {
        self.inputs.doc_url
    }
    #[inline]
    fn page_support(&self) -> &color_scheme::PageSupport {
        &self.page_support
    }
    #[inline]
    fn properties(&self) -> &properties::State {
        &self.properties
    }
    #[inline]
    fn render_clickables(&self) -> &std::collections::HashSet<NodeId> {
        self.inputs.render_clickables
    }
    #[inline]
    fn render_live(&self) -> bool {
        self.inputs.render_live
    }
    fn document_base(&self, id: NodeId) -> html_hints::DocumentBase {
        if self.style_view().frame_owner(id).is_some() {
            self.poison();
        }
        self.inputs.document_base.clone()
    }
    fn slotted_rules<'r>(&self, index: &'r StyleIndex, _: NodeId) -> Vec<&'r StyleRule> {
        if !index.slotted_rules.is_empty() {
            self.poison();
        }
        Vec::new()
    }
    fn container_matches(&self, _: &container_queries::Query, _: NodeId, _: bool) -> bool {
        self.poison();
        false
    }
    fn record_container_read(&self, _: NodeId, _: NodeId, _: u8, _: bool) {
        self.poison();
    }
    fn container_size(&self, _: NodeId) -> Option<[f32; 2]> {
        self.poison();
        None
    }
    fn flat_children(&self, id: NodeId) -> Vec<NodeId> {
        // Slot assignment state lives on the page thread.
        self.poison();
        self.child_iter(id).collect()
    }
    fn transition_value(&self, _: NodeId, _: &str) -> Option<String> {
        None
    }
    fn transition_affects(&self, _: &str) -> bool {
        false
    }
    fn css_transitions_active(&self) -> bool {
        false
    }
    fn animation_value(&self, id: NodeId, _: &str) -> Option<String> {
        if self.inputs.animated.contains(&id) {
            self.poison();
        }
        None
    }
    fn animation_private_row(&self, id: NodeId) -> bool {
        if self.inputs.animated.contains(&id) {
            self.poison();
        }
        false
    }
    #[inline]
    fn computed_cache(&self) -> &RefCell<ComputedCache> {
        &self.computed_cache
    }
    #[inline]
    fn cascaded_cache(&self) -> &RefCell<NodeCache<Rc<CascadedMaps>>> {
        &self.cascaded_cache
    }
    #[inline]
    fn custom_prop_cache(&self) -> &RefCell<CustomPropCache> {
        &self.custom_prop_cache
    }
    #[inline]
    fn style_sharing(&self) -> &RefCell<style_sharing::State> {
        &self.style_sharing
    }
    #[inline]
    fn hidden_cache(&self) -> &RefCell<NodeCache<BoxGeneration>> {
        &self.hidden_cache
    }
    #[inline]
    fn font_cache(&self) -> &RefCell<NodeCache<f32>> {
        &self.font_cache
    }
    #[inline]
    fn font_units_cache(&self) -> &RefCell<NodeCache<(u64, Units)>> {
        &self.font_units_cache
    }
    #[inline]
    fn decoration_cache(&self) -> &RefCell<NodeCache<(bool, bool)>> {
        &self.decoration_cache
    }
}

/// A participant's part of one pass: its backend, its matcher, and the
/// tables that its results' indices refer to.
struct Part<'p, 'i, 'a> {
    worker: Worker<'i, 'a>,
    matcher: Participant<'p, 'a, 'a>,
    /// The matcher's interned rule lists, by index.
    lists: Vec<Rc<Vec<u32>>>,
    cascades: FxHashMap<*const CascadedMaps, u32>,
    rows: FxHashMap<*const RefCell<computed_cache::Row>, u32>,
    /// Keeps interned allocations alive, so their addresses stay unique.
    pinned_cascades: Vec<Rc<CascadedMaps>>,
    pinned_rows: Vec<computed_cache::SharedRow>,
    /// Elements' inline formatting context, derived from the root.
    inline: FxHashMap<NodeId, InlineStyle>,
    /// Whether an element and all its ancestors generate boxes.
    rendered: FxHashMap<NodeId, bool>,
}

impl<'p, 'i, 'a> Part<'p, 'i, 'a> {
    fn new(index: usize, inputs: &'i Inputs<'a>, view: &'p StyleView<'a>) -> Self {
        Part {
            worker: Worker::new(inputs),
            matcher: Participant::new(index, view, inputs.scopes),
            lists: Vec::new(),
            cascades: FxHashMap::default(),
            rows: FxHashMap::default(),
            pinned_cascades: Vec::new(),
            pinned_rows: Vec::new(),
            inline: FxHashMap::default(),
            rendered: FxHashMap::default(),
        }
    }

    fn run(&mut self, stream: &Stream<Run>, range: std::ops::Range<usize>) -> Run {
        let began = Instant::now();
        let matched = self.matcher.match_chunk(stream, range.clone());
        self.lists
            .extend(matched.new_lists.iter().map(|list| Rc::new(list.to_vec())));
        let epoch = self.worker.inputs.style_value_epoch;
        {
            let mut memo = self.worker.matched.borrow_mut();
            for (offset, &list) in matched.lists.iter().enumerate() {
                let (id, _) = stream.entry(range.start + offset);
                memo.put(id, epoch, self.lists[list as usize].clone());
            }
        }
        let mut run = Run {
            matched,
            new_cascades: Vec::new(),
            new_rows: Vec::new(),
            computed: Vec::new(),
            busy: Duration::ZERO,
        };
        for index in range.clone() {
            if self.worker.poisoned.get() {
                break;
            }
            self.compute(stream.entry(index).0);
        }
        if !self.worker.poisoned.get() {
            run.computed = range
                .map(|index| self.export(stream.entry(index).0, &mut run))
                .collect();
        }
        run.busy = began.elapsed();
        run
    }

    fn compute(&mut self, id: NodeId) {
        let inputs = self.worker.inputs;
        compute_element(
            ComputeView(&self.worker),
            id,
            inputs.vp,
            inputs.base,
            &mut self.rendered,
            &mut self.inline,
        );
    }

    /// `id`'s memoized state as owned data, interning its cascade and row.
    fn export(&mut self, id: NodeId, run: &mut Run) -> Computed {
        let worker = &self.worker;
        let inputs = worker.inputs;
        let style_value_epoch = inputs.style_value_epoch;
        let cascade = worker
            .cascaded_cache
            .borrow()
            .get(id, style_value_epoch)
            .cloned()
            .map(|maps| {
                *self.cascades.entry(Rc::as_ptr(&maps)).or_insert_with(|| {
                    run.new_cascades.push((*maps).clone());
                    self.pinned_cascades.push(maps);
                    (self.pinned_cascades.len() - 1) as u32
                })
            });
        let (row, node) = {
            let cache = worker.computed_cache.borrow();
            let row = cache.1.row(id).map(|row| {
                *self.rows.entry(Rc::as_ptr(&row)).or_insert_with(|| {
                    run.new_rows.push(computed_cache::Values::export_row(&row));
                    self.pinned_rows.push(row);
                    (self.pinned_rows.len() - 1) as u32
                })
            });
            (row, cache.1.export_node(id))
        };
        let custom = {
            let cache = worker.custom_prop_cache.borrow();
            cache.1.get(&id).cloned()
        };
        Computed {
            cascade,
            row,
            node,
            font_px: worker
                .font_cache
                .borrow()
                .get(id, style_value_epoch)
                .copied(),
            units: worker
                .font_units_cache
                .borrow()
                .get(id, style_value_epoch)
                .copied(),
            generation: worker.hidden_cache.borrow().get(id, inputs.epoch).copied(),
            decoration: worker
                .decoration_cache
                .borrow()
                .get(id, style_value_epoch)
                .copied(),
            custom,
        }
    }
}

/// What a frame's style consumers read of a rendered element: its display,
/// box and inline records, and the transition endpoints. One definition for
/// the pass and for its verification on the lazy path.
fn compute_element<B: StyleBackend + ?Sized>(
    view: ComputeView<'_, B>,
    id: NodeId,
    vp: Vp,
    base: &url::Url,
    rendered_memo: &mut FxHashMap<NodeId, bool>,
    inline_memo: &mut FxHashMap<NodeId, InlineStyle>,
) {
    if !rendered(view, rendered_memo, id) {
        return;
    }
    let _ = view.computed_display(id);
    let _ = BoxStyle::of(&view, id, vp);
    let _ = inline_context(view, base, inline_memo, id);
    for name in transitions::PROPERTIES {
        let _ = view.computed_value_resolved(id, name);
    }
}

/// Whether `id` and every composed ancestor generate boxes (as transition
/// participation checks: CSS Transitions 1 #starting skips the rest).
fn rendered<B: StyleBackend + ?Sized>(
    view: ComputeView<'_, B>,
    memo: &mut FxHashMap<NodeId, bool>,
    id: NodeId,
) -> bool {
    let nodes = view.style_view().nodes;
    let mut path = Vec::new();
    let mut cursor = Some(id);
    let mut result = loop {
        let Some(node) = cursor else { break true };
        if let Some(&known) = memo.get(&node) {
            break known;
        }
        path.push(node);
        cursor = view.parent_composed(node);
    };
    for &node in path.iter().rev() {
        result = result && !(nodes.is_element(node) && view.is_hidden(node));
        memo.insert(node, result);
    }
    result
}

/// The inline formatting context inside element `id`, derived along its
/// style ancestors from the root context, as box-tree construction does.
fn inline_context<B: StyleBackend + ?Sized>(
    view: ComputeView<'_, B>,
    base: &url::Url,
    memo: &mut FxHashMap<NodeId, InlineStyle>,
    id: NodeId,
) -> InlineStyle {
    let nodes = view.style_view().nodes;
    let mut chain = vec![id];
    let mut style = loop {
        let top = chain[chain.len() - 1];
        match view
            .style_parent(top)
            .filter(|&parent| nodes.is_element(parent))
        {
            Some(parent) => match memo.get(&parent) {
                Some(style) => break style.clone(),
                None => chain.push(parent),
            },
            None => break InlineStyle::root(),
        }
    };
    for node in chain.into_iter().rev() {
        style = InlineStyle::derive(&view, node, &style, base);
        memo.insert(node, style.clone());
    }
    style
}

/// The page thread's side of the results: its caches, and per participant
/// the tables that results' indices refer to.
struct Adopt<'d> {
    dom: &'d Dom,
    shared_lists: FxHashMap<Box<[u32]>, Rc<Vec<u32>>>,
    lists: Vec<Vec<Rc<Vec<u32>>>>,
    cascades: Vec<Vec<Rc<CascadedMaps>>>,
    rows: Vec<Vec<computed_cache::SharedRow>>,
    stored: usize,
    adopted: Vec<NodeId>,
    busy: Duration,
    time: Duration,
}

impl<'d> Adopt<'d> {
    fn new(dom: &'d Dom, stamp: (u64, u64), participants: usize) -> Self {
        // Results belong to this revision; any older memo is retired, as a
        // lazy read would retire it.
        {
            let mut computed = dom.computed_cache.borrow_mut();
            if computed.0 != stamp {
                computed.0 = stamp;
                computed.1.clear();
            }
            let mut custom = dom.custom_prop_cache.borrow_mut();
            if custom.0 != stamp {
                custom.0 = stamp;
                custom.1.clear();
            }
        }
        Adopt {
            dom,
            shared_lists: FxHashMap::default(),
            lists: vec![Vec::new(); participants],
            cascades: vec![Vec::new(); participants],
            rows: vec![Vec::new(); participants],
            stored: 0,
            adopted: Vec::new(),
            busy: Duration::ZERO,
            time: Duration::ZERO,
        }
    }

    fn add(&mut self, stream: &Stream<Run>, run: Run) {
        let started = Instant::now();
        let dom = self.dom;
        let participant = run.matched.participant;
        self.busy += run.busy;
        for rules in run.matched.new_lists {
            let list = match self.shared_lists.get(&rules) {
                Some(list) => list.clone(),
                None => {
                    let list = Rc::new(rules.to_vec());
                    self.shared_lists.insert(rules, list.clone());
                    list
                }
            };
            self.lists[participant].push(list);
        }
        self.cascades[participant].extend(run.new_cascades.into_iter().map(Rc::new));
        let mut computed = dom.computed_cache.borrow_mut();
        for row in run.new_rows {
            let row = computed.1.adopt_row(row);
            self.rows[participant].push(row);
        }
        let style_value_epoch = dom.style_value_epoch;
        {
            let mut selectors = dom.selector_cache.borrow_mut();
            let mut matched = dom.matched_cache.borrow_mut();
            for (offset, &list) in run.matched.lists.iter().enumerate() {
                let (id, _) = stream.entry(run.matched.start + offset);
                let list = &self.lists[participant][list as usize];
                selectors.put(id, dom.selector_epoch, list.clone());
                matched.put(id, style_value_epoch, list.clone());
            }
        }
        let mut cascaded = dom.cascaded_cache.borrow_mut();
        let mut custom = dom.custom_prop_cache.borrow_mut();
        let mut hidden = dom.hidden_cache.borrow_mut();
        let mut fonts = dom.font_cache.borrow_mut();
        let mut units = dom.font_units_cache.borrow_mut();
        let mut decorations = dom.decoration_cache.borrow_mut();
        for (offset, element) in run.computed.into_iter().enumerate() {
            let (id, _) = stream.entry(run.matched.start + offset);
            if let Some(index) = element.cascade {
                let maps = self.cascades[participant][index as usize].clone();
                cascaded.put(id, style_value_epoch, maps);
            }
            let row = element
                .row
                .map(|index| self.rows[participant][index as usize].clone());
            if row.is_some() || element.node.is_some() {
                computed.1.adopt_node(id, row, element.node);
            }
            if let Some(map) = element.custom {
                custom.1.insert(id, map);
            }
            if let Some(generation) = element.generation {
                hidden.put(id, dom.epoch, generation);
            }
            if let Some(px) = element.font_px {
                fonts.put(id, style_value_epoch, px);
            }
            if let Some(value) = element.units {
                units.put(id, style_value_epoch, value);
            }
            if let Some(value) = element.decoration {
                decorations.put(id, style_value_epoch, value);
            }
            self.adopted.push(id);
        }
        self.stored += run.matched.lists.len();
        self.time += started.elapsed();
    }
}

impl Dom {
    /// Pages whose style the pass's workers cannot compute from the shared
    /// inputs alone; see the module documentation.
    fn parallel_style_supported(&self, index: &StyleIndex) -> bool {
        style_records::enabled()
            && !index.has_container_queries
            && !index.has_container_units
            && !index.has_revert_layer
            && index.slotted_rules.is_empty()
            && index
                .properties
                .values()
                .all(|registry| registry.is_empty())
            && self
                .properties
                .javascript
                .values()
                .all(|registry| registry.is_empty())
            && self.transitions.idle()
    }

    /// Before a frame's style consumers run, once per broad invalidation
    /// (a new style value or page font revision): compute the style of every
    /// rendered element that has none yet, in parallel. A lazy read since the
    /// invalidation may already have computed a few. `viewport` and `base`
    /// are the layout's, so the typed records match what box-tree
    /// construction asks for.
    pub(crate) fn prepare_styles(&self, viewport: crate::layout2::Viewport, base: &url::Url) {
        self.flush_style_invalidations();
        let stamp = (
            self.style_value_epoch,
            crate::font_system::page_font_epoch(),
        );
        if self.parallel_style.attempted.get() == Some(stamp) {
            return;
        }
        self.parallel_style.attempted.set(Some(stamp));
        let Some(pool) = style_pool::pool() else {
            return;
        };
        let cost = self
            .parallel_style
            .measured
            .get()
            .unwrap_or(FIRST_ELEMENT_COST);
        let ran = self
            .compute_stale_with(pool, viewport, base, MIN_ELEMENTS, cost)
            .is_some();
        #[cfg(test)]
        self.parallel_style
            .passes
            .set(self.parallel_style.passes.get() + u32::from(ran));
        let _ = ran;
    }

    /// Compute every connected element without a computed row for this
    /// revision, in parallel, if there are at least `min_elements` of them
    /// and their estimated work at `element_cost` each reaches `MIN_WORK`.
    /// Returns the elements whose results were adopted.
    pub(super) fn compute_stale_with(
        &self,
        pool: &style_pool::Pool,
        viewport: crate::layout2::Viewport,
        base: &url::Url,
        min_elements: usize,
        element_cost: Duration,
    ) -> Option<Vec<NodeId>> {
        self.flush_style_invalidations();
        let index = self.style_index();
        if !self.parallel_style_supported(&index) {
            return None;
        }
        let started = Instant::now();
        let (slots, scopes) = self.scope_rules(&index)?;
        let stamp = (
            self.style_value_epoch,
            crate::font_system::page_font_epoch(),
        );
        let fresh = self.computed_cache.borrow().0 == stamp;
        let stale = |node| !fresh || !self.computed_cache.borrow().1.has_row(node);
        let animated = self.animations.origin_elements();
        let pruned = |node| animated.contains(&node);
        let stream = Stream::new(self.nodes.len(), RUN);
        let mut walk = Walk::new(&slots);
        let min_elements = min_elements
            .max(1)
            .max((MIN_WORK.as_nanos() / element_cost.as_nanos().max(1)) as usize);
        if !walk.advance(self, &slots, &stream, min_elements, stale, pruned)
            || walk.count < min_elements
        {
            return None;
        }
        let document_base = self.document_base(DOCUMENT);
        // SAFETY: as in `match_stale_with`: the view is shared only with this
        // pass's job, and `Pool::run` keeps this thread inside the pass until
        // every worker has left it. Meanwhile this thread reads the tree only
        // through `NodesRef` (the walk and its own participant) and writes
        // only memo caches outside the node arena (`Adopt`).
        let view = unsafe { style_view::SharedView::new(self.style_view()) };
        let inputs = Inputs {
            view,
            index: &index,
            scopes: &scopes,
            style_value_epoch: self.style_value_epoch,
            style_epoch: self.style_epoch,
            epoch: self.epoch,
            viewport_px: self.viewport_px,
            device_pixel_ratio: self.device_pixel_ratio,
            doc_url: self.doc_url.as_ref(),
            render_clickables: &self.render_clickables,
            render_live: self.render_live,
            document_base,
            animated: &animated,
            vp: Vp {
                w: viewport.width,
                h: viewport.height,
            },
            base,
        };
        let inputs = &inputs;
        let stream = &stream;
        let job = |participant: usize| {
            let mut part = Part::new(participant, inputs, inputs.view.view());
            while let Some(range) = stream.claim() {
                let run = part.run(stream, range);
                stream
                    .done
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(run);
            }
        };
        let (mut walked, mut joined) = (Duration::ZERO, 1);
        let result = pool.run(pool.participants(), &job, |lead| {
            let walk_started = Instant::now();
            let ok = {
                let _published = Published(stream);
                loop {
                    stream
                        .published
                        .store(walk.count, std::sync::atomic::Ordering::Release);
                    lead.wake(participants_for(work(walk.count, element_cost)));
                    if walk.is_done() {
                        break true;
                    }
                    if !walk.advance(self, &slots, stream, walk.count + RUN, stale, pruned) {
                        break false;
                    }
                }
            };
            walked = walk_started.elapsed();
            joined = lead.participants();
            if !ok {
                return None;
            }
            let mut adopt = Adopt::new(self, stamp, pool.participants());
            let mut own = Part::new(0, inputs, inputs.view.view());
            while adopt.stored < walk.count && !lead.worker_panicked() {
                let ready =
                    std::mem::take(&mut *stream.done.lock().unwrap_or_else(|e| e.into_inner()));
                if !ready.is_empty() {
                    for run in ready {
                        adopt.add(stream, run);
                    }
                    let cost = adopt.busy / adopt.stored.max(1) as u32;
                    if adopt.stored >= 2 * RUN && cost > element_cost * 2 {
                        lead.wake(participants_for(work(walk.count - adopt.stored, cost)));
                    }
                } else if let Some(range) = stream.claim() {
                    let run = own.run(stream, range);
                    adopt.add(stream, run);
                } else {
                    std::hint::spin_loop();
                }
            }
            Some((adopt.adopted, adopt.busy, adopt.time))
        });
        let (adopted, busy, adopting) = result?;
        let count = walk.count;
        self.parallel_style
            .measured
            .set(Some(busy / count.max(1) as u32));
        if casc_diag_on() {
            let wall = started.elapsed();
            casc_bump(|d| {
                d.style_passes += 1;
                d.style_elements += count as u64;
                d.style_threads = d.style_threads.max(joined as u64);
                d.style_wall_us += wall.as_micros() as u64;
                d.style_walk_us += walked.as_micros() as u64;
                d.style_adopt_us += adopting.as_micros() as u64;
                d.style_busy_us += busy.as_micros() as u64;
            });
        }
        if verify_enabled() {
            let (rows, mismatches) = self.parallel_style_mismatches(&adopted, viewport, base);
            eprintln!(
                "STYLEVERIFY compute checked={} rows={rows} mismatches={mismatches}",
                adopted.len()
            );
        }
        Some(adopted)
    }
}

/// What the memo caches hold for one element, for verification.
struct Snapshot {
    row: Option<computed_cache::RowExport>,
    node: Option<computed_cache::NodeExport>,
    cascade: Option<CascadedMaps>,
    font_px: Option<f32>,
    units: Option<(u64, Units)>,
    decoration: Option<(bool, bool)>,
}

impl Dom {
    fn style_snapshot(&self, id: NodeId) -> Snapshot {
        let style_value_epoch = self.style_value_epoch;
        let computed = self.computed_cache.borrow();
        Snapshot {
            row: computed
                .1
                .row(id)
                .map(|row| computed_cache::Values::export_row(&row)),
            node: computed.1.export_node(id),
            cascade: self
                .cascaded_cache
                .borrow()
                .get(id, style_value_epoch)
                .map(|maps| (**maps).clone()),
            font_px: self.font_cache.borrow().get(id, style_value_epoch).copied(),
            units: self
                .font_units_cache
                .borrow()
                .get(id, style_value_epoch)
                .copied(),
            decoration: self
                .decoration_cache
                .borrow()
                .get(id, style_value_epoch)
                .copied(),
        }
    }

    /// Forget every memoized cascade, computed value and record.
    fn clear_style_memos(&self) {
        self.computed_cache.borrow_mut().1.clear();
        self.cascaded_cache.borrow_mut().slots.clear();
        self.custom_prop_cache.borrow_mut().1.clear();
        self.matched_cache.borrow_mut().slots.clear();
        self.hidden_cache.borrow_mut().slots.clear();
        self.font_cache.borrow_mut().slots.clear();
        self.font_units_cache.borrow_mut().slots.clear();
        self.decoration_cache.borrow_mut().slots.clear();
        *self.style_sharing.borrow_mut() = Default::default();
    }

    /// Compare a pass's results for `nodes` (in tree order) with the lazy
    /// path: forget every memo, recompute serially what the pass computed,
    /// and compare each value, record and cascade the pass stored. Returns
    /// the computed rows compared and the mismatches. The serial results
    /// stay cached.
    pub(super) fn parallel_style_mismatches(
        &self,
        nodes: &[NodeId],
        viewport: crate::layout2::Viewport,
        base: &url::Url,
    ) -> (usize, usize) {
        let parallel: Vec<Snapshot> = nodes.iter().map(|&id| self.style_snapshot(id)).collect();
        self.clear_style_memos();
        let vp = Vp {
            w: viewport.width,
            h: viewport.height,
        };
        let (mut rendered_memo, mut inline_memo) = (FxHashMap::default(), FxHashMap::default());
        for &id in nodes {
            compute_element(
                ComputeView(self),
                id,
                vp,
                base,
                &mut rendered_memo,
                &mut inline_memo,
            );
        }
        let (mut rows, mut mismatches) = (0, 0);
        let mut report = |id: NodeId, what: String| {
            mismatches += 1;
            if mismatches <= 8 {
                eprintln!(
                    "STYLEVERIFY compute mismatch node={id} tag={:?} {what}",
                    self.tag_name(id)
                );
            }
        };
        for (&id, parallel) in nodes.iter().zip(parallel) {
            let serial = self.style_snapshot(id);
            // Every value the pass stored is what the lazy path computes.
            let values = parallel
                .row
                .iter()
                .chain(parallel.node.as_ref().map(|node| node.contextual()))
                .flat_map(|row| row.entries())
                .collect::<Vec<_>>();
            rows += usize::from(parallel.row.is_some());
            for (property, value) in values {
                let expected = ComputeView(self).computed_value(id, PROPS[property].name);
                if *value != expected {
                    report(
                        id,
                        format!(
                            "{} parallel={value:?} serial={expected:?}",
                            PROPS[property].name
                        ),
                    );
                }
            }
            let serial_boxes = serial.row.as_ref().map_or(&[][..], |row| row.boxes());
            for (context, value) in parallel.row.as_ref().map_or(&[][..], |row| row.boxes()) {
                match serial_boxes.iter().find(|(c, _)| c == context) {
                    Some((_, expected)) if expected == value => {}
                    Some((_, expected)) => report(
                        id,
                        format!("box record parallel={value:?} serial={expected:?}"),
                    ),
                    None => report(id, "box record missing on the lazy path".into()),
                }
            }
            let inline = |snapshot: &Snapshot| {
                snapshot
                    .node
                    .as_ref()
                    .and_then(|node| node.inline())
                    .map(|(parent, value)| (parent.clone(), value.clone()))
            };
            if let Some(record) = inline(&parallel)
                && inline(&serial) != Some(record.clone())
            {
                report(
                    id,
                    format!(
                        "inline record parallel={:?} serial={:?}",
                        record.1,
                        inline(&serial).map(|(_, value)| value)
                    ),
                );
            }
            let display = |snapshot: &Snapshot| {
                snapshot
                    .node
                    .as_ref()
                    .and_then(|node| node.display().cloned())
            };
            if display(&parallel).is_some() && display(&parallel) != display(&serial) {
                report(
                    id,
                    format!(
                        "display record parallel={:?} serial={:?}",
                        display(&parallel),
                        display(&serial)
                    ),
                );
            }
            if let (Some(a), Some(b)) = (&parallel.cascade, &serial.cascade)
                && !a.same_declarations(b)
            {
                report(id, "cascade differs".into());
            }
            if parallel.font_px.is_some()
                && serial.font_px.is_some()
                && parallel.font_px != serial.font_px
            {
                report(
                    id,
                    format!("font-size {:?} {:?}", parallel.font_px, serial.font_px),
                );
            }
            if parallel.units.is_some() && serial.units.is_some() && parallel.units != serial.units
            {
                report(id, "font units differ".into());
            }
            if parallel.decoration.is_some()
                && serial.decoration.is_some()
                && parallel.decoration != serial.decoration
            {
                report(id, "text decoration differs".into());
            }
        }
        (rows, mismatches)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VIEWPORT: crate::layout2::Viewport = crate::layout2::Viewport {
        width: 1000.,
        height: 800.,
    };

    fn base() -> url::Url {
        url::Url::parse("https://example.test/dir/page.html").unwrap()
    }

    /// Several hundred elements exercising inheritance, custom properties,
    /// font-relative and viewport units, generated content, presentational
    /// hints, display:none subtrees, color schemes and text decoration.
    fn fixture() -> String {
        let mut html = String::from(
            r#"<!doctype html><html lang=en><head><style>
            :root { --gap: 3px; --accent: rgb(10, 20, 30); font-size: 18px }
            body { margin: 8px; color: var(--accent); line-height: 1.4 }
            .card { padding: var(--gap) calc(var(--gap) * 2); border: 1px solid; width: 50% }
            .card h2 { font-size: 1.5em; margin: 0.5em 0; text-transform: uppercase }
            .card:nth-child(3n) { display: none }
            .card:nth-child(4n) p { font: italic bold 12px/1.2 serif; letter-spacing: 0.1em }
            ul li { list-style: square inside }
            .flex { display: flex; gap: 1rem } .flex > * { flex: 1 1 0 }
            .grid { display: grid; grid-template-columns: repeat(2, 1fr) }
            p::before { content: "- "; color: red } p::after { content: attr(data-x) }
            .u { text-decoration: underline } .s { text-decoration: line-through }
            .abs { position: absolute; top: 1em; left: 2ch; z-index: 3; transform: translate(2px, 3px) }
            .hidden-v { visibility: hidden } .o { opacity: 0.5 }
            td { padding: 2px 4px; vertical-align: middle }
            .ws { white-space: pre-wrap; tab-size: 4; word-break: break-all }
            a:any-link { color: blue }
            .vh { height: 10vh; max-width: 30vw }
            input { width: 10em }
            .dark { color-scheme: dark; background: Canvas; color: CanvasText }
            .var { margin-top: var(--missing, 2em); --local: 4px; padding-left: var(--local) }
            </style></head><body>"#,
        );
        for i in 0..40 {
            html.push_str(&format!(
                r#"<section class="card{}{}"><h2>Title {i}</h2><p data-x="{i}" style="margin-left:{i}px">Text <a href="/p{i}">link</a> <b class=u>u</b> <i class=s>s</i></p><ul><li>a</li><li class=var>b<ul><li>c</li></ul></li></ul><div class=flex><span>1</span><span class=o>2</span></div></section>"#,
                if i % 5 == 0 { " dark" } else { "" },
                if i % 7 == 0 { " ws" } else { "" },
            ));
            if i % 6 == 0 {
                html.push_str(
                    r#"<table bgcolor=yellow><tr><td width=40>1</td><td class=abs>2</td></tr></table><div class=grid><div>g</div><div class="hidden-v vh">h</div></div><input type=text><center>c</center><font size=5 color=green>f</font><p align=right>r</p><details><summary>s</summary>d</details><ol><li>o</li></ol>"#,
                );
            }
        }
        html.push_str("<div id=host class=card><b slot=one>light</b></div></body></html>");
        html
    }

    fn document() -> Dom {
        let mut dom = Dom::parse_document(&fixture());
        dom.set_viewport_px(VIEWPORT.width, VIEWPORT.height);
        let host = dom.get_by_id("host").unwrap();
        let root = dom.attach_shadow(host);
        for child in dom.parse_fragment_into(
            "div",
            r#"<style>:host { color: red } .inner { padding: 1em }</style><div class=inner><slot name=one></slot><i>after</i></div>"#,
        ) {
            dom.append(root, child);
        }
        dom
    }

    fn elements(dom: &Dom) -> Vec<NodeId> {
        dom.composed_descendants(DOCUMENT)
            .into_iter()
            .filter(|&node| dom.tag_name(node).is_some())
            .collect()
    }

    /// Run a pass on `pool` and require every adopted result to equal the
    /// lazy path. Returns how many elements were adopted.
    fn pass_and_verify(dom: &Dom, pool: &style_pool::Pool) -> Option<usize> {
        let adopted =
            dom.compute_stale_with(pool, VIEWPORT, &base(), 1, Duration::from_millis(1))?;
        let (rows, mismatches) = dom.parallel_style_mismatches(&adopted, VIEWPORT, &base());
        assert_eq!(mismatches, 0);
        assert!(rows > 0);
        Some(adopted.len())
    }

    #[test]
    fn parallel_computation_equals_the_lazy_path() {
        // Process-wide font revisions from parallel tests expire every style.
        let _inputs = crate::layout2::stable_global_layout_inputs();
        let dom = document();
        for participants in [1, 4] {
            let pool = style_pool::Pool::new(participants);
            dom.clear_style_memos();
            let adopted = pass_and_verify(&dom, &pool).expect("pass ran");
            assert!(adopted > 300, "{adopted}");
        }
    }

    #[test]
    fn records_from_a_pass_serve_box_tree_construction() {
        // Process-wide font revisions from parallel tests expire every style.
        let _inputs = crate::layout2::stable_global_layout_inputs();
        let dom = document();
        let pool = style_pool::Pool::new(4);
        assert!(
            dom.compute_stale_with(&pool, VIEWPORT, &base(), 1, Duration::from_millis(1))
                .is_some()
        );
        let vp = Vp {
            w: VIEWPORT.width,
            h: VIEWPORT.height,
        };
        let card = elements(&dom)
            .into_iter()
            .find(|&node| dom.attr(node, "class") == Some("card dark"))
            .unwrap();
        let hits = dom.computed_cache.borrow().1.box_hits.get();
        let style = BoxStyle::of(&dom, card, vp);
        assert_eq!(dom.computed_cache.borrow().1.box_hits.get(), hits + 1);
        assert_eq!(
            style.padding,
            BoxStyle::of(&ComputeView(&dom), card, vp).padding
        );
        let display_hits = dom.computed_cache.borrow().1.display_hits.get();
        let display = dom.computed_display(card);
        assert_eq!(
            dom.computed_cache.borrow().1.display_hits.get(),
            display_hits + 1
        );
        assert_eq!(display, ComputeView(&dom).computed_display_uncached(card));
    }

    #[test]
    fn partial_and_broad_invalidations_recompute_only_stale_elements() {
        // Process-wide font revisions from parallel tests expire every style.
        let _inputs = crate::layout2::stable_global_layout_inputs();
        let mut dom = document();
        let pool = style_pool::Pool::new(4);
        let all = pass_and_verify(&dom, &pool).expect("first pass");
        // A local change leaves most rows in place.
        let leaf = elements(&dom)
            .into_iter()
            .find(|&node| dom.tag_name(node) == Some("b"))
            .unwrap();
        // As a node-level invalidation retires a subtree's rows.
        dom.computed_cache.borrow_mut().1.remove_node(leaf);
        let partial = pass_and_verify(&dom, &pool).expect("partial pass");
        assert!(partial < all / 2, "{partial} of {all}");
        // A new sheet invalidates everything.
        let style = dom.create_element("style");
        let text = dom.create_text("li { color: teal; padding: 1ex }");
        dom.append(style, text);
        let body = elements(&dom)
            .into_iter()
            .find(|&node| dom.tag_name(node) == Some("body"))
            .unwrap();
        dom.append(body, style);
        assert!(pass_and_verify(&dom, &pool).expect("broad pass") >= all);
    }

    #[test]
    fn animated_subtrees_stay_lazy_and_paint_frames_touch_only_them() {
        // Process-wide font revisions from parallel tests expire every style.
        let _inputs = crate::layout2::stable_global_layout_inputs();
        // CSS Animations 1 #animations: animated values are private to the
        // element and inherited by its subtree; equal cascades elsewhere
        // must not see them.
        let mut html = String::from(
            "<!doctype html><style>body{margin:0} div{color:black;padding:2px}
             #a{animation:fade 2s linear}
             @keyframes fade{from{color:rgb(0, 0, 0)}to{color:rgb(200, 100, 0)}}</style>",
        );
        html.push_str("<div id=a><span id=b>b</span></div>");
        for i in 0..200 {
            html.push_str(&format!("<div id=u{i}><span>u</span></div>"));
        }
        let mut dom = Dom::parse_document(&html);
        dom.set_viewport_px(VIEWPORT.width, VIEWPORT.height);
        let id = |dom: &Dom, name: &str| dom.get_by_id(name).unwrap();
        dom.update_css_animations(0.);
        // A broad invalidation, then a pass.
        dom.set_viewport_px(VIEWPORT.width + 1., VIEWPORT.height);
        let pool = style_pool::Pool::new(4);
        let adopted = dom
            .compute_stale_with(&pool, VIEWPORT, &base(), 1, Duration::from_millis(1))
            .expect("pass ran");
        let (a, b, u) = (id(&dom, "a"), id(&dom, "b"), id(&dom, "u7"));
        assert!(!adopted.contains(&a) && !adopted.contains(&b));
        assert!(adopted.contains(&u));
        {
            let cache = dom.computed_cache.borrow();
            assert!(!cache.1.has_row(a) && !cache.1.has_row(b));
        }
        let unrelated = |dom: &Dom| {
            let cache = dom.computed_cache.borrow();
            let row = cache.1.row(u).unwrap();
            let values = computed_cache::Values::export_row(&row)
                .entries()
                .map(|(property, value)| (property, value.clone()))
                .collect::<Vec<_>>();
            (Rc::as_ptr(&row), values)
        };
        let value = |dom: &Dom, node, name| dom.computed_value_resolved(node, name);
        assert_eq!(value(&dom, u, "color").as_deref(), Some("black"));
        let before = unrelated(&dom);
        dom.update_css_animations(0.);
        dom.update_css_animations(1.);
        assert_eq!(value(&dom, a, "color").as_deref(), Some("rgb(100, 50, 0)"));
        assert_eq!(value(&dom, b, "color").as_deref(), Some("rgb(100, 50, 0)"));
        assert_eq!(value(&dom, a, "padding-top").as_deref(), Some("2px"));
        // A paint-only frame evicted only the animated subtree's color.
        assert_eq!(dom.animations.last_layout_changes, 0);
        assert_eq!(unrelated(&dom), before);
        dom.update_css_animations(1.5);
        assert_eq!(value(&dom, b, "color").as_deref(), Some("rgb(150, 75, 0)"));
        assert_eq!(unrelated(&dom), before);
    }

    #[test]
    fn transition_gained_in_the_same_change_starts_from_the_before_change_value() {
        // Process-wide font revisions from parallel tests expire every style.
        let _inputs = crate::layout2::stable_global_layout_inputs();
        // CSS Transitions 1 #starting: the before-change style is the
        // computed value as of the previous style change event, also for an
        // element whose transition is declared by this change.
        let mut html = String::from(
            "<!doctype html><style>body{margin:0} #t{width:100px}
             #t.open{width:200px;transition:width 1s linear}</style><div id=t>t</div>",
        );
        for i in 0..200 {
            html.push_str(&format!("<p>{i}</p>"));
        }
        let mut dom = Dom::parse_document(&html);
        dom.set_viewport_px(VIEWPORT.width, VIEWPORT.height);
        let t = dom.get_by_id("t").unwrap();
        let pool = style_pool::Pool::new(4);
        assert!(
            dom.compute_stale_with(&pool, VIEWPORT, &base(), 1, Duration::from_millis(1))
                .is_some()
        );
        dom.update_css_transitions(0.);
        // One change both gains the transition and changes the width, and
        // invalidates broadly, so a pass computes the after-change style.
        dom.set_attr(t, "class", "open");
        dom.set_viewport_px(VIEWPORT.width + 1., VIEWPORT.height);
        let adopted = dom
            .compute_stale_with(&pool, VIEWPORT, &base(), 1, Duration::from_millis(1))
            .expect("pass ran");
        assert!(adopted.contains(&t));
        dom.update_css_transitions(0.);
        assert_eq!(
            dom.computed_value_resolved(t, "width").as_deref(),
            Some("100px")
        );
        dom.update_css_transitions(0.5);
        assert_eq!(
            dom.computed_value_resolved(t, "width").as_deref(),
            Some("150px")
        );
        dom.update_css_transitions(1.5);
        assert_eq!(
            dom.computed_value_resolved(t, "width").as_deref(),
            Some("200px")
        );
    }

    /// Many passes with ten participants, random DOM mutations between
    /// them, each pass verified against the lazy path.
    #[test]
    fn passes_survive_random_mutations() {
        // Process-wide font revisions from parallel tests expire every style.
        let _inputs = crate::layout2::stable_global_layout_inputs();
        let mut dom = document();
        let pool = style_pool::Pool::new(10);
        let mut state = 0x2545_f491_4f6c_dd1d_u64;
        let mut random = |bound: usize| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state % bound as u64) as usize
        };
        let classes = [
            "card", "u", "s", "o", "abs", "flex", "grid", "dark", "ws", "var", "",
        ];
        let styles = [
            "color:red",
            "display:none",
            "font-size:2em",
            "padding:var(--gap)",
            "width:10vw",
            "",
        ];
        let mut passes = 0;
        for step in 0..24 {
            for _ in 0..1 + random(5) {
                let nodes = elements(&dom);
                let node = nodes[random(nodes.len())];
                if dom
                    .tag_name(node)
                    .is_some_and(|tag| tag == "html" || tag == "head")
                {
                    continue;
                }
                match random(6) {
                    0 => dom.set_attr(node, "class", classes[random(classes.len())]),
                    1 => dom.set_attr(node, "style", styles[random(styles.len())]),
                    2 if dom.tag_name(node) != Some("body") => dom.detach(node),
                    3 => {
                        for child in dom
                            .parse_fragment_into("div", "<p class=u>new <b>bold</b></p><li>x</li>")
                        {
                            dom.append(node, child);
                        }
                    }
                    4 => dom.set_attr(node, "hidden", ""),
                    _ => dom.remove_attr(node, "class"),
                }
            }
            if random(2) == 0 {
                dom.set_viewport_px(VIEWPORT.width + step as f32, VIEWPORT.height);
            }
            if pass_and_verify(&dom, &pool).is_some() {
                passes += 1;
            }
        }
        assert!(passes >= 12, "{passes}");
    }
}
