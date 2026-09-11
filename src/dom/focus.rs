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
    pub(crate) fn sequential_focus_order(
        &self,
        boxes: &std::collections::HashMap<NodeId, crate::layout2::PxRect>,
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
            let focusable = (index.is_some() || default)
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

#[cfg(test)]
mod tests {
    use super::*;

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
