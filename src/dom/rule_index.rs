//! Necessary subject keys for rule-hash candidate rejection.
//!
//! Selectors 4 §§4.2, 4.4, 19.3: `:is()` and `:where()` match the union of
//! their arguments; each argument's rightmost compound matches the subject.
//! Every argument must supply a necessary key before that union is indexed.
//! This does not expand/rewrite selectors or change specificity. Negation,
//! relational/positional selectors and uncertain alternatives keep the full
//! matcher fallback. The index can admit extra candidates, never omit a match.
//! §§6.1–6.2 require attribute presence for every attribute comparison.
//! §19.3 permits rejecting a child/descendant chain when a required ancestor
//! key is absent. Its sibling, scope and featureless-host semantics still
//! belong to the full matcher. Local CSSWG snapshot: 81c27f686901 (2026-09-06).

use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Key<'a> {
    Id(&'a str),
    Class(&'a str),
    Tag(&'a str),
    Attribute(&'a str),
}

// Bound both retained bucket fanout and extra analysis of adversarial nesting.
// Exceeding any budget makes only the optimization unavailable.
const MAX_KEYS: usize = 64;
const MAX_DEPTH: usize = 16;
const MAX_VISITS: usize = 256;

pub(super) fn subject_keys(compound: &Compound) -> Option<Vec<Key<'_>>> {
    let mut remaining = MAX_VISITS;
    keys(compound, 0, &mut remaining)
}

fn keys<'a>(compound: &'a Compound, depth: usize, remaining: &mut usize) -> Option<Vec<Key<'a>>> {
    if depth >= MAX_DEPTH || *remaining == 0 {
        return None;
    }
    *remaining -= 1;
    if let Some(id) = &compound.id {
        return Some(vec![Key::Id(id)]);
    }
    if let Some(class) = compound.classes.first() {
        return Some(vec![Key::Class(class)]);
    }
    if let Some(tag) = compound.tag.as_deref().filter(|tag| *tag != "*") {
        return Some(vec![Key::Tag(tag)]);
    }
    // Every supported attribute comparison requires the attribute to exist
    // (Selectors 4 #attribute-representation / #attribute-substrings).
    if let Some(attribute) = compound.attrs.first() {
        return Some(vec![Key::Attribute(&attribute.name)]);
    }
    // Multiple logical pseudo-classes in one compound are conjunctive. One
    // fully indexable group's union is a sufficient necessary condition.
    for (group, _) in &compound.selects {
        if *remaining == 0 {
            break;
        }
        if group.is_empty() {
            continue;
        }
        let mut union = Vec::new();
        let complete = group.iter().all(|selector| {
            let Some((_, subject)) = selector.0.last() else {
                return false;
            };
            let Some(alternatives) = keys(subject, depth + 1, remaining) else {
                return false;
            };
            for key in alternatives {
                if !union.contains(&key) {
                    if union.len() == MAX_KEYS {
                        return false;
                    }
                    union.push(key);
                }
            }
            true
        });
        if complete {
            return Some(union);
        }
    }
    None
}

/// A rejection filter, never a selector result. Hash collisions admit extra
/// candidates. The full matcher still decides order, ancestry, scope and state.
pub(super) struct Ancestors {
    bits: [u64; 32],
    next: Option<NodeId>,
    visits: usize,
    keys: usize,
    exhausted: bool,
}

