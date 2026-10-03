//! Parallel selector matching ("phase M" of a style pass).
//!
//! Selectors 4 #match-against-element depends on the element tree and on
//! document state, never on another element's style. So once a broad
//! invalidation has made many selector results stale, they can all be
//! computed independently, without the parent-before-child ordering that the
//! cascade needs. The page thread walks the composed tree and publishes the
//! stale elements, in tree order, as a work list; `style_pool` participants
//! claim contiguous chunks of it as soon as they are published, each keeping
//! an ancestor Bloom filter incrementally across its chunk. When the walk is
//! done, the page thread stores finished chunks into `selector_cache`, and
//! matches chunks itself while none are waiting. Walking and storing thereby
//! overlap the matching instead of adding to it.
//!
//! Both this pass and the lazy path run `StyleView::match_rules`, so results
//! are identical by construction; the ancestor filters only reject rules
//! whose required ancestor keys are absent. Container queries (which need
//! layout sizes), `::slotted()` and `:host` rules stay with the cascade on the
//! page thread. `TRUST_STYLE_VERIFY=1` recomputes every result of each pass
//! on the lazy path and reports differences.

use super::*;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// The lazy path matches this many elements of a new selector revision
/// before a pass is considered. Small restyles never pay for the walk.
const FIRST_ATTEMPT: u32 = 16;
/// Further lazy misses within one revision before walking again.
const LATER_ATTEMPT: u32 = 256;
/// Fewer stale elements than this are left to the lazy path.
const MIN_ELEMENTS: usize = 128;
/// Nor is a pass worth starting for less estimated matching work: on a
/// small or simply styled page, its fixed costs exceed the gain.
const MIN_WORK: Duration = Duration::from_micros(200);
/// Estimated matching work per additional participant woken: a worker's
/// wake-up and cold caches must be repaid.
const WORK_PER_PARTICIPANT: Duration = Duration::from_micros(250);
/// The per-element cost assumed before a document's first pass has been
/// measured. Deliberately low: participants woken too late are woken as
/// finished chunks reveal the cost, but a wasted wake-up is not recovered.
const FIRST_ELEMENT_COST: Duration = Duration::from_nanos(500);
/// Elements per claimed unit of work.
const CHUNK: usize = 32;

/// When to attempt a pass, per arena.
#[derive(Default)]
pub(super) struct Trigger {
    epoch: Cell<Option<u64>>,
    misses: Cell<u32>,
    next: Cell<u32>,
    /// The last pass's matching time per element; a document restyles many
    /// times while it loads.
    measured: Cell<Option<Duration>>,
    #[cfg(test)]
    passes: Cell<u32>,
}

/// Estimated matching work of `elements` at `cost` each.
fn work(elements: usize, cost: Duration) -> Duration {
    cost.saturating_mul(elements.min(u32::MAX as usize) as u32)
}

/// Participants that `remaining` work repays, counting the page thread.
fn participants_for(remaining: Duration) -> usize {
    1 + (remaining.as_nanos() / WORK_PER_PARTICIPANT.as_nanos()) as usize
}

/// One tree scope's rules, borrowed from the style index for a pass.
struct ScopeRules<'r> {
    selectors: Vec<&'r Complex>,
    buckets: Option<&'r RuleBuckets>,
    shadow_host: Option<NodeId>,
}

/// The work list: stale elements published in tree order by the page thread.
struct Stream {
    /// `node << 16 | scope slot` per stale element, each written once before
    /// `published` covers it.
    entries: Box<[AtomicU64]>,
    published: AtomicUsize,
    /// Set once the walk has published its last entry (or unwound).
    complete: AtomicBool,
    /// Index of the next chunk to claim.
    next: AtomicUsize,
    /// Matched chunks waiting for the page thread.
    done: Mutex<Vec<Chunk>>,
}

impl Stream {
    fn entry(&self, index: usize) -> (NodeId, usize) {
        let entry = self.entries[index].load(Ordering::Relaxed);
        ((entry >> 16) as NodeId, (entry & 0xffff) as usize)
    }

