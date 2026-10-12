//! HTML #tabindex-value and #sequential-focus-navigation: keyboard targets
//! come from focus navigation scopes, never CSS paint/visual order or click
//! listeners. Positive tabindex values sort inside their own scope before
//! zero/default values; shadow hosts and slots flatten their nested scopes.

use super::*;

fn tabindex(raw: &str) -> Option<i32> {
    let raw = raw.trim_start_matches(['\t', '\n', '\u{000c}', '\r', ' ']);
    let digits = raw.strip_prefix(['+', '-']).unwrap_or(raw);
    let count = digits.bytes().take_while(u8::is_ascii_digit).count();
    (count > 0)
        .then(|| raw[..raw.len() - digits.len() + count].parse().ok())
        .flatten()
}

impl Dom {
    pub(crate) fn has_tabindex(&self, node: NodeId) -> bool {
        self.attr(node, "tabindex").and_then(tabindex).is_some()
    }

    /// The `tabindex` attribute's value under HTML's rules for parsing
    /// integers, when it has one.
    pub(crate) fn tabindex_value(&self, node: NodeId) -> Option<i32> {
        self.attr(node, "tabindex").and_then(tabindex)
    }

    pub(crate) fn focused_area(&self, document: NodeId) -> Option<NodeId> {
        self.focused_areas.get(&document).copied()
    }

    pub(crate) fn focus_css_affects_rendering(&self) -> bool {
        self.style_index().has_focus_rules
    }

    /// HTML #focus-update-steps and Selectors 4 #the-focus-pseudo /
    /// #the-focus-within-pseudo (local HTML e5071a20, CSSWG 81c27f68).
    /// Focus can restyle descendants, siblings, :has() subjects and shadow
    /// scopes. The elements whose state changes are known exactly, so the
    /// selector dependency routes, including the `:host()` and `::slotted()`
    /// routes across shadow trees, name the subjects to restyle. Repeated
    /// focus is a no-op.
    pub(crate) fn set_focused_area(&mut self, document: NodeId, target: Option<NodeId>) {
        let target = target.filter(|&node| {
            self.is_valid(node)
                && self.nodes[node].owner_document == document
                && self.is_connected(node)
        });
        let old = self.focused_area(document);
        if old == target {
            return;
        }
        if let Some(node) = target {
            self.focused_areas.insert(document, node);
        } else {
            self.focused_areas.remove(&document);
        }
        let changed = self.focus_state_changes(old, target);
        // Selectors 4 #the-focus-visible-pseudo: a focus moved by keyboard
        // (or into a control that takes keyboard input) indicates focus;
        // one moved by a pointing device does not. The flag outlives a
        // focus move to the viewport, for `focus()` to consult.
        if let Some(target) = target {
            let indicated = !self.pointer_modality() || self.supports_keyboard_input(target);
            self.update_control_state(document, |state| state.focus_indicated = indicated);
        }
        // Like the rendering check, consult a current rule index: without
        // focus rules a change needs no invalidation at all.
        let index = self.style_index();
        if !self.invalidate_live_state(&index, &changed) {
            self.touch();
        }
    }

    /// HTML #system-focus: the arena's top-level traversable gained or
    /// lost it. Each Document keeps its focused area; only what the focus
    /// selectors match changes, and exactly those elements restyle.
    pub(crate) fn set_system_focus(&mut self, focused: bool) {
        if self.system_focus == focused {
            return;
        }
        // The elements matching while focused: their state flips.
        let areas: Vec<NodeId> = self.focused_areas.values().copied().collect();
        self.system_focus = true;
        let mut changed: Vec<_> = areas
            .into_iter()
            .flat_map(|area| self.focus_state_changes(Some(area), None))
            .collect();
        changed.sort_unstable_by_key(|&(node, state)| (node, state as u8));
        changed.dedup();
        let visible: Vec<NodeId> = self
            .focused_areas
            .iter()
            .filter(|&(&document, _)| self.focus_indicated(document))
            .map(|(_, &area)| area)
            .collect();
        changed.extend(
            visible
                .into_iter()
                .map(|area| (area, invalidation::LiveState::Focus)),
        );
        self.system_focus = focused;
        let index = self.style_index();
        if changed.is_empty() || !self.invalidate_live_state(&index, &changed) {
            self.touch();
        }
    }

