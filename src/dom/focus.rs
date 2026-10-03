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

    pub(crate) fn focused_area(&self, document: NodeId) -> Option<NodeId> {
        self.focused_areas.get(&document).copied()
    }

    pub(crate) fn focus_css_affects_rendering(&self) -> bool {
        self.style_index().has_focus_rules
    }

    /// HTML #focus-update-steps and Selectors 4 #the-focus-pseudo /
    /// #the-focus-within-pseudo (local HTML e5071a20, CSSWG 81c27f68).
    /// Focus can restyle descendants, siblings, :has() subjects and shadow
    /// scopes. Until there is a dependency proof, invalidate the whole render
    /// while retaining the parsed sheet index. Repeated focus is a no-op.
    pub(crate) fn set_focused_area(&mut self, document: NodeId, target: Option<NodeId>) {
        let target = target.filter(|&node| {
            self.is_valid(node)
                && self.nodes[node].owner_document == document
                && self.is_connected(node)
        });
        if self.focused_area(document) == target {
            return;
        }
        if let Some(node) = target {
            self.focused_areas.insert(document, node);
        } else {
            self.focused_areas.remove(&document);
        }
        if self.focus_css_affects_rendering() {
            self.touch();
        }
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
        // Unlike the layout flat iterator, retain slot elements: each is a
        // focus scope owner, even when it generates no box of its own.
        let mut walk = self.children(DOCUMENT);
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
            let parent = self.parent_flat(node).unwrap_or(DOCUMENT);
            let owner = owners.get(&parent).copied().unwrap_or(DOCUMENT);
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
        let mut pending = scopes.remove(&DOCUMENT).unwrap_or_default();
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
    c.states
        .iter()
        .any(|s| matches!(s, StatePseudo::Focus | StatePseudo::FocusWithin))
        || c.nots.iter().flatten().any(complex_uses_focus)
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
}