    /// Claim the next chunk, waiting while the walk has yet to publish it.
    /// `None` once every published entry has been claimed.
    fn claim(&self) -> Option<std::ops::Range<usize>> {
        let start = self.next.fetch_add(1, Ordering::Relaxed) * CHUNK;
        let mut spins = 0u32;
        loop {
            let published = self.published.load(Ordering::Acquire);
            if published >= start + CHUNK {
                return Some(start..start + CHUNK);
            }
            if self.complete.load(Ordering::Acquire) {
                let published = self.published.load(Ordering::Acquire);
                return (start < published).then(|| start..published.min(start + CHUNK));
            }
            // The walk publishes a chunk every few microseconds.
            spins += 1;
            if spins < 256 {
                std::hint::spin_loop();
            } else {
                std::thread::yield_now();
            }
        }
    }
}

/// Marks the walk complete even if it unwinds, so no participant waits on it.
struct Published<'a>(&'a Stream);

impl Drop for Published<'_> {
    fn drop(&mut self) {
        self.0.complete.store(true, Ordering::Release);
    }
}

/// One matched chunk. Few elements have a rule list of their own (a
/// Wikipedia article's 6,400 elements match 300 distinct lists), so each
/// participant interns its lists and reports per element only an index.
struct Chunk {
    participant: usize,
    start: usize,
    /// Per element, an index into the participant's lists.
    lists: Vec<u32>,
    /// Lists this chunk added to the participant's lists, in index order.
    new_lists: Vec<Box<[u32]>>,
    candidates: u64,
    busy: Duration,
}

/// One participant's matching state, kept across the chunks it claims.
struct Participant<'p, 'a, 'r> {
    index: usize,
    view: &'p StyleView<'a>,
    scopes: &'p [ScopeRules<'r>],
    classes: RefCell<FxHashMap<NodeId, class_tokens::ClassTokens>>,
    bloom: rule_index::AncestorBloom,
    interned: FxHashMap<Box<[u32]>, u32>,
    candidates: Vec<u32>,
    out: Vec<u32>,
}

impl<'p, 'a, 'r> Participant<'p, 'a, 'r> {
    fn new(index: usize, view: &'p StyleView<'a>, scopes: &'p [ScopeRules<'r>]) -> Self {
        Participant {
            index,
            view,
            scopes,
            classes: RefCell::new(FxHashMap::default()),
            bloom: rule_index::AncestorBloom::new(),
            interned: FxHashMap::default(),
            candidates: Vec::new(),
            out: Vec::new(),
        }
    }

    fn match_chunk(&mut self, stream: &Stream, range: std::ops::Range<usize>) -> Chunk {
        let began = Instant::now();
        let mut chunk = Chunk {
            participant: self.index,
            start: range.start,
            lists: Vec::with_capacity(range.len()),
            new_lists: Vec::new(),
            candidates: 0,
            busy: Duration::ZERO,
        };
        let scopes = self.scopes;
        for index in range {
            let (id, slot) = stream.entry(index);
            let rules = &scopes[slot];
            self.out.clear();
            if let Some(buckets) = rules.buckets {
                self.bloom.enter(self.view, id);
                let bloom = &self.bloom;
                chunk.candidates += self.view.match_rules(
                    id,
                    buckets,
                    |ri| rules.selectors[ri as usize],
                    rules.shadow_host,
                    ClassMemo::Local(&self.classes),
                    |required| bloom.may_match(required),
                    &mut self.candidates,
                    &mut self.out,
                );
                self.bloom.push(self.view, id);
            }
            let list = match self.interned.get(self.out.as_slice()) {
                Some(&list) => list,
                None => {
                    let list = self.interned.len() as u32;
                    self.interned.insert(self.out.as_slice().into(), list);
                    chunk.new_lists.push(self.out.as_slice().into());
                    list
                }
            };
            chunk.lists.push(list);
        }
        chunk.busy = began.elapsed();
        chunk
    }
}

/// The page thread's walk over the composed tree: children are pushed in
/// reverse, so it visits light children, then shadow children, as
/// `push_composed_children` orders them. A nested Document or a shadow root
/// starts its own tree scope. Character data has no element descendants.
struct Walk {
    stack: Vec<(NodeId, usize)>,
    count: usize,
}