    fn pointer_modality(&self) -> bool {
        self.element_states
            .get(&self.document())
            .is_some_and(|state| state.pointer_modality)
    }

    /// Selectors 4 #the-focus-visible-pseudo: an element that supports
    /// keyboard input (one that would show a virtual keyboard) indicates
    /// focus however it was focused.
    fn supports_keyboard_input(&self, id: NodeId) -> bool {
        match self.tag_name(id) {
            Some("textarea") => true,
            Some("input") => !matches!(
                self.input_type(id).as_str(),
                "checkbox"
                    | "radio"
                    | "button"
                    | "submit"
                    | "reset"
                    | "image"
                    | "range"
                    | "color"
                    | "file"
                    | "hidden"
            ),
            _ => self
                .style_view()
                .read_write(id, self.tag_name(id).unwrap_or("")),
        }
    }

    /// Whether `document`'s focused area indicates focus.
    pub(crate) fn focus_indicated(&self, document: NodeId) -> bool {
        self.element_states
            .get(&document)
            .is_some_and(|state| state.focus_indicated)
    }

    /// Set whether `document`'s focused area indicates focus (HTML
    /// #dom-focus step 3's `focusVisible`), restyling `:focus-visible`.
    pub(crate) fn set_focus_indicated(&mut self, document: NodeId, indicated: bool) {
        if !self.is_valid(document) || self.focus_indicated(document) == indicated {
            return;
        }
        self.update_control_state(document, |state| state.focus_indicated = indicated);
        if let Some(focused) = self.focused_area(document) {
            let index = self.style_index();
            if !self.invalidate_live_state(&index, &[(focused, invalidation::LiveState::Focus)]) {
                self.touch();
            }
        }
    }

    /// The user's latest interaction modality for the `:focus-visible`
    /// heuristics: keyboard use makes the current focus indicated; pointer
    /// use affects the focus moves that follow.
    pub(crate) fn set_input_modality(&mut self, keyboard: bool) {
        if self.pointer_modality() == keyboard {
            self.update_control_state(self.document(), |state| state.pointer_modality = !keyboard);
        }
        if keyboard {
            let documents: Vec<NodeId> = self.focused_areas.keys().copied().collect();
            for document in documents {
                self.set_focus_indicated(document, true);
            }
        }
    }

    /// The elements whose :focus or :focus-within match differs between the
    /// focused areas `old` and `new` of one Document (see `matches_focus`
    /// and `matches_focus_within`): :focus holds for the focused area and
    /// its shadow-including hosts, :focus-within for its flat-tree inclusive
    /// ancestors, and neither when the area is a navigable container.
    /// Ancestors common to both chains keep their state.
    fn focus_state_changes(
        &self,
        old: Option<NodeId>,
        new: Option<NodeId>,
    ) -> Vec<(NodeId, invalidation::LiveState)> {
        use invalidation::LiveState;
        let chains = |focused: Option<NodeId>| {
            let mut focus = FxHashSet::default();
            let mut within = FxHashSet::default();
            let Some(focused) = focused.filter(|&node| {
                self.is_valid(node) && !matches!(self.tag_name(node), Some("iframe" | "frame"))
            }) else {
                return (focus, within);
            };
            let mut current = Some(focused);
            while let Some(node) = current {
                focus.insert(node);
                current = self.shadow_hosts.get(&self.tree_scope(node)).copied();
            }
            let mut current = Some(focused);
            while let Some(node) = current {
                if self.tag_name(node).is_some() {
                    within.insert(node);
                }
                current = self.parent_flat(node);
            }
            (focus, within)
        };
        let (old_focus, old_within) = chains(old);
        let (new_focus, new_within) = chains(new);
        let mut changed: Vec<_> = old_focus
            .symmetric_difference(&new_focus)
            .map(|&node| (node, LiveState::Focus))
            .chain(
                old_within
                    .symmetric_difference(&new_within)
                    .map(|&node| (node, LiveState::FocusWithin)),
            )
            .collect();
        changed.sort_unstable_by_key(|&(node, state)| (node, state as u8));
        changed
    }

