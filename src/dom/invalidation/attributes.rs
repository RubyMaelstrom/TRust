//! Mutation-to-subject paths, compiled from Selectors 4 §§4, 14 and 15.
//!
//! These are dependency paths, not selectors: a negative test is just as much
//! a dependency as a positive one. Guards omit the changed attribute on the
//! mutated element: testing only its new value would lose former matches.
//! `:has()` reverses the path to its anchor, while `:nth-child(of S)` fans out
//! from changed membership to inclusive siblings before resuming the outer path.
//! The guard a path starts from also requires the tags, ids and classes that
//! its selector's descendant and child combinators demand of ancestors, which
//! no mutation of the element itself changes (`AncestorKey`).
//! See https://drafts.csswg.org/selectors-4/#match-a-selector-against-an-element
//! (local CSSWG snapshot 81c27f68690138345b2b3b6af8ccc42dad3dca1d).
//!
//! Selectors stay within one tree, so these paths follow light-tree axes,
//! except where CSS Shadow 1 (formerly CSS Scoping 1, same snapshot) lets a
//! shadow tree's rules reach a host or its slottables: `:host()` tests the
//! host in its own context while its compound's combinators continue inside
//! the shadow tree (#host-element-in-tree, #host-selector), so it reaches only
//! a host whose shadow tree holds the rule, and `::slotted()` represents the
//! elements assigned, after flattening, to a slot (#slotted-pseudo).
//! Inheritance follows the flat tree (#flattening);
//! `Dom::apply_style_invalidations` restyles flat-tree descendants of every
//! subject, so these paths need only name the subjects themselves.

use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Axis {
    SelfOnly,
    Child,
    Descendant,
    Next,
    Following,
    Parent,
    Ancestor,
    Previous,
    Preceding,
    InclusiveSiblings,
    /// Inherited state: the node and its shadow-including descendants.
    InclusiveDescendants,
    /// A shadow host as `:host()` sees it: the host, every element of its
    /// shadow tree (where the compound's combinators continue), and its
    /// light children, the slottables a `::slotted()` subject can name.
    HostScope,
    /// From a slot to the elements assigned to it after flattening.
    Assigned,
    /// The element itself when it is assigned to a slot: only such an
    /// element can be a `::slotted()` subject.
    Slottable,
    /// From an element whose validity inputs changed to the elements whose
    /// validity pseudo-classes can change (`Dom::validity_subjects`).
    Validity,
}

/// A tag, id or class that some ancestor of an element must carry for a
/// compound to match it: those of the compounds left of a descendant or
/// child combinator (Selectors 4 #descendant-combinators,
/// #child-combinators), each an ancestor of every compound to its right.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum AncestorKey {
    Tag(String),
    Id(String),
    Class(String),
}

impl AncestorKey {
    /// The keys of the compounds left of `selector.0[index]` that must match
    /// its ancestors. Shadow-crossing compounds (`:host`, `::part()`,
    /// `::slotted()`) contribute none.
    fn of(selector: &Complex, index: usize) -> Vec<Self> {
        let mut keys = Vec::new();
        for (left, (following, _)) in selector.0[..index].iter().zip(&selector.0[1..=index]) {
            let compound = &left.1;
            if !matches!(following, Combinator::Child | Combinator::Descendant)
                || compound.host
                || compound.part.is_some()
                || compound.slotted.is_some()
            {
                continue;
            }
            if let Some(tag) = compound.tag.as_ref().filter(|tag| tag.as_str() != "*") {
                keys.push(Self::Tag(tag.clone()));
            }
            if let Some(id) = &compound.id {
                keys.push(Self::Id(id.clone()));
            }
            keys.extend(compound.classes.iter().cloned().map(Self::Class));
        }
        keys
    }