impl Walk {
    /// Publish stale elements into `stream.entries` until `count` reaches
    /// `limit` or the tree is exhausted. `false` if an entry cannot be
    /// represented (the pass must be abandoned).
    fn advance(
        &mut self,
        dom: &Dom,
        slots: &FxHashMap<NodeId, usize>,
        stream: &Stream,
        limit: usize,
    ) -> bool {
        let view = dom.style_view();
        let nodes = view.nodes;
        let slot_of = |scope: NodeId| slots.get(&scope).copied().unwrap_or(0);
        let cache = dom.selector_cache.borrow();
        let shadows = !dom.shadow_roots.is_empty();
        while self.count < limit
            && let Some((node, slot)) = self.stack.pop()
        {
            if nodes.is_element(node) && cache.get(node, dom.selector_epoch).is_none() {
                if node >= 1 << 48 || self.count == stream.entries.len() {
                    return false;
                }
                stream.entries[self.count]
                    .store(((node as u64) << 16) | slot as u64, Ordering::Relaxed);
                self.count += 1;
            }
            if shadows && let Some(&shadow) = dom.shadow_roots.get(&node) {
                let scope = slot_of(shadow);
                let mut child = nodes.last_child(shadow);
                while let Some(c) = child {
                    if nodes.is_container(c) {
                        self.stack.push((c, scope));
                    }
                    child = nodes.prev_sibling(c);
                }
            }
            let mut child = nodes.last_child(node);
            while let Some(c) = child {
                if nodes.is_document(c) {
                    self.stack.push((c, slot_of(c)));
                } else if nodes.is_container(c) {
                    self.stack.push((c, slot));
                }
                child = nodes.prev_sibling(c);
            }
        }
        true
    }
}

/// The page thread's side of the results: `selector_cache` and the shared
/// rule lists, one allocation per distinct list across all participants.
struct Store<'c> {
    cache: std::cell::RefMut<'c, NodeCache<std::rc::Rc<Vec<u32>>>>,
    epoch: u64,
    shared_lists: FxHashMap<Box<[u32]>, std::rc::Rc<Vec<u32>>>,
    /// Per participant, its list indices' shared lists.
    tables: Vec<Vec<std::rc::Rc<Vec<u32>>>>,
    stored: usize,
    candidates: u64,
    busy: Duration,
    time: Duration,
}

impl Store<'_> {
    fn add(&mut self, stream: &Stream, chunk: Chunk) {
        let started = Instant::now();
        self.candidates += chunk.candidates;
        self.busy += chunk.busy;
        let table = &mut self.tables[chunk.participant];
        for rules in chunk.new_lists {
            let list = match self.shared_lists.get(&rules) {
                Some(list) => list.clone(),
                None => {
                    let list = std::rc::Rc::new(rules.to_vec());
                    self.shared_lists.insert(rules, list.clone());
                    list
                }
            };
            table.push(list);
        }
        for (offset, &list) in chunk.lists.iter().enumerate() {
            let (id, _) = stream.entry(chunk.start + offset);
            self.cache.put(id, self.epoch, table[list as usize].clone());
        }
        self.stored += chunk.lists.len();
        self.time += started.elapsed();
    }
}

impl Dom {
    /// Called on each lazy selector-cache miss of an element. Returns whether
    /// a parallel pass ran (so the caller should look in the cache again).
    pub(super) fn parallel_match_on_miss(&self) -> bool {
        if style_pool::configured_participants().is_none() {
            return false;
        }
        let trigger = &self.parallel_match;
        if trigger.epoch.get() != Some(self.selector_epoch) {
            trigger.epoch.set(Some(self.selector_epoch));
            trigger.misses.set(0);
            trigger.next.set(FIRST_ATTEMPT);
        }
        let misses = trigger.misses.get().saturating_add(1);
        trigger.misses.set(misses);
        if misses != trigger.next.get() {
            return false;
        }
        let ran = style_pool::pool().is_some_and(|pool| {
            let cost = trigger.measured.get().unwrap_or(FIRST_ELEMENT_COST);
            self.match_stale_with(pool, MIN_ELEMENTS, cost)
        });
        #[cfg(test)]
        trigger.passes.set(trigger.passes.get() + u32::from(ran));
        trigger.next.set(if ran {
            misses.saturating_add(LATER_ATTEMPT)
        } else {
            misses.saturating_mul(4)
        });
        ran
    }

