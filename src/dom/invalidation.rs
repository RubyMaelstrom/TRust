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
    empty: bool,
    text_state: bool,
    structure_global: bool,
    empty_siblings: bool,
    sibling_structure: bool,
}

impl SelectorDependencies {
    pub(super) fn build<'a>(rules: impl Iterator<Item = &'a StyleRule>) -> Self {
        let mut result = Self::default();
        for rule in rules {
            result.complex(&rule.selector, Impact::Element);
        }
        result
    }

    pub(super) fn attribute(&self, name: &str) -> Option<Impact> {
        self.attributes
            .get(name.to_ascii_lowercase().as_str())
            .copied()
    }

    pub(super) fn retained_bytes(&self) -> usize {
        self.attributes.capacity() * std::mem::size_of::<(String, Impact)>()
            + self.attributes.keys().map(String::capacity).sum::<usize>()
    }

    fn add(&mut self, name: &str, impact: Impact) {
        self.attributes
            .entry(name.to_ascii_lowercase())
            .and_modify(|prior| *prior = (*prior).max(impact))
            .or_insert(impact);
    }

    fn complex(&mut self, selector: &Complex, outer: Impact) {
        self.sibling_structure |= selector.0.iter().any(|(combinator, _)| {
            matches!(
                combinator,
                Combinator::NextSibling | Combinator::SubsequentSibling
            )
        });
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
            popover_open: _,
            never: _,
            never_unknown: _,
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
            self.structure_global = true;
            self.complex(&argument.complex, Impact::All);
        }
        for structural in structural {
            self.sibling_structure |= !matches!(structural, Structural::Empty);
            self.empty |= matches!(structural, Structural::Empty);
            self.empty_siblings |=
                matches!(structural, Structural::Empty) && impact >= Impact::SiblingSubtrees;
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
            self.structure_global |= !matches!(
                state,
                StatePseudo::AnyLink | StatePseudo::Disabled | StatePseudo::Enabled
            );
            self.text_state |= matches!(state, StatePseudo::Dir(_) | StatePseudo::PlaceholderShown);
            // HTML state can propagate through fieldsets, radio groups,
            // inherited language/editability, and flat-tree ancestors. Keep
            // these dependencies broad until separately proven and tested.
            let attributes: &[&str] = match state {
                StatePseudo::AnyLink => &["href"],
                StatePseudo::Checked => &["checked", "selected", "type"],
                StatePseudo::Indeterminate => &["checked", "name", "type", "value", "form", "id"],
                StatePseudo::Disabled | StatePseudo::Enabled => &["disabled"],
                StatePseudo::Required | StatePseudo::Optional => &["required", "type"],
                StatePseudo::ReadWrite | StatePseudo::ReadOnly => {
                    &["contenteditable", "readonly", "disabled", "type"]
                }
                StatePseudo::PlaceholderShown => &["placeholder", "value"],
                StatePseudo::Lang(_) => &["lang", "xml:lang"],
                StatePseudo::Dir(_) => &["dir", "value", "type"],
            };
            for name in attributes {
                self.add(name, Impact::All);
            }
        }
    }
}

impl Dom {
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
        if !self.shadow_roots.is_empty()
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
                    index.selector_dependencies.attribute(name)
                }
                // No current rule index: no proof of independence.
                _ => Some(Impact::All),
            }
        };
        if impact.is_none() && self.shadow_roots.is_empty() {
            return Impact::Element;
        }
        let impact = impact.unwrap_or(Impact::All);
        if impact == Impact::All || !self.shadow_roots.is_empty() {
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

    pub(super) fn invalidate_all_style_values(&mut self) {
        self.style_value_epoch = self.style_value_epoch.wrapping_add(1);
        self.layout_cache.get_mut().clear();
        self.box_tree_cache.get_mut().clear();
    }

    fn invalidate_layout_ancestors(&mut self, node: NodeId) {
        let mut next = Some(node);
        while let Some(id) = next {
            self.layout_cache.get_mut().invalidate(id);
            self.box_tree_cache.get_mut().invalidate(id);
            next = self.nodes[id].parent;
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
        if matches!(
            name.to_ascii_lowercase().as_str(),
            "id" | "name" | "form" | "for" | "type"
        ) {
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
        let external = self.ancestor_is_svg(root)
            || affected
                .iter()
                .any(|&id| matches!(self.tag_name(id), Some("svg" | "meta" | "base")));
        if external {
            self.layout_cache.get_mut().clear();
            self.box_tree_cache.get_mut().clear();
        }
        for id in affected {
            if selectors {
                self.selector_cache.get_mut().invalidate(id);
            }
            self.custom_prop_cache.get_mut().1.remove(&id);
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
        let local = self
            .shadow_roots
            .is_empty()
            .then(|| {
                let index = self.style_cache.borrow();
                index.as_ref().and_then(|(epoch, index)| {
                    (*epoch == self.style_epoch && !index.selector_dependencies.structure_global)
                        .then_some((
                            index.selector_dependencies.empty_siblings,
                            index.selector_dependencies.empty
                                || index.selector_dependencies.sibling_structure,
                        ))
                })
            })
            .flatten();
        let Some((empty_siblings, restyle_children)) = local else {
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
                Some("picture" | "select" | "optgroup" | "fieldset" | "form")
            )
        {
            self.invalidate_style_subtree(root, true);
        } else {
            // Without child-index, sibling, :empty or relational rules, old
            // siblings keep their selectors and inherited styles. The actual
            // inserted/removed subtree is invalidated by the tree operation.
            // Only the parents' formatting structure/flow needs rebuilding.
            if self.ancestor_is_svg(parent)
                || matches!(self.tag_name(parent), Some("svg" | "meta" | "base"))
            {
                self.layout_cache.get_mut().clear();
                self.box_tree_cache.get_mut().clear();
            }
            self.invalidate_layout_ancestors(parent);
        }
    }

    /// Character data / replace-all of text children leaves the element tree
    /// intact. Selectors 4 :empty is relevant only when its truth value changes;
    /// directionality/state and shadow distribution keep the broad fallback.
    pub(super) fn touch_text(&mut self, parent: NodeId, empty_changed: bool) {
        let independent = self.shadow_roots.is_empty() && {
            let index = self.style_cache.borrow();
            index.as_ref().is_some_and(|(epoch, index)| {
                *epoch == self.style_epoch
                    && !index.selector_dependencies.text_state
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
            self.layout_cache.get_mut().clear();
            self.box_tree_cache.get_mut().clear();
        }
        self.invalidate_layout_ancestors(parent);
        self.mark_dom_revision();
        self.dirty_nodes.push((parent, DirtyKind::Content));
        self.record_geometry_dirty(parent, DirtyKind::Content);
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
