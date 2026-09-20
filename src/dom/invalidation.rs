//! Selector dependency invalidation (Selectors 4 §§4, 6, 14, 15, 19.3).
//!
//! Cache pure selector matches independently from the cascade and container
//! conditions. Attribute writes invalidate the selector subjects reachable
//! through the rule's combinators, not every element merely because the DOM
//! revision changed. Relational/filtered positional selectors, element state
//! and cross-shadow dependencies deliberately retain a full fallback.

use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Impact {
    Element,
    Subtree,
    SiblingSubtrees,
    All,
}

#[derive(Default)]
pub(super) struct SelectorDependencies {
    attributes: FxHashMap<String, Impact>,
    relational: Vec<RelationalDependency>,
    structural: Vec<StructureDependency>,
    empty: bool,
    text_direction: bool,
    text_placeholder: bool,
    checked: bool,
    structure_global: bool,
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
struct RelationalDependency {
    tag: Option<String>,
    id: Option<String>,
    classes: Vec<String>,
    anchor_attributes: Vec<String>,
    attributes: Vec<String>,
    siblings: bool,
}

impl RelationalDependency {
    fn new(anchor: &Compound, argument: &HasArg) -> Option<Self> {
        let mut attributes = Vec::new();
        // The first compound is the parser's synthetic :scope anchor.
        for (_, compound) in argument.complex.0.iter().skip(1) {
            // Logical arguments can look outside the relative subtree (e.g.
            // :has(:is(body.theme .hit))); inherited and positional states
            // have additional dependencies. Retain the full fallback for them.
            if !compound.nots.is_empty()
                || !compound.selects.is_empty()
                || !compound.has.is_empty()
                || !compound.structural.is_empty()
                || !compound.states.is_empty()
                || compound.hover
                || compound.target
                || compound.popover_open
                || compound.scope
                || compound.root
                || compound.host
                || compound.host_inner.is_some()
                || compound.slotted.is_some()
                || compound.pseudo.is_some()
                || compound.inert_pseudo_element
            {
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
            anchor_attributes: anchor.attrs.iter().map(|a| a.name.clone()).collect(),
            attributes,
            siblings: argument.sibling,
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
        while let Some(node) = next {
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
    pub(super) fn build<'a>(rules: impl Iterator<Item = &'a StyleRule>) -> Self {
        let mut result = Self::default();
        for rule in rules {
            result.complex(&rule.selector, Impact::Element);
            result.record_structure(&rule.selector, &[], Impact::Element);
        }
        result
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

    fn structure_impact(&self, dom: &Dom, parent: NodeId) -> (bool, bool) {
        let (mut siblings, mut children) = (false, false);
        for dependency in &self.structural {
            match dependency {
                StructureDependency::Empty {
                    guards,
                    siblings: affects_siblings,
                } => {
                    if guards.iter().all(|guard| guard.matches(dom, parent)) {
                        children = true;
                        siblings |= affects_siblings;
                    }
                }
                StructureDependency::ChildIndex(guards) => {
                    children |= dom
                        .child_iter(parent)
                        .any(|node| guards.iter().all(|guard| guard.matches(dom, node)));
                }
                StructureDependency::Siblings { left, right } => {
                    children |= dom.child_iter(parent).any(|node| left.matches(dom, node))
                        && dom.child_iter(parent).any(|node| right.matches(dom, node));
                }
            }
        }
        (siblings, children)
    }

    pub(super) fn attribute(&self, dom: &Dom, node: NodeId, name: &str) -> Option<Impact> {
        let name = name.to_ascii_lowercase();
        let impact = self.attributes.get(name.as_str()).copied();
        if impact == Some(Impact::All)
            || (self.text_direction
                && ((name == "value" && dom.text_may_change_direction(node))
                    // HTML's undefined-dir telephone state is LTR even with
                    // an RTL parent. Type changes need no automatic ancestor.
                    || (name == "type" && dom.tag_name(node) == Some("input"))))
            || (self.checked
                && matches!(
                    (dom.tag_name(node), name.as_str()),
                    (Some("input"), "checked" | "type") | (Some("option"), "selected")
                ))
            || self.relational.iter().any(|dependency| {
                dependency.attributes.contains(&name) && dependency.may_affect(dom, node, false)
            })
        {
            Some(Impact::All)
        } else {
            impact
        }
    }

    pub(super) fn retained_bytes(&self) -> usize {
        self.attributes.capacity() * std::mem::size_of::<(String, Impact)>()
            + self.attributes.keys().map(String::capacity).sum::<usize>()
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
    }

    fn add(&mut self, name: &str, impact: Impact) {
        self.attributes
            .entry(name.to_ascii_lowercase())
            .and_modify(|prior| *prior = (*prior).max(impact))
            .or_insert(impact);
    }

    fn complex(&mut self, selector: &Complex, outer: Impact) {
        for (index, (_, compound)) in selector.0.iter().enumerate() {
            let mut impact = outer;
            for (combinator, _) in &selector.0[index + 1..] {
                impact = impact.max(match combinator {
                    Combinator::Descendant | Combinator::Child => Impact::Subtree,
                    Combinator::NextSibling | Combinator::SubsequentSibling => {
                        Impact::SiblingSubtrees
                    }
                    Combinator::None => Impact::All,
                });
            }
            self.compound(compound, impact);
        }
    }

    fn compound(&mut self, compound: &Compound, impact: Impact) {
        // Exhaustive so future selector forms require a dependency decision.
        let Compound {
            tag: _,
            id,
            classes,
            attrs,
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
            host: _,
            host_inner,
            slotted,
            pseudo: _,
            pseudos: _,
        } = compound;
        if id.is_some() {
            self.add("id", impact);
        }
        if !classes.is_empty() {
            self.add("class", impact);
        }
        for attribute in attrs {
            self.add(&attribute.name, impact);
        }
        for selector in nots
            .iter()
            .flatten()
            .chain(selects.iter().flat_map(|(group, _)| group))
        {
            self.complex(selector, impact);
        }
        for argument in has.iter().flatten() {
            if let Some(dependency) = RelationalDependency::new(compound, argument) {
                self.relational.push(dependency);
            } else {
                self.structure_global = true;
                self.complex(&argument.complex, Impact::All);
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
                    self.complex(selector, Impact::All);
                }
            }
        }
        for inner in [host_inner, slotted].into_iter().flatten() {
            self.compound(inner, Impact::All);
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
            self.structure_global |= !matches!(
                state,
                StatePseudo::AnyLink
                    | StatePseudo::Checked
                    | StatePseudo::Disabled
                    | StatePseudo::Enabled
                    | StatePseudo::Dir(_)
            );
            self.text_direction |= matches!(state, StatePseudo::Dir(_));
            self.text_placeholder |= matches!(state, StatePseudo::PlaceholderShown);
            self.checked |= matches!(state, StatePseudo::Checked);
            // HTML state can propagate through fieldsets, radio groups,
            // inherited language/editability, and flat-tree ancestors. Keep
            // these dependencies broad until separately proven and tested.
            let attributes: &[&str] = match state {
                StatePseudo::AnyLink => &["href"],
                // Filter by element category at mutation time. A script's
                // type attribute cannot change a control's checkedness.
                StatePseudo::Checked => &[],
                StatePseudo::Indeterminate => &["checked", "name", "type", "value", "form", "id"],
                StatePseudo::Disabled | StatePseudo::Enabled => &["disabled"],
                StatePseudo::Required | StatePseudo::Optional => &["required", "type"],
                StatePseudo::ReadWrite | StatePseudo::ReadOnly => {
                    &["contenteditable", "readonly", "disabled", "type"]
                }
                StatePseudo::PlaceholderShown => &["placeholder", "value"],
                StatePseudo::Lang(_) => &["lang", "xml:lang"],
                // Value/type only alter directionality where automatic
                // direction is in use; explicit dir remains conservative.
                StatePseudo::Dir(_) => &["dir"],
            };
            for name in attributes {
                self.add(
                    name,
                    if matches!(state, StatePseudo::AnyLink) {
                        impact
                    } else {
                        Impact::All
                    },
                );
            }
        }
    }
}

impl Dom {
    /// DOM #concept-shadow-including-root / CSS Scoping: selector matching,
    /// inheritance and slot distribution cannot couple separate shadow-
    /// including trees. A detached feature probe must not make unrelated
    /// detached construction invalidate the live document's style cache.
    fn shadow_style_dependencies(&self, node: NodeId) -> bool {
        if self.shadow_roots.is_empty() {
            return false;
        }
        let root = |mut id| {
            while let Some(parent) = self.parent_composed(id) {
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

    pub(super) fn invalidate_attribute_selectors(&mut self, node: NodeId, name: &str) -> Impact {
        // DOM §4.2.2: `slot` and a slot's `name` change distribution without
        // a child-list mutation. That is an implicit cross-tree dependency,
        // including for state resolved through the style parent, even when
        // no rule contains a literal [slot] or [name] selector.
        if self.shadow_style_dependencies(node)
            && (name.eq_ignore_ascii_case("slot")
                || (name.eq_ignore_ascii_case("name") && self.tag_name(node) == Some("slot")))
        {
            self.selector_epoch = self.selector_epoch.wrapping_add(1);
            return Impact::All;
        }
        let impact = {
            let cache = self.style_cache.borrow();
            match cache.as_ref() {
                Some((epoch, index)) if *epoch == self.style_epoch => {
                    index.selector_dependencies.attribute(self, node, name)
                }
                // No current rule index: no proof of independence.
                _ => Some(Impact::All),
            }
        };
        if impact.is_none() && !self.shadow_style_dependencies(node) {
            return Impact::Element;
        }
        let impact = impact.unwrap_or(Impact::All);
        if impact == Impact::All || self.shadow_style_dependencies(node) {
            if casc_diag_on() {
                eprintln!(
                    "DIAGINVALID attribute node={node} tag={:?} id={:?} name={name} impact={impact:?} shadow={}",
                    self.tag_name(node),
                    self.attr(node, "id"),
                    self.shadow_style_dependencies(node)
                );
            }
            self.selector_epoch = self.selector_epoch.wrapping_add(1);
            return Impact::All;
        }
        let root = if impact == Impact::SiblingSubtrees {
            self.nodes[node].parent.unwrap_or(node)
        } else {
            node
        };
        let mut cache = self.selector_cache.borrow_mut();
        cache.invalidate(root);
        if impact != Impact::Element {
            for descendant in self.descendants(root) {
                cache.invalidate(descendant);
            }
        }
        impact
    }

    #[track_caller]
    pub(super) fn invalidate_all_style_values(&mut self) {
        if casc_diag_on() {
            eprintln!(
                "DIAGINVALID all-styles caller={}",
                std::panic::Location::caller()
            );
        }
        self.style_value_epoch = self.style_value_epoch.wrapping_add(1);
        self.layout_cache.get_mut().clear();
        self.box_tree_cache.get_mut().clear();
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
        if changed.iter().any(|&id| self.ancestor_is_svg(id)) {
            self.invalidate_svg_layout();
        }
    }

    fn invalidate_layout_ancestors(&mut self, node: NodeId) {
        let mut next = Some(node);
        while let Some(id) = next {
            self.layout_cache.get_mut().invalidate(id);
            self.box_tree_cache.get_mut().invalidate(id);
            next = self.nodes[id].parent;
        }
    }

    /// SVG 2 #UseShadowTree requires referenced subtree changes to reach every
    /// instance. Rebuild SVG resources and their containing formatting paths;
    /// independent HTML formatting contexts do not depend on those resources.
    /// This deliberately includes every SVG until reference dependencies are
    /// indexed, covering indirect references and shared arena tree scopes.
    fn invalidate_svg_layout(&mut self) {
        let roots: Vec<_> = self
            .nodes
            .iter()
            .enumerate()
            .filter_map(|(node, _)| (self.tag_name(node) == Some("svg")).then_some(node))
            .collect();
        for root in roots {
            self.invalidate_layout_ancestors(root);
        }
    }

    pub(super) fn invalidate_attribute_style_values(
        &mut self,
        node: NodeId,
        name: &str,
        impact: Impact,
    ) {
        if impact == Impact::All {
            self.invalidate_all_style_values();
            return;
        }
        let root = if impact == Impact::SiblingSubtrees || self.tag_name(node) == Some("source") {
            self.nodes[node].parent.unwrap_or(node)
        } else {
            node
        };
        // Named associations and SVG use-instance resources can point outside
        // a selector's subject subtree. Keep broad layout invalidation for
        // these until an explicit reference-dependency index is available.
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
        self.invalidate_style_subtree(root, false);
    }

    /// CSS selectors and inheritance follow the element tree; immutable box
    /// nodes are invalidated in the same operation, plus rebuilt ancestors.
    pub(super) fn invalidate_style_subtree(&mut self, root: NodeId, selectors: bool) {
        // Even an own-element selector/inline style can change inherited
        // computed values, custom properties, link context or font metrics.
        let affected: Vec<_> = std::iter::once(root)
            .chain(self.descendants(root))
            .collect();
        if casc_diag_on() && affected.len() > 256 {
            eprintln!(
                "DIAGINVALID subtree node={root} tag={:?} id={:?} selectors={selectors} count={}",
                self.tag_name(root),
                self.attr(root, "id"),
                affected.len()
            );
        }
        // Do not scan every cached property in the document for a local
        // update. Property IDs are dense, so eviction is bounded by the dirty
        // subtree's size rather than the total cached document.
        let computed = &mut self.computed_cache.get_mut().1;
        if !computed.is_empty() {
            for &id in &affected {
                for property in 0..PROPS.len() {
                    computed.remove(&(id, property));
                }
            }
        }
        if affected
            .iter()
            .any(|&id| matches!(self.tag_name(id), Some("meta" | "base")))
        {
            self.layout_cache.get_mut().clear();
            self.box_tree_cache.get_mut().clear();
        } else if self.ancestor_is_svg(root)
            || affected.iter().any(|&id| self.tag_name(id) == Some("svg"))
        {
            self.invalidate_svg_layout();
        }
        for id in affected {
            self.transitions.invalidate(id);
            if selectors {
                self.selector_cache.get_mut().invalidate(id);
            }
            self.custom_prop_cache.get_mut().1.remove(&id);
            self.properties.invalidate(id);
            self.matched_cache.get_mut().invalidate(id);
            self.cascaded_cache.get_mut().invalidate(id);
            self.font_cache.get_mut().invalidate(id);
            self.font_units_cache.get_mut().invalidate(id);
            self.decoration_cache.get_mut().invalidate(id);
            self.layout_cache.get_mut().invalidate(id);
            self.box_tree_cache.get_mut().invalidate(id);
        }
        self.invalidate_layout_ancestors(root);
    }

    pub(super) fn invalidate_structure(&mut self, parent: NodeId) {
        // Detached construction uses the same dependency proof. Recursively
        // invalidating its entire growing root on each append would make a
        // fragment-building loop quadratic even without structural selectors.
        let local = (!self.shadow_style_dependencies(parent))
            .then(|| {
                let index = self.style_cache.borrow();
                index.as_ref().and_then(|(epoch, index)| {
                    (*epoch == self.style_epoch
                        && !index.selector_dependencies.structure_global
                        && !index.selector_dependencies.relational.iter().any(|dependency| {
                            dependency.may_affect(self, parent, true)
                        })
                        // Selectors 4 #the-dir-pseudo / HTML directionality:
                        // only an automatic-direction ancestor can acquire a
                        // different direction when its descendants change.
                        && !(index.selector_dependencies.text_direction
                            && self.text_may_change_direction(parent)))
                    .then(|| index.selector_dependencies.structure_impact(self, parent))
                })
            })
            .flatten();
        let Some((empty_siblings, restyle_children)) = local else {
            if casc_diag_on() {
                let index = self.style_cache.borrow();
                eprintln!(
                    "DIAGINVALID structure node={parent} tag={:?} id={:?} shadow={} dependencies={:?}",
                    self.tag_name(parent),
                    self.attr(parent, "id"),
                    self.shadow_style_dependencies(parent),
                    index.as_ref().map(|(epoch, index)| (
                        *epoch == self.style_epoch,
                        index.selector_dependencies.structure_global,
                        index
                            .selector_dependencies
                            .relational
                            .iter()
                            .any(|d| d.may_affect(self, parent, true)),
                        index.selector_dependencies.text_direction
                            && self.text_may_change_direction(parent)
                    ))
                );
            }
            self.selector_epoch = self.selector_epoch.wrapping_add(1);
            self.invalidate_all_style_values();
            return;
        };
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
            } else if self.ancestor_is_svg(parent) || self.tag_name(parent) == Some("svg") {
                self.invalidate_svg_layout();
            }
            self.invalidate_layout_ancestors(parent);
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
                    *epoch == self.style_epoch && !index.selector_dependencies.text_placeholder
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
    pub(super) fn touch_text(&mut self, parent: NodeId, empty_changed: bool) {
        let independent = !self.shadow_style_dependencies(parent) && {
            let index = self.style_cache.borrow();
            index.as_ref().is_some_and(|(epoch, index)| {
                *epoch == self.style_epoch
                    && !(index.selector_dependencies.text_direction
                        && self.text_may_change_direction(parent))
                    && !(index.selector_dependencies.text_placeholder
                        && self.tag_name(parent) == Some("textarea"))
                    && !(empty_changed && index.selector_dependencies.empty)
            })
        };
        if !independent {
            self.touch_content(Some(parent));
            return;
        }
        if self.ancestor_is_svg(parent) || self.tag_name(parent) == Some("svg") {
            // SVG 2 #UseShadowTree: referenced text changes must propagate to
            // other SVG instances too, not just this element's ancestors.
            self.invalidate_svg_layout();
        }
        self.invalidate_layout_ancestors(parent);
        self.mark_dom_revision();
        self.dirty_nodes.push((parent, DirtyKind::Content));
        self.record_geometry_dirty(parent, DirtyKind::Content);
    }

    /// Selectors 4 #the-dir-pseudo and HTML #contained-text-auto-directionality:
    /// text only affects directionality inside an automatic-direction subtree.
    /// Explicit ltr/rtl and excluded descendants stop the upward dependency.
    /// Shadow/slot dependencies have already taken the conservative fallback.
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
            (0..dom.node_count())
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
        assert_eq!(
            warm,
            values(dom),
            "local invalidation differs from full recascade"
        );
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
        for node in 0..dom.node_count() {
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