    /// Match every connected element whose selector result is stale, in
    /// parallel, if there are at least `min_elements` of them and their
    /// estimated matching work, at `element_cost` each, reaches `MIN_WORK`.
    pub(super) fn match_stale_with(
        &self,
        pool: &style_pool::Pool,
        min_elements: usize,
        element_cost: Duration,
    ) -> bool {
        let started = Instant::now();
        let index = self.style_index();
        let epoch = self.selector_epoch;
        let view = self.style_view();

        // Every tree scope with rules gets a slot; slot 0 matches nothing.
        let mut slots: FxHashMap<NodeId, usize> = FxHashMap::default();
        let mut scopes = vec![ScopeRules {
            selectors: Vec::new(),
            buckets: None,
            shadow_host: None,
        }];
        for (&scope, rules) in &index.scopes {
            let Some(buckets) = index.buckets.get(&scope) else {
                continue;
            };
            if scopes.len() > 0xffff {
                return false;
            }
            slots.insert(scope, scopes.len());
            scopes.push(ScopeRules {
                selectors: rules.iter().map(|rule| &rule.selector).collect(),
                buckets: Some(buckets),
                shadow_host: self.shadow_hosts.get(&scope).copied(),
            });
        }

        let stream = Stream {
            entries: (0..self.nodes.len()).map(|_| AtomicU64::new(0)).collect(),
            published: AtomicUsize::new(0),
            complete: AtomicBool::new(false),
            next: AtomicUsize::new(0),
            done: Mutex::new(Vec::new()),
        };
        let mut walk = Walk {
            stack: vec![(DOCUMENT, slots.get(&DOCUMENT).copied().unwrap_or(0))],
            count: 0,
        };
        // Find enough work before involving other threads.
        let min_elements = min_elements
            .max(1)
            .max((MIN_WORK.as_nanos() / element_cost.as_nanos().max(1)) as usize);
        if !walk.advance(self, &slots, &stream, min_elements) || walk.count < min_elements {
            return false;
        }

        let scopes = &scopes;
        let stream = &stream;
        // SAFETY: the view is shared only with this pass's job, and
        // `Pool::run` keeps this thread inside the pass until every worker
        // has left it. Meanwhile this thread reads the tree only through
        // `NodesRef` (`Walk`, the matcher) and writes only `selector_cache`
        // (`Store`), never an attribute tendril (see `SharedView::new`).
        let shared = unsafe { style_view::SharedView::new(view) };
        let job = |participant: usize| {
            let mut matcher = Participant::new(participant, shared.view(), scopes);
            while let Some(range) = stream.claim() {
                let chunk = matcher.match_chunk(stream, range);
                stream
                    .done
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(chunk);
            }
        };
        let (mut walked, mut joined) = (Duration::ZERO, 1);
        let stored = pool.run(pool.participants(), &job, |lead| {
            // Publish the rest of the walk chunk by chunk, waking another
            // participant per `WORK_PER_PARTICIPANT` of published work.
            let walk_started = Instant::now();
            let ok = {
                let _published = Published(stream);
                loop {
                    stream.published.store(walk.count, Ordering::Release);
                    lead.wake(participants_for(work(walk.count, element_cost)));
                    if walk.stack.is_empty() {
                        break true;
                    }
                    if !walk.advance(self, &slots, stream, walk.count + CHUNK) {
                        break false;
                    }
                }
            };
            walked = walk_started.elapsed();
            joined = lead.participants();
            if !ok {
                return None;
            }
            // Store finished chunks; match one whenever none is waiting.
            let mut store = Store {
                cache: self.selector_cache.borrow_mut(),
                epoch,
                shared_lists: FxHashMap::default(),
                tables: vec![Vec::new(); pool.participants()],
                stored: 0,
                candidates: 0,
                busy: Duration::ZERO,
                time: Duration::ZERO,
            };
            let mut own = Participant::new(0, shared.view(), scopes);
            while store.stored < walk.count && !lead.worker_panicked() {
                let ready =
                    std::mem::take(&mut *stream.done.lock().unwrap_or_else(|e| e.into_inner()));
                if !ready.is_empty() {
                    for chunk in ready {
                        store.add(stream, chunk);
                    }
                    // Finished chunks measure the cost. If the estimate was
                    // well short, wake whom the remaining work repays.
                    let cost = store.busy / store.stored.max(1) as u32;
                    if store.stored >= 2 * CHUNK && cost > element_cost * 2 {
                        lead.wake(participants_for(work(walk.count - store.stored, cost)));
                    }
                } else if let Some(range) = stream.claim() {
                    store.add(stream, own.match_chunk(stream, range));
                } else {
                    std::hint::spin_loop();
                }
            }
            Some((
                store.shared_lists.len(),
                store.candidates,
                store.busy,
                store.time,
            ))
        });
        let Some((distinct, candidates, busy, merging)) = stored else {
            return false;
        };
        let count = walk.count;
        self.parallel_match
            .measured
            .set(Some(busy / count.max(1) as u32));
        if casc_diag_on() {
            let wall = started.elapsed();
            casc_bump(|d| {
                d.parallel_passes += 1;
                d.parallel_elements += count as u64;
                d.parallel_distinct += distinct as u64;
                d.parallel_candidates += candidates;
                d.parallel_threads = d.parallel_threads.max(joined as u64);
                d.parallel_wall_us += wall.as_micros() as u64;
                d.parallel_collect_us += walked.as_micros() as u64;
                d.parallel_merge_us += merging.as_micros() as u64;
                d.parallel_busy_us += busy.as_micros() as u64;
            });
        }
        if verify_enabled() {
            let elements = (0..count).map(|index| stream.entry(index).0);
            let mismatches = self.parallel_match_mismatches(elements);
            eprintln!("STYLEVERIFY checked={count} mismatches={mismatches}");
        }
        true
    }

