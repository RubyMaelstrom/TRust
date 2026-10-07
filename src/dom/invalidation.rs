//! Selector dependency invalidation (Selectors 4 §§4, 6, 14, 15, 19.3).
//!
//! Cache pure selector matches independently from the cascade and container
//! conditions. Attribute writes invalidate the selector subjects reachable
//! through compiled dependency paths, not every element merely because the
//! DOM revision changed. Dependency proofs are owned by a Document, including
//! its shadow trees, rather than the whole presentation arena. Shadow rules
//! reach across trees only through `:host()` and `::slotted()` routes, and
//! restyling follows the flat tree; slot reassignment, hover and focus state
//! in shadow-including trees, and untracked state associations retain an
//! explicit full fallback. Optional cache budgets never truncate content.

use super::*;

mod attributes;

pub(super) use attributes::LiveState;

/// CSSOM #dom-window-getcomputedstyle requires current, live values at reads;
/// it does not require retiring derived cache entries after every write.
/// Keep only stable node IDs and flush before any style/layout observation.
#[derive(Clone, Default)]
pub(super) struct Pending {
    dirty: std::cell::Cell<bool>,
    roots: RefCell<FxHashMap<NodeId, bool>>,
    #[cfg(test)]
    flushes: std::cell::Cell<usize>,
    #[cfg(test)]
    visited: std::cell::Cell<usize>,
}

impl Pending {
    pub(super) fn retain_nodes(&mut self, live: impl Fn(NodeId) -> bool) {
        self.roots.get_mut().retain(|&node, _| live(node));
        self.dirty.set(!self.roots.get_mut().is_empty());
    }

    pub(super) fn retained_bytes(&self) -> usize {
        self.roots.borrow().capacity() * std::mem::size_of::<(NodeId, bool)>()
    }
}