    /// Removal moves focus to the viewport. Clear before unlinking so an
    /// immediate reinsert cannot resurrect focus or cached ancestor styles.
    pub(super) fn clear_focus_in_subtree(&mut self, root: NodeId) {
        if self.focused_areas.is_empty() {
            return;
        }
        let removed: Vec<_> = self
            .focused_areas
            .iter()
            .filter_map(|(&doc, &node)| {
                let mut current = Some(node);
                while let Some(node) = current {
                    if node == root {
                        return Some(doc);
                    }
                    current = self.parent_composed(node);
                }
                None
            })
            .collect();
        for document in removed {
            self.set_focused_area(document, None);
        }
    }

    #[cfg(test)]
    fn matches_focus(&self, id: NodeId) -> bool {
        self.style_view().matches_focus(id)
    }

    #[cfg(test)]
    fn matches_focus_within(&self, id: NodeId) -> bool {
        self.style_view().matches_focus_within(id)
    }

    #[cfg(test)]
    pub(crate) fn sequential_focus_order(
        &self,
        boxes: &std::collections::HashMap<NodeId, crate::layout2::PxRect>,
    ) -> Vec<NodeId> {
        self.sequential_focus_order_with_subwidgets(boxes, &Default::default())
    }

    /// HTML #focusable-area: UA subwidgets use their element as DOM anchor,
    /// including its tabindex, inert ancestors, and shadow focus scope.
    pub(crate) fn sequential_focus_order_with_subwidgets(
        &self,
        boxes: &std::collections::HashMap<NodeId, crate::layout2::PxRect>,
        subwidgets: &std::collections::HashSet<NodeId>,
    ) -> Vec<NodeId> {
        let mut owners = FxHashMap::default();
        let mut scopes: FxHashMap<NodeId, Vec<(NodeId, i32, bool)>> = FxHashMap::default();
        let mut inert = std::collections::HashSet::new();
        // HTML #modal-dialogs-and-inert-subtrees: while a document is blocked
        // by a modal dialog, only that dialog's flat-tree subtree is focusable.
        let blocked = self.any_document_blocked();
        // Unlike the layout flat iterator, retain slot elements: each is a
        // focus scope owner, even when it generates no box of its own.
        let mut walk = self.children(self.document());
        walk.reverse();
        while let Some(node) = walk.pop() {
            let children = if let Some(root) = self.shadow_root(node) {
                self.children(root)
            } else if self.tag_name(node) == Some("slot") {
                let assigned = self.slot_assigned_nodes(node);
                if assigned.is_empty() {
                    self.children(node)
                } else {
                    assigned
                }
            } else {
                self.children(node)
            };
            walk.extend(children.into_iter().rev());
            let Some(tag) = self.tag_name(node) else {
                continue;
            };
            let parent = self.parent_flat(node).unwrap_or(self.document());
            let owner = owners.get(&parent).copied().unwrap_or(self.document());
            let scope = self.shadow_root(node).is_some() || tag == "slot";
            owners.insert(node, if scope { node } else { owner });
            if self.attr(node, "inert").is_some() || inert.contains(&parent) {
                inert.insert(node);
                continue;
            }
            let index = self.attr(node, "tabindex").and_then(tabindex);
            if index.is_some_and(|index| index < 0) {
                continue;
            }
            let default = match tag {
                "a" | "area" => self.attr(node, "href").is_some(),
                "button" | "select" | "textarea" | "iframe" | "frame" | "object" => true,
                "input" => !self
                    .attr(node, "type")
                    .is_some_and(|ty| ty.eq_ignore_ascii_case("hidden")),
                "summary" => {
                    self.tag_name(parent) == Some("details")
                        && self
                            .child_iter(parent)
                            .find(|&child| self.tag_name(child) == Some("summary"))
                            == Some(node)
                }
                _ => self.is_contenteditable_host(node),
            };
            let focusable = (index.is_some() || default || subwidgets.contains(&node))
                && !(blocked && self.is_inert(node))
                && !self.actually_disabled(node, tag)
                && boxes.contains_key(&node)
                && !self.visibility_hidden(node)
                && !self.is_hidden(node);
            if focusable || scope {
                scopes
                    .entry(owner)
                    .or_default()
                    .push((node, index.unwrap_or(0), focusable));
            }
        }
        for entries in scopes.values_mut() {
            // Stable sort preserves tree order for equal/default values.
            entries.sort_by_key(|&(_, index, _)| if index > 0 { (0, index) } else { (1, 0) });
        }
        let mut pending = scopes.remove(&self.document()).unwrap_or_default();
        pending.reverse();
        let mut result = Vec::new();
        while let Some((node, _, focusable)) = pending.pop() {
            if focusable {
                result.push(node);
            }
            if let Some(children) = scopes.remove(&node) {
                pending.extend(children.into_iter().rev());
            }
        }
        result
    }
}