    /// Compare cached results with the lazy serial computation.
    pub(super) fn parallel_match_mismatches(
        &self,
        elements: impl Iterator<Item = NodeId>,
    ) -> usize {
        let index = self.style_index();
        let view = self.style_view();
        let mut mismatches = 0;
        for id in elements {
            let scope = self.tree_scope(id);
            let expected = match (index.scopes.get(&scope), index.buckets.get(&scope)) {
                (Some(rules), Some(buckets)) => {
                    let mut ancestors = None;
                    let mut out = Vec::new();
                    view.match_rules(
                        id,
                        buckets,
                        |ri| &rules[ri as usize].selector,
                        self.shadow_hosts.get(&scope).copied(),
                        ClassMemo::Document(&self.class_cache),
                        |required| {
                            ancestors
                                .get_or_insert_with(|| rule_index::Ancestors::of(&view, id))
                                .may_match(&view, required)
                        },
                        &mut Vec::new(),
                        &mut out,
                    );
                    out
                }
                _ => Vec::new(),
            };
            let actual = self
                .selector_cache
                .borrow()
                .get(id, self.selector_epoch)
                .map(|rules| rules.as_ref().clone());
            if actual.as_ref() != Some(&expected) {
                mismatches += 1;
                if mismatches <= 8 {
                    eprintln!(
                        "STYLEVERIFY mismatch node={id} tag={:?} parallel={actual:?} serial={expected:?}",
                        self.tag_name(id)
                    );
                }
            }
        }
        mismatches
    }
}