    /// Whether a shadow-including ancestor of `node` carries the key: a
    /// superset of the ancestors selector matching walks in `node`'s tree.
    fn present(&self, dom: &Dom, node: NodeId) -> bool {
        let mut current = dom.parent_composed(node);
        while let Some(ancestor) = current {
            if match self {
                Self::Tag(tag) => dom.tag_name(ancestor) == Some(tag.as_str()),
                Self::Id(id) => dom.attr(ancestor, "id") == Some(id.as_str()),
                Self::Class(class) => {
                    dom.tag_name(ancestor).is_some()
                        && dom.matches_classes(ancestor, std::slice::from_ref(class))
                }
            } {
                return true;
            }
            current = dom.parent_composed(ancestor);
        }
        false
    }

    fn retained_bytes(&self) -> usize {
        match self {
            Self::Tag(text) | Self::Id(text) | Self::Class(text) => text.capacity(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Step {
    axis: Axis,
    tag: Option<String>,
    id: Option<String>,
    classes: Vec<String>,
    attributes: Vec<String>,
    /// A `HostScope` step's rule (its data's address): only a host whose
    /// own shadow tree holds the rule can match its `:host`.
    rule: usize,
    /// On the guard of the compound a route starts from: keys its element's
    /// ancestors must carry. A mutation never changes its node's strict
    /// ancestors, so lacking one, the element matched before and matches
    /// after neither.
    ancestors: Vec<AncestorKey>,
}

impl Step {
    fn new(axis: Axis, tag: &Option<String>) -> Self {
        Self {
            axis,
            tag: tag.as_ref().filter(|tag| tag.as_str() != "*").cloned(),
            id: None,
            classes: Vec::new(),
            attributes: Vec::new(),
            rule: 0,
            ancestors: Vec::new(),
        }
    }

    fn compound(axis: Axis, compound: &Compound) -> Self {
        let mut step = Self::new(axis, &compound.tag);
        step.id = compound.id.clone();
        step.classes = compound.classes.clone();
        step.attributes = compound
            .attrs
            .iter()
            .map(|a| a.name.to_ascii_lowercase())
            .collect();
        step
    }

    fn matches(&self, dom: &Dom, node: NodeId, changed: NodeId, name: &str) -> bool {
        dom.tag_name(node)
            .is_some_and(|tag| self.tag.as_ref().is_none_or(|want| want == tag))
            && (node == changed && name == "id"
                || self
                    .id
                    .as_ref()
                    .is_none_or(|id| dom.attr(node, "id") == Some(id)))
            && (node == changed && name == "class" || dom.matches_classes(node, &self.classes))
            && self.attributes.iter().all(|attribute| {
                node == changed && name == attribute || dom.attr(node, attribute).is_some()
            })
            && self.ancestors.iter().all(|key| key.present(dom, node))
    }

    fn retained_bytes(&self) -> usize {
        self.tag.as_ref().map_or(0, String::capacity)
            + self.id.as_ref().map_or(0, String::capacity)
            + self.ancestors.capacity() * std::mem::size_of::<AncestorKey>()
            + self
                .ancestors
                .iter()
                .map(AncestorKey::retained_bytes)
                .sum::<usize>()
            + [&self.classes, &self.attributes]
                .into_iter()
                .map(|v| {
                    v.capacity() * std::mem::size_of::<String>()
                        + v.iter().map(String::capacity).sum::<usize>()
                })
                .sum::<usize>()
    }

    fn forward(combinator: &Combinator, compound: &Compound) -> Self {
        Self::compound(
            match combinator {
                Combinator::Child => Axis::Child,
                Combinator::Descendant => Axis::Descendant,
                Combinator::NextSibling => Axis::Next,
                Combinator::SubsequentSibling => Axis::Following,
                // CSS Shadow 1 #part: the part elements are in the
                // originating host's composed subtree, which restyling the
                // host as a subject covers.
                Combinator::None | Combinator::Part => Axis::SelfOnly,
            },
            compound,
        )
    }

    fn reverse(combinator: &Combinator, compound: &Compound) -> Self {
        Self::compound(
            match combinator {
                Combinator::Child => Axis::Parent,
                Combinator::Descendant => Axis::Ancestor,
                Combinator::NextSibling => Axis::Previous,
                Combinator::SubsequentSibling => Axis::Preceding,
                Combinator::None | Combinator::Part => Axis::SelfOnly,
            },
            compound,
        )
    }
}

/// The attributes (and validity state) that HTML #constraints reads from a
/// control, its options or its barring ancestors.
const VALIDITY_INPUTS: [&str; 18] = [
    "value",
    "type",
    "required",
    "min",
    "max",
    "step",
    "pattern",
    "maxlength",
    "minlength",
    "multiple",
    "size",
    "disabled",
    "readonly",
    "checked",
    "selected",
    "command",
    "commandfor",
    super::super::input::VALIDITY_DEPENDENCY,
];

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Route {
    source_tag: Option<String>,
    steps: Vec<Step>,
}

/// Element states held by Document state rather than attributes or the
/// tree: HTML #selector-focus, Selectors 4 #the-focus-within-pseudo (both
/// changed by the focus update steps), HTML #selector-target (changed by
/// scrolling to a fragment) and HTML #selector-active (changed by pointer and
/// key activation). Their routes start at an element whose state changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::dom) enum LiveState {
    Focus,
    FocusWithin,
    Target,
    Active,
}

const LIVE_STATES: usize = 4;

/// Overflow expires optional caches; it NEVER truncates selector matching or
/// document content. Deduplication keeps repeated utility rules cheap.
#[derive(Default)]
pub(super) struct Dependencies {
    attributes: FxHashMap<String, FxHashSet<Route>>,
    classes: FxHashMap<String, FxHashSet<Route>>,
    /// Indexed by `LiveState`; kept apart from `attributes` so that no
    /// author attribute name can alias a state.
    states: [FxHashSet<Route>; LIVE_STATES],
    broad: FxHashSet<String>,
    /// A state inside an argument these paths cannot follow (such as a
    /// `:has()` in `:host()`), which takes the complete fallback. Indexed by
    /// `LiveState`.
    broad_states: [bool; LIVE_STATES],
    overflow: bool,
    routes: usize,
    steps: usize,
    direction: bool,
    /// A validity pseudo-class or `:default` occurs: changes that move a
    /// control between form owners or radio groups take the fallback.
    form_state: bool,
    /// The rule being recorded (its data's address), for `HostScope` steps.
    rule: usize,
}

/// Where a dependency path starts: an attribute (or class token) name, or a
/// live state.
enum Source<'a> {
    Attribute(&'a str, bool),
    State(LiveState),
}

impl Dependencies {
    pub(super) fn record(&mut self, selector: &Complex, rule: usize) {
        self.rule = rule;
        self.complex(selector, &[], false);
    }

    fn add(&mut self, name: &str, class: bool, tag: &Option<String>, path: &[Step], broad: bool) {
        self.add_source(Source::Attribute(name, class), tag, path, broad);
    }

    fn add_source(&mut self, source: Source, tag: &Option<String>, path: &[Step], broad: bool) {
        if broad {
            match source {
                Source::Attribute(_, true) => self.broad.insert("class".into()),
                Source::Attribute(name, false) => self.broad.insert(name.to_ascii_lowercase()),
                Source::State(state) => {
                    self.broad_states[state as usize] = true;
                    true
                }
            };
            return;
        }
        if self.overflow {
            return;
        }
        // Bound compiled metadata independently of DOM size. A cache fallback
        // is safe; a partially compiled dependency is not.
        if self.routes >= 65_536 || self.steps.saturating_add(path.len()) > 262_144 {
            self.overflow = true;
            self.attributes.clear();
            self.classes.clear();
            self.states = Default::default();
            return;
        }
        let route = Route {
            source_tag: tag.as_ref().filter(|tag| tag.as_str() != "*").cloned(),
            steps: path.to_vec(),
        };
        let routes = match source {
            Source::Attribute(name, true) => {
                self.classes.entry(name.to_ascii_lowercase()).or_default()
            }
            Source::Attribute(name, false) => self
                .attributes
                .entry(name.to_ascii_lowercase())
                .or_default(),
            Source::State(state) => &mut self.states[state as usize],
        };
        if routes.insert(route) {
            self.routes += 1;
            self.steps += path.len();
        }
    }

    fn complex(&mut self, selector: &Complex, outer: &[Step], broad: bool) {
        // `::slotted()` is a pseudo-element of the subject slot: the elements
        // it represents, not the slot, are the rule's real subjects. It has
        // no meaning inside a nested selector argument; keep the fallback.
        let slotted = selector
            .0
            .last()
            .and_then(|(_, compound)| compound.slotted.as_deref());
        let broad = broad || (slotted.is_some() && !outer.is_empty());
        for (index, (_, compound)) in selector.0.iter().enumerate() {
            let mut path: Vec<_> = selector.0[index + 1..]
                .iter()
                .map(|(combinator, compound)| Step::forward(combinator, compound))
                .collect();
            if let Some(slotted) = slotted {
                path.push(Step::compound(Axis::Assigned, slotted));
            }
            path.extend_from_slice(outer);
            self.compound_within(compound, &path, broad, AncestorKey::of(selector, index));
        }
    }

    fn compound(&mut self, compound: &Compound, path: &[Step], broad: bool) {
        self.compound_within(compound, path, broad, Vec::new());
    }

    /// Record `compound`'s dependencies; `ancestors` are the keys its
    /// complex selector requires of the element's ancestors.
    fn compound_within(
        &mut self,
        compound: &Compound,
        path: &[Step],
        broad: bool,
        ancestors: Vec<AncestorKey>,
    ) {
        // A `:host` compound matches the host, whose combinators lead into
        // its shadow tree rather than along the host's own light tree.
        let host_scope = [Step {
            rule: self.rule,
            ..Step::new(Axis::HostScope, &None)
        }];
        let path = if compound.host { &host_scope[..] } else { path };
        // `:host:has()` reads the shadow tree as the host's children, which
        // the reverse light-tree axes below cannot climb back out of.
        let broad = broad || (compound.host && !compound.has.is_empty());
        let mut guard = Step::compound(Axis::SelfOnly, compound);
        if !compound.host {
            guard.ancestors = ancestors;
        }
        let mut guarded_path = vec![guard];
        guarded_path.extend_from_slice(path);
        let path = guarded_path.as_slice();
        // Keep this exhaustive: adding a selector requires a dependency choice.
        let Compound {
            tag,
            id,
            classes,
            attrs,
            nots,
            selects,
            has,
            structural,
            states,
            host_inner,
            slotted,
            popover_open,
            target,
            hover: _,
            never: _,
            inert_pseudo_element: _,
            scope: _,
            root: _,
            host,
            pseudo: _,
            part: _,
            part_follows: _,
            pseudos: _,
            relative_anchor: _,
        } = compound;
        if id.is_some() {
            self.add("id", false, tag, path, broad);
        }
        for class in classes {
            self.add(class, true, tag, path, broad);
        }
        for attr in attrs {
            self.add(&attr.name, false, tag, path, broad);
        }
        if *popover_open {
            self.add("popover", false, tag, path, broad);
        }
        if *target {
            self.add_source(Source::State(LiveState::Target), tag, path, broad);
        }
        let mut same_subject = vec![Step::new(Axis::SelfOnly, tag)];
        same_subject.extend_from_slice(path);
        for inner in nots
            .iter()
            .flatten()
            .chain(selects.iter().flat_map(|(list, _)| list))
        {
            self.complex(inner, &same_subject, broad);
        }
        for argument in has.iter().flatten() {
            // Index zero is the synthetic anchor. Dependencies inside a
            // logical argument can start outside the relative subtree; their
            // ordinary forward path first returns to this compound.
            for index in 1..argument.complex.0.len() {
                let mut reverse: Vec<_> = (1..=index)
                    .rev()
                    .map(|i| Step::reverse(&argument.complex.0[i].0, &argument.complex.0[i - 1].1))
                    .collect();
                reverse.extend_from_slice(&same_subject);
                self.compound(&argument.complex.0[index].1, &reverse, broad);
            }
        }
        for structural in structural {
            if let Structural::Nth {
                of: Some(selectors),
                ..
            } = structural
            {
                let mut ranks = vec![Step::new(Axis::InclusiveSiblings, tag)];
                ranks.extend_from_slice(path);
                for inner in selectors {
                    self.complex(inner, &ranks, broad);
                }
            }
        }
        if let Some(inner) = host_inner {
            // CSS Shadow 1 #host-selector: the argument tests the host "in
            // its normal context". A `:has()` there could look past the
            // light-tree axes these paths walk; keep its fallback.
            debug_assert!(*host);
            self.compound(inner, &host_scope, broad || !inner.has.is_empty());
        }
        if let Some(inner) = slotted {
            // CSS Shadow 1 #slotted-pseudo: the argument tests the assigned
            // element itself, which is then the subject.
            self.compound(
                inner,
                &[Step::new(Axis::Slottable, &None)],
                broad || !inner.has.is_empty(),
            );
        }
        for state in states {
            let (names, inherited): (&[&str], bool) = match state {
                // The focus update steps name every element whose state
                // changes, its flat-tree ancestors included for
                // :focus-within (`Dom::set_focused_area`).
                // :focus-visible changes with focus, and with indication
                // changes that name the focused element under Focus.
                StatePseudo::Focus | StatePseudo::FocusWithin | StatePseudo::FocusVisible => {
                    let state = if matches!(state, StatePseudo::FocusWithin) {
                        LiveState::FocusWithin
                    } else {
                        LiveState::Focus
                    };
                    self.add_source(Source::State(state), tag, path, broad);
                    continue;
                }
                // Modal state changes only through `set_dialog_modal`, which
                // invalidates every style; the `open` attribute alone does not
                // change it (HTML dialog attribute change steps).
                StatePseudo::Modal => (&[], false),
                // Likewise `set_fullscreen_flag` invalidates every style.
                StatePseudo::Fullscreen => (&[], false),
                StatePseudo::AnyLink => (&["href"], false),
                StatePseudo::Checked => {
                    // HTML #selector-checked. Radio-group state writers must
                    // invalidate EACH changed control; checkedness is not an
                    // inherited state or a document-wide selector dependency.
                    for (name, source) in [
                        ("checked", "input"),
                        ("type", "input"),
                        ("selected", "option"),
                    ] {
                        if tag.as_deref().is_none_or(|tag| tag == "*" || tag == source) {
                            self.add(name, false, &Some(source.into()), path, broad);
                        }
                    }
                    continue;
                }
                StatePseudo::Indeterminate => {
                    // Radio form/name associations can cross subtrees. Keep
                    // this explicit until the form-owner graph owns this edge.
                    for name in ["checked", "name", "type", "value", "form", "id"] {
                        self.add(name, false, &None, &[], true);
                    }
                    continue;
                }
                StatePseudo::Disabled | StatePseudo::Enabled => (&["disabled"], true),
                StatePseudo::Required | StatePseudo::Optional => (&["required", "type"], false),
                StatePseudo::ReadWrite | StatePseudo::ReadOnly => {
                    (&["contenteditable", "readonly", "disabled", "type"], true)
                }
                StatePseudo::PlaceholderShown => (&["placeholder", "value", "type"], false),
                // HTML #selector-defined / #selector-custom: the element's own
                // custom element state, changed through `Dom` setters that
                // touch these dependency names (and `is`, which can make a
                // never-upgraded element undefined).
                StatePseudo::Defined => (&[super::super::input::DEFINED_DEPENDENCY, "is"], false),
                StatePseudo::Custom(_) => (&[super::super::input::CUSTOM_STATE_DEPENDENCY], false),
                StatePseudo::Lang(_) => (&["lang", "xml:lang"], true),
                StatePseudo::Dir(_) => {
                    self.direction = true;
                    // Automatic direction can observe text outside the subject
                    // subtree; retain its established conservative fallback.
                    self.add("dir", false, &None, &[], true);
                    continue;
                }
                // HTML #constraints: a control's validity reads its own
                // attributes, value, checkedness and validity state, its
                // radio group, option list, barring ancestors (a disabled
                // fieldset, handled by the Validity axis) and datalist
                // ancestors (structural); forms and fieldsets read their
                // controls. The Validity axis names every element whose
                // match can change; ownership changes take the fallback in
                // `subjects`.
                StatePseudo::Valid
                | StatePseudo::Invalid
                | StatePseudo::UserValid
                | StatePseudo::UserInvalid
                | StatePseudo::InRange
                | StatePseudo::OutOfRange => {
                    self.form_state = true;
                    let mut validity_path = path.to_vec();
                    if let Some(first) = validity_path.first_mut() {
                        first.axis = Axis::Validity;
                    }
                    for name in VALIDITY_INPUTS {
                        self.add(name, false, &None, &validity_path, broad);
                    }
                    continue;
                }
                // HTML #selector-default: the element's own `checked` or
                // `selected` attribute, or its being the first submit button
                // its form owner has (button types and ownership take the
                // fallback; tree order is structural).
                StatePseudo::Default => {
                    self.form_state = true;
                    for name in [
                        "checked",
                        "selected",
                        "type",
                        "form",
                        "command",
                        "commandfor",
                    ] {
                        self.add(name, false, &None, &[], true);
                    }
                    continue;
                }
                // HTML #selector-open: the open attribute, the picker state,
                // and whether a select is a drop-down box or an input type
                // supports a picker.
                StatePseudo::Open => (
                    &[
                        "open",
                        super::super::input::OPEN_DEPENDENCY,
                        "type",
                        "multiple",
                        "size",
                    ],
                    false,
                ),
                // HTML #selector-heading: the heading level adds the
                // `headingoffset` of every inclusive ancestor up to one with
                // `headingreset`.
                StatePseudo::Heading(None) => (&[], false),
                // HTML #selector-active: `set_active_element` names every
                // element whose state flips, its flat-tree ancestors included.
                StatePseudo::Active => {
                    self.add_source(Source::State(LiveState::Active), tag, path, broad);
                    continue;
                }
                StatePseudo::Media(_) => (&[super::super::input::MUTED_DEPENDENCY, "muted"], false),
                StatePseudo::Heading(Some(_)) => (&["headingoffset", "headingreset"], true),
            };
            for name in names {
                if inherited {
                    let mut inherited_path = vec![Step::new(Axis::InclusiveDescendants, tag)];
                    inherited_path.extend_from_slice(path);
                    self.add(name, false, &None, &inherited_path, broad);
                } else {
                    self.add(name, false, tag, path, broad);
                }
            }
        }
    }

    /// None means a complete cache invalidation, never incomplete results.
    pub(super) fn subjects(
        &self,
        dom: &Dom,
        node: NodeId,
        name: &str,
        old_class: Option<&str>,
    ) -> Option<Vec<NodeId>> {
        let name = name.to_ascii_lowercase();
        if self.overflow
            || self.broad.contains(&name)
            || (self.direction
                && ((name == "value" && dom.text_may_change_direction(node))
                    || (name == "type" && dom.tag_name(node) == Some("input"))))
            // HTML #reset-the-form-owner and #radio-button-group: the old
            // owner or group no longer reaches the changed control.
            || (self.form_state
                && (name == "form"
                    || (name == "id" && dom.tag_name(node) == Some("form"))
                    || (matches!(name.as_str(), "name" | "type")
                        && dom.tag_name(node) == Some("input"))))
        {
            return None;
        }
        let mut routes = FxHashSet::default();
        if let Some(raw) = self.attributes.get(&name) {
            routes.extend(raw);
        }
        if name == "class" {
            if let Some(old) = old_class {
                let old: FxHashSet<_> = old.split_ascii_whitespace().collect();
                let new: FxHashSet<_> = dom
                    .attr(node, "class")
                    .unwrap_or("")
                    .split_ascii_whitespace()
                    .collect();
                for token in old.symmetric_difference(&new) {
                    if let Some(paths) = self.classes.get(&token.to_ascii_lowercase()) {
                        routes.extend(paths);
                    }
                }
            } else {
                routes.extend(self.classes.values().flatten());
            }
        }
        let mut result = FxHashSet::default();
        // A budget limits optimization work, not CSS semantics. On an
        // adversarial route graph, full invalidation is cheaper and correct.
        let mut budget = dom.node_count().saturating_mul(16).max(4096);
        Self::walk(dom, node, &name, routes, &mut budget, &mut result)?;
        Some(result.into_iter().collect())
    }

    /// Whether any selector can observe `state`. Overflow discarded the
    /// routes that would answer, so it conservatively says yes.
    pub(super) fn observes(&self, state: LiveState) -> bool {
        self.overflow
            || self.broad_states[state as usize]
            || !self.states[state as usize].is_empty()
    }

    /// The selector subjects whose match can change when `node` gains or
    /// loses `state`. None means a complete cache invalidation, never
    /// incomplete results.
    pub(super) fn state_subjects(
        &self,
        dom: &Dom,
        node: NodeId,
        state: LiveState,
    ) -> Option<Vec<NodeId>> {
        if self.overflow || self.broad_states[state as usize] {
            return None;
        }
        let mut result = FxHashSet::default();
        let mut budget = dom.node_count().saturating_mul(16).max(4096);
        // No attribute changed, so every guard tests a current value: the
        // empty name exempts none (attribute names are never empty).
        let routes = &self.states[state as usize];
        Self::walk(dom, node, "", routes, &mut budget, &mut result)?;
        Some(result.into_iter().collect())
    }

    /// Follows each route from `changed`, adding the elements it reaches.
    /// `name` is the changed attribute, whose guard on `changed` is exempt.
    fn walk<'a>(
        dom: &Dom,
        changed: NodeId,
        name: &str,
        routes: impl IntoIterator<Item = &'a Route>,
        budget: &mut usize,
        result: &mut FxHashSet<NodeId>,
    ) -> Option<()> {
        let node = changed;
        for route in routes {
            if route
                .source_tag
                .as_ref()
                .is_some_and(|tag| dom.tag_name(node) != Some(tag.as_str()))
            {
                continue;
            }
            let mut current = vec![node];
            for step in &route.steps {
                let mut next = FxHashSet::default();
                for node in current {
                    let mut visit = |id| {
                        *budget = budget.checked_sub(1)?;
                        if step.matches(dom, id, changed, name) {
                            next.insert(id);
                        }
                        Some(())
                    };
                    match step.axis {
                        Axis::SelfOnly => visit(node)?,
                        Axis::Child => {
                            for id in dom.child_iter(node) {
                                visit(id)?;
                            }
                        }
                        Axis::Descendant => {
                            for id in dom.descendants(node) {
                                visit(id)?;
                            }
                        }
                        Axis::InclusiveDescendants => {
                            // Inherited states (lang, disabled, editability):
                            // `:lang()` reads the flat tree, which reaches a
                            // host's shadow tree and a slot's assigned nodes.
                            // Visiting both trees over-approximates either.
                            let mut stack = vec![node];
                            let mut seen = FxHashSet::default();
                            while let Some(id) = stack.pop() {
                                if !seen.insert(id) {
                                    continue;
                                }
                                visit(id)?;
                                dom.push_composed_children(id, &mut stack);
                                if dom.tag_name(id) == Some("slot") {
                                    stack.extend(dom.slot_assigned_nodes(id));
                                }
                            }
                        }
                        // CSS Shadow 1 #host-selector: `:host` and `:host()`
                        // match nothing but a shadow host, and only "in the
                        // context of" its own shadow tree.
                        Axis::HostScope => {
                            if let Some(shadow) = dom.shadow_root(node)
                                && dom.shadow_tree_holds_rule(shadow, step.rule)
                            {
                                visit(node)?;
                                for id in dom.descendants(shadow) {
                                    visit(id)?;
                                }
                                for id in dom.child_iter(node) {
                                    visit(id)?;
                                }
                            }
                        }
                        Axis::Slottable => {
                            if dom.assigned_slot(node).is_some() {
                                visit(node)?;
                            }
                        }
                        Axis::Assigned => {
                            if dom.tag_name(node) == Some("slot") {
                                for id in dom.flat_slot_nodes(node) {
                                    visit(id)?;
                                }
                            }
                        }
                        Axis::Validity => {
                            for id in dom.validity_subjects(node) {
                                visit(id)?;
                            }
                        }
                        Axis::Parent => {
                            if let Some(id) = dom.selector_parent(node) {
                                visit(id)?;
                            }
                        }
                        Axis::Ancestor => {
                            let mut parent = dom.selector_parent(node);
                            while let Some(id) = parent {
                                visit(id)?;
                                parent = dom.selector_parent(id);
                            }
                        }
                        Axis::Next | Axis::Following => {
                            let mut sibling = dom.next_element_sibling(node);
                            while let Some(id) = sibling {
                                visit(id)?;
                                if step.axis == Axis::Next {
                                    break;
                                }
                                sibling = dom.next_element_sibling(id);
                            }
                        }
                        Axis::Previous | Axis::Preceding => {
                            let mut sibling = dom.prev_element_sibling(node);
                            while let Some(id) = sibling {
                                visit(id)?;
                                if step.axis == Axis::Previous {
                                    break;
                                }
                                sibling = dom.prev_element_sibling(id);
                            }
                        }
                        Axis::InclusiveSiblings => {
                            if let Some(parent) = dom.nodes[node].parent {
                                for id in dom.child_iter(parent) {
                                    visit(id)?;
                                }
                            } else {
                                visit(node)?;
                            }
                        }
                    }
                }
                current = next.into_iter().collect();
                if current.is_empty() {
                    break;
                }
            }
            result.extend(current);
        }
        Some(())
    }

    pub(super) fn retained_bytes(&self) -> usize {
        fn routes_bytes(routes: &FxHashSet<Route>) -> usize {
            routes.capacity() * std::mem::size_of::<Route>()
                + routes
                    .iter()
                    .map(|route| {
                        route.source_tag.as_ref().map_or(0, String::capacity)
                            + route.steps.capacity() * std::mem::size_of::<Step>()
                            + route.steps.iter().map(Step::retained_bytes).sum::<usize>()
                    })
                    .sum::<usize>()
        }
        [&self.attributes, &self.classes]
            .into_iter()
            .map(|map| {
                map.capacity() * std::mem::size_of::<(String, FxHashSet<Route>)>()
                    + map
                        .iter()
                        .map(|(name, routes)| name.capacity() + routes_bytes(routes))
                        .sum::<usize>()
            })
            .sum::<usize>()
            + self.states.iter().map(routes_bytes).sum::<usize>()
            + self.broad.capacity() * std::mem::size_of::<String>()
            + self.broad.iter().map(String::capacity).sum::<usize>()
    }
}