fn deferred_invalidation_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("TRUST_DEFER_STYLE_INVALIDATION").as_deref() != Ok("0"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Impact {
    Element,
    SiblingSubtrees,
}

/// DOM #concept-node-document / #concept-document-tree, Selectors 4
/// #match-against-tree and CSS Cascade 5 #filtering (local CSSWG 81c27f68,
/// DOM a2331a45). Child navigables share storage, not selector dependencies.
///
/// Shadow rules remain in their owning Document's dependency union: :host,
/// ::slotted(), inherited state and slot distribution can cross shadow roots.
/// Actual rule matching remains tree-scoped. A detached node conservatively
/// uses its node Document's proof; adoption updates that identity atomically.
#[derive(Default)]
pub(super) struct SelectorDependencyIndex {
    documents: FxHashMap<NodeId, std::sync::Arc<SelectorDependencies>>,
    empty: SelectorDependencies,
}

impl SelectorDependencyIndex {
    pub(super) fn build(dom: &Dom, scopes: &FxHashMap<NodeId, Vec<StyleRule>>) -> Self {
        let mut result = Self::default();
        let mut documents = FxHashMap::<NodeId, Vec<&StyleRule>>::default();
        for (&scope, rules) in scopes {
            if !rules.is_empty() {
                documents
                    .entry(dom.nodes[scope].owner_document)
                    .or_default()
                    .extend(rules);
            }
        }
        let mut shared =
            FxHashMap::<Vec<*const StyleRuleData>, std::sync::Arc<SelectorDependencies>>::default();
        for (document, mut rules) in documents {
            // Source order does not affect a dependency union. Identity is
            // safe here because the complete rule set remains Rc-owned by
            // this immutable StyleIndex throughout construction and use.
            rules.sort_unstable_by_key(|rule| std::sync::Arc::as_ptr(&rule.data));
            rules.dedup_by(|a, b| std::sync::Arc::ptr_eq(&a.data, &b.data));
            let key = rules
                .iter()
                .map(|rule| std::sync::Arc::as_ptr(&rule.data))
                .collect();
            // Compare the complete identity vector, never just its hash.
            // Shared sheets contribute in EVERY Document without duplicating
            // a potentially large compiled graph for identical rule sets.
            let dependencies = shared.entry(key).or_insert_with(|| {
                let mut dependencies = SelectorDependencies::default();
                for rule in rules {
                    dependencies.record(rule);
                }
                dependencies.finish();
                std::sync::Arc::new(dependencies)
            });
            result.documents.insert(document, dependencies.clone());
        }
        result
    }

    fn for_node(&self, dom: &Dom, node: NodeId) -> &SelectorDependencies {
        self.documents
            .get(&dom.nodes[node].owner_document)
            .map(std::sync::Arc::as_ref)
            .unwrap_or(&self.empty)
    }

    pub(super) fn retained_bytes(&self) -> usize {
        let mut seen = FxHashSet::default();
        self.documents.capacity()
            * std::mem::size_of::<(NodeId, std::sync::Arc<SelectorDependencies>)>()
            + self
                .documents
                .values()
                .filter(|dependencies| seen.insert(std::sync::Arc::as_ptr(dependencies)))
                .map(|dependencies| {
                    std::mem::size_of::<SelectorDependencies>() + dependencies.retained_bytes()
                })
                .sum::<usize>()
    }
}

#[derive(Default)]
pub(super) struct SelectorDependencies {
    attributes: attributes::Dependencies,
    relational: Vec<RelationalDependency>,
    structural: Vec<StructureDependency>,
    empty: bool,
    text_direction: bool,
    text_placeholder: bool,
    /// `:indeterminate` radios read their whole group's checkedness (HTML
    /// #selector-indeterminate), so only a child-list change that moves a radio
    /// or a form can restyle an element outside the changed subtree.
    indeterminate: bool,
    structure_global: bool,
    /// `structural`/`relational` positions bucketed by one necessary key, built
    /// by `finish`. Every child-list change consults them, so a page's many
    /// `:first-child`, sibling and `:has()` rules must not each scan the tree.
    empty_keys: KeyIndex,
    child_index_keys: KeyIndex,
    sibling_left_keys: KeyIndex,
    sibling_right_keys: KeyIndex,
    anchor_keys: KeyIndex,
    sibling_anchors: Vec<u32>,
}

/// Dependencies bucketed by one necessary key of the compound they test, like
/// `RuleBuckets`: a node can only satisfy those under its id, one of its
/// classes or its tag, plus the unkeyed ones.
#[derive(Default)]
struct KeyIndex {
    by_id: FxHashMap<String, Vec<u32>>,
    by_class: FxHashMap<String, Vec<u32>>,
    by_tag: FxHashMap<String, Vec<u32>>,
    unkeyed: Vec<u32>,
}

impl KeyIndex {
    fn insert(&mut self, index: usize, tag: Option<&str>, id: Option<&str>, classes: &[String]) {
        let index = index as u32;
        if let Some(id) = id {
            self.by_id.entry(id.to_string()).or_default().push(index);
        } else if let Some(class) = classes.first() {
            self.by_class.entry(class.clone()).or_default().push(index);
        } else if let Some(tag) = tag.filter(|tag| *tag != "*") {
            self.by_tag.entry(tag.to_string()).or_default().push(index);
        } else {
            self.unkeyed.push(index);
        }
    }

    fn insert_step(&mut self, index: usize, guard: Option<&StructureGuard>) {
        match guard.and_then(|guard| guard.0.last()) {
            Some((_, tag, id, classes)) => {
                self.insert(index, tag.as_deref(), id.as_deref(), classes)
            }
            None => self.insert(index, None, None, &[]),
        }
    }

    fn is_empty(&self) -> bool {
        self.by_id.is_empty()
            && self.by_class.is_empty()
            && self.by_tag.is_empty()
            && self.unkeyed.is_empty()
    }

    /// Calls `visit` with every dependency `node` might satisfy until it
    /// returns true; returns whether it did.
    fn any(&self, dom: &Dom, node: NodeId, mut visit: impl FnMut(u32) -> bool) -> bool {
        if self.unkeyed.iter().any(|&index| visit(index)) {
            return true;
        }
        if !self.by_id.is_empty()
            && let Some(indices) = dom.attr(node, "id").and_then(|id| self.by_id.get(id))
            && indices.iter().any(|&index| visit(index))
        {
            return true;
        }
        if !self.by_class.is_empty()
            && let Some(classes) = dom.attr(node, "class")
        {
            for class in classes.split_ascii_whitespace() {
                if let Some(indices) = self.by_class.get(class)
                    && indices.iter().any(|&index| visit(index))
                {
                    return true;
                }
            }
        }
        !self.by_tag.is_empty()
            && dom
                .tag_name(node)
                .and_then(|tag| self.by_tag.get(tag))
                .is_some_and(|indices| indices.iter().any(|&index| visit(index)))
    }

    fn retained_bytes(&self) -> usize {
        [&self.by_id, &self.by_class, &self.by_tag]
            .into_iter()
            .map(|map| {
                map.capacity() * std::mem::size_of::<(String, Vec<u32>)>()
                    + map
                        .iter()
                        .map(|(key, indices)| key.capacity() + indices.capacity() * 4)
                        .sum::<usize>()
            })
            .sum::<usize>()
            + self.unkeyed.capacity() * 4
    }
}

/// A necessary, positive part of a selector. State/logical/positional tests
/// are omitted so checking a dependency cannot exclude a future match.
#[derive(Clone)]
struct StructureGuard(Vec<StructureStep>);

// true = direct parent, false = any ancestor (the first step has no relation).
type StructureStep = (bool, Option<String>, Option<String>, Vec<String>);

impl StructureGuard {
    fn prefix(selector: &Complex, end: usize) -> Self {
        // Sibling adjacency can change during this mutation. Keep only the
        // ancestor suffix, whose relationships to an existing child survive.
        let start = (1..=end)
            .rev()
            .find(|&index| {
                !matches!(
                    selector.0[index].0,
                    Combinator::Child | Combinator::Descendant
                )
            })
            .unwrap_or(0);
        Self(
            selector.0[start..=end]
                .iter()
                .map(|(combinator, compound)| {
                    (
                        *combinator == Combinator::Child,
                        compound.tag.clone(),
                        compound.id.clone(),
                        compound.classes.clone(),
                    )
                })
                .collect(),
        )
    }

    fn matches(&self, dom: &Dom, node: NodeId) -> bool {
        fn matches(dom: &Dom, node: NodeId, parts: &[StructureStep]) -> bool {
            let Some(((direct_parent, tag, id, classes), rest)) = parts.split_last() else {
                return true;
            };
            if !dom
                .tag_name(node)
                .is_some_and(|name| tag.as_ref().is_none_or(|tag| tag == "*" || tag == name))
                || id
                    .as_ref()
                    .is_some_and(|id| dom.attr(node, "id") != Some(id))
                || !dom.matches_classes(node, classes)
            {
                return false;
            }
            if rest.is_empty() {
                return true;
            }
            let mut parent = dom.selector_parent(node);
            while let Some(node) = parent {
                if matches(dom, node, rest) {
                    return true;
                }
                if *direct_parent {
                    break;
                }
                parent = dom.selector_parent(node);
            }
            false
        }
        matches(dom, node, &self.0)
    }

    fn retained_bytes(&self) -> usize {
        self.0.capacity() * std::mem::size_of::<StructureStep>()
            + self
                .0
                .iter()
                .map(|(_, tag, id, classes)| {
                    tag.as_ref().map_or(0, String::capacity)
                        + id.as_ref().map_or(0, String::capacity)
                        + classes.capacity() * std::mem::size_of::<String>()
                        + classes.iter().map(String::capacity).sum::<usize>()
                })
                .sum::<usize>()
    }
}

enum StructureDependency {
    Empty {
        guards: Vec<StructureGuard>,
        siblings: bool,
    },
    ChildIndex(Vec<StructureGuard>),
    Siblings {
        left: StructureGuard,
        right: StructureGuard,
    },
}

impl StructureDependency {
    fn retained_bytes(&self) -> usize {
        match self {
            Self::Empty { guards, .. } | Self::ChildIndex(guards) => {
                guards.capacity() * std::mem::size_of::<StructureGuard>()
                    + guards
                        .iter()
                        .map(StructureGuard::retained_bytes)
                        .sum::<usize>()
            }
            Self::Siblings { left, right } => left.retained_bytes() + right.retained_bytes(),
        }
    }
}

/// Selectors 4 #relative / #relational. A simple forward relative selector
/// can only observe an anchor's descendants or following-sibling forest.
/// Positive anchor tests deliberately omit logical/state tests: extra possible
/// anchors cost work, while excluding a real one could retain stale styles.
/// The positive simple selectors a `:has()` subject must match: its own
/// compound, conjoined with the compound around a `:not()`/`:is()` argument
/// whose subject is the same element. Inside `:not()` the inner parts are
/// negated, but the `:has()` result still only matters for elements matching
/// both, so the conjunction stays a necessary anchor condition.
#[derive(Clone, Default)]
struct AnchorParts {
    tag: Option<String>,
    id: Option<String>,
    classes: Vec<String>,
    attributes: Vec<String>,
    /// Contradictory parts (`div:not(span:has(x))`): the `:has()` can never
    /// change the result, so it needs no dependency.
    impossible: bool,
}

impl AnchorParts {
    fn and(&self, compound: &Compound) -> Self {
        let mut parts = self.clone();
        match (&parts.tag, compound.tag.as_deref()) {
            (_, None | Some("*")) => {}
            (None, Some(tag)) => parts.tag = Some(tag.to_string()),
            (Some(current), Some(tag)) if current == "*" => parts.tag = Some(tag.to_string()),
            (Some(current), Some(tag)) => parts.impossible |= current != tag,
        }
        match (&parts.id, &compound.id) {
            (Some(current), Some(id)) => parts.impossible |= current != id,
            (None, Some(id)) => parts.id = Some(id.clone()),
            _ => {}
        }
        parts.classes.extend(compound.classes.iter().cloned());
        parts
            .attributes
            .extend(compound.attrs.iter().map(|a| a.name.clone()));
        parts
    }
}

struct RelationalDependency {
    tag: Option<String>,
    id: Option<String>,
    classes: Vec<String>,
    anchor_attributes: Vec<String>,
    attributes: Vec<String>,
    siblings: bool,
    /// For an argument whose every combinator is `>` (`:has(> a > b)`), the
    /// number of elements it spans: a changed node can only alter anchors that
    /// close above it. `None` when a descendant step makes the reach unbounded.
    depth: Option<usize>,
    /// A `+`/`~` combinator follows the anchor's compound, so a subject can be
    /// a following sibling of the anchor rather than inside it.
    subject_siblings: bool,
}

/// Whether a `:has()` argument compound only tests its own element (see
/// `RelationalDependency::new`). A one-compound `:not()`/`:is()` argument
/// (`.bar:not(.--dismissed)`) does too, so it is checked recursively.
fn relational_compound(compound: &Compound) -> bool {
    let local_logical = compound
        .nots
        .iter()
        .flatten()
        .chain(compound.selects.iter().flat_map(|(group, _)| group))
        .all(|selector| selector.0.len() == 1 && relational_compound(&selector.0[0].1));
    local_logical
        && compound.has.is_empty()
        && !compound
            .structural
            .iter()
            .any(|structural| matches!(structural, Structural::Nth { of: Some(_), .. }))
        // Radio groups associate controls across the tree; every other state
        // reads the element, its ancestry or its own children.
        && !compound
            .states
            .iter()
            .any(|state| matches!(state, StatePseudo::Indeterminate))
        && !compound.target
        && !compound.popover_open
        && !compound.scope
        && !compound.root
        && !compound.host
        && compound.host_inner.is_none()
        && compound.slotted.is_none()
        && compound.pseudo.is_none()
        && !compound.inert_pseudo_element
}

impl RelationalDependency {
    fn new(anchor: &AnchorParts, argument: &HasArg, subject_siblings: bool) -> Option<Self> {
        let mut attributes = Vec::new();
        // The first compound is the parser's synthetic :scope anchor.
        for (_, compound) in argument.complex.0.iter().skip(1) {
            // Logical arguments can look outside the relative subtree (e.g.
            // :has(:is(body.theme .hit))), as can `:nth-child(of S)` and
            // association states (:indeterminate radio groups); retain the
            // full fallback for them. This dependency only answers child-list
            // changes (attribute changes reverse :has() paths in `attributes`),
            // and every child-list change that can alter an argument match lies
            // below or beside a possible anchor, which `restyle_root` walks.
            // So other states and positional pseudo-classes are safe: focus
            // changes reverse :has() paths like attributes do
            // (`invalidate_live_state`), hover
            // transitions compare every hover subject including :has()
            // anchors (`set_hover_chain`), :dir() keeps the text-direction
            // fallback, fieldset parents restyle their subtree (:disabled),
            // and attribute-backed states change through attributes.
            // YouTube's `:not(:has(#masthead-container:focus-within))`,
            // Discourse's `:has(.socket:hover)`, `:has(.switch:disabled)` and
            // Wikipedia's
            // `:has(.reference-text:dir(ltr))` / `a:has(+ … + a:last-of-type)`
            // otherwise made every insertion expire every style.
            if !relational_compound(compound) {
                return None;
            }
            if compound.id.is_some() {
                attributes.push(String::from("id"));
            }
            if !compound.classes.is_empty() {
                attributes.push(String::from("class"));
            }
            attributes.extend(compound.attrs.iter().map(|a| a.name.to_ascii_lowercase()));
        }
        attributes.sort_unstable();
        attributes.dedup();
        Some(Self {
            tag: anchor.tag.clone(),
            id: anchor.id.clone(),
            classes: anchor.classes.clone(),
            anchor_attributes: anchor.attributes.clone(),
            attributes,
            siblings: argument.sibling,
            depth: (!argument.sibling
                && argument
                    .complex
                    .0
                    .iter()
                    .skip(1)
                    .all(|(combinator, _)| *combinator == Combinator::Child))
            .then(|| argument.complex.0.len().saturating_sub(1)),
            subject_siblings,
        })
    }

    fn possible_anchor(&self, dom: &Dom, node: NodeId) -> bool {
        dom.tag_name(node).is_some_and(|tag| {
            self.tag
                .as_ref()
                .is_none_or(|want| want == "*" || want == tag)
                && self
                    .id
                    .as_ref()
                    .is_none_or(|want| dom.attr(node, "id") == Some(want))
                && dom.matches_classes(node, &self.classes)
                && self
                    .anchor_attributes
                    .iter()
                    .all(|name| dom.attr(node, name).is_some())
        })
    }

    /// The subtree a child-list change under `parent` can restyle through this
    /// `:has()`, as (levels above `parent`, root): the topmost possible anchor,
    /// or its parent when a sibling combinator follows it, because a selector's
    /// subject is the anchor, a following sibling of it, or a descendant of
    /// either. A sibling-relative argument's anchors are `parent`'s children
    /// or earlier siblings of its ancestors, so the root is that level's parent.
    fn restyle_root(&self, dom: &Dom, parent: NodeId) -> Option<(usize, NodeId)> {
        if self.siblings {
            let mut root = dom
                .child_iter(parent)
                .any(|child| self.possible_anchor(dom, child))
                .then_some((0, parent));
            let mut next = Some(parent);
            let mut level = 0;
            while let Some(node) = next {
                level += 1;
                next = dom.selector_parent(node);
                let mut previous = dom.prev_element_sibling(node);
                while let Some(sibling) = previous {
                    if self.possible_anchor(dom, sibling) {
                        root = Some((level, next.unwrap_or(node)));
                        break;
                    }
                    previous = dom.prev_element_sibling(sibling);
                }
            }
            return root;
        }
        let mut root = None;
        let mut next = Some(parent);
        let mut level = 0;
        while let Some(node) = next {
            if self.depth.is_some_and(|depth| level >= depth) {
                break;
            }
            level += 1;
            next = dom.selector_parent(node);
            if self.possible_anchor(dom, node) {
                root = Some(if self.subject_siblings {
                    (level, next.unwrap_or(node))
                } else {
                    (level - 1, node)
                });
            }
        }
        root
    }

    fn may_affect(&self, dom: &Dom, node: NodeId, child_list: bool) -> bool {
        // Insertion/removal may change the following siblings of any child,
        // including when the removed child is still linked during invalidation.
        if child_list
            && self.siblings
            && dom
                .child_iter(node)
                .any(|child| self.possible_anchor(dom, child))
        {
            return true;
        }
        let mut next = Some(node);
        let mut level = 0;
        while let Some(node) = next {
            // `node` is the changed child's parent, so an anchor of a `>`-only
            // argument lies fewer than `depth` levels up.
            if self.depth.is_some_and(|depth| level >= depth) {
                break;
            }
            level += 1;
            if self.possible_anchor(dom, node) {
                return true;
            }
            if self.siblings {
                let mut previous = dom.prev_element_sibling(node);
                while let Some(sibling) = previous {
                    if self.possible_anchor(dom, sibling) {
                        return true;
                    }
                    previous = dom.prev_element_sibling(sibling);
                }
            }
            next = dom.selector_parent(node);
        }
        false
    }

    fn retained_bytes(&self) -> usize {
        self.tag.as_ref().map_or(0, String::capacity)
            + self.id.as_ref().map_or(0, String::capacity)
            + [&self.classes, &self.anchor_attributes, &self.attributes]
                .into_iter()
                .map(|v| {
                    v.capacity() * std::mem::size_of::<String>()
                        + v.iter().map(String::capacity).sum::<usize>()
                })
                .sum::<usize>()
    }
}

impl SelectorDependencies {
    fn finish(&mut self) {
        for (index, dependency) in self.structural.iter().enumerate() {
            match dependency {
                StructureDependency::Empty { guards, .. } => {
                    self.empty_keys.insert_step(index, guards.first());
                }
                StructureDependency::ChildIndex(guards) => {
                    self.child_index_keys.insert_step(index, guards.first());
                }
                StructureDependency::Siblings { left, right } => {
                    self.sibling_left_keys.insert_step(index, Some(left));
                    self.sibling_right_keys.insert_step(index, Some(right));
                }
            }
        }
        for (index, dependency) in self.relational.iter().enumerate() {
            if dependency.siblings {
                self.sibling_anchors.push(index as u32);
            } else {
                self.anchor_keys.insert(
                    index,
                    dependency.tag.as_deref(),
                    dependency.id.as_deref(),
                    &dependency.classes,
                );
            }
        }
    }

    fn record(&mut self, rule: &StyleRule) {
        self.attributes.record(&rule.selector);
        self.complex(&rule.selector);
        self.record_structure(&rule.selector, &[], Impact::Element);
    }

    /// Selectors 4 #child-index / #the-empty-pseudo / sibling combinators.
    /// A child-list mutation changes ranks/adjacency of its direct children
    /// and emptiness of the parent. Rules for other parents cannot observe it.
    fn record_structure(
        &mut self,
        selector: &Complex,
        inherited: &[StructureGuard],
        outer: Impact,
    ) {
        for (index, (combinator, compound)) in selector.0.iter().enumerate() {
            if compound.structural.is_empty()
                && compound.nots.is_empty()
                && compound.selects.is_empty()
                && !matches!(
                    combinator,
                    Combinator::NextSibling | Combinator::SubsequentSibling
                )
            {
                continue;
            }
            let mut guards = inherited.to_vec();
            guards.push(StructureGuard::prefix(selector, index));
            let siblings = outer >= Impact::SiblingSubtrees
                || selector.0[index + 1..].iter().any(|(combinator, _)| {
                    matches!(
                        combinator,
                        Combinator::NextSibling | Combinator::SubsequentSibling
                    )
                });
            if compound
                .structural
                .iter()
                .any(|s| matches!(s, Structural::Empty))
            {
                self.structural.push(StructureDependency::Empty {
                    guards: guards.clone(),
                    siblings,
                });
            }
            if compound
                .structural
                .iter()
                .any(|s| matches!(s, Structural::Nth { .. }))
            {
                self.structural
                    .push(StructureDependency::ChildIndex(guards.clone()));
            }
            if index > 0
                && matches!(
                    combinator,
                    Combinator::NextSibling | Combinator::SubsequentSibling
                )
            {
                self.structural.push(StructureDependency::Siblings {
                    left: StructureGuard::prefix(selector, index - 1),
                    right: StructureGuard::prefix(selector, index),
                });
            }
            for inner in compound
                .nots
                .iter()
                .flatten()
                .chain(compound.selects.iter().flat_map(|(group, _)| group))
            {
                // A single-compound logical argument tests this same node.
                // Complex arguments may test an ancestor/sibling instead;
                // dropping outer guards for those is conservative.
                self.record_structure(
                    inner,
                    if inner.0.len() == 1 { &guards } else { &[] },
                    if siblings {
                        Impact::SiblingSubtrees
                    } else {
                        outer
                    },
                );
            }
        }
    }

    /// The single subtree covering every `:has()` restyle a child-list change
    /// under `parent` can cause (all roots lie on its ancestor chain).
    fn relational_root(&self, dom: &Dom, parent: NodeId) -> Option<NodeId> {
        let mut top: Option<(usize, NodeId)> = None;
        let mut raise = |root: (usize, NodeId)| {
            if top.is_none_or(|(level, _)| root.0 > level) {
                top = Some(root);
            }
        };
        for &index in &self.sibling_anchors {
            if let Some(root) = self.relational[index as usize].restyle_root(dom, parent) {
                raise(root);
            }
        }
        if !self.anchor_keys.is_empty() {
            // One ancestor walk for every other dependency (see `restyle_root`).
            let mut next = Some(parent);
            let mut level = 0;
            while let Some(node) = next {
                level += 1;
                let up = dom.selector_parent(node);
                self.anchor_keys.any(dom, node, |index| {
                    let dependency = &self.relational[index as usize];
                    if dependency.depth.is_none_or(|depth| level <= depth)
                        && dependency.possible_anchor(dom, node)
                    {
                        raise(if dependency.subject_siblings {
                            (level, up.unwrap_or(node))
                        } else {
                            (level - 1, node)
                        });
                    }
                    false
                });
                next = up;
            }
        }
        top.map(|(_, root)| root)
    }

    fn structure_impact(&self, dom: &Dom, parent: NodeId) -> (bool, bool) {
        let (mut siblings, mut children) = (false, false);
        self.empty_keys.any(dom, parent, |index| {
            if let StructureDependency::Empty {
                guards,
                siblings: affects_siblings,
            } = &self.structural[index as usize]
                && guards.iter().all(|guard| guard.matches(dom, parent))
            {
                children = true;
                siblings |= affects_siblings;
            }
            siblings
        });
        if children {
            return (siblings, children);
        }
        // Each remaining form only adds `children`; stop at the first proof.
        children = dom.child_iter(parent).any(|node| {
            self.child_index_keys.any(dom, node, |index| {
                matches!(
                    &self.structural[index as usize],
                    StructureDependency::ChildIndex(guards)
                        if guards.iter().all(|guard| guard.matches(dom, node))
                )
            })
        });
        if !children && !self.sibling_left_keys.is_empty() {
            let side = |keys: &KeyIndex, left: bool| {
                let mut matched = FxHashSet::default();
                for node in dom.child_iter(parent) {
                    keys.any(dom, node, |index| {
                        if let StructureDependency::Siblings { left: l, right: r } =
                            &self.structural[index as usize]
                            && (if left { l } else { r }).matches(dom, node)
                        {
                            matched.insert(index);
                        }
                        false
                    });
                }
                matched
            };
            let left = side(&self.sibling_left_keys, true);
            children = !left.is_empty()
                && side(&self.sibling_right_keys, false)
                    .iter()
                    .any(|index| left.contains(index));
        }
        (siblings, children)
    }

    pub(super) fn retained_bytes(&self) -> usize {
        self.attributes.retained_bytes()
            + self.relational.capacity() * std::mem::size_of::<RelationalDependency>()
            + self
                .relational
                .iter()
                .map(RelationalDependency::retained_bytes)
                .sum::<usize>()
            + self.structural.capacity() * std::mem::size_of::<StructureDependency>()
            + self
                .structural
                .iter()
                .map(StructureDependency::retained_bytes)
                .sum::<usize>()
            + [
                &self.empty_keys,
                &self.child_index_keys,
                &self.sibling_left_keys,
                &self.sibling_right_keys,
                &self.anchor_keys,
            ]
            .into_iter()
            .map(KeyIndex::retained_bytes)
            .sum::<usize>()
            + self.sibling_anchors.capacity() * 4
    }

    fn complex(&mut self, selector: &Complex) {
        self.complex_in(selector, &AnchorParts::default(), false);
    }

    /// `subject` holds the enclosing compound's parts when `selector` is a
    /// `:not()`/`:is()` argument: its last compound describes the same element,
    /// which `subject_siblings` says may be followed by a sibling combinator.
    fn complex_in(&mut self, selector: &Complex, subject: &AnchorParts, subject_siblings: bool) {
        let last = selector.0.len().saturating_sub(1);
        for (index, (_, compound)) in selector.0.iter().enumerate() {
            let siblings_after = selector.0[index + 1..].iter().any(|(combinator, _)| {
                matches!(
                    combinator,
                    Combinator::NextSibling | Combinator::SubsequentSibling
                )
            });
            if index == last {
                self.compound_in(compound, subject, subject_siblings);
            } else {
                self.compound_in(compound, &AnchorParts::default(), siblings_after);
            }
        }
    }

    fn compound(&mut self, compound: &Compound) {
        self.compound_in(compound, &AnchorParts::default(), false);
    }

    fn compound_in(&mut self, compound: &Compound, outer: &AnchorParts, siblings_after: bool) {
        let anchor = outer.and(compound);
        // Exhaustive so future selector forms require a dependency decision.
        let Compound {
            tag: _,
            id: _,
            classes: _,
            attrs: _,
            nots,
            selects,
            has,
            hover: _,
            target: _,
            popover_open: _,
            never: _,
            inert_pseudo_element: _,
            structural,
            states,
            scope: _,
            root: _,
            host,
            host_inner,
            slotted,
            pseudo: _,
            pseudos: _,
            relative_anchor: _,
        } = compound;
        // CSS Shadow 1 #host-element-in-tree: for its own shadow tree's
        // selectors the host stands in for the shadow root, so a `:has()` on
        // `:host` reads that tree, beyond the anchors found by climbing
        // selector parents (which stop at the shadow root).
        if *host
            && (!has.is_empty()
                || host_inner
                    .as_ref()
                    .is_some_and(|inner| !inner.has.is_empty()))
        {
            self.structure_global = true;
        }
        for selector in nots
            .iter()
            .flatten()
            .chain(selects.iter().flat_map(|(group, _)| group))
        {
            self.complex_in(selector, &anchor, siblings_after);
        }
        for argument in has.iter().flatten() {
            if anchor.impossible {
                continue;
            }
            if let Some(dependency) = RelationalDependency::new(&anchor, argument, siblings_after) {
                self.relational.push(dependency);
                // The argument's own text-content, direction and placeholder
                // dependencies (`:empty`, `:dir()`, …) still apply to the text
                // and attribute paths; none of the accepted forms is global.
                self.complex(&argument.complex);
            } else {
                self.structure_global = true;
                self.complex(&argument.complex);
            }
        }
        for structural in structural {
            self.empty |= matches!(structural, Structural::Empty);
            if let Structural::Nth {
                of: Some(selectors),
                ..
            } = structural
            {
                self.structure_global = true;
                // Changing membership changes other siblings' ranks and may
                // feed a selector with additional combinators outside it.
                for selector in selectors {
                    self.complex(selector);
                }
            }
        }
        for inner in [host_inner, slotted].into_iter().flatten() {
            self.compound(inner);
        }
        for state in states {
            // HTML #concept-element-disabled / #concept-fe-disabled:
            // enabledness follows this subtree's ancestry and the first
            // legend of a fieldset. Structural invalidation already covers
            // fieldset/select/optgroup children and moved descendants. A
            // :disabled rule elsewhere is not a document-wide dependency.
            // Link state likewise depends on the element's own href.
            // HTML #selector-checked observes a control's checkedness or
            // selectedness, not unrelated child lists. Changes to those
            // states (including radio-group updates) still invalidate through
            // checked/selected below; moving a subtree invalidates its styles.
            // Focus changes/removal explicitly invalidate focus-dependent
            // styles; unrelated child-list edits do not change focus. Shadow
            // slot redistribution already takes the shadow-scope fallback.
            // Removing a modal dialog clears its flag through
            // `set_dialog_modal`, which invalidates every style.
            // :required/:optional and :read-write/:read-only read the element's
            // own attributes and type, editing-host and fieldset ancestry
            // (covered like :disabled); :lang() reads ancestor attributes only.
            // The tree operation already restyles the moved subtree, so none
            // of them changes outside it. :placeholder-shown reads a textarea's
            // own children, handled at that parent below.
            self.structure_global |= !matches!(
                state,
                StatePseudo::AnyLink
                    | StatePseudo::Focus
                    | StatePseudo::FocusWithin
                    | StatePseudo::Checked
                    | StatePseudo::Disabled
                    | StatePseudo::Enabled
                    | StatePseudo::Dir(_)
                    | StatePseudo::Modal
                    | StatePseudo::Indeterminate
                    | StatePseudo::Required
                    | StatePseudo::Optional
                    | StatePseudo::ReadWrite
                    | StatePseudo::ReadOnly
                    | StatePseudo::Lang(_)
                    | StatePseudo::PlaceholderShown
            );
            self.indeterminate |= matches!(state, StatePseudo::Indeterminate);
            self.text_direction |= matches!(state, StatePseudo::Dir(_));
            self.text_placeholder |= matches!(state, StatePseudo::PlaceholderShown);
        }
    }
}

impl Dom {
    /// DOM #concept-shadow-including-root / CSS Scoping: selector matching,
    /// inheritance and slot distribution cannot couple separate shadow-
    /// including trees. A detached feature probe must not make unrelated
    /// detached construction invalidate the live document's style cache.
    pub(super) fn shadow_style_dependencies(&self, node: NodeId) -> bool {
        if self.shadow_roots.is_empty() {
            return false;
        }
        let root = |mut id| {
            // A Document's arena parent is only a presentation edge to its
            // iframe. DOM's shadow-including root never traverses that edge.
            while !matches!(self.nodes[id].data, NodeData::Document) {
                let Some(parent) = self.parent_composed(id) else {
                    break;
                };
                id = parent;
            }
            id
        };
        let mutation_root = root(node);
        self.shadow_roots
            .keys()
            .any(|&host| root(host) == mutation_root)
    }

    #[cfg(test)]
    pub(crate) fn force_cold_style_layout_for_test(&mut self) {
        self.mark();
        self.layout_cache.get_mut().cold = true;
    }

    pub(super) fn invalidate_attribute_selectors(
        &mut self,
        node: NodeId,
        name: &str,
        old_class: Option<&str>,
    ) -> Option<Vec<NodeId>> {
        // DOM §4.2.2: `slot` and a slot's `name` change distribution without
        // a child-list mutation. That is an implicit cross-tree dependency,
        // including for state resolved through the style parent, even when
        // no rule contains a literal [slot] or [name] selector.
        if self.shadow_style_dependencies(node)
            && (name.eq_ignore_ascii_case("slot")
                || (name.eq_ignore_ascii_case("name") && self.tag_name(node) == Some("slot")))
        {
            self.selector_epoch = self.selector_epoch.wrapping_add(1);
            return None;
        }
        let subjects = {
            let cache = self.style_cache.borrow();
            match cache.as_ref() {
                Some((epoch, index)) if *epoch == self.style_epoch => index
                    .selector_dependencies
                    .for_node(self, node)
                    .attributes
                    .subjects(self, node, name, old_class),
                // No current rule index: no proof of independence.
                _ => None,
            }
        };
        // Selector matching never crosses into another tree, and inserting a
        // detached tree restyles all of it. Frameworks set attributes on nodes
        // before inserting them; with a broad dependency (`dir`, `lang`, …)
        // that expired every style of the live document. Only that fallback
        // takes the detached tree: a local proof stays cheaper than a walk.
        if subjects.is_none()
            && !self.shadow_style_dependencies(node)
            && !self.is_dom_connected(node)
        {
            let mut root = node;
            while let Some(parent) = self.parent_composed(root) {
                root = parent;
            }
            self.invalidate_style_subtree(root, true);
            return Some(Vec::new());
        }
        // Shadow rules are in this Document's routes: `:host()` and
        // `::slotted()` reach the host, its shadow tree and its slottables,
        // and restyling a subject restyles its flat-tree descendants.
        if subjects.is_none() {
            if casc_diag_on() {
                eprintln!(
                    "DIAGINVALID attribute node={node} tag={:?} id={:?} name={name} impact=All shadow={}",
                    self.tag_name(node),
                    self.attr(node, "id"),
                    self.shadow_style_dependencies(node)
                );
            }
            self.selector_epoch = self.selector_epoch.wrapping_add(1);
            return None;
        }
        let mut cache = self.selector_cache.borrow_mut();
        for &subject in subjects.as_ref().unwrap() {
            cache.invalidate(subject);
        }
        subjects
    }

    /// The focus update steps (HTML #focus-update-steps) change :focus and
    /// :focus-within (Selectors 4 #the-focus-pseudo,
    /// #the-focus-within-pseudo), and scrolling to a fragment changes
    /// :target (HTML #selector-target), on exactly the elements of `changed`.
    /// Like an attribute write, that can only change the selector subjects
    /// reached through compiled routes: the element itself, descendant and
    /// sibling dependents, `:has()` anchors and their dependents, and
    /// `:nth-child(of …)` siblings. Restyle those and, for inheritance, their
    /// descendants. `index` must reflect the current sheet set. Returns
    /// false when only a complete invalidation is provably correct: shadow
    /// trees can couple tree scopes (`:host`, `::slotted()`, slot
    /// distribution) beyond these routes.
    pub(super) fn invalidate_live_state(
        &mut self,
        index: &StyleIndex,
        changed: &[(NodeId, LiveState)],
    ) -> bool {
        let observed: Vec<_> = changed
            .iter()
            .copied()
            .filter(|&(node, state)| {
                index
                    .selector_dependencies
                    .for_node(self, node)
                    .attributes
                    .observes(state)
            })
            .collect();
        if observed.is_empty() {
            // No selector in the node's Document reads the state; matches()
            // reads Document state directly and caches nothing.
            return true;
        }
        if observed
            .iter()
            .any(|&(node, _)| self.shadow_style_dependencies(node))
        {
            if casc_diag_on() {
                eprintln!("DIAGINVALID live-state shadow={changed:?}");
            }
            return false;
        }
        let mut subjects = FxHashSet::default();
        for &(node, state) in &observed {
            let Some(reached) = index
                .selector_dependencies
                .for_node(self, node)
                .attributes
                .state_subjects(self, node, state)
            else {
                return false;
            };
            subjects.extend(reached);
        }
        if subjects.is_empty() {
            return true;
        }
        let mut subjects: Vec<_> = subjects.into_iter().collect();
        subjects.sort_unstable();
        {
            let mut cache = self.selector_cache.borrow_mut();
            for &subject in &subjects {
                cache.invalidate(subject);
            }
        }
        // Descendants keep their selector matches: every element whose match
        // changed is a subject. They can still inherit changed values.
        self.invalidate_style_subtrees(&subjects, false);
        self.mark_dom_revision();
        for &subject in &subjects {
            self.dirty_nodes.push((subject, DirtyKind::Attr));
            self.record_geometry_dirty(subject, DirtyKind::Attr);
        }
        true
    }

    /// The parsed rule index when it reflects the current sheet set, without
    /// building one: no current index means no proof of independence.
    pub(super) fn current_style_index(&self) -> Option<std::rc::Rc<StyleIndex>> {
        let cache = self.style_cache.borrow();
        let (epoch, index) = cache.as_ref()?;
        (*epoch == self.style_epoch).then(|| index.clone())
    }

    #[track_caller]
    pub(super) fn invalidate_all_style_values(&mut self) {
        #[cfg(feature = "architecture-diagnostics")]
        super::architecture_diagnostics::invalidate(None);
        if casc_diag_on() {
            eprintln!(
                "DIAGINVALID all-styles caller={}",
                std::panic::Location::caller()
            );
        }
        self.style_value_epoch = self.style_value_epoch.wrapping_add(1);
        self.layout_cache.get_mut().clear();
        self.box_tree_cache.get_mut().clear();
        self.svg_dependencies.get_mut().clear_proofs();
    }

    /// Activation metadata participates in inline inheritance and anonymous
    /// fallback boxes, but not in the CSS cascade. Cover both slot distribution
    /// and ordinary ancestry without expiring unrelated layout or style values.
    pub(super) fn invalidate_activation_layout(&mut self, changed: &[NodeId]) {
        let mut invalid = FxHashSet::default();
        let mut descendants = changed.to_vec();
        while let Some(node) = descendants.pop() {
            if invalid.insert(node) {
                self.push_composed_children(node, &mut descendants);
                if self.tag_name(node) == Some("slot") {
                    descendants.extend(self.flat_slot_nodes(node));
                }
            }
        }
        let mut ancestors = changed.to_vec();
        let mut visited = FxHashSet::default();
        while let Some(node) = ancestors.pop() {
            if visited.insert(node) {
                invalid.insert(node);
                ancestors.extend(self.parent_composed(node));
                ancestors.extend(self.parent_flat(node));
            }
        }
        for node in invalid {
            self.layout_cache.get_mut().invalidate(node);
            self.box_tree_cache.get_mut().invalidate(node);
        }
    }

    /// Animation-origin values can affect explicit inheritance and containing
    /// block constraints, but cannot change selectors or base declarations.
    pub(super) fn invalidate_transition_layout(&mut self, changed: &[NodeId]) {
        self.invalidate_activation_layout(changed);
        let consumers = self.svg_dirty_consumers(changed, false);
        self.invalidate_layout_paths(consumers);
    }

    fn invalidate_layout_ancestors(&mut self, node: NodeId) {
        self.invalidate_layout_paths([node]);
    }

    pub(super) fn invalidate_attribute_style_values(
        &mut self,
        node: NodeId,
        name: &str,
        subjects: Option<Vec<NodeId>>,
    ) {
        let Some(mut roots) = subjects else {
            self.invalidate_all_style_values();
            return;
        };
        let root = if self.tag_name(node) == Some("source") {
            self.nodes[node].parent.unwrap_or(node)
        } else {
            node
        };
        // HTML form/label/named associations can point outside a selector's
        // subject subtree and retain their existing broad layout invalidation.
        // SVG instance resources are tracked separately by svg_dirty_consumers,
        // including unresolved IDs and sources outside the consumer's subtree.
        if self.is_connected(node)
            && matches!(
                name.to_ascii_lowercase().as_str(),
                "id" | "name" | "form" | "for" | "type"
            )
        {
            if casc_diag_on() {
                eprintln!(
                    "DIAGINVALID association node={node} tag={:?} name={name}",
                    self.tag_name(node)
                );
            }
            self.layout_cache.get_mut().clear();
            self.box_tree_cache.get_mut().clear();
        }
        roots.push(root);
        self.invalidate_style_subtrees(&roots, false);
    }

    /// CSS selectors and inheritance follow the element tree; immutable box
    /// nodes are invalidated in the same operation, plus rebuilt ancestors.
    pub(super) fn invalidate_style_subtree(&mut self, root: NodeId, selectors: bool) {
        self.invalidate_style_subtrees(&[root], selectors);
    }

    /// DOM #concept-node-replace-all removes a forest without routing each
    /// child through detach(). Preserve those old roots before links change;
    /// a later walk of only the parent cannot find its departed descendants.
    pub(super) fn invalidate_detaching_children(&mut self, parent: NodeId) {
        self.invalidate_style_subtree(parent, true);
        if deferred_invalidation_enabled() {
            let mut next = self.nodes[parent].first_child;
            while let Some(child) = next {
                next = self.nodes[child].next_sibling;
                self.invalidate_style_subtree(child, true);
            }
        }
    }

    fn invalidate_style_subtrees(&mut self, roots: &[NodeId], selectors: bool) {
        if deferred_invalidation_enabled() {
            let pending = &mut self.pending_style_invalidations;
            for &root in roots {
                *pending.roots.get_mut().entry(root).or_default() |= selectors;
            }
            pending.dirty.set(!pending.roots.get_mut().is_empty());
        } else {
            self.apply_style_invalidations(roots.iter().map(|&root| (root, selectors)));
        }
    }

    #[inline]
    pub(crate) fn flush_style_invalidations(&self) {
        if self.pending_style_invalidations.dirty.get() {
            self.flush_style_invalidations_slow();
        }
    }

    #[cold]
    fn flush_style_invalidations_slow(&self) {
        let pending = &self.pending_style_invalidations;
        if !pending.dirty.replace(false) {
            return;
        }
        let mut roots = std::mem::take(&mut *pending.roots.borrow_mut());
        #[cfg(test)]
        pending.flushes.set(pending.flushes.get() + 1);
        self.apply_style_invalidations(roots.drain());
        // Reuse bounded queue storage; no node owner or GC root is retained.
        *pending.roots.borrow_mut() = roots;
    }

    fn apply_style_invalidations(&self, roots: impl IntoIterator<Item = (NodeId, bool)>) {
        // Even an own-element selector/inline style can change inherited
        // computed values, custom properties, link context or font metrics.
        // Coalesce overlapping subjects before visiting descendants. In
        // particular `:has()` can return an ancestor AND its descendants;
        // repeatedly walking each subtree would make one mutation quadratic.
        let mut seen = FxHashMap::<NodeId, bool>::default();
        let mut pending: Vec<_> = roots.into_iter().collect();
        let roots: Vec<_> = pending.iter().map(|&(node, _)| node).collect();
        let selectors = pending.iter().any(|&(_, selectors)| selectors);
        let mut affected = Vec::new();
        while let Some((node, selectors)) = pending.pop() {
            if self.nodes.get(node).is_none() {
                continue;
            }
            match seen.entry(node) {
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert(selectors);
                    affected.push(node);
                }
                std::collections::hash_map::Entry::Occupied(mut entry) => {
                    if !selectors || *entry.get() {
                        continue;
                    }
                    // A selector-dirty descendant can overlap an inherited-
                    // value-only ancestor; propagate the stronger proof once.
                    *entry.get_mut() = true;
                }
            }
            pending.extend(self.child_iter(node).map(|child| (child, selectors)));
            // CSS Shadow 1 #flattening: inherited values follow the flat
            // tree, which puts a host's shadow-tree children under the host
            // and a slot's assigned nodes (after flattening) under the slot.
            // The shadow root itself is included: values that propagate
            // along the composed tree (decorations) are cached on it too.
            if let Some(shadow) = self.shadow_root(node) {
                pending.push((shadow, selectors));
            }
            // A slot's flat-tree children are its assigned nodes; an assigned
            // slot is itself a flat-tree element and expands in turn. (Its
            // fallback children are its light children, already queued.)
            if self.tag_name(node) == Some("slot") {
                pending.extend(
                    self.slot_assigned_nodes(node)
                        .into_iter()
                        .map(|assigned| (assigned, selectors)),
                );
            }
        }
        #[cfg(test)]
        self.pending_style_invalidations
            .visited
            .set(self.pending_style_invalidations.visited.get() + affected.len());
        let mut resource_roots = self.svg_dirty_consumers(&affected, selectors);
        #[cfg(feature = "architecture-diagnostics")]
        super::architecture_diagnostics::invalidate(Some(affected.len()));
        if casc_diag_on() && affected.len() > 256 {
            eprintln!(
                "DIAGINVALID subtrees roots={} selectors={selectors} count={}",
                roots.len(),
                affected.len()
            );
        }
        // Ownership is by node. Retirement visits only populated values;
        // neither the document nor the entire property registry is scanned.
        {
            let mut computed = self.computed_cache.borrow_mut();
            for &id in &affected {
                computed.1.remove_node(id);
            }
        }
        if affected
            .iter()
            .any(|&id| matches!(self.tag_name(id), Some("meta" | "base")))
        {
            self.layout_cache.borrow_mut().clear();
            self.box_tree_cache.borrow_mut().clear();
        }
        for id in affected {
            self.transitions.invalidate(id);
            self.animations.invalidate(id);
            if seen[&id] {
                self.selector_cache.borrow_mut().invalidate(id);
            }
            self.custom_prop_cache.borrow_mut().1.remove(&id);
            self.properties.invalidate(id);
            self.matched_cache.borrow_mut().invalidate(id);
            self.cascaded_cache.borrow_mut().invalidate(id);
            self.font_cache.borrow_mut().invalidate(id);
            self.font_units_cache.borrow_mut().invalidate(id);
            self.decoration_cache.borrow_mut().invalidate(id);
            self.layout_cache.borrow_mut().invalidate(id);
            self.box_tree_cache.borrow_mut().invalidate(id);
        }
        resource_roots.extend_from_slice(&roots);
        self.invalidate_layout_paths(resource_roots);
    }

    /// Whether `root`'s subtree holds a radio button or a form: either can
    /// change another radio's group (HTML #radio-button-group), including
    /// controls elsewhere that name the form through `form=`.
    fn subtree_has_radio_group_member(&self, root: NodeId) -> bool {
        let mut stack = vec![root];
        while let Some(node) = stack.pop() {
            match self.tag_name(node) {
                Some("form") => return true,
                Some("input") if self.input_type(node) == "radio" => return true,
                _ => {}
            }
            self.push_composed_children(node, &mut stack);
        }
        false
    }

    /// DOM #find-slotables and CSS Shadow 1 #flattening, #slotted-pseudo: a
    /// child-list change can reassign slottables, which changes `::slotted()`
    /// matches and the flat-tree parent their inherited values come from.
    /// Only a shadow host's children are slottables, and only inserting or
    /// removing a `<slot>` changes which slot receives them. The changed
    /// subtrees themselves are restyled by the tree operation.
    fn child_list_may_reassign_slots(&self, parent: NodeId, changed: &[NodeId]) -> bool {
        if !self.shadow_style_dependencies(parent) {
            return false;
        }
        if self.shadow_root(parent).is_some() || changed.is_empty() {
            return true;
        }
        changed.iter().any(|&node| {
            self.nodes.get(node).is_some()
                && (self.tag_name(node) == Some("slot")
                    || self
                        .descendants(node)
                        .any(|descendant| self.tag_name(descendant) == Some("slot")))
        })
    }

    /// `changed` names the inserted/removed children (empty when unknown).
    pub(super) fn invalidate_structure(&mut self, parent: NodeId, changed: &[NodeId]) {
        // Detached construction uses the same dependency proof. Recursively
        // invalidating its entire growing root on each append would make a
        // fragment-building loop quadratic even without structural selectors.
        let local = (!self.child_list_may_reassign_slots(parent, changed))
            .then(|| {
                let index = self.style_cache.borrow();
                index.as_ref().and_then(|(epoch, index)| {
                    let dependencies = index.selector_dependencies.for_node(self, parent);
                    let relational_root = dependencies.relational_root(self, parent);
                    (*epoch == self.style_epoch
                        && !dependencies.structure_global
                        // Selectors 4 #the-dir-pseudo / HTML directionality:
                        // only an automatic-direction ancestor can acquire a
                        // different direction when its descendants change.
                        && !(dependencies.text_direction
                            && self.text_may_change_direction(parent))
                        && !(dependencies.indeterminate
                            && (changed.is_empty()
                                || changed.iter().any(|&node| self.subtree_has_radio_group_member(node))))
                        && !(dependencies.text_placeholder
                            && self.tag_name(parent) == Some("textarea")))
                    .then(|| {
                        let (empty_siblings, restyle_children) =
                            dependencies.structure_impact(self, parent);
                        (empty_siblings, restyle_children, relational_root)
                    })
                })
            })
            .flatten();
        let Some((empty_siblings, restyle_children, relational_root)) = local else {
            if casc_diag_on() {
                let index = self.style_cache.borrow();
                eprintln!(
                    "DIAGINVALID structure node={parent} tag={:?} id={:?} slots={} dependencies={:?}",
                    self.tag_name(parent),
                    self.attr(parent, "id"),
                    self.child_list_may_reassign_slots(parent, changed),
                    index.as_ref().map(|(epoch, index)| {
                        let dependencies = index.selector_dependencies.for_node(self, parent);
                        (
                            *epoch == self.style_epoch,
                            dependencies.structure_global,
                            dependencies
                                .relational
                                .iter()
                                .any(|d| d.may_affect(self, parent, true)),
                            dependencies.text_direction && self.text_may_change_direction(parent),
                        )
                    })
                );
            }
            self.selector_epoch = self.selector_epoch.wrapping_add(1);
            self.invalidate_all_style_values();
            return;
        };
        // Selectors 4 #relational: a possible :has() anchor above the change.
        if let Some(root) = relational_root {
            self.invalidate_style_subtree(root, true);
        }
        // Insertions/removals change sibling ranks, adjacency, inherited
        // parentage and the parent's :empty state. A parent :empty followed
        // by a sibling combinator can also restyle its sibling forest.
        let root = if empty_siblings {
            self.nodes[parent].parent.unwrap_or(parent)
        } else {
            parent
        };
        if restyle_children
            || matches!(
                self.tag_name(parent),
                // Replacing a body's document-wide link-color hints can
                // also restyle links outside that body's subtree.
                Some("html" | "picture" | "select" | "optgroup" | "fieldset")
            )
        {
            self.invalidate_style_subtree(root, true);
        } else {
            // Without child-index, sibling, :empty or relational rules, old
            // siblings keep their selectors and inherited styles. The actual
            // inserted/removed subtree is invalidated by the tree operation.
            // This also holds for a form. HTML #reset-the-form-owner resolves
            // ownership from the current tree; inserting a measurement span
            // does not restyle its existing controls. State selectors with
            // association dependencies retain the global fallback above.
            // Only the parents' formatting structure/flow needs rebuilding.
            if matches!(self.tag_name(parent), Some("meta" | "base")) {
                self.layout_cache.get_mut().clear();
                self.box_tree_cache.get_mut().clear();
            }
            let mut roots = self.svg_dirty_consumers(&[parent], true);
            roots.push(parent);
            self.invalidate_layout_paths(roots);
        }
    }

    pub(super) fn touch_input_value(&mut self, node: NodeId) {
        // A dirty input value does not mutate its attribute or child list
        // (HTML #dom-input-value). Keep the state-dependent selector fallback,
        // but do not discard unrelated trees merely to reshape control text.
        let independent = !self.shadow_style_dependencies(node)
            && !self.text_may_change_direction(node)
            && self
                .style_cache
                .borrow()
                .as_ref()
                .is_some_and(|(epoch, index)| {
                    *epoch == self.style_epoch
                        && !index
                            .selector_dependencies
                            .for_node(self, node)
                            .text_placeholder
                });
        if !independent {
            // Placeholder state must invalidate its selector subjects, even
            // when the value transition leaves the element tree unchanged.
            self.touch_attr(node, "value");
            return;
        }
        self.invalidate_layout_ancestors(node);
        self.mark_dom_revision();
        self.dirty_nodes.push((node, DirtyKind::Content));
        self.record_geometry_dirty(node, DirtyKind::Content);
    }

    /// Character data / replace-all of text children leaves the element tree
    /// intact. Selectors 4 :empty is relevant only when its truth value changes;
    /// directionality/state and shadow distribution keep the broad fallback.
    /// `character_data` identifies an existing Text node, whose slot assignment
    /// cannot change merely because its data changes (DOM #find-slotables).
    pub(super) fn touch_text(
        &mut self,
        parent: NodeId,
        empty_changed: bool,
        direction_changed: bool,
        character_data: Option<NodeId>,
    ) {
        let stable_shadow_text = character_data.is_some() && !empty_changed && !direction_changed;
        // A shadow host's Text children are slottables (DOM #find-slotables).
        let independent = (self.shadow_root(parent).is_none() || stable_shadow_text) && {
            let index = self.style_cache.borrow();
            index.as_ref().is_some_and(|(epoch, index)| {
                let dependencies = index.selector_dependencies.for_node(self, parent);
                *epoch == self.style_epoch
                    && !(direction_changed
                        && dependencies.text_direction
                        && self.text_may_change_direction(parent))
                    && !(dependencies.text_placeholder && self.tag_name(parent) == Some("textarea"))
                    && !(empty_changed && dependencies.empty)
            })
        };
        if !independent {
            // Character data moves no elements; only the text node changed.
            self.touch_content(Some(parent), character_data.as_slice());
            return;
        }
        let mut roots = self.svg_dirty_consumers(&[parent], false);
        roots.push(character_data.unwrap_or(parent));
        self.invalidate_layout_paths(roots);
        self.mark_dom_revision();
        self.dirty_nodes.push((parent, DirtyKind::Content));
        // CSS Lists 3 #inheriting-counters / Content 3 #quote-values:
        // counters and quote depth follow elements and generated content.
        // This text-only change preserves their tree order and styles, so
        // unrelated counter/quote subtrees retain their layout. A subsequent
        // container-query style change still expires layout through the query
        // update, and the selector-dependent cases above keep the fallback.
        self.record_local_geometry_dirty(parent, DirtyKind::Content);
    }

    /// Selectors 4 #the-dir-pseudo and HTML #contained-text-auto-directionality:
    /// text only affects directionality inside an automatic-direction subtree.
    /// Explicit ltr/rtl and excluded descendants stop the upward dependency.
    /// The walk follows the flat tree (`style_parent`), as directionality does
    /// through slots and shadow roots.
    fn text_may_change_direction(&self, parent: NodeId) -> bool {
        let mut current = Some(parent);
        while let Some(node) = current {
            match self.attr(node, "dir") {
                Some(value) if value.eq_ignore_ascii_case("auto") => return true,
                Some(value)
                    if value.eq_ignore_ascii_case("ltr") || value.eq_ignore_ascii_case("rtl") =>
                {
                    return false;
                }
                _ => {}
            }
            match self.tag_name(node) {
                Some("bdi") => return true,
                Some("script" | "style" | "textarea") => return false,
                _ => {}
            }
            current = self.style_parent(node);
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deferred_styles_coalesce_writes_and_keep_live_inherited_reads() {
        let mut dom = Dom::parse_document(&format!(
            "<style>#root {{color:red}} #stable {{color:blue}}</style><body><div id=root>{}</div><aside id=stable>other</aside>",
            "<span>text</span>".repeat(200)
        ));
        let root = dom.get_by_id("root").unwrap();
        let stable = dom.get_by_id("stable").unwrap();
        let leaf = dom.child_iter(root).next().unwrap();
        assert_eq!(
            dom.computed_value_resolved(leaf, "color").as_deref(),
            Some("red")
        );
        let retained = dom.cascaded_maps(stable);
        let before = dom.pending_style_invalidations.flushes.get();
        let visited = dom.pending_style_invalidations.visited.get();
        for n in 0..100 {
            dom.set_attr(root, "style", &format!("color:green;width:{n}px"));
        }
        if deferred_invalidation_enabled() {
            assert_eq!(dom.pending_style_invalidations.flushes.get(), before);
            assert_eq!(dom.pending_style_invalidations.visited.get(), visited);
        }
        assert_eq!(
            dom.computed_value_resolved(root, "width").as_deref(),
            Some("99px")
        );
        assert_eq!(
            dom.computed_value_resolved(leaf, "color").as_deref(),
            Some("green")
        );
        if deferred_invalidation_enabled() {
            assert_eq!(dom.pending_style_invalidations.flushes.get(), before + 1);
            assert!(dom.pending_style_invalidations.visited.get() - visited <= 401);
        }
        assert!(std::rc::Rc::ptr_eq(&retained, &dom.cascaded_maps(stable)));
        dom.set_attr(root, "style", "color:purple");
        assert_eq!(
            dom.computed_value_resolved(leaf, "color").as_deref(),
            Some("purple")
        );
        assert_style_values_match_cold(&mut dom);
    }

    #[test]
    fn deferred_styles_replace_all_preserves_departing_subtree_invalidation() {
        let mut dom = Dom::parse_document(
            "<style>#parent article span{height:41px}</style><div id=parent><article id=old><span id=leaf>kept identity</span></article></div>",
        );
        let parent = dom.get_by_id("parent").unwrap();
        let old = dom.get_by_id("old").unwrap();
        let leaf = dom.get_by_id("leaf").unwrap();
        assert_eq!(
            dom.computed_value_resolved(leaf, "height").as_deref(),
            Some("41px")
        );
        dom.replace_all_children(parent, Vec::new());
        assert_eq!(dom.nodes[old].parent, None);
        assert_eq!(dom.nodes[leaf].parent, Some(old));
        assert_ne!(
            dom.computed_value_resolved(leaf, "height").as_deref(),
            Some("41px")
        );
        assert_matches_full_scan(&dom);
        assert_style_values_match_cold(&mut dom);
    }

    #[test]
    fn deferred_styles_follow_moves_and_overlapping_selector_and_inheritance_changes() {
        let mut dom = Dom::parse_document(
            "<style>#left{--c:red} #right{--c:blue} span{color:var(--c)} .flag{font-size:30px} span:nth-child(2){height:9px}</style><body><div id=left><span id=a>A</span><span id=b>B</span></div><div id=right><span id=c>C</span></div></body>",
        );
        let left = dom.get_by_id("left").unwrap();
        let right = dom.get_by_id("right").unwrap();
        let a = dom.get_by_id("a").unwrap();
        let b = dom.get_by_id("b").unwrap();
        let c = dom.get_by_id("c").unwrap();
        assert_eq!(
            dom.computed_value_resolved(a, "color").as_deref(),
            Some("red")
        );
        assert_eq!(
            dom.computed_value_resolved(b, "height").as_deref(),
            Some("9px")
        );
        dom.set_attr(left, "style", "--c:green");
        dom.set_attr(a, "class", "flag");
        dom.append(right, a);
        dom.append(left, c);
        assert_eq!(
            dom.computed_value_resolved(a, "color").as_deref(),
            Some("blue")
        );
        assert_eq!(
            dom.computed_value_resolved(c, "color").as_deref(),
            Some("green")
        );
        assert_eq!(
            dom.computed_value_resolved(a, "font-size").as_deref(),
            Some("30px")
        );
        assert_eq!(
            dom.computed_value_resolved(c, "height").as_deref(),
            Some("9px")
        );
        assert_ne!(
            dom.computed_value_resolved(b, "height").as_deref(),
            Some("9px")
        );
        assert_style_values_match_cold(&mut dom);
    }

    fn assert_style_values_match_cold(dom: &mut Dom) {
        let properties = [
            "color",
            "font-size",
            "width",
            "height",
            "line-height",
            "text-decoration",
            "padding-left",
            "display",
        ];
        let values = |dom: &Dom| {
            dom.live_ids()
                .filter(|&id| dom.tag_name(id).is_some())
                .map(|id| {
                    (
                        id,
                        properties
                            .iter()
                            .map(|prop| dom.computed_value_resolved(id, prop))
                            .collect::<Vec<_>>(),
                        dom.font_px(id),
                        dom.text_decoration(id),
                    )
                })
                .collect::<Vec<_>>()
        };
        let warm = values(dom);
        dom.mark();
        let cold = values(dom);
        if warm != cold {
            for (w, c) in warm.iter().zip(&cold) {
                if w != c {
                    eprintln!(
                        "DIFF {:?} parent={:?} flat={:?}\n  warm {w:?}\n  cold {c:?}",
                        dom.tag_name(w.0),
                        dom.nodes[w.0].parent,
                        dom.parent_flat(w.0)
                    );
                }
            }
        }
        assert_eq!(warm, cold, "local invalidation differs from full recascade");
    }

    #[test]
    fn detached_construction_preserves_independent_cached_styles() {
        let mut dom = Dom::parse_document(
            "<style>.wide {font-size:30px} span {color:green}</style><body>live</body>",
        );
        let root = dom.create_element("div");
        let stable = dom.create_element("span");
        let changing = dom.create_element("section");
        dom.append(root, stable);
        dom.append(root, changing);
        let before = dom.cascaded_maps(stable);
        for _ in 0..100 {
            let child = dom.create_element("span");
            dom.set_text(child, "new");
            dom.append(changing, child);
            dom.set_attr(child, "class", "wide");
        }
        assert!(std::rc::Rc::ptr_eq(&before, &dom.cascaded_maps(stable)));
        assert_style_values_match_cold(&mut dom);
    }

    #[test]
    fn form_measurement_children_preserve_independent_control_styles() {
        let mut dom = Dom::parse_document(
            "<style>form {font-size:18px} input {color:purple} input:disabled {color:gray}</style>\
             <form id=entry><input id=editor value=hello><fieldset disabled><input id=disabled></fieldset></form>\
             <form id=other></form><input id=external form=entry>",
        );
        assert_style_values_match_cold(&mut dom);
        let entry = dom.get_by_id("entry").unwrap();
        let editor = dom.get_by_id("editor").unwrap();
        let external = dom.get_by_id("external").unwrap();
        let probe = dom.create_element("span");
        dom.set_text(probe, "measurement");
        for append in [true, false] {
            let retained = dom.cascaded_maps(editor);
            if append {
                dom.append(entry, probe);
            } else {
                dom.detach(probe);
            }
            assert!(std::rc::Rc::ptr_eq(&retained, &dom.cascaded_maps(editor)));
            assert_eq!(dom.form_owner(editor), Some(entry));
            assert_eq!(dom.form_owner(external), Some(entry));
            assert_style_values_match_cold(&mut dom);
        }
        let other = dom.get_by_id("other").unwrap();
        dom.append(other, editor);
        assert_eq!(dom.form_owner(editor), Some(other));
        assert_eq!(dom.form_owner(external), Some(entry));
        assert_style_values_match_cold(&mut dom);

        // Positional/relational dependencies and inherited form styles still
        // follow the changed child list (Selectors 4 §§4.5, 14.2, 14.3).
        let mut dom = Dom::parse_document(
            "<style>form:empty {color:red} form:has(> span) {font-size:27px}\
             form > input:last-child {padding-left:9px} form:has(input:required) {color:blue}</style>\
             <form id=entry><input required></form><form id=other></form>",
        );
        assert_style_values_match_cold(&mut dom);
        let entry = dom.get_by_id("entry").unwrap();
        let other = dom.get_by_id("other").unwrap();
        let probe = dom.create_element("span");
        for parent in [entry, other] {
            dom.append(parent, probe);
            assert_style_values_match_cold(&mut dom);
            dom.detach(probe);
            assert_style_values_match_cold(&mut dom);
        }
    }

    #[test]
    fn structural_rules_only_invalidate_their_possible_parents() {
        let mut dom = Dom::parse_document(
            r#"<style>
            .menu > li:first-child {color:red}
            .menu > li:not(:last-child) span {font-size:23px}
            .menu > li:nth-child(2) + li {padding-left:7px}
            #empty:not(:empty) + p {color:blue}
            .cards > :is(article:first-child, article:last-child) {height:40px}
        </style><header id=header><ul class=menu id=menu><li id=first><span>A</span></li>
        <li id=second><span>B</span></li><li>C</li></ul><svg width=16 height=16><circle r=7/></svg>
        </header><div id=empty></div><p>following</p><section class=cards><article>card</article></section>"#,
        );
        assert_style_values_match_cold(&mut dom);
        let header = dom.get_by_id("header").unwrap();
        let menu = dom.get_by_id("menu").unwrap();
        let first = dom.get_by_id("first").unwrap();
        let second = dom.get_by_id("second").unwrap();
        let probe = dom.create_element("span");
        dom.set_text(probe, "measurement");
        for append in [true, false] {
            let retained = dom.cascaded_maps(first);
            if append {
                dom.append(header, probe);
            } else {
                dom.detach(probe);
            }
            assert!(
                std::rc::Rc::ptr_eq(&retained, &dom.cascaded_maps(first)),
                "inserting under the header does not change its nested list's ranks"
            );
            assert_style_values_match_cold(&mut dom);
        }
        for step in 0..5 {
            match step {
                0 => dom.insert_before(menu, second, Some(first)),
                1 => dom.detach(second),
                2 => dom.append(menu, second),
                3 => dom.append(dom.get_by_id("empty").unwrap(), probe),
                _ => dom.detach(probe),
            }
            assert_style_values_match_cold(&mut dom);
        }
    }

    #[test]
    fn structural_dependency_guards_cover_new_adjacency_and_logical_ancestors() {
        let mut dom = Dom::parse_document(
            r#"<style>
            #order .start + .target span {color:red}
            #order .start ~ .target:not(:last-child) {font-size:25px}
            .target:not(.start + .target) span {padding-left:9px}
            section:not(.outer > :first-child) p {height:35px}
            .outer > :is(:first-child, :last-child) p {color:green}
            #empty:empty ~ section p {width:90px}
        </style><div class=outer><div id=empty></div><section id=order>
        <i class=start id=start></i><b id=gap></b><div class=target id=target><span>T</span><p>P</p></div>
        <footer id=tail></footer></section><section><p>other</p></section></div>"#,
        );
        assert_style_values_match_cold(&mut dom);
        let order = dom.get_by_id("order").unwrap();
        let gap = dom.get_by_id("gap").unwrap();
        let target = dom.get_by_id("target").unwrap();
        let empty = dom.get_by_id("empty").unwrap();
        for step in 0..6 {
            match step {
                0 => dom.detach(gap), // the formerly non-adjacent pair now matches
                1 => dom.insert_before(order, gap, Some(target)),
                2 => dom.detach(dom.get_by_id("tail").unwrap()),
                3 => dom.append(empty, gap), // :empty affects following sibling forests
                4 => dom.detach(dom.get_by_id("start").unwrap()),
                _ => dom.detach(empty), // logical ancestor ranks change
            }
            assert_style_values_match_cold(&mut dom);
        }
    }

    #[test]
    fn detached_shadow_probe_does_not_invalidate_an_unrelated_measurement_tree() {
        let mut dom = Dom::parse_document(
            "<html dir=ltr><style>div:dir(rtl){color:red}</style><body id=body><main id=stable>live</main></body>",
        );
        let probe = dom.create_element("probe-host");
        dom.attach_shadow(probe);
        let stable = dom.get_by_id("stable").unwrap();
        let before = dom.cascaded_maps(stable);
        let measure = dom.create_element("span");
        dom.set_attr(measure, "style", "position:absolute;white-space:pre");
        dom.set_text(measure, "typed characters");
        assert!(std::rc::Rc::ptr_eq(&before, &dom.cascaded_maps(stable)));
        let body = dom.get_by_id("body").unwrap();
        dom.append(body, measure);
        dom.detach(measure);
        assert_style_values_match_cold(&mut dom);
    }

    #[test]
    fn document_dependencies_do_not_restyle_unrelated_frame_construction() {
        // DOM #concept-document-tree; Selectors 4 #match-against-tree;
        // CSS Cascade 5 #filtering. An arena presentation edge from a child
        // Document to its iframe is not a CSS selector/inheritance edge.
        let mut dom = Dom::parse_document(
            "<style>:lang(fr){color:red} #outer{color:green}</style>\
             <body><main id=outer>outer</main><iframe id=frame></iframe></body>",
        );
        let frame = dom.get_by_id("frame").unwrap();
        dom.install_frame_document(
            frame,
            "<style>#stable{color:blue}</style><body><main id=stable>stable</main>\
             <section id=changing></section></body>",
            "https://frame.test/",
        )
        .unwrap();
        let find = |name| {
            dom.descendants(frame)
                .find(|&id| dom.attr(id, "id") == Some(name))
                .unwrap()
        };
        let (stable, changing) = (find("stable"), find("changing"));
        let outer = dom.get_by_id("outer").unwrap();
        let child = dom.create_element("span");
        dom.set_text(child, "new");
        for append in [true, false, true] {
            let retained_outer = dom.cascaded_maps(outer);
            let retained_child = dom.cascaded_maps(stable);
            if append {
                dom.append(changing, child);
            } else {
                dom.detach(child);
            }
            assert!(std::rc::Rc::ptr_eq(
                &retained_outer,
                &dom.cascaded_maps(outer)
            ));
            assert!(std::rc::Rc::ptr_eq(
                &retained_child,
                &dom.cascaded_maps(stable)
            ));
            assert_eq!(dom.computed_value(outer, "color").as_deref(), Some("green"));
            assert_eq!(dom.computed_value(stable, "color").as_deref(), Some("blue"));
            assert_style_values_match_cold(&mut dom);
        }
    }

    #[test]
    fn document_attribute_dependencies_preserve_other_documents_and_live_matches() {
        let mut dom = Dom::parse_document(
            "<style>input:indeterminate{width:77px} #outer{color:green}</style>\
             <body><main id=outer>outer</main><iframe id=frame></iframe></body>",
        );
        let frame = dom.get_by_id("frame").unwrap();
        dom.install_frame_document(
            frame,
            "<style>input{width:11px} input:checked{width:29px}</style>\
             <body><input id=changing type=checkbox><main id=stable>stable</main></body>",
            "https://frame.test/",
        )
        .unwrap();
        let find = |name| {
            dom.descendants(frame)
                .find(|&id| dom.attr(id, "id") == Some(name))
                .unwrap()
        };
        let (changing, stable) = (find("changing"), find("stable"));
        let outer = dom.get_by_id("outer").unwrap();
        for checked in [true, false, true] {
            let retained_outer = dom.cascaded_maps(outer);
            let retained_child = dom.cascaded_maps(stable);
            if checked {
                dom.set_attr(changing, "checked", "");
            } else {
                dom.remove_attr(changing, "checked");
            }
            assert!(std::rc::Rc::ptr_eq(
                &retained_outer,
                &dom.cascaded_maps(outer)
            ));
            assert!(std::rc::Rc::ptr_eq(
                &retained_child,
                &dom.cascaded_maps(stable)
            ));
            assert_eq!(
                dom.computed_value(changing, "width").as_deref(),
                Some(if checked { "29px" } else { "11px" })
            );
            assert_style_values_match_cold(&mut dom);
        }
    }

    #[test]
    fn shared_sheet_parses_contribute_dependencies_to_every_document() {
        let mut dom =
            Dom::parse_document("<body><iframe id=left></iframe><iframe id=right></iframe></body>");
        let frames = [
            dom.get_by_id("left").unwrap(),
            dom.get_by_id("right").unwrap(),
        ];
        let html = "<style>.panel{height:11px}.panel:has(.flag){height:29px}</style>\
                    <body><main class=panel><span></span></main></body>";
        for &frame in &frames {
            dom.install_frame_document(frame, html, "https://frame.test/")
                .unwrap();
        }
        let documents = frames.map(|frame| dom.frame_document(frame).unwrap());
        let panels = frames.map(|frame| {
            dom.child_iter(dom.frame_body(frame).unwrap())
                .next()
                .unwrap()
        });
        let leaves = panels.map(|panel| dom.child_iter(panel).next().unwrap());
        let index = dom.style_index();
        assert!(
            std::sync::Arc::ptr_eq(
                &index.scopes[&documents[0]][0].data,
                &index.scopes[&documents[1]][0].data,
            ),
            "fixture must exercise one immutable parse shared by two Documents"
        );
        assert_eq!(index.selector_dependencies.documents.len(), 2);
        assert!(
            std::sync::Arc::ptr_eq(
                &index.selector_dependencies.documents[&documents[0]],
                &index.selector_dependencies.documents[&documents[1]],
            ),
            "identical per-Document rule sets share one compiled dependency graph"
        );
        for changed in [0, 1] {
            let retained = dom.cascaded_maps(panels[1 - changed]);
            dom.set_attr(leaves[changed], "class", "flag");
            assert_eq!(
                dom.computed_value(panels[changed], "height").as_deref(),
                Some("29px")
            );
            assert!(std::rc::Rc::ptr_eq(
                &retained,
                &dom.cascaded_maps(panels[1 - changed])
            ));
            assert_style_values_match_cold(&mut dom);
            dom.remove_attr(leaves[changed], "class");
            assert_eq!(
                dom.computed_value(panels[changed], "height").as_deref(),
                Some("11px")
            );
            assert_style_values_match_cold(&mut dom);
        }
    }

    #[test]
    fn document_dependency_proofs_follow_stylesheet_replacement() {
        // HTML's "update a style block" removes the old sheet before installing
        // the new one. A retained per-Document proof must never outlive that
        // rule-set change, including newly introduced relational dependencies.
        let mut dom = Dom::parse_document("<body><iframe id=frame></iframe></body>");
        let frame = dom.get_by_id("frame").unwrap();
        dom.install_frame_document(
            frame,
            "<style>section{height:3px}</style><body><section></section></body>",
            "https://frame.test/",
        )
        .unwrap();
        let sheet = dom
            .descendants(frame)
            .find(|&id| dom.tag_name(id) == Some("style"))
            .unwrap();
        let section = dom
            .child_iter(dom.frame_body(frame).unwrap())
            .next()
            .unwrap();
        assert_eq!(
            dom.computed_value(section, "height").as_deref(),
            Some("3px")
        );
        for height in [11, 17] {
            dom.set_text(
                sheet,
                &format!(
                    "section{{height:3px}} section:empty{{height:{height}px}} \
                 section:has(> em){{height:29px}}"
                ),
            );
            assert_eq!(
                dom.computed_value(section, "height").as_deref(),
                Some(format!("{height}px").as_str())
            );
            let child = dom.create_element("em");
            dom.append(section, child);
            assert_eq!(
                dom.computed_value(section, "height").as_deref(),
                Some("29px")
            );
            assert_style_values_match_cold(&mut dom);
            dom.detach(child);
            assert_eq!(
                dom.computed_value(section, "height").as_deref(),
                Some(format!("{height}px").as_str())
            );
            assert_style_values_match_cold(&mut dom);
        }
    }

    #[test]
    fn shadow_dependencies_stop_at_document_roots_and_follow_adoption() {
        let mut dom = Dom::parse_document(
            "<body><main id=outer>outer</main><iframe id=left></iframe>\
             <iframe id=right></iframe></body>",
        );
        let (left, right) = (
            dom.get_by_id("left").unwrap(),
            dom.get_by_id("right").unwrap(),
        );
        for (frame, color) in [(left, "red"), (right, "blue")] {
            dom.install_frame_document(
                frame,
                &format!(
                    "<body style='color:{color}'><main>stable</main><section></section></body>"
                ),
                "https://frame.test/",
            )
            .unwrap();
        }
        let (left_body, right_body) = (
            dom.frame_body(left).unwrap(),
            dom.frame_body(right).unwrap(),
        );
        let host = dom.create_element("x-host");
        dom.append(left_body, host);
        let shadow = dom.attach_shadow(host);
        let leaf = dom.create_element("span");
        dom.append(shadow, leaf);
        let outer = dom.get_by_id("outer").unwrap();
        let stable = dom.child_iter(right_body).next().unwrap();
        let changing = dom.child_iter(right_body).nth(1).unwrap();
        assert!(dom.shadow_style_dependencies(leaf));
        assert!(dom.shadow_style_dependencies(left_body));
        assert!(!dom.shadow_style_dependencies(right_body));
        assert!(!dom.shadow_style_dependencies(outer));
        assert_eq!(
            dom.computed_value_resolved(leaf, "color").as_deref(),
            Some("red")
        );

        let probe = dom.create_element("span");
        let retained_outer = dom.cascaded_maps(outer);
        let retained_child = dom.cascaded_maps(stable);
        dom.append(changing, probe);
        assert!(std::rc::Rc::ptr_eq(
            &retained_outer,
            &dom.cascaded_maps(outer)
        ));
        assert!(std::rc::Rc::ptr_eq(
            &retained_child,
            &dom.cascaded_maps(stable)
        ));
        assert_style_values_match_cold(&mut dom);

        dom.append(right_body, host);
        assert_eq!(dom.owner_document(leaf), dom.frame_document(right));
        assert!(!dom.shadow_style_dependencies(left_body));
        assert!(dom.shadow_style_dependencies(right_body));
        assert_eq!(
            dom.computed_value_resolved(leaf, "color").as_deref(),
            Some("blue")
        );
        dom.set_attr(host, "style", "color:purple");
        assert_eq!(
            dom.computed_value_resolved(leaf, "color").as_deref(),
            Some("purple")
        );
        assert_style_values_match_cold(&mut dom);
    }

    #[test]
    fn shadow_rules_invalidate_only_their_hosts_and_slottables() {
        // CSS Shadow 1 #host-selector, #slotted-pseudo and #flattening (local
        // CSSWG snapshot 81c27f686901): `:host()` tests the host and continues
        // inside its shadow tree, `::slotted()` names elements assigned to a
        // slot, and inheritance follows the flat tree. A document with a
        // shadow root (reflection.ai's consent banner, every Next.js route
        // announcer) used to restyle every element on any mutation.
        let mut dom = Dom::parse_document(
            "<style>.wide {font-size:30px} .tone {color:green}</style>\
             <body><main id=stable class=tone>stable<span id=stable_child>s</span></main>\
             <div id=toggler>toggle</div>\
             <x-host id=host class=one><span id=slotted class=s>slotted<b id=deep>deep</b></span></x-host></body>",
        );
        let id = |dom: &Dom, name: &str| dom.get_by_id(name).unwrap();
        let (stable, toggler, host, slotted, deep) = (
            id(&dom, "stable"),
            id(&dom, "toggler"),
            id(&dom, "host"),
            id(&dom, "slotted"),
            id(&dom, "deep"),
        );
        let root = dom.attach_shadow(host);
        let style = dom.create_element("style");
        let css = dom.create_text(
            ":host(.dark) .inner {color:rgb(255, 0, 0)} :host(.dark) {font-size:20px} \
             ::slotted(.s.big) {font-size:31px} .wrap.blue {color:rgb(0, 0, 255)} \
             slot.loud::slotted(*) {padding-left:7px} :host(.tall) {line-height:3}",
        );
        dom.append(style, css);
        dom.append(root, style);
        let wrap = dom.create_element("div");
        dom.set_attr(wrap, "class", "wrap");
        let inner = dom.create_element("span");
        dom.set_attr(inner, "class", "inner");
        let text = dom.create_text("inner");
        dom.append(inner, text);
        let slot = dom.create_element("slot");
        dom.append(wrap, inner);
        dom.append(wrap, slot);
        dom.append(root, wrap);
        let value = |dom: &Dom, node, property| dom.computed_value_resolved(node, property);
        let retained = |dom: &Dom, before: &std::rc::Rc<CascadedMaps>| {
            std::rc::Rc::ptr_eq(before, &dom.cascaded_maps(stable))
        };
        assert_style_values_match_cold(&mut dom);

        // An unrelated light-tree class change stays local.
        let (before, epoch) = (dom.cascaded_maps(stable), dom.style_value_epoch);
        dom.set_attr(toggler, "class", "wide");
        assert_eq!(dom.font_px(toggler), 30.);
        assert_eq!(
            dom.style_value_epoch, epoch,
            "a light class change expired every style"
        );
        assert!(retained(&dom, &before));
        assert_style_values_match_cold(&mut dom);

        // :host() reaches the host and its shadow tree; inherited values
        // reach the slotted element through the slot.
        let (before, epoch) = (dom.cascaded_maps(stable), dom.style_value_epoch);
        dom.set_attr(host, "class", "one dark");
        assert_eq!(
            value(&dom, inner, "color").as_deref(),
            Some("rgb(255, 0, 0)")
        );
        assert_eq!(dom.font_px(host), 20.);
        assert_eq!(dom.font_px(slotted), 20.);
        assert_eq!(dom.font_px(deep), 20.);
        assert_eq!(
            dom.style_value_epoch, epoch,
            "a host class change expired every style"
        );
        assert!(retained(&dom, &before));
        assert_style_values_match_cold(&mut dom);

        // ::slotted() names the assigned element itself.
        let before = dom.cascaded_maps(stable);
        dom.set_attr(slotted, "class", "s big");
        assert_eq!(dom.font_px(slotted), 31.);
        assert_eq!(dom.font_px(deep), 31.);
        assert!(retained(&dom, &before));
        assert_style_values_match_cold(&mut dom);

        // A shadow-tree change reaches the slotted content it passes values to.
        let before = dom.cascaded_maps(stable);
        dom.set_attr(wrap, "class", "wrap blue");
        assert_eq!(
            value(&dom, slotted, "color").as_deref(),
            Some("rgb(0, 0, 255)")
        );
        assert_eq!(
            value(&dom, deep, "color").as_deref(),
            Some("rgb(0, 0, 255)")
        );
        assert!(retained(&dom, &before));
        assert_style_values_match_cold(&mut dom);

        // A slot's own class selects which elements ::slotted() represents.
        dom.set_attr(slot, "class", "loud");
        assert_eq!(value(&dom, slotted, "padding-left").as_deref(), Some("7px"));
        assert_style_values_match_cold(&mut dom);

        // An inline style on the host is inherited through the flat tree.
        let before = dom.cascaded_maps(stable);
        dom.set_attr(host, "style", "font-size:13px");
        assert_eq!(dom.font_px(wrap), 13.);
        assert_eq!(dom.font_px(inner), 13.);
        dom.set_attr(host, "class", "one dark tall");
        assert_eq!(
            value(&dom, wrap, "line-height").as_deref(),
            value(&dom, host, "line-height").as_deref()
        );
        assert!(retained(&dom, &before));
        assert_style_values_match_cold(&mut dom);

        // Child-list changes outside a host's children stay local; a host's
        // children are slottables, and a moved slot reassigns them.
        let (before, epoch) = (dom.cascaded_maps(stable), dom.style_value_epoch);
        let probe = dom.create_element("span");
        dom.append(toggler, probe);
        dom.set_attr(probe, "class", "wide");
        assert_eq!(
            dom.style_value_epoch, epoch,
            "a light insertion expired every style"
        );
        assert!(retained(&dom, &before));
        assert_style_values_match_cold(&mut dom);
        let extra = dom.create_element("em");
        dom.set_attr(extra, "class", "s big");
        dom.append(host, extra);
        assert_eq!(dom.font_px(extra), 31.);
        assert_style_values_match_cold(&mut dom);
        let first = dom.create_element("slot");
        dom.insert_before(wrap, first, Some(inner));
        assert_style_values_match_cold(&mut dom);
    }

    #[test]
    fn random_mutations_across_shadow_trees_match_full_recascade() {
        for seed in [
            0x2545_f491_4f6c_dd1du64,
            0x9e37_79b9_7f4a_7c15,
            0xdead_beef_cafe_f00d,
        ] {
            random_shadow_mutations(seed, 600);
        }
    }

    #[test]
    #[ignore]
    fn stress_random_shadow_mutations() {
        for seed in 1..=60u64 {
            random_shadow_mutations(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1, 800);
        }
    }

    fn random_shadow_mutations(seed: u64, steps: usize) {
        // Incremental invalidation must never be less correct than a full
        // recascade (CSS Shadow 1 #host-selector, #slotted-pseudo,
        // #flattening; Selectors 4 #lang-pseudo). A forwarding slot, nested
        // hosts, `:host()`, `::slotted()`, inherited `:lang()` and `:has()`
        // inside the shadow tree, mutated in a fixed pseudo-random order.
        let mut dom = Dom::parse_document(
            "<style>.a .x {color:rgb(1, 0, 0)} .c + .x {font-size:11px} .e > .x {padding-left:3px} \
             .b {line-height:2} .d .y {color:rgb(0, 1, 0)} \
             .a ~ .y {height:4px} [slot=named] {width:6px} :nth-child(2 of .x) {padding-left:8px} \
             .b:has(> .c) {font-size:15px}</style>\
             <body><div id=l0 class=x><span id=l1 class=y>t</span></div>\
             <x-outer id=outer><p id=s0 class=x>one</p><p id=s1 class=y>two</p></x-outer>\
             <section id=l2><i id=l3 class=x>i</i></section></body>",
        );
        let outer = dom.get_by_id("outer").unwrap();
        let outer_root = dom.attach_shadow(outer);
        let shadow_css = ":host(.a) .x {color:rgb(2, 0, 0)} :host(.b) {font-size:21px} \
             :host(:not(.c)) .y {padding-left:5px} ::slotted(.d) {line-height:3} \
             slot.e::slotted(*) {color:rgb(0, 0, 2)} .f .x {font-size:13px} \
             :lang(fr) {text-decoration:underline} .x:has(.g) {width:7px} :host .h {height:9px} \
             slot[name=named]::slotted(*) {line-height:4} :host([lang]) .y {width:2px} \
             .x > .y {color:rgb(3, 3, 3)} :not(:host(.h)) .x {height:5px} .f + .y {display:block}";
        let mut elements = vec![
            dom.get_by_id("l0").unwrap(),
            dom.get_by_id("l1").unwrap(),
            dom.get_by_id("l2").unwrap(),
            dom.get_by_id("l3").unwrap(),
            dom.get_by_id("s0").unwrap(),
            dom.get_by_id("s1").unwrap(),
            outer,
        ];
        let build = |dom: &mut Dom, root: NodeId, css: &str, elements: &mut Vec<NodeId>| {
            let style = dom.create_element("style");
            let text = dom.create_text(css);
            dom.append(style, text);
            dom.append(root, style);
            let wrap = dom.create_element("div");
            dom.set_attr(wrap, "class", "x");
            let leaf = dom.create_element("span");
            dom.set_attr(leaf, "class", "y");
            let text = dom.create_text("leaf");
            dom.append(leaf, text);
            let slot = dom.create_element("slot");
            dom.append(wrap, leaf);
            dom.append(wrap, slot);
            dom.append(root, wrap);
            elements.extend([wrap, leaf, slot]);
            (wrap, slot)
        };
        let (outer_wrap, _) = build(&mut dom, outer_root, shadow_css, &mut elements);
        // A nested host forwards the outer slot's content through its own.
        let inner = dom.create_element("x-inner");
        dom.append(outer_wrap, inner);
        let forward = dom.create_element("slot");
        dom.append(inner, forward);
        let inner_root = dom.attach_shadow(inner);
        build(&mut dom, inner_root, shadow_css, &mut elements);
        elements.extend([inner, forward]);
        assert_style_values_match_cold(&mut dom);

        let mut state = seed;
        let mut next = |bound: usize| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state % bound as u64) as usize
        };
        let classes = ["a", "b", "c", "d", "e", "f", "g", "h", "x", "y"];
        let shadow_parents = elements.clone();
        for _ in 0..steps {
            let node = elements[next(elements.len())];
            match next(9) {
                0 => dom.set_attr(node, "lang", ["fr", "en"][next(2)]),
                1 => dom.set_attr(node, "style", &format!("font-size:{}px", 10 + next(8))),
                2 => {
                    // Light or shadow insertion, possibly of a slot.
                    let parent = shadow_parents[next(shadow_parents.len())];
                    let child = dom.create_element(["span", "slot", "b"][next(3)]);
                    dom.set_attr(child, "class", classes[next(classes.len())]);
                    dom.append(parent, child);
                    elements.push(child);
                }
                3 if node != outer && node != inner => dom.detach(node),
                4 => dom.set_attr(node, "slot", ["", "named"][next(2)]),
                5 => dom.set_attr(node, "name", ["", "named"][next(2)]),
                _ => {
                    let tokens: Vec<_> = classes.iter().filter(|_| next(3) == 0).copied().collect();
                    dom.set_attr(node, "class", &tokens.join(" "));
                }
            }
            assert_style_values_match_cold(&mut dom);
        }
    }

    #[test]
    fn detached_shadow_inheritance_and_adoption_expire_cross_tree_styles() {
        let mut dom = Dom::parse_document("<body>live document</body>");
        let host = dom.create_element("x-host");
        let shadow = dom.attach_shadow(host);
        let leaf = dom.create_element("span");
        dom.append(shadow, leaf);
        dom.set_attr(host, "style", "color:red;font-size:12px");
        assert_eq!(
            dom.computed_value_resolved(leaf, "color").as_deref(),
            Some("red")
        );
        assert_eq!(dom.font_px(leaf), 12.);
        dom.set_attr(host, "style", "color:blue;font-size:24px");
        assert_eq!(
            dom.computed_value_resolved(leaf, "color").as_deref(),
            Some("blue")
        );
        assert_eq!(dom.font_px(leaf), 24.);
        assert_style_values_match_cold(&mut dom);

        let document = dom.parse_document_into("<body>new document</body>");
        let before = dom.cascaded_maps(leaf);
        assert_eq!(dom.adopt_node(document, host), Ok(DOCUMENT));
        for id in [host, shadow, leaf] {
            assert_eq!(dom.owner_document(id), Some(document));
        }
        assert!(!std::rc::Rc::ptr_eq(&before, &dom.cascaded_maps(leaf)));
        assert_style_values_match_cold(&mut dom);
    }

    #[test]
    fn inherited_numeric_lengths_expire_when_font_metrics_change() {
        let dom = Dom::parse_document(
            "<style>#parent {line-height:2ch}</style><div id=parent><span id=leaf>text</span></div>",
        );
        let leaf = dom.get_by_id("leaf").unwrap();
        let expected = dom.computed_value(leaf, "line-height");
        {
            let mut cache = dom.computed_cache.borrow_mut();
            cache.0.1 = crate::font_system::page_font_epoch().wrapping_sub(1);
            cache.1.insert(
                (leaf, prop_index("line-height").unwrap()),
                Some("999px".to_owned()),
            );
        }
        assert_eq!(dom.computed_value(leaf, "line-height"), expected);
    }

    #[test]
    fn noninherited_values_remain_current_across_warm_style_reads() {
        // CSS Cascade 5 #computed / #inherit / #inherit-initial: explicit
        // inheritance also applies to normally non-inherited properties.
        // Their computed percentage remains a percentage until layout.
        let mut dom = Dom::parse_document(
            r#"<style>
            #parent { width:50%; --edge:3px; opacity:.7 }
            #leaf { width:inherit; padding-left:var(--edge); opacity:unset }
            #parent.changed { width:75%; --edge:9px }
            #parent:has(.flag) #leaf { opacity:.4 }
            #leaf:empty { margin-left:7px }
            </style><div id=parent><div id=leaf>text</div></div>"#,
        );
        let parent = dom.get_by_id("parent").unwrap();
        let leaf = dom.get_by_id("leaf").unwrap();
        for _ in 0..2 {
            assert_eq!(dom.computed_value(leaf, "width").as_deref(), Some("50%"));
            assert_eq!(
                dom.computed_value_resolved(leaf, "padding-left").as_deref(),
                Some("3px")
            );
            assert_eq!(dom.computed_value(leaf, "opacity"), None);
            assert_eq!(dom.computed_value(leaf, "margin-left"), None);
        }
        dom.set_attr(parent, "class", "changed");
        assert_eq!(dom.computed_value(leaf, "width").as_deref(), Some("75%"));
        assert_eq!(
            dom.computed_value_resolved(leaf, "padding-left").as_deref(),
            Some("9px")
        );
        dom.set_text(leaf, "");
        assert_eq!(
            dom.computed_value(leaf, "margin-left").as_deref(),
            Some("7px")
        );
        let flag = dom.create_element("span");
        dom.set_attr(flag, "class", "flag");
        dom.append(parent, flag);
        assert_eq!(dom.computed_value(leaf, "opacity").as_deref(), Some(".4"));
        assert_style_values_match_cold(&mut dom);
        dom.detach(flag);
        assert_eq!(dom.computed_value(leaf, "opacity"), None);
        dom.set_attr(leaf, "style", "width:initial;opacity:inherit");
        assert_eq!(dom.computed_value(leaf, "width"), None);
        assert_eq!(dom.computed_value(leaf, "opacity").as_deref(), Some(".7"));
        assert_style_values_match_cold(&mut dom);

        // CSS Syntax 3 #consume-token / #consume-ident-like-token: escaped
        // and commented keywords still take the complete tokenizer path.
        for keyword in ["inherit", "INHERIT", r"\69 nherit", "/*a*/inherit/*b*/"] {
            dom.set_attr(leaf, "style", &format!("width:var(--missing,{keyword})"));
            assert_eq!(
                dom.computed_value_resolved(leaf, "width").as_deref(),
                Some("75%")
            );
        }
        for value in ["25%", "calc(20px + 3%)", "auto"] {
            dom.set_attr(
                leaf,
                "style",
                &format!("--value:{value};width:var(--value)"),
            );
            assert_eq!(
                dom.computed_value_resolved(leaf, "width").as_deref(),
                Some(value)
            );
        }
    }

    #[test]
    fn computed_styles_survive_text_ticks_and_unrelated_attribute_writes() {
        let mut dom = Dom::parse_document(
            r#"<style>
            body { color:green; --size:17px }
            #clock:empty + aside { color:red }
            #island { font-size:var(--size) }
        </style><div id=clock>12:00</div><aside id=island><b id=leaf>unchanged</b></aside>"#,
        );
        let clock = dom.get_by_id("clock").unwrap();
        let island = dom.get_by_id("island").unwrap();
        let before = dom.cascaded_maps(island);
        let font = dom.font_px(island);
        let child = dom.children(clock)[0];
        dom.set_text(clock, "12:01");
        assert_ne!(
            dom.children(clock)[0],
            child,
            "replace-all must create a new Text identity"
        );
        assert!(dom.node(child).parent.is_none());
        assert!(std::rc::Rc::ptr_eq(&before, &dom.cascaded_maps(island)));
        dom.set_attr(clock, "style", "color:blue");
        assert!(std::rc::Rc::ptr_eq(&before, &dom.cascaded_maps(island)));
        assert_eq!(font, dom.font_px(island));
        assert_style_values_match_cold(&mut dom);
        dom.set_text(clock, "");
        assert_eq!(dom.computed_value(island, "color").as_deref(), Some("red"));
        assert_style_values_match_cold(&mut dom);
    }

    #[test]
    fn local_style_invalidation_preserves_inheritance_and_relational_dependencies() {
        let html = r#"<style>
            #parent { --size:12px; color:green }
            #parent.active { --size:24px; color:blue; text-decoration:underline }
            #leaf { font-size:var(--size); width:2em; line-height:150% }
            #parent.active + #other .desc { color:purple }
            body:has(#clock:empty) #other { --size:31px }
            #other { font-size:var(--size, 15px) }
        </style><div id=parent><span id=leaf>leaf</span><span id=clock>tick</span></div>
        <aside id=other><b class=desc>other</b></aside>"#;
        let mut dom = Dom::parse_document(html);
        assert_style_values_match_cold(&mut dom);
        for class in ["active", "", "active"] {
            dom.set_attr(dom.get_by_id("parent").unwrap(), "class", class);
            assert_style_values_match_cold(&mut dom);
        }
        for text in ["", "tock", " ", "tick"] {
            dom.set_text(dom.get_by_id("clock").unwrap(), text);
            assert_style_values_match_cold(&mut dom);
        }
        let leaf = dom.get_by_id("leaf").unwrap();
        let other = dom.get_by_id("other").unwrap();
        dom.append(other, leaf);
        assert_style_values_match_cold(&mut dom);
    }

    fn assert_matches_full_scan(dom: &Dom) {
        let index = dom.style_index();
        for node in dom.live_ids() {
            if dom.tag_name(node).is_none() {
                continue;
            }
            let scope = dom.tree_scope(node);
            let expected = index
                .scopes
                .get(&scope)
                .map(|rules| {
                    rules
                        .iter()
                        .enumerate()
                        .filter(|(_, rule)| {
                            dom.matches_complex(node, &rule.selector.0, None)
                                && (rule_pseudo(rule).is_some()
                                    || rule
                                        .containers
                                        .iter()
                                        .all(|query| query.matches(dom, node, false)))
                        })
                        .map(|(index, _)| index as u32)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            assert_eq!(
                *dom.matched_rules(node),
                expected,
                "stale selector matches at node {node}"
            );
        }
    }

    fn cached(dom: &Dom, id: &str) -> std::rc::Rc<Vec<u32>> {
        let node = dom.get_by_id(id).unwrap();
        let _ = dom.matched_rules(node);
        dom.selector_cache
            .borrow()
            .get(node, dom.selector_epoch)
            .unwrap()
            .clone()
    }

    #[test]
    fn class_invalidation_uses_changed_tokens_without_unrelated_sibling_dependencies() {
        let mut dom = Dom::parse_document(
            "<!doctype html><style>.unrelated + div {height:70px}.panel.open{height:30px}.panel:not(.open){height:10px}</style><main><div id=panel class=panel></div><div id=stable>stable</div></main>",
        );
        let panel = dom.get_by_id("panel").unwrap();
        let stable = cached(&dom, "stable");
        for class in [
            "panel open",
            "panel\topen open",
            "panel OPEN",
            "panel",
            "unrelated",
        ] {
            dom.set_attr(panel, "class", class);
            assert_matches_full_scan(&dom);
            if class != "unrelated" {
                assert!(
                    std::rc::Rc::ptr_eq(&stable, &cached(&dom, "stable")),
                    "{class}"
                );
                assert_eq!(
                    dom.computed_value_resolved(panel, "height").as_deref(),
                    Some(if class.split_ascii_whitespace().any(|c| c == "open") {
                        "30px"
                    } else {
                        "10px"
                    })
                );
            } else {
                assert_eq!(
                    dom.computed_value_resolved(dom.get_by_id("stable").unwrap(), "height")
                        .as_deref(),
                    Some("70px")
                );
            }
        }
        dom.remove_attr(panel, "class");
        assert_matches_full_scan(&dom);
        assert_eq!(
            dom.computed_value_resolved(dom.get_by_id("stable").unwrap(), "height"),
            None
        );
    }

    #[test]
    fn class_invalidation_preserves_raw_relational_and_filtered_index_dependencies() {
        for rule in [
            "[class='a b'] + div {width:12px}",
            ".root:has(.hit) .target{height:34px}",
            "div:nth-child(2 of .hit) .target{width:56px}",
            ":is(.on,.off) ~ div .target{margin-top:7px}",
        ] {
            let mut dom = Dom::parse_document(&format!(
                "<!doctype html><style>{rule}</style><main class=root><div id=first class=hit></div><div id=second class=hit><span class=target></span></div><div class=target></div></main>"
            ));
            let first = dom.get_by_id("first").unwrap();
            let second = dom.get_by_id("second").unwrap();
            for class in ["a b", "b a", "hit on", "off", "", "hit"] {
                dom.set_attr(first, "class", class);
                assert_matches_full_scan(&dom);
                assert_eq!(
                    dom.computed_value_resolved(second, "width").as_deref(),
                    if class == "a b" && rule.starts_with("[class") {
                        Some("12px")
                    } else {
                        None
                    }
                );
            }
            dom.remove_attr(first, "class");
            dom.remove_attr(second, "class");
            assert_matches_full_scan(&dom);
        }
    }

    #[test]
    fn unrelated_relational_anchors_preserve_styles_during_text_measurement() {
        let mut dom = Dom::parse_document(
            "<style>.dialog:has(.drop) .label{color:red}\
             #tools:has(> [data-active]){width:60px}\
             .previous:has(~ .current){height:35px}</style>\
             <main><form id=form><input id=input></form><aside id=stable>kept</aside></main>\
             <section><div class=dialog><i class=label>label</i></div>\
             <div id=tools></div><div class=previous></div><div></div></section>",
        );
        let before = cached(&dom, "stable");
        let form = dom.get_by_id("form").unwrap();
        let measure = dom.create_element("span");
        dom.set_text(measure, "measured text");
        dom.set_attr(measure, "class", "measurement");
        dom.append(form, measure);
        assert!(std::rc::Rc::ptr_eq(&before, &cached(&dom, "stable")));
        dom.set_attr(measure, "class", "drop current");
        dom.set_attr(measure, "data-active", "");
        assert!(std::rc::Rc::ptr_eq(&before, &cached(&dom, "stable")));
        assert_matches_full_scan(&dom);
        dom.detach(measure);
        assert!(std::rc::Rc::ptr_eq(&before, &cached(&dom, "stable")));
        assert_style_values_match_cold(&mut dom);
    }

    fn focus(dom: &mut Dom, id: Option<&str>) {
        let target = id.map(|id| dom.get_by_id(id).unwrap());
        dom.set_focused_area(DOCUMENT, target);
    }

    fn value_of(dom: &Dom, id: &str, property: &str) -> Option<String> {
        dom.computed_value_resolved(dom.get_by_id(id).unwrap(), property)
    }

    #[test]
    fn focus_changes_restyle_only_focus_dependent_subjects() {
        // Speedometer TodoMVC's `.todo-area :focus` and `.toggle:focus + label`,
        // plus :focus-within ancestors, :has() anchors and :nth-child(of S).
        let mut dom = Dom::parse_document(
            "<style>.area :focus{width:11px}\
             .toggle:focus + label{width:12px}\
             .form:focus-within .hint{color:red}\
             .form:focus-within{height:13px}\
             .card:has(input:focus) .title{width:14px}\
             li:nth-child(1 of .done:focus-within) span{color:green}\
             .form :not(:focus){padding-left:3px}</style>\
             <section class=area id=area>\
             <form class=form id=form><input id=a class=toggle><label id=la>a</label>\
             <p class=hint id=hint>hint</p><input id=b></form>\
             <div class=card id=card><input id=c><b class=title id=title>t</b></div>\
             <ul><li id=li1>x</li><li class=done id=li2><span id=span2>s</span><input id=d></li></ul>\
             </section>\
             <aside id=stable><span id=stable_child>independent</span></aside>",
        );
        assert_matches_full_scan(&dom);
        let stable = cached(&dom, "stable_child");
        let epoch = dom.style_value_epoch;
        let check = |dom: &mut Dom, stable: &std::rc::Rc<Vec<u32>>| {
            assert_eq!(dom.style_value_epoch, epoch, "focus expired every style");
            assert!(std::rc::Rc::ptr_eq(stable, &cached(dom, "stable_child")));
            assert_matches_full_scan(dom);
        };

        focus(&mut dom, Some("a"));
        assert_eq!(value_of(&dom, "a", "width").as_deref(), Some("11px"));
        assert_eq!(value_of(&dom, "la", "width").as_deref(), Some("12px"));
        assert_eq!(value_of(&dom, "hint", "color").as_deref(), Some("red"));
        assert_eq!(value_of(&dom, "form", "height").as_deref(), Some("13px"));
        assert_ne!(value_of(&dom, "a", "padding-left").as_deref(), Some("3px"));
        assert_eq!(value_of(&dom, "b", "padding-left").as_deref(), Some("3px"));
        check(&mut dom, &stable);

        // The form stays :focus-within, so its dependents keep their caches.
        let hint = dom.cascaded_maps(dom.get_by_id("hint").unwrap());
        focus(&mut dom, Some("b"));
        assert_ne!(value_of(&dom, "a", "width").as_deref(), Some("11px"));
        assert_eq!(value_of(&dom, "b", "width").as_deref(), Some("11px"));
        assert_ne!(value_of(&dom, "la", "width").as_deref(), Some("12px"));
        assert_eq!(value_of(&dom, "a", "padding-left").as_deref(), Some("3px"));
        assert_eq!(value_of(&dom, "hint", "color").as_deref(), Some("red"));
        assert!(std::rc::Rc::ptr_eq(
            &hint,
            &dom.cascaded_maps(dom.get_by_id("hint").unwrap())
        ));
        check(&mut dom, &stable);

        focus(&mut dom, Some("c"));
        assert_ne!(value_of(&dom, "hint", "color").as_deref(), Some("red"));
        assert_ne!(value_of(&dom, "form", "height").as_deref(), Some("13px"));
        assert_eq!(value_of(&dom, "title", "width").as_deref(), Some("14px"));
        check(&mut dom, &stable);

        focus(&mut dom, Some("d"));
        assert_ne!(value_of(&dom, "title", "width").as_deref(), Some("14px"));
        assert_eq!(value_of(&dom, "span2", "color").as_deref(), Some("green"));
        check(&mut dom, &stable);

        focus(&mut dom, None);
        assert_ne!(value_of(&dom, "span2", "color").as_deref(), Some("green"));
        assert_ne!(value_of(&dom, "d", "width").as_deref(), Some("11px"));
        check(&mut dom, &stable);
        assert_style_values_match_cold(&mut dom);
    }

    #[test]
    fn focus_leaving_a_removed_subtree_or_entering_a_navigable_container_restyles_ancestors() {
        let mut dom = Dom::parse_document(
            "<style>.wrap:focus-within .hint{color:red}iframe:focus{width:31px}\
             .wrap:focus-within{height:9px}</style>\
             <div class=wrap id=wrap><input id=a><iframe id=frame></iframe>\
             <p class=hint id=hint>h</p></div><div id=other><input id=b></div>",
        );
        let red = |dom: &Dom| value_of(dom, "hint", "color").as_deref() == Some("red");
        focus(&mut dom, Some("a"));
        assert!(red(&dom));
        // HTML #selector-focus excludes navigable containers, so neither
        // the iframe nor its ancestors match while it is the focused area.
        focus(&mut dom, Some("frame"));
        assert!(!red(&dom));
        assert_ne!(value_of(&dom, "frame", "width").as_deref(), Some("31px"));
        assert_ne!(value_of(&dom, "wrap", "height").as_deref(), Some("9px"));
        focus(&mut dom, Some("a"));
        assert!(red(&dom));
        assert_matches_full_scan(&dom);
        // Removal runs the focus fixup steps before unlinking.
        let a = dom.get_by_id("a").unwrap();
        dom.detach(a);
        assert_eq!(dom.focused_area(DOCUMENT), None);
        assert!(!red(&dom));
        assert_ne!(value_of(&dom, "wrap", "height").as_deref(), Some("9px"));
        focus(&mut dom, Some("b"));
        assert!(!red(&dom));
        assert_matches_full_scan(&dom);
        assert_style_values_match_cold(&mut dom);
    }

    #[test]
    fn fragment_navigation_restyles_only_target_subjects() {
        let mut dom = Dom::parse_document(
            "<style>:target{width:21px}:target + p{color:red}\
             section:has(> :target) h2{color:green}\
             p:not(:target){padding-left:2px}</style>\
             <section id=s1><h2 id=h1>a</h2><p id=one>One</p><p id=after1>x</p></section>\
             <section id=s2><h2 id=h2>b</h2><p id=two>Two</p><p id=after2>y</p></section>\
             <aside id=stable><span id=stable_child>independent</span></aside>\
             <table id=hinted background=img/a.png><tr><td>t</td></tr></table>",
        );
        dom.set_doc_url(url::Url::parse("https://example.test/page/").ok());
        // HTML rendering, Tables: the background hint resolves against the
        // node document's base URL.
        let background = |dom: &Dom| {
            dom.computed_value(dom.get_by_id("hinted").unwrap(), "background-image")
                .unwrap_or_default()
        };
        assert!(background(&dom).contains("example.test/page/img/a.png"));
        assert_matches_full_scan(&dom);
        let stable = cached(&dom, "stable_child");
        let (values, sheets) = (dom.style_value_epoch, dom.style_epoch);
        let check = |dom: &Dom, stable: &std::rc::Rc<Vec<u32>>| {
            assert_eq!(dom.style_value_epoch, values, "expired every style");
            assert_eq!(dom.style_epoch, sheets, "reparsed the style sheets");
            assert!(std::rc::Rc::ptr_eq(stable, &cached(dom, "stable_child")));
            assert_matches_full_scan(dom);
        };

        // Fragment navigation: a fragment-only URL change resolves every
        // relative URL as before, and only the targets change state.
        dom.set_doc_url(url::Url::parse("https://example.test/page/#one").ok());
        dom.set_fragment_target(Some("one"));
        assert_eq!(value_of(&dom, "one", "width").as_deref(), Some("21px"));
        assert_eq!(value_of(&dom, "after1", "color").as_deref(), Some("red"));
        assert_eq!(value_of(&dom, "h1", "color").as_deref(), Some("green"));
        assert_ne!(
            value_of(&dom, "one", "padding-left").as_deref(),
            Some("2px")
        );
        assert!(background(&dom).contains("example.test/page/img/a.png"));
        check(&dom, &stable);

        dom.set_doc_url(url::Url::parse("https://example.test/page/#two").ok());
        dom.set_fragment_target(Some("two"));
        assert_ne!(value_of(&dom, "one", "width").as_deref(), Some("21px"));
        assert_ne!(value_of(&dom, "after1", "color").as_deref(), Some("red"));
        assert_ne!(value_of(&dom, "h1", "color").as_deref(), Some("green"));
        assert_eq!(
            value_of(&dom, "one", "padding-left").as_deref(),
            Some("2px")
        );
        assert_eq!(value_of(&dom, "two", "width").as_deref(), Some("21px"));
        assert_eq!(value_of(&dom, "after2", "color").as_deref(), Some("red"));
        assert_eq!(value_of(&dom, "h2", "color").as_deref(), Some("green"));
        check(&dom, &stable);

        dom.set_fragment_target(None);
        assert_ne!(value_of(&dom, "two", "width").as_deref(), Some("21px"));
        assert_ne!(value_of(&dom, "h2", "color").as_deref(), Some("green"));
        check(&dom, &stable);

        // Another resource changes every relative URL's base.
        dom.set_doc_url(url::Url::parse("https://example.test/other/#two").ok());
        assert_ne!(dom.style_epoch, sheets);
        assert!(background(&dom).contains("example.test/other/img/a.png"));
        assert_matches_full_scan(&dom);
        assert_style_values_match_cold(&mut dom);
    }

    #[test]
    fn focus_and_target_after_a_sheet_change_use_the_new_rules() {
        let mut dom = Dom::parse_document(
            "<style>.wrap:focus-within .hint{color:red}</style>\
             <div class=wrap id=wrap><input id=a><p class=hint id=hint>h</p></div>\
             <p id=goal>goal</p>",
        );
        assert_ne!(value_of(&dom, "hint", "color").as_deref(), Some("red"));
        // The rule index is stale until the next style query: focus builds
        // a current one for its proof, :target takes the complete fallback.
        let style = dom.create_element("style");
        dom.set_text(
            style,
            "#hint{width:5px}.wrap:focus-within{height:7px}:target{width:8px}",
        );
        dom.append(dom.get_by_id("wrap").unwrap(), style);
        focus(&mut dom, Some("a"));
        dom.set_fragment_target(Some("goal"));
        assert_eq!(value_of(&dom, "hint", "color").as_deref(), Some("red"));
        assert_eq!(value_of(&dom, "hint", "width").as_deref(), Some("5px"));
        assert_eq!(value_of(&dom, "wrap", "height").as_deref(), Some("7px"));
        assert_eq!(value_of(&dom, "goal", "width").as_deref(), Some("8px"));
        assert_matches_full_scan(&dom);
        assert_style_values_match_cold(&mut dom);
    }

    #[test]
    fn focus_in_a_shadow_including_tree_keeps_the_complete_fallback() {
        let mut dom = Dom::parse_document(
            "<style>.wrap:focus-within .hint{color:red}</style>\
             <div class=wrap><input id=a><p class=hint id=hint>h</p></div><x-host id=host></x-host>",
        );
        let host = dom.get_by_id("host").unwrap();
        let shadow = dom.attach_shadow(host);
        let inner = dom.create_element("button");
        dom.append(shadow, inner);
        assert_ne!(value_of(&dom, "hint", "color").as_deref(), Some("red"));
        let epoch = dom.style_value_epoch;
        focus(&mut dom, Some("a"));
        assert_ne!(dom.style_value_epoch, epoch);
        assert_eq!(value_of(&dom, "hint", "color").as_deref(), Some("red"));
        dom.set_focused_area(DOCUMENT, Some(inner));
        assert_ne!(value_of(&dom, "hint", "color").as_deref(), Some("red"));
        assert_matches_full_scan(&dom);
        assert_style_values_match_cold(&mut dom);
    }

    #[test]
    fn focus_states_inside_has_keep_unrelated_child_list_changes_local() {
        // YouTube's `ytd-app[masthead-hidden]:not(:has(#masthead-container:focus-within))`.
        let mut dom = Dom::parse_document(
            "<style>.app[hidden-top]:not(:has(#top:focus-within)) .content{color:red}\
             .card:has(a:focus) .title{width:20px}</style>\
             <main class=app hidden-top><div id=top><a id=link href=#>top</a></div>\
             <p class=content id=content>content</p></main>\
             <section id=list><div class=card><a id=card_link href=#>a</a><b class=title id=title>t</b></div></section>\
             <aside id=stable><span id=stable_child>independent</span></aside>",
        );
        let red = |dom: &Dom| {
            dom.computed_value_resolved(dom.get_by_id("content").unwrap(), "color")
                .as_deref()
                == Some("red")
        };
        assert!(red(&dom));
        let before = cached(&dom, "stable_child");
        let epoch = dom.style_value_epoch;
        let list = dom.get_by_id("list").unwrap();
        let row = dom.create_element("div");
        dom.set_text(row, "row");
        dom.append(list, row);
        dom.detach(row);
        assert_eq!(
            dom.style_value_epoch, epoch,
            "a child-list change expired the document"
        );
        assert!(std::rc::Rc::ptr_eq(&before, &cached(&dom, "stable_child")));
        assert_matches_full_scan(&dom);
        // Focus changes still restyle the anchors.
        dom.set_focused_area(DOCUMENT, dom.get_by_id("link"));
        assert!(!red(&dom));
        dom.set_focused_area(DOCUMENT, dom.get_by_id("card_link"));
        assert!(red(&dom));
        assert_eq!(
            dom.computed_value_resolved(dom.get_by_id("title").unwrap(), "width")
                .as_deref(),
            Some("20px")
        );
        assert_matches_full_scan(&dom);
        assert_style_values_match_cold(&mut dom);
    }

    #[test]
    fn indeterminate_rules_keep_radio_free_child_list_changes_local() {
        // Twitch's checkbox styles: `:indeterminate + label`.
        let mut dom = Dom::parse_document(
            "<style>input:indeterminate + label{color:red}</style>\
             <form id=form><input id=a type=radio name=g><label id=la>a</label>\
             <input id=b type=radio name=g><label id=lb>b</label></form>\
             <section id=list></section>\
             <aside id=stable><span id=stable_child>independent</span></aside>",
        );
        let red = |dom: &Dom, id: &str| {
            dom.computed_value_resolved(dom.get_by_id(id).unwrap(), "color")
                .as_deref()
                == Some("red")
        };
        assert!(red(&dom, "la") && red(&dom, "lb"));
        let before = cached(&dom, "stable_child");
        let epoch = dom.style_value_epoch;
        let list = dom.get_by_id("list").unwrap();
        let row = dom.create_element("div");
        dom.set_text(row, "row");
        dom.append(list, row);
        dom.detach(row);
        assert_eq!(
            dom.style_value_epoch, epoch,
            "a radio-free change expired the document"
        );
        assert!(std::rc::Rc::ptr_eq(&before, &cached(&dom, "stable_child")));
        assert_matches_full_scan(&dom);
        // HTML #selector-indeterminate: a checked radio joining the group
        // clears every member's state, and leaving restores it.
        let checked = dom.create_element("input");
        dom.set_attr(checked, "type", "radio");
        dom.set_attr(checked, "name", "g");
        dom.set_attr(checked, "checked", "");
        dom.append(dom.get_by_id("form").unwrap(), checked);
        assert!(!red(&dom, "la") && !red(&dom, "lb"));
        dom.detach(checked);
        assert!(red(&dom, "la") && red(&dom, "lb"));
        assert_matches_full_scan(&dom);
        assert_style_values_match_cold(&mut dom);
    }

    #[test]
    fn has_anchors_restyle_only_their_parent_subtree() {
        // YouTube: `.ytwChannelBlocksViewModelListWrapper :has(>.yt-icon-shape)`
        // has no constraints of its own, so every element is a possible anchor.
        let mut dom = Dom::parse_document(
            "<style>.wrap :has(> .icon){width:9px} .wrap :has(> .icon) + .after{color:red}\
             .card:has(.badge) .title{height:5px}</style>\
             <div class=wrap><section id=box><p id=para>x</p><b class=after id=after>a</b></section></div>\
             <div class=card id=card><div id=inner><b class=title id=title>t</b></div></div>\
             <aside id=stable><span id=stable_child>independent</span></aside>",
        );
        let value = |dom: &Dom, id: &str, property: &str| {
            dom.computed_value_resolved(dom.get_by_id(id).unwrap(), property)
        };
        let before = cached(&dom, "stable_child");
        let epoch = dom.style_value_epoch;
        let para = dom.get_by_id("para").unwrap();
        let icon = dom.create_element("i");
        dom.set_attr(icon, "class", "icon");
        dom.append(para, icon);
        assert_eq!(value(&dom, "para", "width").as_deref(), Some("9px"));
        assert_eq!(value(&dom, "after", "color").as_deref(), Some("red"));
        let inner = dom.get_by_id("inner").unwrap();
        let badge = dom.create_element("span");
        dom.set_attr(badge, "class", "badge");
        dom.append(inner, badge);
        assert_eq!(value(&dom, "title", "height").as_deref(), Some("5px"));
        assert_eq!(
            dom.style_value_epoch, epoch,
            "a :has() anchor expired the document"
        );
        assert!(std::rc::Rc::ptr_eq(&before, &cached(&dom, "stable_child")));
        assert_matches_full_scan(&dom);
        dom.detach(icon);
        dom.detach(badge);
        assert_ne!(value(&dom, "para", "width").as_deref(), Some("9px"));
        assert_ne!(value(&dom, "after", "color").as_deref(), Some("red"));
        assert_ne!(value(&dom, "title", "height").as_deref(), Some("5px"));
        assert_eq!(dom.style_value_epoch, epoch);
        assert_matches_full_scan(&dom);
        assert_style_values_match_cold(&mut dom);
    }

    #[test]
    fn direction_positional_and_sibling_has_arguments_stay_local() {
        // Wikipedia: `.mw-cite-dir-auto:has(.reference-text:dir(ltr))` and
        // `a:has(+ .mw-editsection-divider + a.mw-editsection-visualeditor:last-of-type)`.
        let mut dom = Dom::parse_document(
            "<style>.cite:has(.ref:dir(rtl)) .num{color:red}\
             a:has(+ .divider + a.ve:last-of-type){width:7px}\
             .menu:has(input:checked) .label{height:3px}\
             ul.list:has(> li.hot:first-child){color:blue}\
             .chat:has(.bar:not(.dismissed, .loading)) .note{color:green}\
             .toggle:has(.switch:disabled) .knob{width:4px}</style>\
             <div class=cite id=cite><span class=num id=num>1</span><span class=ref id=ref>r</span></div>\
             <p id=edit><a id=edit_link href=#>edit</a><span class=divider>|</span><a class=ve id=ve href=#>ve</a></p>\
             <div class=menu><input type=checkbox id=box><b class=label id=label>l</b></div>\
             <ul class=list id=list><li id=first>a</li></ul>\
             <div class=chat id=chat><b class=note id=note>n</b></div>\
             <div class=toggle><fieldset disabled id=fieldset></fieldset><input class=switch id=switch><b class=knob id=knob>k</b></div>\
             <section id=elsewhere></section>\
             <aside id=stable><span id=stable_child>independent</span></aside>",
        );
        let value = |dom: &Dom, id: &str, property: &str| {
            dom.computed_value_resolved(dom.get_by_id(id).unwrap(), property)
        };
        assert_eq!(value(&dom, "edit_link", "width").as_deref(), Some("7px"));
        let before = cached(&dom, "stable_child");
        let epoch = dom.style_value_epoch;
        let elsewhere = dom.get_by_id("elsewhere").unwrap();
        let row = dom.create_element("div");
        dom.set_text(row, "row");
        dom.append(elsewhere, row);
        dom.detach(row);
        assert!(std::rc::Rc::ptr_eq(&before, &cached(&dom, "stable_child")));
        // A sibling-relative argument restyles its anchors' parent subtree.
        let ve = dom.get_by_id("ve").unwrap();
        let edit = dom.get_by_id("edit").unwrap();
        dom.detach(ve);
        assert_ne!(value(&dom, "edit_link", "width").as_deref(), Some("7px"));
        dom.append(edit, ve);
        assert_eq!(value(&dom, "edit_link", "width").as_deref(), Some("7px"));
        // :dir() inside :has() follows a moved descendant's new ancestry.
        let cite = dom.get_by_id("cite").unwrap();
        let rtl = dom.create_element("span");
        dom.set_attr(rtl, "dir", "rtl");
        dom.append(cite, rtl);
        dom.append(rtl, dom.get_by_id("ref").unwrap());
        assert_eq!(value(&dom, "num", "color").as_deref(), Some("red"));
        // Positional arguments under a `>` step.
        let list = dom.get_by_id("list").unwrap();
        let hot = dom.create_element("li");
        dom.set_attr(hot, "class", "hot");
        dom.insert_before(list, hot, dom.get_by_id("first"));
        assert_eq!(value(&dom, "list", "color").as_deref(), Some("blue"));
        let cold = dom.create_element("li");
        dom.insert_before(list, cold, Some(hot));
        assert_ne!(value(&dom, "list", "color").as_deref(), Some("blue"));
        // :checked inside :has() follows the attribute path.
        dom.set_attr(dom.get_by_id("box").unwrap(), "checked", "");
        assert_eq!(value(&dom, "label", "height").as_deref(), Some("3px"));
        // Discourse: `:has(.chat-pinned-bar:not(.--dismissed,.--loading))`.
        let bar = dom.create_element("i");
        dom.set_attr(bar, "class", "bar");
        dom.append(dom.get_by_id("chat").unwrap(), bar);
        assert_eq!(value(&dom, "note", "color").as_deref(), Some("green"));
        dom.set_attr(bar, "class", "bar dismissed");
        assert_ne!(value(&dom, "note", "color").as_deref(), Some("green"));
        // Discourse: `.d-toggle-switch:has(.composer-event__livestream-switch:disabled)`;
        // moving the control into a disabled fieldset disables it.
        assert_ne!(value(&dom, "knob", "width").as_deref(), Some("4px"));
        dom.append(
            dom.get_by_id("fieldset").unwrap(),
            dom.get_by_id("switch").unwrap(),
        );
        assert_eq!(value(&dom, "knob", "width").as_deref(), Some("4px"));
        assert_eq!(
            dom.style_value_epoch, epoch,
            "a :has() argument expired the document"
        );
        assert!(std::rc::Rc::ptr_eq(&before, &cached(&dom, "stable_child")));
        assert_matches_full_scan(&dom);
        assert_style_values_match_cold(&mut dom);
    }

    #[test]
    fn local_state_selectors_keep_unrelated_child_list_changes_local() {
        // Discourse: `:lang(zh_CN)` (a Latin language here: CJK text would load
        // a process-wide fallback font under parallel layout-cache tests),
        // `#input:not(:placeholder-shown)+.submit`,
        // `:focus:required`, `.node:has(.socket:hover) .icon`.
        let mut dom = Dom::parse_document(
            "<style>.activity:lang(fr){color:red} #input:not(:placeholder-shown)+.submit{width:20px}\
             input:required{height:7px} textarea:placeholder-shown{color:blue}\
             .node:has(.socket:hover) .icon{width:9px} [contenteditable]:read-write{color:green}</style>\
             <div lang=fr><span class=activity id=activity>x</span></div>\
             <input id=input placeholder=p required><b class=submit id=submit>s</b>\
             <textarea id=area placeholder=hint></textarea>\
             <div class=node><i class=socket id=socket>o</i><b class=icon id=icon>i</b></div>\
             <div contenteditable id=editor><span id=editable>e</span></div>\
             <section id=list></section>\
             <aside id=stable><span id=stable_child>independent</span></aside>",
        );
        let value = |dom: &Dom, id: &str, property: &str| {
            dom.computed_value_resolved(dom.get_by_id(id).unwrap(), property)
        };
        assert_eq!(value(&dom, "activity", "color").as_deref(), Some("red"));
        assert_eq!(value(&dom, "area", "color").as_deref(), Some("blue"));
        let before = cached(&dom, "stable_child");
        let epoch = dom.style_value_epoch;
        let list = dom.get_by_id("list").unwrap();
        let row = dom.create_element("div");
        dom.set_text(row, "row");
        dom.append(list, row);
        dom.detach(row);
        assert_eq!(
            dom.style_value_epoch, epoch,
            "a child-list change expired the document"
        );
        assert!(std::rc::Rc::ptr_eq(&before, &cached(&dom, "stable_child")));
        assert_matches_full_scan(&dom);
        // A textarea's own text children decide its :placeholder-shown.
        let area = dom.get_by_id("area").unwrap();
        let text = dom.create_text("typed");
        dom.append(area, text);
        assert_ne!(value(&dom, "area", "color").as_deref(), Some("blue"));
        dom.detach(text);
        assert_eq!(value(&dom, "area", "color").as_deref(), Some("blue"));
        // Moved content takes its new ancestors' language and editability.
        let moved = dom.get_by_id("activity").unwrap();
        dom.append(dom.get_by_id("editor").unwrap(), moved);
        assert_ne!(value(&dom, "activity", "color").as_deref(), Some("red"));
        assert_eq!(value(&dom, "editable", "color").as_deref(), Some("green"));
        // Hover transitions still restyle :has() anchors.
        dom.set_hover_chain(dom.get_by_id("socket"));
        assert_eq!(value(&dom, "icon", "width").as_deref(), Some("9px"));
        assert_matches_full_scan(&dom);
        assert_style_values_match_cold(&mut dom);
    }

    #[test]
    fn relational_anchor_invalidation_covers_siblings_moves_and_complex_fallbacks() {
        for selector in [
            ".anchor:has(> .hit) .label",
            ".anchor:has(.wrapper > [data-hit]) .label",
            ".anchor:has(+ .hit) .label",
            ".anchor:has(~ .wrapper .hit) .label",
            ":not(.anchor:has(.hit)) > .label",
            ":is(.anchor:has(.hit)) + .tail .label",
            ".anchor:has(.hit:first-child) .label",
            ".anchor:has(:is(.theme .hit)) .label",
            ".anchor:has(.hit) .label::before",
        ] {
            let mut dom = Dom::parse_document(&format!(
                "<style>{selector}{{color:red;width:70px;content:'yes'}}</style>\
                 <main id=root><section id=anchor class=anchor><i class=label>label</i>\
                 <div id=inside class=wrapper></div></section>\
                 <div id=after class='wrapper tail'><b class=label>after</b></div></main>\
                 <aside id=detached></aside>"
            ));
            let root = dom.get_by_id("root").unwrap();
            let anchor = dom.get_by_id("anchor").unwrap();
            let inside = dom.get_by_id("inside").unwrap();
            let after = dom.get_by_id("after").unwrap();
            let hit = dom.create_element("b");
            dom.set_attr(hit, "class", "hit");
            dom.set_attr(hit, "data-hit", "yes");
            assert_matches_full_scan(&dom);
            for parent in [anchor, inside, after, root] {
                dom.append(parent, hit);
                assert_matches_full_scan(&dom);
                dom.set_attr(hit, "class", "miss");
                assert_matches_full_scan(&dom);
                dom.set_attr(hit, "class", "hit");
                assert_matches_full_scan(&dom);
                dom.remove_attr(hit, "data-hit");
                assert_matches_full_scan(&dom);
                dom.set_attr(hit, "data-hit", "yes");
                assert_matches_full_scan(&dom);
                dom.detach(hit);
                assert_matches_full_scan(&dom);
            }
            dom.insert_before(root, hit, Some(after));
            assert_matches_full_scan(&dom);
            dom.append(after, hit);
            dom.set_attr(root, "class", "theme");
            assert_matches_full_scan(&dom);
            dom.remove_attr(root, "class");
            assert_matches_full_scan(&dom);
            dom.replace_all_children(after, Vec::new());
            assert_matches_full_scan(&dom);
            assert_style_values_match_cold(&mut dom);
        }
    }

    #[test]
    fn control_values_and_directional_insertions_preserve_unrelated_styles() {
        let mut dom = Dom::parse_document(
            "<style>:dir(rtl){color:red} :dir(ltr){color:green}</style>\
             <main dir=ltr><form id=form><input id=input></form><aside id=stable>kept</aside></main>\
             <section id=auto dir=auto><span id=dependent></span></section>",
        );
        let stable = cached(&dom, "stable");
        let input = dom.get_by_id("input").unwrap();
        dom.set_input_value(input, "typed", true);
        assert!(std::rc::Rc::ptr_eq(&stable, &cached(&dom, "stable")));
        let probe = dom.create_element("span");
        dom.set_text(probe, "measurement");
        dom.append(dom.get_by_id("form").unwrap(), probe);
        assert!(std::rc::Rc::ptr_eq(&stable, &cached(&dom, "stable")));
        assert_style_values_match_cold(&mut dom);
        let dependent = dom.get_by_id("dependent").unwrap();
        let before = dom.matched_rules(dependent);
        dom.set_text(probe, "אבג");
        dom.append(dom.get_by_id("auto").unwrap(), probe);
        assert!(
            !std::rc::Rc::ptr_eq(&before, &dom.matched_rules(dependent)),
            "automatic directionality must retain full dependency invalidation"
        );
        assert_style_values_match_cold(&mut dom);
        dom.detach(probe);
        assert_style_values_match_cold(&mut dom);
    }

    #[test]
    fn typing_preserves_styles_until_direction_or_emptiness_changes() {
        // HTML #text-node-directionality and Selectors 4 :dir()/:empty:
        // changing an existing Text node leaves every element selector
        // unchanged when its first strong direction and emptiness agree.
        let mut dom = Dom::parse_document(
            "<style>:dir(rtl){color:red} :dir(ltr){color:green} \
             body:has(#editor:empty) #dependent{color:blue} \
             p:nth-child(2 of :not(:empty)){padding:2px}</style> \
             <div id=editor dir=auto>a</div><aside id=dependent>stable</aside>",
        );
        let node = dom.children(dom.get_by_id("editor").unwrap())[0];
        for (text, unchanged) in [
            ("ab", true),
            ("🙂 abc אבג", true),
            ("123 אבג", false),
            ("مرحبا xyz", true),
            ("123", false),
            ("456", true),
            ("", false),
        ] {
            let before = cached(&dom, "dependent");
            dom.set_text(node, text);
            assert_eq!(
                std::rc::Rc::ptr_eq(&before, &cached(&dom, "dependent")),
                unchanged,
                "{text:?}"
            );
            assert_matches_full_scan(&dom);
            assert_style_values_match_cold(&mut dom);
        }
        assert_eq!(text_node_directionality("\u{202a}123🙂"), None);
        assert_eq!(text_node_directionality("\u{202a}123אa"), Some(true));
        assert_eq!(text_node_directionality("123aא"), Some(false));
    }

    #[test]
    fn input_value_state_invalidates_placeholder_and_directional_subjects() {
        let mut dom = Dom::parse_document(
            "<style>input:placeholder-shown + aside{color:red}\
             form:has(input:placeholder-shown){width:100px}\
             input:dir(rtl) + aside{font-size:30px}</style>\
             <form><input id=input placeholder=hint dir=auto><aside id=other>other</aside></form>",
        );
        let input = dom.get_by_id("input").unwrap();
        let other = dom.get_by_id("other").unwrap();
        assert_style_values_match_cold(&mut dom);
        for value in ["typed", "אבג", ""] {
            dom.set_input_value(input, value, true);
            assert_eq!(
                dom.computed_value_resolved(other, "color").as_deref() == Some("red"),
                value.is_empty()
            );
            assert_style_values_match_cold(&mut dom);
            assert_matches_full_scan(&dom);
        }
    }

    #[test]
    fn selector_invalidation_preserves_independent_matches_but_updates_cascade() {
        let mut dom = Dom::parse_document(
            r#"<style>
            #a { color:red; width:50px } #b { color:green }
            [data-state] { color:blue }
        </style><div id="a"></div><div id="b"></div>"#,
        );
        let a = dom.get_by_id("a").unwrap();
        let a_before = cached(&dom, "a");
        let b_before = cached(&dom, "b");
        dom.set_attr(a, "style", "color:purple;width:123px");
        assert_eq!(dom.computed_style(a, "width").as_deref(), Some("123px"));
        assert_eq!(dom.computed_style(a, "color").as_deref(), Some("purple"));
        assert!(std::rc::Rc::ptr_eq(&a_before, &cached(&dom, "a")));
        assert!(std::rc::Rc::ptr_eq(&b_before, &cached(&dom, "b")));
        dom.set_attr(a, "data-state", "on");
        assert!(
            !std::rc::Rc::ptr_eq(&a_before, &cached(&dom, "a")),
            "attribute selector must be reevaluated"
        );
        assert!(
            std::rc::Rc::ptr_eq(&b_before, &cached(&dom, "b")),
            "local dependency must not discard unrelated matches"
        );
        assert_matches_full_scan(&dom);
        dom.remove_attr(a, "data-state");
        assert_matches_full_scan(&dom);
    }

    #[test]
    fn relational_matching_does_not_discard_candidates_after_a_work_limit() {
        let dom = Dom::parse_document(&format!(
            "<main id=anchor><b class=hit></b>{}</main><aside id=after>following</aside>",
            "<i></i>".repeat(10_000)
        ));
        for selector in [
            "#anchor:has(> .hit)",
            "#anchor:has(.hit)",
            "#anchor:not(:has(.missing))",
        ] {
            let parsed = SelectorList::parse(selector).unwrap();
            assert_eq!(
                dom.query(DOCUMENT, &parsed, false),
                vec![dom.get_by_id("anchor").unwrap()]
            );
        }
        let parsed = SelectorList::parse("#anchor:has(+ #after)").unwrap();
        assert_eq!(
            dom.query(DOCUMENT, &parsed, false),
            vec![dom.get_by_id("anchor").unwrap()]
        );
    }

    #[test]
    fn attribute_dependency_paths_match_uncached_selectors_in_both_directions() {
        // Selectors 4 #matches, #negation, #relational and #the-nth-child-pseudo.
        // Exercise both acquiring and losing a match, including dependencies
        // outside a :has() argument's forward relative subtree.
        let selectors = [
            ".flag > section > span",
            ".flag + section span",
            ".flag ~ section > span",
            "section:is(.flag > section) > span",
            "section:not(.flag + section) span",
            "section:where(:not(.flag section)) > span",
            "section:has(> span.flag) + section span",
            "section:has(+ section > span.flag) > span",
            "section:has(~ section .flag) span",
            "main:has(section:is(.flag > section)) > section > span",
            "main:has(section:not(.flag + section)) > section > span",
            "section:not(:has(> .flag)) + section > span",
            "section:nth-child(2 of .flag) > span",
            "section:nth-last-child(2 of :not(.flag)) > span",
            "section:nth-child(2 of .flag > section) + section span",
            "section:nth-child(2 of :has(> span.flag)) span",
            "main:has(> section:nth-child(2 of .flag)) > section > span",
            "section:has(> span:nth-child(2 of .flag)) ~ section span",
            "section:is(:has(.flag), :not(.flag + section)) > span",
            "section:is(.flag ~ section, main:not(.flag) > section) span",
            "[class] + section > span",
            "[class~=flag] ~ section span",
        ];
        for selector in selectors {
            let mut dom = Dom::parse_document(&format!(
                "<!doctype html><style>{selector}{{color:red;width:37px;--test:yes}}</style>\
                 <main id=m><section id=a><span id=a1>one</span><span id=a2>two</span></section>\
                 <section id=b><span id=b1>three</span><span id=b2>four</span></section>\
                 <section id=c><span id=c1>five</span></section></main><aside id=stable>stable</aside>"
            ));
            for target in ["m", "a", "a1", "a2", "b", "b1", "b2", "c", "c1"] {
                let node = dom.get_by_id(target).unwrap();
                for value in [Some("flag"), Some("other flag"), Some("other"), None] {
                    assert_matches_full_scan(&dom);
                    assert_style_values_match_cold(&mut dom);
                    if let Some(value) = value {
                        dom.set_attr(node, "class", value);
                    } else {
                        dom.remove_attr(node, "class");
                    }
                    assert_matches_full_scan(&dom);
                    assert_style_values_match_cold(&mut dom);
                }
            }
        }
    }

    #[test]
    fn relational_and_checked_attribute_paths_retain_independent_styles() {
        let mut dom = Dom::parse_document(
            "<style>input:checked + label{color:red} section:has(> input:checked) .badge{width:20px}\
             section:nth-child(2 of :has(input:checked)) .badge{height:17px}</style>\
             <main><section><input id=check type=checkbox><label id=label>label</label><b class=badge>badge</b></section>\
             <section><input type=checkbox checked><b class=badge>badge</b></section></main>\
             <aside id=stable><span id=stable_child>independent</span></aside>",
        );
        let check = dom.get_by_id("check").unwrap();
        let stable = dom.get_by_id("stable_child").unwrap();
        for checked in [true, false, true, false] {
            assert_matches_full_scan(&dom);
            let matches = cached(&dom, "stable_child");
            let cascade = dom.cascaded_maps(stable);
            let epoch = dom.style_value_epoch;
            if checked {
                dom.set_attr(check, "checked", "");
            } else {
                dom.remove_attr(check, "checked");
            }
            assert_eq!(
                dom.style_value_epoch, epoch,
                "local state expired the document"
            );
            assert!(std::rc::Rc::ptr_eq(&matches, &cached(&dom, "stable_child")));
            assert!(std::rc::Rc::ptr_eq(&cascade, &dom.cascaded_maps(stable)));
            assert_eq!(
                dom.computed_value_resolved(dom.get_by_id("label").unwrap(), "color")
                    .as_deref()
                    == Some("red"),
                checked
            );
            assert_matches_full_scan(&dom);
            assert_style_values_match_cold(&mut dom);
        }
    }

    #[test]
    fn inherited_state_paths_include_descendants_but_not_unrelated_branches() {
        for (attribute, value, selector) in [
            ("disabled", "", "input:disabled + span"),
            ("contenteditable", "true", "div:read-write + span"),
            ("lang", "fr", "div:lang(fr) + span"),
        ] {
            let mut dom = Dom::parse_document(&format!(
                "<style>{selector}{{color:red}}</style><fieldset id=parent><legend><input><span>legend</span></legend>\
                 <input><span>control</span><div>editable/language</div><span>following</span></fieldset>\
                 <aside id=stable>unrelated</aside>"
            ));
            let parent = dom.get_by_id("parent").unwrap();
            let stable = dom.get_by_id("stable").unwrap();
            for set in [true, false] {
                assert_matches_full_scan(&dom);
                let before = dom.cascaded_maps(stable);
                if set {
                    dom.set_attr(parent, attribute, value);
                } else {
                    dom.remove_attr(parent, attribute);
                }
                assert!(std::rc::Rc::ptr_eq(&before, &dom.cascaded_maps(stable)));
                assert_matches_full_scan(&dom);
                assert_style_values_match_cold(&mut dom);
            }
        }
    }

    #[test]
    fn selector_invalidation_follows_descendants_siblings_and_logical_arguments() {
        let mut dom = Dom::parse_document(
            r#"<style>
            [data-desc] .leaf { color:red }
            [data-next] + .after .leaf { width:111px }
            [data-later] ~ .after { height:13px }
            :is([data-inner] .nested, [data-direct]) + .tail .leaf { color:blue }
            :where([data-where] > .nested) .leaf { color:green }
            .after:not([data-not] .after) .leaf { opacity:0.5 }
        </style><main id="root"><section id="before"><div class="nested" id="nested"><span class="leaf" id="inside">in</span></div>
            <div class="tail"><span class="leaf" id="tail">tail</span></div></section>
            <section class="after" id="after"><span class="leaf" id="outside">out</span></section></main>
            <aside id="unrelated">unrelated</aside>"#,
        );
        assert_matches_full_scan(&dom);
        for target in ["before", "root", "nested", "after", "inside"] {
            for attribute in [
                "data-desc",
                "data-next",
                "data-later",
                "data-inner",
                "data-direct",
                "data-where",
                "data-not",
            ] {
                let node = dom.get_by_id(target).unwrap();
                dom.set_attr(node, attribute, "yes");
                assert_matches_full_scan(&dom);
                dom.remove_attr(node, attribute);
                assert_matches_full_scan(&dom);
            }
        }
        // A descendant dependency on `before` cannot reach the independent
        // aside. (The test above also exercises broader root-sibling cases.)
        let unrelated = cached(&dom, "unrelated");
        let before = dom.get_by_id("before").unwrap();
        dom.set_attr(before, "data-desc", "yes");
        assert!(std::rc::Rc::ptr_eq(&unrelated, &cached(&dom, "unrelated")));
    }

    #[test]
    fn state_attributes_preserve_unrelated_matches_without_losing_dependencies() {
        let mut dom = Dom::parse_document(
            r#"<html dir=ltr><style>
            :dir(rtl) { color:red }
            input:checked + span { width:20px }
            a:any-link + span { height:30px }
            [value=active] + span { color:green }
            </style><section><input id=query><span id=next>next</span>
            <input id=automatic dir=auto><span>automatic</span>
            <input id=check type=checkbox><span>check</span>
            <a id=link></a><span>link</span></section><aside id=independent>independent</aside>
            <section dir=rtl><input id=telephone></section>"#,
        );
        let independent = cached(&dom, "independent");
        let query = dom.get_by_id("query").unwrap();
        dom.set_attr(query, "value", "active");
        assert!(std::rc::Rc::ptr_eq(
            &independent,
            &cached(&dom, "independent")
        ));
        assert_eq!(
            dom.computed_style(dom.get_by_id("next").unwrap(), "color")
                .as_deref(),
            Some("green")
        );
        dom.set_attr(dom.get_by_id("link").unwrap(), "href", "/target");
        assert!(std::rc::Rc::ptr_eq(
            &independent,
            &cached(&dom, "independent")
        ));
        let script = dom.create_element("script");
        dom.set_attr(script, "type", "text/javascript");
        assert!(std::rc::Rc::ptr_eq(
            &independent,
            &cached(&dom, "independent")
        ));
        assert_matches_full_scan(&dom);
        assert_style_values_match_cold(&mut dom);
        for (id, attribute, value) in [
            ("automatic", "value", "שלום"),
            ("automatic", "type", "tel"),
            ("telephone", "type", "tel"),
            ("telephone", "type", "text"),
            ("check", "checked", ""),
            ("check", "type", "text"),
            ("link", "href", ""),
        ] {
            dom.set_attr(dom.get_by_id(id).unwrap(), attribute, value);
            assert_matches_full_scan(&dom);
            assert_style_values_match_cold(&mut dom);
        }
    }

    #[test]
    fn checked_selectors_preserve_styles_during_unrelated_child_mutations() {
        let mut dom = Dom::parse_document(
            r#"<style>
            input:checked + .label { color:red }
            option:checked { color:green }
            #destination input:checked ~ .label { width:15px }
            </style><form id=search><input id=query></form>
            <section id=controls><input id=check type=checkbox checked><b class=label id=label>label</b>
            <select><optgroup id=group><option id=option selected>one</option></optgroup></select></section>
            <section id=destination></section><aside id=independent>independent</aside>"#,
        );
        let independent = cached(&dom, "independent");
        let label = cached(&dom, "label");
        let span = dom.create_element("span");
        dom.set_text(span, "measured text");
        dom.append(dom.get_by_id("search").unwrap(), span);
        dom.detach(span);
        assert!(std::rc::Rc::ptr_eq(
            &independent,
            &cached(&dom, "independent")
        ));
        assert!(std::rc::Rc::ptr_eq(&label, &cached(&dom, "label")));
        assert_matches_full_scan(&dom);
        assert_style_values_match_cold(&mut dom);
        for (id, attr) in [("check", "checked"), ("option", "selected")] {
            let node = dom.get_by_id(id).unwrap();
            dom.remove_attr(node, attr);
            assert_matches_full_scan(&dom);
            assert_style_values_match_cold(&mut dom);
            dom.set_attr(node, attr, "");
            assert_matches_full_scan(&dom);
            assert_style_values_match_cold(&mut dom);
        }
        dom.append(
            dom.get_by_id("destination").unwrap(),
            dom.get_by_id("controls").unwrap(),
        );
        assert_matches_full_scan(&dom);
        assert_style_values_match_cold(&mut dom);
        dom.detach(dom.get_by_id("group").unwrap());
        assert_matches_full_scan(&dom);
        assert_style_values_match_cold(&mut dom);
    }

    #[test]
    fn selector_invalidation_relational_positional_and_state_changes_match_full_scan() {
        let mut dom = Dom::parse_document(
            r#"<style>
            section:has(> [data-selected]) .badge { color:red }
            section:has(+ [data-after]) .badge { color:blue }
            li:nth-child(2 of [data-eligible]) { color:green }
            li:nth-last-child(1 of :is([data-eligible], .active)) ~ li { width:12px }
            input:checked { color:red } input:indeterminate { color:blue }
            input:disabled { width:15px } input:enabled { height:25px }
            :read-write { color:purple } :lang(fr) .badge { height:17px }
            :dir(rtl) { margin-left:3px }
            section:empty { height:9px }
        </style><main id="root"><section id="first"><i class="badge" id="badge"></i><b id="child"></b></section><section id="second"></section>
            <ul><li id="one"></li><li id="two"></li><li id="three"></li></ul>
            <fieldset id="fieldset"><input id="radio1" type="radio" name="group"><input id="radio2" type="radio" name="group"></fieldset></main>"#,
        );
        assert_matches_full_scan(&dom);
        for (target, attribute, value) in [
            ("child", "data-selected", "yes"),
            ("second", "data-after", "yes"),
            ("one", "data-eligible", "yes"),
            ("two", "data-eligible", "yes"),
            ("three", "data-eligible", "yes"),
            ("two", "class", "active"),
            ("radio1", "checked", ""),
            ("fieldset", "disabled", ""),
            ("root", "contenteditable", "true"),
            ("root", "lang", "fr"),
            ("root", "dir", "rtl"),
        ] {
            let node = dom.get_by_id(target).unwrap();
            dom.set_attr(node, attribute, value);
            assert_matches_full_scan(&dom);
        }
        for (target, attribute) in [
            ("child", "data-selected"),
            ("second", "data-after"),
            ("one", "data-eligible"),
            ("radio1", "checked"),
            ("fieldset", "disabled"),
            ("root", "lang"),
            ("root", "dir"),
        ] {
            dom.remove_attr(dom.get_by_id(target).unwrap(), attribute);
            assert_matches_full_scan(&dom);
        }
        dom.set_text(dom.get_by_id("second").unwrap(), "new content");
        assert_matches_full_scan(&dom);
    }

    #[test]
    fn selector_invalidation_does_not_cache_container_query_applicability() {
        let mut dom = Dom::parse_document(
            r#"<style>
            #container { container-type:inline-size; width:300px }
            #child { width:40px }
            @container (width > 200px) { #child { width:160px } }
        </style><div id="container"><div id="child"></div></div>"#,
        );
        let base = url::Url::parse("https://example.com/").unwrap();
        let viewport = crate::layout2::Viewport::new(640., 480.);
        let measure = |dom: &Dom| {
            crate::layout2::measure_boxes_css(
                dom,
                &base,
                viewport,
                &[],
                &Default::default(),
                &Default::default(),
            )
            .0
        };
        let child = dom.get_by_id("child").unwrap();
        assert_eq!(measure(&dom)[&child].width, 160.);
        let selectors = cached(&dom, "child");
        dom.set_attr(dom.get_by_id("container").unwrap(), "style", "width:100px");
        assert_eq!(measure(&dom)[&child].width, 40.);
        assert!(std::rc::Rc::ptr_eq(&selectors, &cached(&dom, "child")));
        assert_matches_full_scan(&dom);
    }

    #[test]
    fn selector_invalidation_style_attribute_selectors_and_inherited_variables_stay_live() {
        let mut dom = Dom::parse_document(
            r#"<style>
            #child { width:var(--size, 40px); color:inherit }
            [style] > #child { height:20px }
            [style*="100"] + #sibling { height:30px }
        </style><div id="parent"><div id="child"></div></div><div id="sibling"></div>"#,
        );
        let child = dom.get_by_id("child").unwrap();
        for style in ["--size:100px;color:red", "--size:200px;color:green", ""] {
            dom.set_attr(dom.get_by_id("parent").unwrap(), "style", style);
            assert_matches_full_scan(&dom);
            let width = dom.computed_value_resolved(child, "width");
            assert_eq!(
                width.as_deref(),
                Some(if style.contains("100") {
                    "100px"
                } else if style.contains("200") {
                    "200px"
                } else {
                    "40px"
                })
            );
        }
        dom.remove_attr(dom.get_by_id("parent").unwrap(), "style");
        assert_matches_full_scan(&dom);
    }

    #[test]
    fn selector_invalidation_shadow_distribution_and_inheritance_match_full_scan() {
        let mut dom = Dom::parse_document(
            r#"<style>
            #item { width:var(--size) }
            #item:lang(fr) { color:red }
            #item:dir(rtl) { height:20px }
        </style><x-host id="host"><span id="item" slot="first"></span></x-host>"#,
        );
        let host = dom.get_by_id("host").unwrap();
        let item = dom.get_by_id("item").unwrap();
        let shadow = dom.attach_shadow(host);
        let mut slots = Vec::new();
        for (name, language, direction, size) in [
            ("first", "fr", "rtl", "120px"),
            ("second", "en", "ltr", "80px"),
        ] {
            let slot = dom.create_element("slot");
            dom.set_attr(slot, "name", name);
            dom.set_attr(slot, "lang", language);
            dom.set_attr(slot, "dir", direction);
            dom.set_attr(slot, "style", &format!("--size:{size}"));
            dom.append(shadow, slot);
            slots.push(slot);
        }
        assert_matches_full_scan(&dom);
        assert_eq!(
            dom.computed_value_resolved(item, "width").as_deref(),
            Some("120px")
        );
        let first = cached(&dom, "item");
        dom.set_attr(item, "slot", "second");
        assert_matches_full_scan(&dom);
        assert!(!std::rc::Rc::ptr_eq(&first, &cached(&dom, "item")));
        assert_eq!(
            dom.computed_value_resolved(item, "width").as_deref(),
            Some("80px")
        );
        dom.set_attr(slots[0], "name", "second");
        assert_matches_full_scan(&dom);
        assert_eq!(
            dom.computed_value_resolved(item, "width").as_deref(),
            Some("120px")
        );
        dom.remove_attr(slots[0], "name");
        assert_matches_full_scan(&dom);
        assert_eq!(
            dom.computed_value_resolved(item, "width").as_deref(),
            Some("80px")
        );
    }

    #[test]
    fn selector_invalidation_structure_is_dependent_but_stylesheets_and_viewport_are_global() {
        let mut dom = Dom::parse_document(
            r#"<style id="sheet">
            #root:empty { width:20px }
            .item:first-child { color:red }
            @media (min-width:700px) { .item { width:90px } }
        </style><main id="root"><span class="item" id="item"></span></main>"#,
        );
        dom.set_viewport_px(640., 480.);
        assert_matches_full_scan(&dom);
        let before = cached(&dom, "item");
        dom.set_viewport_px(800., 480.);
        assert_matches_full_scan(&dom);
        assert!(!std::rc::Rc::ptr_eq(&before, &cached(&dom, "item")));
        let before = cached(&dom, "item");
        let preceding = dom.create_element("span");
        dom.insert_before(
            dom.get_by_id("root").unwrap(),
            preceding,
            dom.get_by_id("item"),
        );
        assert_matches_full_scan(&dom);
        assert!(
            !std::rc::Rc::ptr_eq(&before, &cached(&dom, "item")),
            "first-child changed"
        );
        let before = cached(&dom, "item");
        dom.set_text(dom.get_by_id("sheet").unwrap(), ".item { height:80px }");
        assert_matches_full_scan(&dom);
        assert!(!std::rc::Rc::ptr_eq(&before, &cached(&dom, "item")));
        let before = cached(&dom, "item");
        let child = dom.create_element("span");
        dom.set_attr(child, "class", "item");
        dom.append(dom.get_by_id("root").unwrap(), child);
        assert_matches_full_scan(&dom);
        assert!(
            std::rc::Rc::ptr_eq(&before, &cached(&dom, "item")),
            "without structural rules an unrelated new sibling preserves matches"
        );
    }
}