fn verify_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("TRUST_STYLE_VERIFY").is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Several hundred elements exercising every matcher feature that reads
    /// the tree or document state, with long class lists and repetition so
    /// passes span many chunks.
    fn fixture() -> String {
        let mut html = String::from(
            r#"<!doctype html><html lang=en><head><style>
            * { margin:0 }
            main .card > h2 { color:red }
            .card:nth-child(2n+1) .title { width:1px }
            .card + .card { height:2px }
            .card ~ aside p { top:3px }
            :is(.a, .b) :where(span, em):not(.x) { left:4px }
            section:has(> .flag) p { right:5px }
            section:has(+ aside) { bottom:6px }
            [data-k^=pre] { padding:1px }
            [data-k$=FIX i] { margin:2px }
            [DATA-ALT] { border-width:3px }
            li:first-child, li:last-of-type { border-width:1px }
            ul li:nth-last-child(odd of .item) { opacity:.5 }
            .long-class-name-number-07.another-long-class-name-for-tokens { color:blue }
            #unique .deep b { color:green }
            main :not(.missing) :not(.absent) :not(.gone) b { min-width:7px }
            input:checked + label, input:disabled, input:placeholder-shown { color:gray }
            input:indeterminate, input:required, textarea:read-write { max-width:9px }
            fieldset:disabled input:enabled { min-height:8px }
            p:empty, p:only-child { display:block }
            a:any-link { color:purple }
            div:lang(fr) span { quotes:none }
            span:dir(rtl) { color:orange }
            em:first-of-type:nth-of-type(1) { font-style:normal }
            .card:hover h2 { color:black }
            :root .card .title::before { content:"x" }
            body > main > section.card:nth-child(3) ~ .card { left:1px }
            </style></head><body><main id=unique class="a">"#,
        );
        let long =
            "long-class-name-number-07 padding-class another-long-class-name-for-tokens trailing";
        for i in 0..48 {
            let lang = if i % 7 == 0 { " lang=fr" } else { "" };
            let dir = if i % 5 == 0 { " dir=rtl" } else { "" };
            html.push_str(&format!(
                r#"<section class="card {}"{lang}{dir}><h2 class=title data-k=prefix{i}>T{i}</h2>
                <div class="deep {long}"><b>bold</b><span class=x>s</span><em>e</em><em>f</em></div>
                <ul><li class=item>1</li><li>2</li><li class=item>3</li><li class=item DATA-ALT=y>4</li></ul>
                <p></p><p><a href="/{i}">link</a></p>{}</section>"#,
                if i % 2 == 0 { "b" } else { "c" },
                if i % 3 == 0 { "<span class=flag></span>" } else { "" },
            ));
            if i % 9 == 0 {
                html.push_str(&format!(
                    r#"<aside><p data-k=suFIX>aside</p><fieldset disabled><input required>
                    <input type=checkbox checked><label>l</label><input placeholder=hint>
                    <input type=radio name=g{i}><textarea></textarea></fieldset></aside>"#
                ));
            }
        }
        html.push_str(
            "<div id=host class=card><b slot=one class=light>light</b></div></main></body></html>",
        );
        html
    }

    fn attach_shadow_tree(dom: &mut Dom) {
        let host = dom.get_by_id("host").unwrap();
        let root = dom.attach_shadow(host);
        let children = dom.parse_fragment_into(
            "div",
            r#"<style>
              :host { color:red } .inner > :not(.absent) { height:2px }
              .inner b, slot + i { width:3px } ::slotted(.light) { top:4px }
            </style><div class=inner><b>inside</b><slot name=one></slot><i>after</i></div>"#,
        );
        for child in children {
            dom.append(root, child);
        }
    }

    fn elements(dom: &Dom) -> Vec<NodeId> {
        dom.composed_descendants(DOCUMENT)
            .into_iter()
            .filter(|&node| dom.tag_name(node).is_some())
            .collect()
    }

    /// Every element's cached result equals both the lazy path and a scan of
    /// every rule in its tree scope with the full matcher.
    fn assert_cache_complete_and_exact(dom: &Dom) {
        let nodes = elements(dom);
        assert_eq!(dom.parallel_match_mismatches(nodes.iter().copied()), 0);
        let index = dom.style_index();
        for node in nodes {
            let scope = dom.tree_scope(node);
            let expected: Vec<u32> = index.scopes.get(&scope).map_or_else(Vec::new, |rules| {
                rules
                    .iter()
                    .enumerate()
                    .filter(|(_, rule)| dom.matches_complex(node, &rule.selector.0, None))
                    .map(|(ri, _)| ri as u32)
                    .collect()
            });
            let cached = dom
                .selector_cache
                .borrow()
                .get(node, dom.selector_epoch)
                .cloned();
            assert_eq!(
                cached.as_deref(),
                Some(&expected),
                "node {node} {:?}",
                dom.tag_name(node)
            );
        }
    }

    #[test]
    fn parallel_pass_equals_serial_matching_across_mutations() {
        let mut dom = Dom::parse_document(&fixture());
        attach_shadow_tree(&mut dom);
        let pool = style_pool::Pool::new(4);
        let host = dom.get_by_id("host").unwrap();
        let main = dom.get_by_id("unique").unwrap();
        for step in 0..6 {
            match step {
                1 => dom.set_attr(main, "class", "b"),
                2 => {
                    let target = dom.descendants(main).nth(40);
                    dom.set_hover_chain(target);
                }
                3 => {
                    let card = dom
                        .descendants(main)
                        .find(|&n| dom.tag_name(n) == Some("section"));
                    dom.detach(card.unwrap());
                }
                4 => dom.set_attr(host, "class", "card a"),
                5 => {
                    let style = dom.create_element("style");
                    let text = dom.create_text(".card .deep span { color:teal }");
                    dom.append(style, text);
                    dom.append(main, style);
                }
                _ => {}
            }
            let stale = elements(&dom)
                .into_iter()
                .filter(|&n| {
                    dom.selector_cache
                        .borrow()
                        .get(n, dom.selector_epoch)
                        .is_none()
                })
                .count();
            assert_eq!(
                dom.match_stale_with(&pool, 1, Duration::from_millis(1)),
                stale > 0,
                "step {step}"
            );
            assert_cache_complete_and_exact(&dom);
        }
    }

    #[test]
    fn single_participant_and_large_chunks_agree() {
        let mut dom = Dom::parse_document(&fixture());
        attach_shadow_tree(&mut dom);
        let pool = style_pool::Pool::new(1);
        assert!(dom.match_stale_with(&pool, 1, Duration::from_millis(1)));
        assert_cache_complete_and_exact(&dom);
    }

    #[test]
    fn concurrent_documents_share_the_pool_safely() {
        let threads: Vec<_> = (0..3)
            .map(|_| {
                std::thread::spawn(|| {
                    for _ in 0..3 {
                        let mut dom = Dom::parse_document(&fixture());
                        attach_shadow_tree(&mut dom);
                        let pool = style_pool::pool();
                        if let Some(pool) = pool {
                            assert!(dom.match_stale_with(pool, 1, Duration::from_millis(1)));
                        }
                        assert_cache_complete_and_exact(&dom);
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
    }

    #[test]
    fn lazy_reads_trigger_one_pass_per_broad_invalidation() {
        let mut dom = Dom::parse_document(&fixture());
        // Timing-independent: as if a pass had measured a microsecond each.
        dom.parallel_match
            .measured
            .set(Some(Duration::from_micros(1)));
        let nodes = elements(&dom);
        for node in &nodes {
            let _ = dom.computed_style(*node, "color");
        }
        let after_first = dom.parallel_match.passes.get();
        if style_pool::configured_participants().is_some() {
            assert_eq!(after_first, 1);
        }
        assert_cache_complete_and_exact(&dom);
        // Text-only reads never count as selector misses.
        for node in dom.descendants(DOCUMENT).collect::<Vec<_>>() {
            let _ = dom.computed_style(node, "color");
        }
        assert_eq!(dom.parallel_match.passes.get(), after_first);
        let style = dom.create_element("style");
        let text = dom.create_text("p { color:red }");
        dom.append(style, text);
        let head = dom
            .descendants(DOCUMENT)
            .find(|&n| dom.tag_name(n) == Some("head"));
        dom.append(head.unwrap(), style);
        dom.parallel_match
            .measured
            .set(Some(Duration::from_micros(1)));
        for node in &nodes {
            let _ = dom.computed_style(*node, "color");
        }
        if style_pool::configured_participants().is_some() {
            assert_eq!(dom.parallel_match.passes.get(), after_first + 1);
        }
        assert_cache_complete_and_exact(&dom);
    }
}
