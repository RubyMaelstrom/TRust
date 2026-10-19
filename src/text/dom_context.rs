//! CSS Text 4 #text-transform-mapping and HTML #rendered-text-collection-steps
//! (CSSWG 81c27f686901, WHATWG HTML e5071a20c856). Reading innerText on an
//! inline subtree still uses its surrounding inline text for conditional
//! casing. Floats and positioned boxes neither contribute nor split words;
//! atomic inline boxes and block boundaries do split them.

use std::collections::HashMap;

use super::transform::Context;
use crate::dom::{Dom, NodeData, NodeId};

pub(crate) struct DomTextContexts {
    contexts: Vec<Context>,
    nodes: HashMap<NodeId, (usize, usize)>,
}

impl DomTextContexts {
    pub fn new(dom: &Dom, nodes: impl Iterator<Item = NodeId>) -> Self {
        let mut roots = std::collections::HashSet::new();
        let mut result = Self {
            contexts: Vec::new(),
            nodes: HashMap::new(),
        };
        for node in nodes {
            let Some(mut root) = dom.parent_flat(node) else {
                continue;
            };
            while inline(dom, root) && !out_of_flow(dom, root) {
                let Some(parent) = dom.parent_flat(root) else {
                    break;
                };
                root = parent;
            }
            if !roots.insert(root) {
                continue;
            }
            let mut text = String::new();
            result.collect(dom, root, root, &mut text);
            result.contexts.push(Context::new(&text));
        }
        result
    }

    pub fn get(&self, node: NodeId) -> Option<(&Context, usize)> {
        self.nodes
            .get(&node)
            .map(|&(context, offset)| (&self.contexts[context], offset))
    }

    fn collect(&mut self, dom: &Dom, node: NodeId, root: NodeId, text: &mut String) {
        if let NodeData::Text(data) = &dom.node(node).data {
            let parent = dom.parent_flat(node).unwrap_or(root);
            let whitespace = dom
                .computed_value_resolved(parent, "white-space")
                .unwrap_or_else(|| "normal".into());
            self.nodes.insert(node, (self.contexts.len(), text.len()));
            text.push_str(&normalize(data, &whitespace));
            return;
        }
        if dom.effective_display(node).as_deref() == Some("none") {
            return;
        }
        if node != root {
            if out_of_flow(dom, node) {
                return;
            }
            if dom.tag_name(node) == Some("br") {
                text.push('\n');
                return;
            }
            if !inline(dom, node) {
                text.push('\u{fffc}');
                return;
            }
        }
        for child in dom.flat_children(node) {
            self.collect(dom, child, root, text);
        }
    }
}

fn inline(dom: &Dom, node: NodeId) -> bool {
    !matches!(
        dom.tag_name(node),
        Some(
            "img"
                | "input"
                | "textarea"
                | "select"
                | "video"
                | "audio"
                | "canvas"
                | "iframe"
                | "object"
                | "embed"
                | "svg"
        )
    ) && matches!(
        dom.effective_display(node).as_deref(),
        None | Some("inline" | "contents" | "ruby" | "ruby-base" | "ruby-text")
    )
}

fn out_of_flow(dom: &Dom, node: NodeId) -> bool {
    dom.computed_value_resolved(node, "position")
        .is_some_and(|v| matches!(v.as_str(), "absolute" | "fixed"))
        || dom
            .computed_value_resolved(node, "float")
            .is_some_and(|v| v != "none")
}

// Match whitespace phase one in renderedTextCollection, before the host
// call. Keeping the same bytes gives Context exact offsets after collapsing.
fn normalize(text: &str, whitespace: &str) -> String {
    if !matches!(whitespace, "normal" | "nowrap" | "pre-line") {
        return text.into();
    }
    let text = if whitespace == "pre-line" {
        text.replace("\r\n", "\n").replace('\r', "\n")
    } else {
        text.into()
    };
    let mut out = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        let space = |c| {
            if whitespace == "pre-line" {
                matches!(c, ' ' | '\t')
            } else {
                matches!(c, ' ' | '\t' | '\n' | '\r' | '\u{c}')
            }
        };
        if space(c) {
            while chars.peek().is_some_and(|c| space(*c)) {
                chars.next();
            }
            if whitespace != "pre-line" || (!out.ends_with('\n') && chars.peek() != Some(&'\n')) {
                out.push(' ');
            }
        } else {
            out.push(c);
        }
    }
    out
}