#[cfg(test)]
thread_local! {
    static ANCESTOR_VISITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Lazy necessary-key searches through the subject's selector ancestors.
/// Cache only queried keys, and advance each search only as far as matching
/// needs. Full prefix matching preserves scope/state; hash collisions fall
/// back. A successful near ancestor never forces a scan of the whole chain.
pub(super) struct AncestorMatches {
    subject: NodeId,
    shadow_host: Option<NodeId>,
    searches: FxHashMap<u64, AncestorSearch>,
    entries: usize,
    key_bytes: usize,
    disabled: bool,
}

struct AncestorSearch {
    kind: u8,
    name: String,
    nodes: Vec<NodeId>,
    next: Option<NodeId>,
}

pub(super) enum AncestorCandidate {
    Unavailable,
    End,
    Node(NodeId),
}

impl AncestorMatches {
    const MAX_EDGES: usize = 4096;
    const MAX_KEY_BYTES: usize = 64 * 1024;

    pub(super) fn new(subject: NodeId, shadow_host: Option<NodeId>) -> Self {
        Self {
            subject,
            shadow_host,
            searches: FxHashMap::default(),
            entries: 0,
            key_bytes: 0,
            disabled: false,
        }
    }

    pub(super) fn candidate(
        &mut self,
        dom: &Dom,
        subject: NodeId,
        compound: &Compound,
        context: SelectorContext<'_>,
        index: usize,
    ) -> AncestorCandidate {
        if self.disabled || subject != self.subject || context.shadow_host != self.shadow_host {
            return AncestorCandidate::Unavailable;
        }
        let key = compound
            .id
            .as_deref()
            .map(|s| (1, s))
            .or_else(|| compound.classes.first().map(|s| (0, s.as_str())))
            .or_else(|| {
                compound
                    .tag
                    .as_deref()
                    .filter(|s| *s != "*")
                    .map(|s| (2, s))
            });
        let Some((kind, value)) = key else {
            return AncestorCandidate::Unavailable;
        };
        let entry = match self.searches.entry(key_hash(kind, value)) {
            std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::hash_map::Entry::Vacant(entry) => {
                if self.entries == Self::MAX_EDGES
                    || value.len() > Self::MAX_KEY_BYTES - self.key_bytes
                {
                    self.disabled = true;
                    return AncestorCandidate::Unavailable;
                }
                self.entries += 1;
                self.key_bytes += value.len();
                entry.insert(AncestorSearch {
                    kind,
                    name: value.into(),
                    nodes: Vec::new(),
                    next: dom.selector_parent_in(subject, context),
                })
            }
        };
        if entry.kind != kind || entry.name != value {
            return AncestorCandidate::Unavailable;
        }
        while entry.nodes.len() <= index {
            let Some(node) = entry.next else {
                return AncestorCandidate::End;
            };
            entry.next = dom.selector_parent_in(node, context);
            let matches = match kind {
                0 => dom.matches_classes(node, std::slice::from_ref(&entry.name)),
                1 => dom.attr(node, "id") == Some(value),
                _ => dom.tag_name(node) == Some(value),
            };
            if matches {
                if self.entries == Self::MAX_EDGES {
                    self.disabled = true;
                    return AncestorCandidate::Unavailable;
                }
                self.entries += 1;
                entry.nodes.push(node);
            }
        }
        AncestorCandidate::Node(entry.nodes[index])
    }
}

/// Results of recursive Selectors 4 #match-against-element subproblems during
/// ONE element's rule matching. Prefixes can otherwise revisit the same
/// ancestors exponentially. No result outlives this immutable match pass;
/// pointer identities refer only to the live stylesheet's selector storage.
type MatchKey = (NodeId, usize, usize, Option<NodeId>, Option<NodeId>);

#[derive(Default)]
pub(super) struct MatchMemo {
    values: FxHashMap<MatchKey, bool>,
    #[cfg(test)]
    computed: usize,
}

impl MatchMemo {
    const MAX_ENTRIES: usize = 4096;

    pub(super) fn get(
        &self,
        node: NodeId,
        parts: &[(Combinator, Compound)],
        context: SelectorContext<'_>,
    ) -> Option<bool> {
        self.values
            .get(&(
                node,
                parts.as_ptr() as usize,
                parts.len(),
                context.scope,
                context.shadow_host,
            ))
            .copied()
    }

    pub(super) fn put(
        &mut self,
        node: NodeId,
        parts: &[(Combinator, Compound)],
        context: SelectorContext<'_>,
        matched: bool,
    ) {
        #[cfg(test)]
        {
            self.computed += 1;
        }
        if self.values.len() < Self::MAX_ENTRIES {
            self.values.insert(
                (
                    node,
                    parts.as_ptr() as usize,
                    parts.len(),
                    context.scope,
                    context.shadow_host,
                ),
                matched,
            );
        }
    }
}

fn key_hash(kind: u8, value: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hash = rustc_hash::FxHasher::default();
    kind.hash(&mut hash);
    value.hash(&mut hash);
    hash.finish()
}

fn bits(kind: u8, value: &str) -> [u16; 2] {
    let hash = key_hash(kind, value);
    [(hash & 2047) as u16, ((hash >> 32) & 2047) as u16]
}

pub(super) fn ancestor_requirements(selector: &Complex) -> Vec<u16> {
    let mut required = Vec::new();
    // A chain of child/descendant combinators gives necessary ancestors.
    // Stop at a sibling combinator: that compound need not be an ancestor.
    for position in (1..selector.0.len()).rev() {
        if !matches!(
            selector.0[position].0,
            Combinator::Child | Combinator::Descendant
        ) {
            break;
        }
        let compound = &selector.0[position - 1].1;
        for (kind, value) in compound
            .classes
            .iter()
            .map(|s| (0, s.as_str()))
            .chain(compound.id.iter().map(|s| (1, s.as_str())))
            .chain(
                compound
                    .tag
                    .as_deref()
                    .filter(|s| *s != "*")
                    .map(|s| (2, s)),
            )
        {
            // Any subset of necessary keys is safe; bound retained analysis.
            if required.len() >= 32 {
                return required;
            }
            required.extend(bits(kind, value));
        }
    }
    required
}

impl Ancestors {
    pub(super) fn of(dom: &Dom, id: NodeId) -> Self {
        Self {
            bits: [0; 32],
            next: dom.parent_composed(id),
            visits: 0,
            keys: 0,
            exhausted: false,
        }
    }

    pub(super) fn may_match(&mut self, dom: &Dom, required: &[u16]) -> bool {
        loop {
            if self.exhausted
                || required
                    .iter()
                    .all(|&bit| self.bits[bit as usize / 64] & (1 << (bit % 64)) != 0)
            {
                return true;
            }
            let Some(node) = self.next else {
                return false;
            };
            // Rejection is optional. Bound its extra traversal when the
            // subject's full compound might reject before ancestry matters.
            // An incomplete filter must always admit the full matcher.
            if self.visits == 64 {
                self.exhausted = true;
                return true;
            }
            self.visits += 1;
            #[cfg(test)]
            ANCESTOR_VISITS.set(ANCESTOR_VISITS.get() + 1);
            self.next = dom.parent_composed(node);
            // Composed ancestry is a conservative superset of selector
            // ancestry, including shadow hosts. Slot distribution does not
            // replace a light-DOM element's selector parent.
            for (kind, value) in dom
                .attr(node, "class")
                .into_iter()
                .flat_map(str::split_ascii_whitespace)
                .map(|s| (0, s))
                .chain(dom.attr(node, "id").map(|s| (1, s)))
                .chain(dom.tag_name(node).map(|s| (2, s)))
            {
                if self.keys == 256 {
                    self.exhausted = true;
                    return true;
                }
                self.keys += 1;
                for bit in bits(kind, value) {
                    self.bits[bit as usize / 64] |= 1 << (bit % 64);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore]
    fn selector_workload_profile() {
        let html = if let Some(path) = std::env::var_os("TRUST_SELECTOR_PROFILE_FILE") {
            std::fs::read_to_string(path).unwrap()
        } else if std::env::var_os("TRUST_SELECTOR_PROFILE_DEEP").is_some() {
            let mut html = String::from("<style>");
            for n in 0..2000 {
                html.push_str(&format!(".scope[data-variant='{n}'] b {{ color:red }}"));
            }
            html.push_str("</style><main class=scope data-variant=123>");
            html.push_str(&"<div>".repeat(100));
            html.push_str("<b>target</b>");
            html.push_str(&"</div>".repeat(100));
            html.push_str("</main>");
            html
        } else {
            let mut html = String::from("<style>");
            for n in 0..1000 {
                html.push_str(&format!(".scope-{n} :not(.excluded) > :not(.empty) {{ color:red }} :is([data-{n}], .subject-{n}) {{ width:1px }}"));
            }
            html.push_str("</style><main class=scope-123>");
            html.push_str(&"<section><p data-123=yes>layout content</p></section>".repeat(220));
            html.push_str("</main>");
            html
        };
        let dom = Dom::parse_document(&html);
        let index = dom.style_index();
        let Some(rules) = index.scopes.get(&DOCUMENT) else {
            eprintln!("SELECTOR_PROFILE no document-scoped stylesheet rules");
            return;
        };
        let buckets = &index.buckets[&DOCUMENT];
        let nodes: Vec<_> = dom
            .descendants(DOCUMENT)
            .filter(|&n| dom.tag_name(n).is_some())
            .collect();
        // Compare the exact same rule set with ancestor rejection disabled.
        // Both paths retain subject indexing and must return identical matches.
        let mut unfiltered = RuleBuckets::build(rules);
        unfiltered.ancestors.clear();
        let mut reference = None;
        for (name, buckets, recursive) in [
            ("subject-only", &unfiltered, false),
            ("ancestors", buckets, false),
            ("recursive-reuse", buckets, true),
        ] {
            let mut samples = Vec::new();
            for _ in 0..11 {
                let start = std::time::Instant::now();
                let mut candidates = Vec::new();
                let mut count = 0;
                let mut matches = Vec::new();
                for &node in &nodes {
                    let memo = RefCell::new(MatchMemo::default());
                    let mut context = dom.selector_context(node, None);
                    let ancestors = recursive
                        .then(|| RefCell::new(AncestorMatches::new(node, context.shadow_host)));
                    if recursive {
                        context.memo = Some(&memo);
                        context.ancestors = ancestors.as_ref();
                    }
                    candidates.clear();
                    buckets.candidates(&dom, node, &mut candidates);
                    count += candidates.len();
                    matches.extend(
                        candidates
                            .iter()
                            .copied()
                            .filter(|&ri| {
                                dom.matches_complex_uncached(
                                    node,
                                    &rules[ri as usize].selector.0,
                                    context,
                                )
                            })
                            .map(|ri| (node, ri)),
                    );
                }
                samples.push((start.elapsed(), count));
                if let Some(reference) = &reference {
                    assert_eq!(&matches, reference);
                } else {
                    reference = Some(matches);
                }
            }
            samples.sort_unstable();
            eprintln!(
                "SELECTOR_MATCH mode={name} median_us={} candidates={} matches={}",
                samples[5].0.as_micros(),
                samples[5].1,
                reference.as_ref().unwrap().len()
            );
        }
        fn compound(c: &Compound, depth: usize) -> serde_json::Value {
            if depth > 6 {
                return serde_json::json!("depth limit");
            }
            serde_json::json!({
                "tag":c.tag, "id":c.id, "classes":c.classes,
                "attrs":c.attrs.iter().map(|a| &a.name).collect::<Vec<_>>(),
                "selects":c.selects.iter().map(|(group,_)| group.iter().map(|c| complex(c,depth+1)).collect::<Vec<_>>()).collect::<Vec<_>>(),
                "nots":c.nots.len(), "has":c.has.len(), "never":c.never,
                "states":c.states.len(), "structural":c.structural.len(), "root":c.root,
            })
        }
        fn complex(c: &Complex, depth: usize) -> serde_json::Value {
            serde_json::json!(
                c.0.iter()
                    .map(|(_, c)| compound(c, depth))
                    .collect::<Vec<_>>()
            )
        }
        let mut expensive = Vec::new();
        for &ri in &buckets.universal {
            let rule = &rules[ri as usize];
            let start = std::time::Instant::now();
            let hits = nodes
                .iter()
                .filter(|&&n| dom.matches_complex(n, &rule.selector.0, None))
                .count();
            expensive.push((start.elapsed(), ri, hits));
        }
        expensive.sort_by_key(|(time, _, _)| std::cmp::Reverse(*time));
        eprintln!(
            "SELECTOR_PROFILE rules={} universal={} nodes={} total_us={}",
            rules.len(),
            buckets.universal.len(),
            nodes.len(),
            expensive.iter().map(|v| v.0.as_micros()).sum::<u128>()
        );
        for (time, ri, hits) in expensive.iter().take(20) {
            eprintln!(
                "SELECTOR_COST us={} hits={} selector={}",
                time.as_micros(),
                hits,
                complex(&rules[*ri as usize].selector, 0)
            );
        }
    }

    fn assert_candidates_cover_full_scan(dom: &Dom) {
        let index = dom.style_index();
        for node in dom.live_ids() {
            if dom.tag_name(node).is_none() {
                continue;
            }
            let scope = dom.tree_scope(node);
            let Some(rules) = index.scopes.get(&scope) else {
                continue;
            };
            let mut candidates = Vec::new();
            index.buckets[&scope].candidates(dom, node, &mut candidates);
            assert!(
                candidates.windows(2).all(|pair| pair[0] < pair[1]),
                "candidate union must be unique"
            );
            let actual = candidates
                .into_iter()
                .filter(|ri| dom.matches_complex(node, &rules[*ri as usize].selector.0, None))
                .collect::<Vec<_>>();
            let expected = rules
                .iter()
                .enumerate()
                .filter(|(_, rule)| dom.matches_complex(node, &rule.selector.0, None))
                .map(|(ri, _)| ri as u32)
                .collect::<Vec<_>>();
            assert_eq!(
                actual, expected,
                "candidate rejection omitted a match on node {node}"
            );
            let context = dom.selector_context(node, None);
            let ancestors = RefCell::new(AncestorMatches::new(node, context.shadow_host));
            let context = SelectorContext {
                ancestors: Some(&ancestors),
                ..context
            };
            let indexed = rules
                .iter()
                .enumerate()
                .filter(|(_, rule)| dom.matches_complex_uncached(node, &rule.selector.0, context))
                .map(|(ri, _)| ri as u32)
                .collect::<Vec<_>>();
            assert_eq!(
                indexed, expected,
                "ancestor buckets omitted or added a match on {node}"
            );
            assert_eq!(
                *dom.matched_rules(node),
                expected,
                "cached matcher disagrees on node {node}"
            );
        }
    }

    #[test]
    fn cursor_candidates_preserve_cascade_and_avoid_unrelated_elements() {
        let mut dom = Dom::parse_document(
            r#"<style>
            * { box-sizing:border-box; margin:0 }
            .active :is(.target, #alternate) { cursor:pointer }
            [data-cursor] { cursor:var(--cursor) }
            .target:hover { cursor:crosshair }
            </style><main id=root class=active><a id=target class=target>one</a>
            <span id=alternate>two</span><span id=inline style='cursor:pointer'>three</span>
            <span id=variable data-cursor style='--cursor:pointer'>four</span>
            <p id=unrelated>unrelated</p></main>"#,
        );
        // Detached feature probes must not disable candidate rejection for
        // the whole document merely because a shadow root exists somewhere.
        let probe = dom.create_element("div");
        dom.attach_shadow(probe);
        for active in ["active", ""] {
            dom.set_attr(dom.get_by_id("root").unwrap(), "class", active);
            let nodes = dom.composed_descendants(DOCUMENT);
            let candidates = dom.cursor_style_candidates(&nodes);
            let pointers = |ids: &[NodeId]| {
                ids.iter()
                    .copied()
                    .filter(|&node| {
                        dom.computed_style(node, "cursor").as_deref() == Some("pointer")
                    })
                    .collect::<Vec<_>>()
            };
            assert_eq!(pointers(&candidates), pointers(&nodes));
            assert!(!candidates.contains(&dom.get_by_id("unrelated").unwrap()));
        }
        let unrelated = dom.get_by_id("unrelated").unwrap();
        dom.set_attr(unrelated, "style", "cursor:pointer");
        assert!(
            dom.cursor_style_candidates(&dom.composed_descendants(DOCUMENT))
                .contains(&unrelated)
        );
    }

    #[test]
    fn rule_index_logical_subjects_preserve_full_matching_and_specificity() {
        let mut dom = Dom::parse_document(
            r#"<style>
            .prose :where(p, h2, h3):not(:where(.not-prose *)) { margin:2px }
            .prose :where(a strong, thead th strong, .emphasis) { padding:3px }
            :is(#one, .red, span):where(.active, .another) { top:4px }
            :where(:is(.red, .blue), :where(.green, .yellow)) { left:5px }
            :where(.m\:thing) { right:6px }
            :is(:not(.blocked), .red) { bottom:7px }
            :not(:is(.hidden, .disabled)) { min-height:8px }
            :has(> .mark) { min-width:9px }
            :nth-child(2 of :is(.red, .blue)) { max-height:10px }
            :where([data-any], .red) { max-width:11px }
            :is(.red, *) { border-width:1px }
            .red { color:red; height:10px }
            :where(.red) { color:blue }
            :is(#absent, .red) { height:20px }
        </style><main class="prose"><p id="one" class="red red active m:thing"><strong class="mark">one</strong></p>
            <div class="not-prose"><p id="two" data-any="yes" class="blue">two</p></div>
            <a><strong>three</strong></a><span class="emphasis green another"></span></main>"#,
        );
        assert_candidates_cover_full_scan(&dom);
        let one = dom.get_by_id("one").unwrap();
        assert_eq!(dom.computed_style(one, "color").as_deref(), Some("red"));
        assert_eq!(dom.computed_style(one, "height").as_deref(), Some("20px"));
        dom.set_attr(one, "class", "green another");
        assert_candidates_cover_full_scan(&dom);
        dom.remove_attr(dom.get_by_id("two").unwrap(), "data-any");
        assert_candidates_cover_full_scan(&dom);
    }

    #[test]
    fn rule_index_rejects_impossible_functional_candidates_before_matching() {
        let mut css = String::new();
        for index in 0..500 {
            css.push_str(&format!(".scope :where(.target-{index}) {{ color:red }}"));
        }
        let dom = Dom::parse_document(&format!(
            "<style>{css}</style><main class=scope><span id=target class=target-123></span><span id=unrelated></span></main>"
        ));
        assert_candidates_cover_full_scan(&dom);
        let index = dom.style_index();
        let buckets = &index.buckets[&DOCUMENT];
        assert!(
            buckets.universal.is_empty(),
            "functional class subjects must not enter the universal bucket"
        );
        let mut candidates = Vec::new();
        buckets.candidates(&dom, dom.get_by_id("target").unwrap(), &mut candidates);
        assert_eq!(
            candidates.len(),
            1,
            "499 impossible rules must not reach the full matcher"
        );
        candidates.clear();
        buckets.candidates(&dom, dom.get_by_id("unrelated").unwrap(), &mut candidates);
        assert!(candidates.is_empty());
    }

    #[test]
    fn rule_index_fanout_and_depth_limits_preserve_universal_fallback() {
        let wide = format!(
            ":is({})",
            (0..MAX_KEYS + 1)
                .map(|n| format!(".c{n}"))
                .collect::<Vec<_>>()
                .join(",")
        );
        let deep = format!(
            "{}.c0{}",
            ":is(".repeat(MAX_DEPTH + 2),
            ")".repeat(MAX_DEPTH + 2)
        );
        for selector in [&wide, &deep] {
            let parsed = SelectorList::parse(selector).unwrap();
            assert!(subject_keys(&parsed.0[0].0.last().unwrap().1).is_none());
        }
        let dom = Dom::parse_document(&format!(
            "<style>{wide} {{ color:red }} {deep} {{ height:5px }}</style><p class=c0>match</p>"
        ));
        assert_candidates_cover_full_scan(&dom);
        assert_eq!(dom.style_index().buckets[&DOCUMENT].universal.len(), 2);
    }

    #[test]
    fn ancestor_and_attribute_rejection_preserves_mutations_and_combinators() {
        let mut dom = Dom::parse_document(
            r#"<style>
          main.on > section.branch :not(.missing) { color:red }
          #outer .branch > :is([data-kind^=pre], p, [DATA-ALT]) { width:3px }
          .branch + aside > :not(.missing) { height:4px }
          .branch ~ aside :not(.missing) { top:5px }
          main .branch + aside :not(.missing) { left:6px }
          .branch :is(.outer, :not(.outer)) > :not(.missing) { right:7px }
          main:not(.off) :where([data-kind$=fix], [data-alt*=es]) { bottom:8px }
          [data-kind~=prefix] { padding:1px }
          [data-kind|=prefix] { margin:2px }
          [data-alt="YES" i] { border-width:1px }
          :is([absent], .on) > [data-empty=""] { min-width:9px }
          [viewBox] { max-width:10px }
        </style><main id=outer class=on><section id=branch class=branch>
          <p id=subject data-kind=prefix data-alt=yes>subject</p><div><b>deep</b></div>
        </section><aside id=aside><b id=sibling>adjacent</b></aside><div data-empty=""></div>
        <svg viewBox="0 0 10 10"></svg></main>"#,
        );
        assert_candidates_cover_full_scan(&dom);
        let subject = dom.get_by_id("subject").unwrap();
        let branch = dom.get_by_id("branch").unwrap();
        let outer = dom.get_by_id("outer").unwrap();
        let aside = dom.get_by_id("aside").unwrap();
        dom.set_attr(outer, "class", "off");
        assert_candidates_cover_full_scan(&dom);
        dom.set_attr(branch, "class", "");
        assert_candidates_cover_full_scan(&dom);
        dom.append(aside, subject);
        assert_candidates_cover_full_scan(&dom);
        dom.remove_attr(subject, "data-kind");
        dom.set_attr(subject, "DATA-ALT", "test");
        assert_candidates_cover_full_scan(&dom);
        dom.set_attr(branch, "class", "branch");
        dom.set_attr(outer, "class", "on");
        assert_candidates_cover_full_scan(&dom);
    }

    #[test]
    fn ancestor_rejection_preserves_shadow_and_slot_selector_ancestry() {
        let mut dom = Dom::parse_document(
            r#"<style>
            #host > .light { color:red } .outside .light { height:4px }
          </style><main class=outside><div id=host><b id=light class=light>light</b></div></main>"#,
        );
        let host = dom.get_by_id("host").unwrap();
        let root = dom.attach_shadow(host);
        let children = dom.parse_fragment_into("div", r#"<style>
          :host > .inside :not(.absent) { color:green }
          .inside > :not(.absent) { height:2px }
          .outside :not(.absent) { width:99px }
          .inside + section :not(.absent) { top:3px }
        </style><div class=inside><p id=inside>inside</p><slot></slot></div><section><i>sibling</i></section>"#);
        for child in children {
            dom.append(root, child);
        }
        assert_candidates_cover_full_scan(&dom);
        let light = dom.get_by_id("light").unwrap();
        dom.set_attr(host, "class", "inside");
        assert_candidates_cover_full_scan(&dom);
        dom.append(root, light);
        assert_candidates_cover_full_scan(&dom);
    }

    #[test]
    fn absent_ancestors_and_attributes_reject_large_rule_sets() {
        let mut css = String::new();
        for n in 0..1000 {
            css.push_str(&format!(".scope-{n} > :not(.missing) {{ color:red }} :is([data-{n}], .subject-{n}) {{ width:1px }}"));
        }
        let dom = Dom::parse_document(&format!(
            "<style>{css}</style><main class=scope-123><p id=target data-123=yes>match</p><div class=subject-456>second</div></main>"
        ));
        assert_candidates_cover_full_scan(&dom);
        let index = dom.style_index();
        let mut candidates = Vec::new();
        index.buckets[&DOCUMENT].candidates(
            &dom,
            dom.get_by_id("target").unwrap(),
            &mut candidates,
        );
        assert!(
            candidates.len() < 20,
            "filter must reject >99% of impossible candidates, got {}",
            candidates.len()
        );
    }

    #[test]
    fn recursive_match_reuse_preserves_backtracking_scopes_and_state() {
        let mut dom = Dom::parse_document(
            r#"<main id=root><section class=scope><div><div><div><b id=target>text</b></div></div></div></section><section class=other><i>other</i></section></main>"#,
        );
        let selectors = [
            ":is(.missing, #absent) :not(.x) :not(.y) :not(.z)",
            "main :not(.x) :not(.y) :not(.z)",
            ":is(:hover, .scope) :not(:is(.other *))",
            "section:has(> div :is(b, i))",
            "section:has(+ .other)",
            "div:nth-child(1 of :not(:empty)) :not(.x)",
            ":scope > section:has(b)",
        ]
        .map(|s| SelectorList::parse(s).unwrap());
        let target = dom.get_by_id("target").unwrap();
        for step in 0..4 {
            match step {
                1 => {
                    dom.set_hover_chain(Some(target));
                }
                2 => dom.set_attr(target, "class", "x"),
                3 => dom.append(dom.get_by_id("root").unwrap(), target),
                _ => (),
            }
            for node in dom
                .descendants(DOCUMENT)
                .filter(|&n| dom.tag_name(n).is_some())
            {
                let memo = RefCell::new(MatchMemo::default());
                // Exercise both :has anchors and distinct API scoping roots
                // inside the same memo to prove context is part of its key.
                for scope in [None, dom.get_by_id("root"), Some(node)] {
                    let cold = dom.selector_context(node, scope);
                    let warm = SelectorContext {
                        memo: Some(&memo),
                        ..cold
                    };
                    for selector in &selectors {
                        let expected = dom.matches_complex_in(node, &selector.0[0].0, cold);
                        for _ in 0..2 {
                            assert_eq!(
                                dom.matches_complex_in(node, &selector.0[0].0, warm),
                                expected
                            );
                        }
                    }
                }
                assert!(memo.borrow().values.len() <= MatchMemo::MAX_ENTRIES);
            }
        }
    }

    #[test]
    fn recursive_selector_backtracking_has_bounded_subproblems() {
        let dom = Dom::parse_document(&format!(
            "{}<b id=target>text</b>{}",
            "<div>".repeat(24),
            "</div>".repeat(24)
        ));
        let selector = SelectorList::parse(&format!(
            ":is(.absent, #absent){}",
            " :not(.excluded)".repeat(9)
        ))
        .unwrap();
        let target = dom.get_by_id("target").unwrap();
        let memo = RefCell::new(MatchMemo::default());
        let context = SelectorContext {
            memo: Some(&memo),
            ..dom.selector_context(target, None)
        };
        assert!(!dom.matches_complex_uncached(target, &selector.0[0].0, context));
        let computed = memo.borrow().computed;
        assert!(
            (20..1000).contains(&computed),
            "repeated ancestor backtracking: {computed} subproblems"
        );
    }

    #[test]
    fn ancestor_matching_budget_falls_back_without_partial_results() {
        let classes = (0..=AncestorMatches::MAX_EDGES)
            .map(|i| format!("k{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        let dom = Dom::parse_document(&format!(
            "<div class='{classes}'><b id=target>text</b></div>"
        ));
        let node = dom.get_by_id("target").unwrap();
        let base = dom.selector_context(node, None);
        let ancestors = RefCell::new(AncestorMatches::new(node, base.shadow_host));
        for key in 0..=AncestorMatches::MAX_EDGES {
            let selector = SelectorList::parse(&format!(".k{key}")).unwrap();
            let result =
                ancestors
                    .borrow_mut()
                    .candidate(&dom, node, &selector.0[0].0[0].1, base, 0);
            if matches!(result, AncestorCandidate::Unavailable) {
                break;
            }
        }
        assert!(ancestors.borrow().disabled);
        assert!(ancestors.borrow().entries <= AncestorMatches::MAX_EDGES);
        let selector = SelectorList::parse(&format!(".k{} b", AncestorMatches::MAX_EDGES)).unwrap();
        let context = SelectorContext {
            ancestors: Some(&ancestors),
            ..base
        };
        assert!(dom.matches_complex_uncached(node, &selector.0[0].0, context));
    }

    #[test]
    fn unrelated_descendant_rules_do_not_scan_subject_ancestors() {
        let mut dom = Dom::parse_document(&format!(
            "<style>.absent b {{ color:red }} * {{ color:green }}</style>{}text{}",
            "<div>".repeat(500),
            "</div>".repeat(500)
        ));
        ANCESTOR_VISITS.set(0);
        assert_candidates_cover_full_scan(&dom);
        assert_eq!(ANCESTOR_VISITS.get(), 0);

        // Adding the one dependent subject permits its ancestor walk; the
        // unrelated candidates must still avoid quadratic total traversal.
        let leaf = dom
            .descendants(DOCUMENT)
            .filter(|&node| dom.tag_name(node) == Some("div"))
            .last()
            .unwrap();
        let child = dom.create_element("b");
        dom.append(leaf, child);
        ANCESTOR_VISITS.set(0);
        assert_candidates_cover_full_scan(&dom);
        assert!((1..=2 * dom.node_count()).contains(&ANCESTOR_VISITS.get()));
    }

    #[test]
    fn ancestor_rejection_stops_early_and_preserves_budget_fallbacks() {
        let dom = Dom::parse_document(&format!(
            "<style>.scope div {{ color:red }}</style>{}text{}",
            "<div class=scope>".repeat(500),
            "</div>".repeat(500)
        ));
        ANCESTOR_VISITS.set(0);
        assert_candidates_cover_full_scan(&dom);
        // The nearest ancestor suffices on every nested subject. Building
        // the complete ancestry would turn a linear match into quadratic work.
        assert!(ANCESTOR_VISITS.get() <= 2 * dom.node_count());

        let dom = Dom::parse_document(&format!(
            "<style>.absent :hover {{ color:red }}</style>{}text{}",
            "<div>".repeat(500),
            "</div>".repeat(500)
        ));
        ANCESTOR_VISITS.set(0);
        assert_candidates_cover_full_scan(&dom);
        assert!(ANCESTOR_VISITS.get() <= 128 * dom.node_count());

        // A required key beyond either budget must reach the full matcher.
        let classes = (0..600).map(|i| format!("unused-{i} ")).collect::<String>();
        let dom = Dom::parse_document(&format!(
            "<style>.required b {{ color:red }}</style><main class='{classes}required'>{}<b>target</b>{}</main>",
            "<div>".repeat(100),
            "</div>".repeat(100)
        ));
        assert_candidates_cover_full_scan(&dom);
        let dom = Dom::parse_document(&format!(
            "<style>.required b {{ color:red }}</style><main class='{classes}required'><b>target</b></main>"
        ));
        assert_candidates_cover_full_scan(&dom);
    }

    #[test]
    fn ancestor_searches_stop_after_first_sufficient_match() {
        let classes = (0..5000)
            .map(|i| format!("unused-{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        let dom = Dom::parse_document(&format!(
            "<main class='{classes}'><section class=scope><b id=target>text</b></section></main>"
        ));
        let node = dom.get_by_id("target").unwrap();
        let selector = SelectorList::parse(".scope b").unwrap();
        let base = dom.selector_context(node, None);
        let ancestors = RefCell::new(AncestorMatches::new(node, base.shadow_host));
        let context = SelectorContext {
            ancestors: Some(&ancestors),
            ..base
        };
        assert!(dom.matches_complex_uncached(node, &selector.0[0].0, context));
        // One queried key and its one required ancestor. The distant class
        // list must not be indexed, exhaust the budget, or enter this search.
        assert_eq!(ancestors.borrow().entries, 2);
        assert_eq!(ancestors.borrow().searches.len(), 1);
        assert!(!ancestors.borrow().disabled);
    }
}