pub(super) fn complex_uses_focus(complex: &Complex) -> bool {
    complex.0.iter().any(|(_, c)| compound_uses_focus(c))
}

fn compound_uses_focus(c: &Compound) -> bool {
    c.states.iter().any(|s| {
        matches!(
            s,
            StatePseudo::Focus | StatePseudo::FocusWithin | StatePseudo::FocusVisible
        )
    }) || c.nots.iter().flatten().any(complex_uses_focus)
        || c.selects
            .iter()
            .any(|(group, _)| group.iter().any(complex_uses_focus))
        || c.has
            .iter()
            .flatten()
            .any(|arg| complex_uses_focus(&arg.complex))
        || c.structural.iter().any(|s| match s {
            Structural::Nth { of: Some(of), .. } => of.iter().any(complex_uses_focus),
            _ => false,
        })
        || c.host_inner.as_deref().is_some_and(compound_uses_focus)
        || c.slotted.as_deref().is_some_and(compound_uses_focus)
}

impl StyleView<'_> {
    /// HTML #selector-focus additionally matches shadow hosts, but excludes
    /// navigable containers. Ordinary parents do not inherit :focus.
    pub(super) fn matches_focus(&self, id: NodeId) -> bool {
        if !self.state.system_focus {
            return false;
        }
        let focused = self
            .state
            .focused_areas
            .get(&self.nodes.owner_document(id))
            .copied();
        if focused.is_some_and(|node| matches!(self.tag_name(node), Some("iframe" | "frame"))) {
            return false;
        }
        let mut current = focused;
        while let Some(node) = current {
            if node == id {
                return true;
            }
            current = self.state.shadow_hosts.get(&self.tree_scope(node)).copied();
        }
        false
    }

    /// Selectors 4 #the-focus-visible-pseudo: the focused element itself,
    /// while its Document indicates focus.
    pub(super) fn matches_focus_visible(&self, id: NodeId) -> bool {
        let document = self.nodes.owner_document(id);
        self.state.system_focus
            && self.state.focused_areas.get(&document) == Some(&id)
            && !matches!(self.tag_name(id), Some("iframe" | "frame"))
            && self
                .state
                .element_states
                .get(&document)
                .is_some_and(|state| state.focus_indicated)
    }

    pub(super) fn matches_focus_within(&self, id: NodeId) -> bool {
        let Some(focused) = self
            .state
            .focused_areas
            .get(&self.nodes.owner_document(id))
            .copied()
        else {
            return false;
        };
        if !self.matches_focus(focused) {
            return false;
        }
        let mut current = Some(focused);
        while let Some(node) = current {
            if node == id {
                return true;
            }
            current = self.parent_flat(node);
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn focus_on_a_navigable_container_does_not_style_its_shadow_host() {
        let mut dom = Dom::parse_document("<x-host id=host></x-host>");
        let host = dom.get_by_id("host").unwrap();
        let root = dom.attach_shadow(host);
        let frame = dom.create_element("iframe");
        dom.append(root, frame);
        dom.set_focused_area(DOCUMENT, Some(frame));
        for node in [frame, host] {
            assert!(!dom.matches_focus(node));
            assert!(!dom.matches_focus_within(node));
        }
        let button = dom.create_element("button");
        dom.append(root, button);
        dom.set_focused_area(DOCUMENT, Some(button));
        assert!(dom.matches_focus(host));
        assert!(dom.matches_focus_within(host));
    }

    fn order(dom: &Dom) -> Vec<String> {
        let boxes = dom
            .flat_descendants(DOCUMENT)
            .into_iter()
            .map(|node| {
                (
                    node,
                    crate::layout2::PxRect {
                        left: 0.0,
                        top: 0.0,
                        width: 10.0,
                        height: 10.0,
                        css_width: None,
                        css_height: None,
                    },
                )
            })
            .collect();
        dom.sequential_focus_order(&boxes)
            .into_iter()
            .map(|node| dom.attr(node, "id").unwrap().to_string())
            .collect()
    }

    #[test]
    fn sequential_focus_ignores_pointer_wrappers_and_visual_order() {
        let dom = Dom::parse_document(
            r#"<input id=email><div onclick='x()' style='position:absolute;top:0'>
            <input id=password><button id=eye></button></div>
            <button id=submit></button><div id=early tabindex=' +2suffix'></div>
            <div id=first tabindex=1></div><button id=skip tabindex=-1></button>
            <button disabled></button><input type=hidden><div inert><input></div>
            <fieldset disabled><legend><input id=legend></legend><input></fieldset>
            <input id=readonly readonly><a>no href</a><a id=link href='/'>link</a>"#,
        );
        assert_eq!(
            order(&dom),
            [
                "first", "early", "email", "password", "eye", "submit", "legend", "readonly",
                "link"
            ]
        );
    }

    #[test]
    fn sequential_focus_flattens_shadow_and_slot_scopes() {
        let mut dom = Dom::parse_document(
            "<input id=before><x-host id=host><input id=slotted tabindex=1></x-host><input id=after tabindex=2>",
        );
        let host = dom.get_by_id("host").unwrap();
        let root = dom.attach_shadow(host);
        for (tag, id, index) in [
            ("input", "shadow", "3"),
            ("slot", "slot", ""),
            ("input", "last", ""),
        ] {
            let node = dom.create_element(tag);
            dom.set_attr(node, "id", id);
            if !index.is_empty() {
                dom.set_attr(node, "tabindex", index);
            }
            dom.append(root, node);
        }
        assert_eq!(
            order(&dom),
            ["after", "before", "shadow", "slotted", "last"]
        );
        dom.set_attr(host, "tabindex", "-1");
        assert_eq!(order(&dom), ["after", "before"]);
    }

    #[test]
    fn focus_visible_follows_the_input_modality() {
        // Selectors 4 #the-focus-visible-pseudo heuristics: keyboard focus
        // and keyboard-input controls indicate focus, pointer focus on other
        // elements does not, and keyboard use indicates the current focus.
        let mut dom = Dom::parse_document(
            "<style>:focus-visible{color:red}</style><button id=b>b</button>\
             <input id=t><input id=c type=checkbox><a id=l href=#>l</a>",
        );
        let red = |dom: &Dom, id: &str| {
            dom.computed_value_resolved(dom.get_by_id(id).unwrap(), "color")
                .as_deref()
                == Some("red")
        };
        let focus = |dom: &mut Dom, id: &str| {
            let node = dom.get_by_id(id).unwrap();
            dom.set_focused_area(DOCUMENT, Some(node));
        };
        // No pointer interaction yet: focus is indicated.
        focus(&mut dom, "b");
        assert!(red(&dom, "b") && dom.matches_focus(dom.get_by_id("b").unwrap()));
        dom.set_input_modality(false);
        focus(&mut dom, "c");
        assert!(!red(&dom, "c") && !red(&dom, "b"));
        focus(&mut dom, "t");
        assert!(red(&dom, "t"), "a text input supports keyboard input");
        focus(&mut dom, "l");
        assert!(!red(&dom, "l"));
        dom.set_input_modality(true);
        assert!(red(&dom, "l"), "keyboard use indicates the current focus");
        dom.set_focus_indicated(DOCUMENT, false);
        assert!(!red(&dom, "l"));
        dom.set_focused_area(DOCUMENT, None);
        assert!(!red(&dom, "l"));
        let parsed = SelectorList::parse(":focus-visible").unwrap();
        assert_eq!(parsed.0[0].specificity(), (0, 1, 0));
    }
}
