//! Mutation-to-subject paths, compiled from Selectors 4 §§4, 14 and 15.
//!
//! These are dependency paths, not selectors: a negative test is just as much
//! a dependency as a positive one. Guards omit the changed attribute on the
//! mutated element: testing only its new value would lose former matches.
//! `:has()` reverses the path to its anchor, while `:nth-child(of S)` fans out
//! from changed membership to inclusive siblings before resuming the outer path.
//! See https://drafts.csswg.org/selectors-4/#match-a-selector-against-an-element
//! (local CSSWG snapshot 81c27f68690138345b2b3b6af8ccc42dad3dca1d).

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
    InclusiveDescendants,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Step {
    axis: Axis,
    tag: Option<String>,
    id: Option<String>,
    classes: Vec<String>,
    attributes: Vec<String>,
}

impl Step {
    fn new(axis: Axis, tag: &Option<String>) -> Self {
        Self {
            axis,
            tag: tag.as_ref().filter(|tag| tag.as_str() != "*").cloned(),
            id: None,
            classes: Vec::new(),
            attributes: Vec::new(),
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
    }

    fn retained_bytes(&self) -> usize {
        self.tag.as_ref().map_or(0, String::capacity)
            + self.id.as_ref().map_or(0, String::capacity)
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
                Combinator::None => Axis::SelfOnly,
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
                Combinator::None => Axis::SelfOnly,
            },
            compound,
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Route {
    source_tag: Option<String>,
    steps: Vec<Step>,
}

/// Overflow expires optional caches; it NEVER truncates selector matching or
/// document content. Deduplication keeps repeated utility rules cheap.
#[derive(Default)]
pub(super) struct Dependencies {
    attributes: FxHashMap<String, FxHashSet<Route>>,
    classes: FxHashMap<String, FxHashSet<Route>>,
    broad: FxHashSet<String>,
    overflow: bool,
    routes: usize,
    steps: usize,
    direction: bool,
}

impl Dependencies {
    pub(super) fn record(&mut self, selector: &Complex) {
        self.complex(selector, &[], false);
    }

    fn add(&mut self, name: &str, class: bool, tag: &Option<String>, path: &[Step], broad: bool) {
        let name = name.to_ascii_lowercase();
        if broad {
            self.broad.insert(if class { "class".into() } else { name });
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
            return;
        }
        let route = Route {
            source_tag: tag.as_ref().filter(|tag| tag.as_str() != "*").cloned(),
            steps: path.to_vec(),
        };
        let map = if class {
            &mut self.classes
        } else {
            &mut self.attributes
        };
        if map.entry(name).or_default().insert(route) {
            self.routes += 1;
            self.steps += path.len();
        }
    }

    fn complex(&mut self, selector: &Complex, outer: &[Step], broad: bool) {
        for (index, (_, compound)) in selector.0.iter().enumerate() {
            let mut path: Vec<_> = selector.0[index + 1..]
                .iter()
                .map(|(combinator, compound)| Step::forward(combinator, compound))
                .collect();
            path.extend_from_slice(outer);
            self.compound(compound, &path, broad);
        }
    }

    fn compound(&mut self, compound: &Compound, path: &[Step], broad: bool) {
        let mut guarded_path = vec![Step::compound(Axis::SelfOnly, compound)];
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
            target: _,
            hover: _,
            never: _,
            inert_pseudo_element: _,
            scope: _,
            root: _,
            host: _,
            pseudo: _,
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
        for inner in [host_inner, slotted].into_iter().flatten() {
            // Tree-scoped matching requires a scope-bearing route. The caller
            // also protects implicit slot and inherited flat-tree dependencies.
            self.compound(inner, path, true);
        }
        for state in states {
            let (names, inherited): (&[&str], bool) = match state {
                StatePseudo::Focus | StatePseudo::FocusWithin => (&[], false),
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
                StatePseudo::Lang(_) => (&["lang", "xml:lang"], true),
                StatePseudo::Dir(_) => {
                    self.direction = true;
                    // Automatic direction can observe text outside the subject
                    // subtree; retain its established conservative fallback.
                    self.add("dir", false, &None, &[], true);
                    continue;
                }
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
        let changed = node;
        // A budget limits optimization work, not CSS semantics. On an
        // adversarial route graph, full invalidation is cheaper and correct.
        let mut budget = dom.node_count().saturating_mul(16).max(4096);
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
                        budget = budget.checked_sub(1)?;
                        if step.matches(dom, id, changed, &name) {
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
                        Axis::Descendant | Axis::InclusiveDescendants => {
                            if step.axis == Axis::InclusiveDescendants {
                                visit(node)?;
                            }
                            for id in dom.descendants(node) {
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
        Some(result.into_iter().collect())
    }

    pub(super) fn retained_bytes(&self) -> usize {
        [&self.attributes, &self.classes]
            .into_iter()
            .map(|map| {
                map.capacity() * std::mem::size_of::<(String, FxHashSet<Route>)>()
                    + map
                        .iter()
                        .map(|(name, routes)| {
                            name.capacity()
                                + routes.capacity() * std::mem::size_of::<Route>()
                                + routes
                                    .iter()
                                    .map(|route| {
                                        route.source_tag.as_ref().map_or(0, String::capacity)
                                            + route.steps.capacity() * std::mem::size_of::<Step>()
                                            + route
                                                .steps
                                                .iter()
                                                .map(Step::retained_bytes)
                                                .sum::<usize>()
                                    })
                                    .sum::<usize>()
                        })
                        .sum::<usize>()
            })
            .sum::<usize>()
            + self.broad.capacity() * std::mem::size_of::<String>()
            + self.broad.iter().map(String::capacity).sum::<usize>()
    }
}
