//! The live, scriptable document: an arena DOM built straight from
//! html5ever, mutated by page JavaScript (through js.rs), then either
//! laid out into rows directly (layout.rs) or serialized back to HTML
//! for the app to re-parse and lay out.
//!
//! Deliberately NOT rcdom: a mutable DOM can't live with rcdom's
//! Node::drop force-clearing children (see CLAUDE.md), and an arena of
//! indices gives JS a flat, GC-free handle type — wrappers hold a
//! `NodeId`, the whole arena drops with the page.

use std::borrow::Cow;
use std::cell::{Ref, RefCell};

use html5ever::interface::{ElementFlags, NodeOrText, QuirksMode, TreeSink};
use html5ever::tendril::{StrTendril, TendrilSink};
use html5ever::{Attribute, ParseOpts, QualName, ns};

pub type NodeId = usize;

pub enum NodeData {
    Document,
    /// A document fragment: template contents, fragment-parse roots.
    Fragment,
    Doctype,
    Comment(String),
    Text(String),
    Element {
        name: QualName,
        attrs: Vec<Attribute>,
        /// `<template>` parses its children into a separate fragment.
        template_contents: Option<NodeId>,
    },
}

pub struct Node {
    pub parent: Option<NodeId>,
    pub first_child: Option<NodeId>,
    pub last_child: Option<NodeId>,
    pub prev_sibling: Option<NodeId>,
    pub next_sibling: Option<NodeId>,
    pub data: NodeData,
}

/// Elements that close themselves in HTML serialization.
const VOID_ELEMENTS: [&str; 14] = [
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr",
];

pub struct Dom {
    nodes: Vec<Node>,
    /// host element → shadow root fragment (attachShadow).
    shadow_roots: std::collections::HashMap<NodeId, NodeId>,
    /// and the reverse: shadow root fragment → host element.
    shadow_hosts: std::collections::HashMap<NodeId, NodeId>,
    /// Set by every tree/attribute mutation; the living page takes it
    /// to decide whether a dispatch warrants re-extraction at all.
    dirty: bool,
    /// Monotonic mutation counter (bumped with `dirty`); keys the
    /// cached visibility cascade so it rebuilds only after changes.
    epoch: u64,
    /// adoptedStyleSheets text per scope (DOCUMENT or a shadow root
    /// fragment), pushed by the prelude on adoption/replaceSync.
    adopted_styles: std::collections::HashMap<NodeId, String>,
    /// Fetched `<link rel=stylesheet>` text, keyed by the link element.
    external_sheets: std::collections::HashMap<NodeId, String>,
    /// Lazily built visibility cascade, valid for one epoch.
    style_cache: RefCell<Option<(u64, std::rc::Rc<StyleIndex>)>>,
}

/// The document node is always index 0.
pub const DOCUMENT: NodeId = 0;

impl Default for Dom {
    fn default() -> Self {
        Self::new()
    }
}

impl Dom {
    pub fn new() -> Self {
        let mut dom = Dom {
            nodes: Vec::new(),
            shadow_roots: std::collections::HashMap::new(),
            shadow_hosts: std::collections::HashMap::new(),
            dirty: false,
            epoch: 0,
            adopted_styles: std::collections::HashMap::new(),
            external_sheets: std::collections::HashMap::new(),
            style_cache: RefCell::new(None),
        };
        dom.new_node(NodeData::Document);
        dom
    }

    /// True when anything mutated since the last call; resets the flag.
    pub fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    /// Every mutation comes through here: the dirty bit for the living
    /// page, the epoch for the cached visibility cascade.
    fn touch(&mut self) {
        self.dirty = true;
        self.epoch = self.epoch.wrapping_add(1);
    }

    /// Parse a full HTML document into a fresh arena.
    pub fn parse_document(html: &str) -> Self {
        let sink = Sink {
            dom: RefCell::new(Dom::new()),
        };
        html5ever::parse_document(sink, ParseOpts::default()).one(StrTendril::from(html))
    }

    fn new_node(&mut self, data: NodeData) -> NodeId {
        self.nodes.push(Node {
            parent: None,
            first_child: None,
            last_child: None,
            prev_sibling: None,
            next_sibling: None,
            data,
        });
        self.nodes.len() - 1
    }

    pub fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id]
    }

    pub fn is_valid(&self, id: NodeId) -> bool {
        id < self.nodes.len()
    }

    pub fn create_element(&mut self, tag: &str) -> NodeId {
        let tag = tag.to_ascii_lowercase();
        // Script-created templates need their content fragment exactly
        // like parser-created ones (Lit renders through them).
        let template_contents = (tag == "template").then(|| self.new_node(NodeData::Fragment));
        let name = QualName::new(None, ns!(html), tag.into());
        self.new_node(NodeData::Element {
            name,
            attrs: Vec::new(),
            template_contents,
        })
    }

    pub fn create_text(&mut self, text: &str) -> NodeId {
        self.new_node(NodeData::Text(text.to_string()))
    }

    pub fn create_fragment(&mut self) -> NodeId {
        self.new_node(NodeData::Fragment)
    }

    pub fn create_comment(&mut self, text: &str) -> NodeId {
        self.new_node(NodeData::Comment(text.to_string()))
    }

    /// Unlink a node from its parent and siblings (the node and its
    /// subtree stay in the arena; arenas only ever grow — page-lifetime
    /// memory is the deal).
    pub fn detach(&mut self, id: NodeId) {
        self.touch();
        let (parent, prev, next) = {
            let n = &self.nodes[id];
            (n.parent, n.prev_sibling, n.next_sibling)
        };
        if let Some(prev) = prev {
            self.nodes[prev].next_sibling = next;
        }
        if let Some(next) = next {
            self.nodes[next].prev_sibling = prev;
        }
        if let Some(parent) = parent {
            if self.nodes[parent].first_child == Some(id) {
                self.nodes[parent].first_child = next;
            }
            if self.nodes[parent].last_child == Some(id) {
                self.nodes[parent].last_child = prev;
            }
        }
        let n = &mut self.nodes[id];
        n.parent = None;
        n.prev_sibling = None;
        n.next_sibling = None;
    }

    pub fn append(&mut self, parent: NodeId, child: NodeId) {
        self.touch();
        self.detach(child);
        let old_last = self.nodes[parent].last_child;
        self.nodes[child].parent = Some(parent);
        self.nodes[child].prev_sibling = old_last;
        if let Some(last) = old_last {
            self.nodes[last].next_sibling = Some(child);
        } else {
            self.nodes[parent].first_child = Some(child);
        }
        self.nodes[parent].last_child = Some(child);
    }

    /// Insert `child` under `parent` immediately before `reference`;
    /// append when `reference` is None (DOM insertBefore semantics).
    pub fn insert_before(&mut self, parent: NodeId, child: NodeId, reference: Option<NodeId>) {
        let Some(reference) = reference else {
            self.append(parent, child);
            return;
        };
        if self.nodes[reference].parent != Some(parent) {
            // A real DOM throws NotFoundError; tolerate with an append.
            self.append(parent, child);
            return;
        }
        self.touch();
        self.detach(child);
        let prev = self.nodes[reference].prev_sibling;
        self.nodes[child].parent = Some(parent);
        self.nodes[child].prev_sibling = prev;
        self.nodes[child].next_sibling = Some(reference);
        self.nodes[reference].prev_sibling = Some(child);
        match prev {
            Some(prev) => self.nodes[prev].next_sibling = Some(child),
            None => self.nodes[parent].first_child = Some(child),
        }
    }

    /// Append text, merging into a trailing text node like a parser would.
    pub fn append_text(&mut self, parent: NodeId, text: &str) {
        if let Some(last) = self.nodes[parent].last_child
            && let NodeData::Text(existing) = &mut self.nodes[last].data
        {
            existing.push_str(text);
            self.touch();
            return;
        }
        let t = self.create_text(text);
        self.append(parent, t);
    }

    pub fn children(&self, id: NodeId) -> Vec<NodeId> {
        let mut out = Vec::new();
        let mut next = self.nodes[id].first_child;
        while let Some(c) = next {
            out.push(c);
            next = self.nodes[c].next_sibling;
        }
        out
    }

    /// The subtree under `root` in document order, excluding `root`.
    pub fn descendants(&self, root: NodeId) -> Vec<NodeId> {
        let mut out = Vec::new();
        let mut stack: Vec<NodeId> = self.children(root);
        stack.reverse();
        while let Some(id) = stack.pop() {
            out.push(id);
            let mut kids = self.children(id);
            kids.reverse();
            stack.extend(kids);
        }
        out
    }

    pub fn tag_name(&self, id: NodeId) -> Option<&str> {
        match &self.nodes[id].data {
            NodeData::Element { name, .. } => Some(&name.local),
            _ => None,
        }
    }

    pub fn attr(&self, id: NodeId, name: &str) -> Option<&str> {
        match &self.nodes[id].data {
            NodeData::Element { attrs, .. } => attrs
                .iter()
                .find(|a| str::eq_ignore_ascii_case(&a.name.local, name))
                .map(|a| &*a.value),
            _ => None,
        }
    }

    pub fn set_attr(&mut self, id: NodeId, name: &str, value: &str) {
        if let NodeData::Element { attrs, .. } = &mut self.nodes[id].data {
            let name = name.to_ascii_lowercase();
            if let Some(a) = attrs.iter_mut().find(|a| *a.name.local == name) {
                // Idempotent writes are free: no dirty, no redraw.
                if *a.value == *value {
                    return;
                }
                a.value = StrTendril::from(value);
            } else {
                attrs.push(Attribute {
                    name: QualName::new(None, ns!(), name.into()),
                    value: StrTendril::from(value),
                });
            }
            self.touch();
        }
    }

    pub fn remove_attr(&mut self, id: NodeId, name: &str) {
        self.touch();
        if let NodeData::Element { attrs, .. } = &mut self.nodes[id].data {
            attrs.retain(|a| !str::eq_ignore_ascii_case(&a.name.local, name));
        }
    }

    pub fn attr_names(&self, id: NodeId) -> Vec<String> {
        match &self.nodes[id].data {
            NodeData::Element { attrs, .. } => {
                attrs.iter().map(|a| a.name.local.to_string()).collect()
            }
            _ => Vec::new(),
        }
    }

    /// Is this element hidden — by the `hidden` attribute, or by the
    /// `display`/`visibility` mini-cascade (inline style, `<style>`
    /// elements, shadow sheets, adoptedStyleSheets, fetched `<link>`
    /// sheets)? Winner per property is the lexicographic max of
    /// (!important, inline, specificity, source order) — inline beats
    /// sheets except under !important, the real rules for a single
    /// author origin. Hidden subtrees don't render. This is visibility,
    /// not a CSS engine: no inheritance, no @-rules, two properties.
    pub fn is_hidden(&self, id: NodeId) -> bool {
        if self.attr(id, "hidden").is_some() {
            return true;
        }
        self.cascaded(id, "display").as_deref() == Some("none")
            || matches!(
                self.cascaded(id, "visibility").as_deref(),
                Some("hidden" | "collapse")
            )
    }

    /// The cascaded `display` value for an element (the mini-cascade
    /// winner), or `None` when no rule sets it. `hidden` attribute counts
    /// as `display:none`. Drives block/inline flow in the layout pass and
    /// is baked into the serialized HTML so the re-parsed layout arena
    /// sees the same computed display the engine did.
    pub fn computed_display(&self, id: NodeId) -> Option<String> {
        if self.attr(id, "hidden").is_some() {
            return Some("none".to_string());
        }
        self.cascaded(id, "display")
    }

    /// The cascaded value of any tracked property (the layout reads
    /// margin/padding/text-align through this), or `None` when unset.
    pub fn computed_style(&self, id: NodeId, prop: &str) -> Option<String> {
        self.cascaded(id, prop)
    }

    /// The mini-cascade winner for one property (`display` or
    /// `visibility` — the two the style index tracks). Inline styles beat
    /// tree rules by specificity/order, `!important` and source order
    /// resolved by `CascadeKey`.
    fn cascaded(&self, id: NodeId, prop: &str) -> Option<String> {
        let mut winner: Option<(CascadeKey, String)> = None;
        if let Some(style) = self.attr(id, "style") {
            for decl in style.split(';') {
                let Some((k, v, important)) = parse_decl(decl) else {
                    continue;
                };
                for (pk, pv) in expand_box_shorthand(&k, &v) {
                    if pk == prop {
                        consider(&mut winner, (important, true, (0, 0, 0), usize::MAX), &pv);
                    }
                }
            }
        }
        let index = self.style_index();
        if let Some(rules) = index.scopes.get(&self.tree_scope(id)) {
            for r in rules {
                // `div::before{…}` rules target a generated box, not the
                // element — skip them in the element-property cascade.
                if rule_pseudo(r).is_some() || !self.matches_complex(id, &r.selector.0) {
                    continue;
                }
                for (pk, (imp, v)) in &r.decls {
                    if pk == prop {
                        consider(&mut winner, (*imp, false, r.specificity, r.order), v);
                    }
                }
            }
        }
        winner.map(|(_, v)| v)
    }

    /// The resolved `content` text for an element's `::before`/`::after`
    /// box, or `None` when no rule sets it (or it resolves to `none`/an
    /// unsupported value like `counter()`). Reads only pseudo-element rules
    /// in the element's tree scope (inline styles can't target a pseudo).
    pub fn pseudo_content(&self, id: NodeId, which: PseudoEl) -> Option<String> {
        let index = self.style_index();
        let rules = index.scopes.get(&self.tree_scope(id))?;
        let mut winner: Option<(CascadeKey, String)> = None;
        for r in rules {
            if rule_pseudo(r) != Some(which) || !self.matches_complex(id, &r.selector.0) {
                continue;
            }
            for (pk, (imp, v)) in &r.decls {
                if pk == "content" {
                    consider(&mut winner, (*imp, false, r.specificity, r.order), v);
                }
            }
        }
        let raw = winner.map(|(_, v)| v)?;
        self.parse_content_value(id, &raw)
    }

    /// Resolve a `content` value to display text: a quoted string (with CSS
    /// `\HEX`/`\c` escapes), `attr(name)` → the element's attribute, or
    /// `none`/`normal`/unsupported (`counter()`, `url()`) → `None`.
    fn parse_content_value(&self, id: NodeId, raw: &str) -> Option<String> {
        let v = raw.trim();
        if v.is_empty() || v == "none" || v == "normal" {
            return None;
        }
        if let Some(s) = unquote_css(v) {
            return (!s.is_empty()).then_some(s);
        }
        if let Some(inner) = v.strip_prefix("attr(").and_then(|r| r.strip_suffix(')')) {
            return self.attr(id, inner.trim()).map(str::to_owned);
        }
        None
    }

    /// The root of a node's tree: DOCUMENT for the light DOM, the
    /// shadow fragment for shadow content. An element consults only its
    /// own scope's sheets (selector matching can't cross the boundary
    /// either — ancestor walks stop at fragment roots).
    fn tree_scope(&self, id: NodeId) -> NodeId {
        let mut cur = id;
        while let Some(p) = self.nodes[cur].parent {
            cur = p;
        }
        cur
    }

    /// The visibility cascade for the current epoch, built on first use
    /// after any mutation, shared until the next one.
    fn style_index(&self) -> std::rc::Rc<StyleIndex> {
        let mut cache = self.style_cache.borrow_mut();
        if let Some((epoch, idx)) = cache.as_ref()
            && *epoch == self.epoch
        {
            return idx.clone();
        }
        let idx = std::rc::Rc::new(self.build_style_index());
        *cache = Some((self.epoch, idx.clone()));
        idx
    }

    fn build_style_index(&self) -> StyleIndex {
        let mut index = StyleIndex::default();
        let mut order = 0;
        for id in self.composed_descendants(DOCUMENT) {
            let css: Cow<str> = match self.tag_name(id) {
                Some("style") => Cow::Owned(self.text_content(id)),
                Some("link") => match self.external_sheets.get(&id) {
                    Some(css) => Cow::Borrowed(css.as_str()),
                    None => continue,
                },
                _ => continue,
            };
            let scope = index.scopes.entry(self.tree_scope(id)).or_default();
            parse_sheet(&css, &mut order, scope);
        }
        // Adopted sheets cascade after their scope's tree sheets (their
        // order values are necessarily higher); cross-scope order is
        // moot — an element only reads its own scope. Sort for
        // determinism across HashMap iteration.
        let mut adopted: Vec<_> = self.adopted_styles.iter().collect();
        adopted.sort_by_key(|(scope, _)| **scope);
        for (scope, css) in adopted {
            parse_sheet(css, &mut order, index.scopes.entry(*scope).or_default());
        }
        index
    }

    /// adoptedStyleSheets text for a scope (DOCUMENT or a shadow root),
    /// pushed by the prelude on adoption and on replace/replaceSync.
    /// Idempotent pushes are free — no dirty, no rebuild.
    pub fn set_adopted_styles(&mut self, scope: NodeId, css: &str) {
        if self.adopted_styles.get(&scope).map(String::as_str) == Some(css)
            || (css.trim().is_empty() && !self.adopted_styles.contains_key(&scope))
        {
            return;
        }
        self.adopted_styles.insert(scope, css.to_string());
        self.touch();
    }

    fn is_stylesheet_link(&self, id: NodeId) -> bool {
        self.tag_name(id) == Some("link")
            && self.attr(id, "rel").is_some_and(|r| {
                r.split_ascii_whitespace()
                    .any(|w| w.eq_ignore_ascii_case("stylesheet"))
            })
    }

    /// Raw hrefs of external stylesheets, document order, so the fetch
    /// pipeline can resolve and download them before scripts run.
    pub fn stylesheet_links(&self) -> Vec<String> {
        self.descendants(DOCUMENT)
            .into_iter()
            .filter(|&id| self.is_stylesheet_link(id))
            .filter_map(|id| self.attr(id, "href").map(str::to_string))
            .collect()
    }

    /// Attach fetched `<link rel=stylesheet>` bodies (keyed by the raw
    /// href attribute) to their link elements; the cascade reads them
    /// scope-aware like any `<style>`.
    pub fn attach_external_sheets(&mut self, sheets: &[(String, String)]) {
        for (href, css) in sheets {
            let hit = self.descendants(DOCUMENT).into_iter().find(|&id| {
                !self.external_sheets.contains_key(&id)
                    && self.is_stylesheet_link(id)
                    && self.attr(id, "href") == Some(href.as_str())
            });
            if let Some(id) = hit {
                self.external_sheets.insert(id, css.clone());
                self.touch();
            }
        }
    }

    /// Attach a shadow root (a fragment) to a host element; rendering
    /// flattens it in place of the host's light children, with `<slot>`
    /// projection. Idempotent per host, like the real API isn't — pages
    /// that double-attach get the same root back rather than a throw.
    pub fn attach_shadow(&mut self, host: NodeId) -> NodeId {
        if let Some(&root) = self.shadow_roots.get(&host) {
            return root;
        }
        let root = self.create_fragment();
        self.shadow_roots.insert(host, root);
        self.shadow_hosts.insert(root, host);
        self.touch();
        root
    }

    /// Parent in the COMPOSED tree: shadow roots hand off to their host
    /// (event paths and ancestor checks cross shadow boundaries).
    pub fn parent_composed(&self, id: NodeId) -> Option<NodeId> {
        self.nodes[id]
            .parent
            .or_else(|| self.shadow_hosts.get(&id).copied())
    }

    /// Document-order walk of the COMPOSED tree: light children plus
    /// every shadow tree (interactive content hides in there).
    pub fn composed_descendants(&self, root: NodeId) -> Vec<NodeId> {
        let mut out = Vec::new();
        let mut stack: Vec<NodeId> = self.children(root);
        stack.reverse();
        while let Some(id) = stack.pop() {
            out.push(id);
            let mut kids = self.children(id);
            if let Some(shadow) = self.shadow_root(id) {
                kids.extend(self.children(shadow));
            }
            kids.reverse();
            stack.extend(kids);
        }
        out
    }

    pub fn shadow_root(&self, host: NodeId) -> Option<NodeId> {
        self.shadow_roots.get(&host).copied()
    }

    /// Where innerHTML-ish operations land: a template's content
    /// fragment, everyone else themselves.
    pub fn content_target(&self, id: NodeId) -> NodeId {
        match &self.nodes[id].data {
            NodeData::Element {
                template_contents: Some(c),
                ..
            } => *c,
            _ => id,
        }
    }

    /// The host's light children assigned to a slot (by name, or the
    /// default slot). Text nodes always belong to the default slot.
    fn slot_assigned(&self, host: NodeId, slot_name: Option<&str>) -> Vec<NodeId> {
        self.children(host)
            .into_iter()
            .filter(|&c| match (self.attr(c, "slot"), slot_name) {
                (Some(a), Some(n)) => a == n,
                (None, None) => true,
                _ => false,
            })
            .collect()
    }

    /// Concatenated descendant text (DOM textContent).
    /// A comment's data (Lit's binding markers live there).
    pub fn comment_text(&self, id: NodeId) -> Option<&str> {
        match &self.nodes[id].data {
            NodeData::Comment(t) => Some(t),
            _ => None,
        }
    }

    pub fn set_comment_text(&mut self, id: NodeId, text: &str) {
        if let NodeData::Comment(t) = &mut self.nodes[id].data
            && t != text
        {
            *t = text.to_string();
            self.touch();
        }
    }

    pub fn text_content(&self, id: NodeId) -> String {
        let mut out = String::new();
        if let NodeData::Text(t) = &self.nodes[id].data {
            return t.clone();
        }
        for d in self.descendants(id) {
            if let NodeData::Text(t) = &self.nodes[d].data {
                out.push_str(t);
            }
        }
        out
    }

    pub fn set_text(&mut self, id: NodeId, text: &str) {
        match &mut self.nodes[id].data {
            // Idempotent writes are free: no dirty, no redraw.
            NodeData::Text(t) if *t == text => (),
            NodeData::Text(t) => {
                *t = text.to_string();
                self.touch();
            }
            _ => {
                // A single-text-child rewrite to the same value is the
                // hot no-op (counters, clocks): skip it cheaply.
                let kids = self.children(id);
                if let [only] = kids[..]
                    && let NodeData::Text(t) = &self.nodes[only].data
                    && *t == text
                {
                    return;
                }
                self.touch();
                for c in kids {
                    self.detach(c);
                }
                let t = self.create_text(text);
                self.append(id, t);
            }
        }
    }

    /// Deep-copy a subtree (or a single node when `deep` is false).
    /// Template content propagates per the HTML cloning steps: a cloned
    /// template always owns a fresh content fragment, populated when
    /// deep (webcomponents-loader probes exactly this).
    pub fn clone_subtree(&mut self, id: NodeId, deep: bool) -> NodeId {
        let data = match &self.nodes[id].data {
            NodeData::Document | NodeData::Fragment => NodeData::Fragment,
            NodeData::Doctype => NodeData::Doctype,
            NodeData::Comment(t) => NodeData::Comment(t.clone()),
            NodeData::Text(t) => NodeData::Text(t.clone()),
            NodeData::Element { name, attrs, .. } => NodeData::Element {
                name: name.clone(),
                attrs: attrs.clone(),
                template_contents: None,
            },
        };
        let src_content = match &self.nodes[id].data {
            NodeData::Element {
                template_contents: Some(c),
                ..
            } => Some(*c),
            _ => None,
        };
        let copy = self.new_node(data);
        if let Some(sc) = src_content {
            let frag = self.new_node(NodeData::Fragment);
            if let NodeData::Element {
                template_contents, ..
            } = &mut self.nodes[copy].data
            {
                *template_contents = Some(frag);
            }
            if deep {
                for c in self.children(sc) {
                    let cc = self.clone_subtree(c, true);
                    self.append(frag, cc);
                }
            }
        }
        if deep {
            for c in self.children(id) {
                let cc = self.clone_subtree(c, true);
                self.append(copy, cc);
            }
        }
        copy
    }

    /// Parse an HTML snippet in the context of `parent`'s tag and return
    /// the new nodes (already transplanted into this arena, detached).
    pub fn parse_fragment_into(&mut self, context_tag: &str, html: &str) -> Vec<NodeId> {
        let sink = Sink {
            dom: RefCell::new(Dom::new()),
        };
        let context = QualName::new(None, ns!(html), context_tag.to_ascii_lowercase().into());
        let frag: Dom =
            html5ever::parse_fragment(sink, ParseOpts::default(), context, Vec::new(), false)
                .one(StrTendril::from(html));
        // The fragment's children land under <html> under the document.
        let html_el = frag
            .children(DOCUMENT)
            .into_iter()
            .find(|&c| frag.tag_name(c) == Some("html"))
            .unwrap_or(DOCUMENT);
        frag.children(html_el)
            .into_iter()
            .map(|c| self.transplant(&frag, c))
            .collect()
    }

    /// Deep-copy a subtree from another arena into this one. Template
    /// content rides along (html5ever parks template children there).
    fn transplant(&mut self, other: &Dom, id: NodeId) -> NodeId {
        let data = match &other.nodes[id].data {
            NodeData::Document | NodeData::Fragment => NodeData::Fragment,
            NodeData::Doctype => NodeData::Doctype,
            NodeData::Comment(t) => NodeData::Comment(t.clone()),
            NodeData::Text(t) => NodeData::Text(t.clone()),
            NodeData::Element { name, attrs, .. } => NodeData::Element {
                name: name.clone(),
                attrs: attrs.clone(),
                template_contents: None,
            },
        };
        let src_content = match &other.nodes[id].data {
            NodeData::Element {
                template_contents: Some(c),
                ..
            } => Some(*c),
            _ => None,
        };
        let copy = self.new_node(data);
        if let Some(sc) = src_content {
            let frag = self.new_node(NodeData::Fragment);
            if let NodeData::Element {
                template_contents, ..
            } = &mut self.nodes[copy].data
            {
                *template_contents = Some(frag);
            }
            for c in other.children(sc) {
                let cc = self.transplant(other, c);
                self.append(frag, cc);
            }
        }
        for c in other.children(id) {
            let cc = self.transplant(other, c);
            self.append(copy, cc);
        }
        copy
    }

    /// First element (document order) whose id attribute matches.
    pub fn get_by_id(&self, target: &str) -> Option<NodeId> {
        self.descendants(DOCUMENT)
            .into_iter()
            .find(|&d| self.attr(d, "id") == Some(target))
    }

    /// Serialize a subtree to HTML (for the app to re-parse and lay
    /// out). `<script>` has done its job by now and `<noscript>` means
    /// "JS didn't run" — when this serializer is called, it did — so both
    /// are dropped, as are doctypes and `<template>` (inert by
    /// definition). The cascaded `display` is baked onto each element so
    /// the re-parsed layout arena flows it the way the engine computed.
    pub fn serialize(&self, root: NodeId) -> String {
        let mut out = String::new();
        self.serialize_node(root, None, &mut out);
        out
    }

    pub fn inner_html(&self, id: NodeId) -> String {
        let mut out = String::new();
        for c in self.children(self.content_target(id)) {
            self.serialize_node(c, None, &mut out);
        }
        out
    }

    fn serialize_node(&self, id: NodeId, host: Option<NodeId>, out: &mut String) {
        match &self.nodes[id].data {
            NodeData::Document | NodeData::Fragment => {
                for c in self.children(id) {
                    self.serialize_node(c, host, out);
                }
            }
            NodeData::Doctype => {}
            // Comments survive round-trips (Lit's markers) and the
            // layout pass ignores them.
            NodeData::Comment(t) => {
                out.push_str("<!--");
                out.push_str(&t.replace("--", "- -"));
                out.push_str("-->");
            }
            NodeData::Text(t) => out.push_str(&escape_text(t)),
            NodeData::Element { name, attrs, .. } => {
                let tag: &str = &name.local;
                if matches!(tag, "script" | "noscript" | "template" | "style") || self.is_hidden(id)
                {
                    return;
                }
                // <slot> inside a shadow tree: project the host's light
                // children (or the slot's own fallback content).
                if tag == "slot"
                    && let Some(h) = host
                {
                    let assigned = self.slot_assigned(h, self.attr(id, "name"));
                    if assigned.is_empty() {
                        for c in self.children(id) {
                            self.serialize_node(c, host, out);
                        }
                    } else {
                        for c in assigned {
                            self.serialize_node(c, None, out);
                        }
                    }
                    return;
                }
                out.push('<');
                out.push_str(tag);
                self.write_attrs(id, attrs, &mut |_, _| None, out);
                out.push('>');
                if VOID_ELEMENTS.contains(&tag) {
                    return;
                }
                // A shadow root renders IN PLACE of the light children
                // (flattened — text extraction wants content, not
                // composition fidelity).
                if let Some(root) = self.shadow_root(id) {
                    for c in self.children(root) {
                        self.serialize_node(c, Some(id), out);
                    }
                } else {
                    for c in self.children(id) {
                        self.serialize_node(c, host, out);
                    }
                }
                out.push_str("</");
                out.push_str(tag);
                out.push('>');
            }
        }
    }

    /// Serialize for a LIVING page: like `serialize`, but elements in
    /// `clickable` become followable — non-anchors are wrapped in
    /// `<a href="x-trust-js:<id>:">` markers (the form-marker trick),
    /// and live anchors get their href rewritten to
    /// `x-trust-js:<id>:<original-href>` so clicks route through the
    /// page actor (which navigates only if not defaultPrevented).
    pub fn serialize_live(
        &self,
        root: NodeId,
        clickable: &std::collections::HashSet<NodeId>,
    ) -> String {
        let mut out = String::new();
        self.serialize_live_node(root, None, clickable, &mut out);
        out
    }

    fn serialize_live_node(
        &self,
        id: NodeId,
        host: Option<NodeId>,
        clickable: &std::collections::HashSet<NodeId>,
        out: &mut String,
    ) {
        let NodeData::Element { name, attrs, .. } = &self.nodes[id].data else {
            return self.serialize_node_with(
                id,
                &mut |c, o| self.serialize_live_node(c, host, clickable, o),
                out,
            );
        };
        let tag: &str = &name.local;
        if matches!(tag, "script" | "noscript" | "template" | "style") || self.is_hidden(id) {
            return;
        }
        if tag == "slot"
            && let Some(h) = host
        {
            let assigned = self.slot_assigned(h, self.attr(id, "name"));
            if assigned.is_empty() {
                for c in self.children(id) {
                    self.serialize_live_node(c, host, clickable, out);
                }
            } else {
                for c in assigned {
                    self.serialize_live_node(c, None, clickable, out);
                }
            }
            return;
        }
        let is_click = clickable.contains(&id);
        let is_anchor = tag == "a";
        if is_click && !is_anchor {
            out.push_str(&format!("<a href=\"x-trust-js:{id}:\">"));
            // An icon-only clickable would render as an empty (and so
            // unselectable) link: give it a visible label.
            if self.text_content(id).trim().is_empty() {
                let label = self
                    .attr(id, "aria-label")
                    .or_else(|| self.attr(id, "title"))
                    .or_else(|| self.attr(id, "value"))
                    .unwrap_or("button");
                out.push('[');
                out.push_str(&escape_text(label));
                out.push(']');
            }
        }
        out.push('<');
        out.push_str(tag);
        self.write_attrs(
            id,
            attrs,
            &mut |name, value| {
                (is_click && is_anchor && name == "href")
                    .then(|| format!("x-trust-js:{id}:{value}"))
            },
            out,
        );
        // A live anchor that never had an href still needs the marker.
        if is_click && is_anchor && self.attr(id, "href").is_none() {
            out.push_str(&format!(" href=\"x-trust-js:{id}:\""));
        }
        out.push('>');
        if !VOID_ELEMENTS.contains(&tag) {
            if let Some(root) = self.shadow_root(id) {
                for c in self.children(root) {
                    self.serialize_live_node(c, Some(id), clickable, out);
                }
            } else {
                for c in self.children(id) {
                    self.serialize_live_node(c, host, clickable, out);
                }
            }
            out.push_str("</");
            out.push_str(tag);
            out.push('>');
        }
        if is_click && !is_anchor {
            out.push_str("</a>");
        }
    }

    /// Write an element's attributes, baking the cascaded `display` into
    /// its `style` so the re-parsed layout arena flows it the way the
    /// engine (which has the external sheets) computed. `rewrite` lets the
    /// live serializer substitute an attribute value (anchor href markers).
    fn write_attrs(
        &self,
        id: NodeId,
        attrs: &[Attribute],
        rewrite: &mut dyn FnMut(&str, &str) -> Option<String>,
        out: &mut String,
    ) {
        // Bake the cascaded box/layout properties (the engine has the
        // sheets; the re-parsed layout arena doesn't) into the element's
        // inline style. `display:none`/`visibility:hidden` are already
        // dropped, so they never need baking.
        let mut bake = String::new();
        for &prop in BAKE_PROPS {
            if let Some(v) = self.cascaded(id, prop) {
                if prop == "display" && v == "none" {
                    continue;
                }
                bake.push_str(prop);
                bake.push(':');
                bake.push_str(&escape_attr(&v));
                bake.push(';');
            }
        }
        let mut style_done = false;
        for a in attrs {
            let name: &str = &a.name.local;
            let replaced = rewrite(name, &a.value);
            let value = replaced.as_deref().unwrap_or(&a.value);
            out.push(' ');
            out.push_str(name);
            out.push_str("=\"");
            out.push_str(&escape_attr(value));
            if name == "style" && !bake.is_empty() {
                if !value.trim().is_empty() && !value.trim_end().ends_with(';') {
                    out.push(';');
                }
                out.push_str(&bake);
                style_done = true;
            } else if name == "style" {
                style_done = true;
            }
            out.push('"');
        }
        if !bake.is_empty() && !style_done {
            out.push_str(" style=\"");
            out.push_str(&bake);
            out.push('"');
        }
        // Bake generated content (the layout arena has no `<style>` to
        // re-cascade `::before`/`::after`); the layout reads these attrs.
        for (which, attr) in [
            (PseudoEl::Before, "data-trust-before"),
            (PseudoEl::After, "data-trust-after"),
        ] {
            if let Some(t) = self.pseudo_content(id, which) {
                out.push(' ');
                out.push_str(attr);
                out.push_str("=\"");
                out.push_str(&escape_attr(&t));
                out.push('"');
            }
        }
    }

    /// Non-element serialization shared between the plain and live
    /// serializers: documents/fragments recurse via `kids`, text
    /// escapes, the rest vanish.
    fn serialize_node_with(
        &self,
        id: NodeId,
        kids: &mut dyn FnMut(NodeId, &mut String),
        out: &mut String,
    ) {
        match &self.nodes[id].data {
            NodeData::Document | NodeData::Fragment => {
                for c in self.children(id) {
                    kids(c, out);
                }
            }
            NodeData::Doctype => {}
            NodeData::Comment(t) => {
                out.push_str("<!--");
                out.push_str(&t.replace("--", "- -"));
                out.push_str("-->");
            }
            NodeData::Text(t) => out.push_str(&escape_text(t)),
            NodeData::Element { .. } => unreachable!("elements handled by callers"),
        }
    }

    /// All `<script>` elements in document order, as (src-attr, inline
    /// source, type-attr) — the execution schedule for js.rs.
    pub fn scripts(&self) -> Vec<(Option<String>, String, Option<String>)> {
        self.descendants(DOCUMENT)
            .into_iter()
            .filter(|&d| self.tag_name(d) == Some("script"))
            .map(|d| {
                (
                    self.attr(d, "src").map(str::to_string),
                    self.text_content(d),
                    self.attr(d, "type").map(str::to_string),
                )
            })
            .collect()
    }

    /// querySelector(All): match descendants of `root` against a
    /// selector list, document order.
    pub fn query(&self, root: NodeId, selectors: &SelectorList, first_only: bool) -> Vec<NodeId> {
        let mut out = Vec::new();
        for d in self.descendants(root) {
            if self.matches(d, selectors) {
                out.push(d);
                if first_only {
                    break;
                }
            }
        }
        out
    }

    pub fn matches(&self, id: NodeId, selectors: &SelectorList) -> bool {
        selectors.0.iter().any(|c| self.matches_complex(id, &c.0))
    }

    fn matches_complex(&self, id: NodeId, parts: &[(Combinator, Compound)]) -> bool {
        let Some(((comb, compound), rest)) = parts.split_last() else {
            return false;
        };
        if !self.matches_compound(id, compound) {
            return false;
        }
        if rest.is_empty() {
            return true;
        }
        match comb {
            Combinator::Child => self.nodes[id]
                .parent
                .is_some_and(|p| self.matches_complex(p, rest)),
            Combinator::Descendant | Combinator::None => {
                let mut up = self.nodes[id].parent;
                while let Some(a) = up {
                    if self.matches_complex(a, rest) {
                        return true;
                    }
                    up = self.nodes[a].parent;
                }
                false
            }
        }
    }

    fn matches_compound(&self, id: NodeId, c: &Compound) -> bool {
        if c.never {
            return false;
        }
        let Some(tag) = self.tag_name(id) else {
            return false;
        };
        if let Some(want) = &c.tag
            && want != "*"
            && want != tag
        {
            return false;
        }
        if let Some(want) = &c.id
            && self.attr(id, "id") != Some(want.as_str())
        {
            return false;
        }
        if !c.classes.is_empty() {
            let classes = self.attr(id, "class").unwrap_or("");
            let have: Vec<&str> = classes.split_ascii_whitespace().collect();
            if !c.classes.iter().all(|w| have.contains(&w.as_str())) {
                return false;
            }
        }
        for sel in &c.attrs {
            match self.attr(id, &sel.name) {
                None => return false,
                Some(got) => {
                    if !sel.matches(got) {
                        return false;
                    }
                }
            }
        }
        c.nots.iter().all(|n| !self.matches_compound(id, n))
    }
}

fn escape_text(s: &str) -> Cow<'_, str> {
    if !s.contains(['&', '<', '>']) {
        return Cow::Borrowed(s);
    }
    Cow::Owned(
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;"),
    )
}

fn escape_attr(s: &str) -> Cow<'_, str> {
    if !s.contains(['&', '<', '>', '"']) {
        return Cow::Borrowed(s);
    }
    Cow::Owned(
        s.replace('&', "&amp;")
            .replace('"', "&quot;")
            .replace('<', "&lt;")
            .replace('>', "&gt;"),
    )
}

// ---- Selector subset ------------------------------------------------

/// The workhorse selector grammar: `tag`, `*`, `#id`, `.class`,
/// `[attr]`, `[attr⊙=value]` (⊙ ∈ {ε, ~, |, ^, $, *}), `:not(compound)`,
/// compounds thereof, descendant (space) and child (`>`) combinators,
/// comma lists. Interaction pseudos (`:hover`…) and pseudo-elements
/// parse but never match — valid CSS that can't be true in our world.
/// The exotic combinators wait for a page that actually needs them.
pub struct SelectorList(Vec<Complex>);

struct Complex(Vec<(Combinator, Compound)>);

#[derive(PartialEq)]
enum Combinator {
    /// Leftmost compound: nothing to its left.
    None,
    Descendant,
    Child,
}

/// The `::before` / `::after` generated-content pseudo-elements (CSS2
/// `:before`/`:after` legacy spelling too). The only pseudo-elements we
/// act on; others parse but never match.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PseudoEl {
    Before,
    After,
}

#[derive(Default)]
struct Compound {
    tag: Option<String>,
    id: Option<String>,
    classes: Vec<String>,
    attrs: Vec<AttrSel>,
    /// `:not(...)` arguments: the compound matches only if none do.
    nots: Vec<Compound>,
    /// `:hover`, `:nth-child(…)` and other pseudos we can't satisfy: parse
    /// fine, match never (fail-open — a never-matching hide rule hides
    /// nothing, and its comma-siblings stay alive).
    never: bool,
    /// `::before`/`::after`: the rule targets a generated-content box on
    /// the matched element, NOT the element itself. The element-property
    /// cascade skips these; `pseudo_content` consults only these.
    pseudo: Option<PseudoEl>,
    /// Pseudo-class count, for specificity only.
    pseudos: u32,
}

struct AttrSel {
    name: String,
    op: AttrOp,
    value: Option<String>,
}

/// CSS attribute selector operators: `=`, `~=`, `|=`, `^=`, `$=`, `*=`.
enum AttrOp {
    Exact,
    Includes,
    Dash,
    Prefix,
    Suffix,
    Substring,
}

impl AttrSel {
    fn matches(&self, got: &str) -> bool {
        let Some(want) = &self.value else {
            return true; // bare [attr]: presence is enough
        };
        match self.op {
            AttrOp::Exact => got == want,
            AttrOp::Includes => got.split_ascii_whitespace().any(|w| w == want),
            AttrOp::Dash => {
                got == want
                    || got
                        .strip_prefix(want.as_str())
                        .is_some_and(|r| r.starts_with('-'))
            }
            AttrOp::Prefix => !want.is_empty() && got.starts_with(want.as_str()),
            AttrOp::Suffix => !want.is_empty() && got.ends_with(want.as_str()),
            AttrOp::Substring => !want.is_empty() && got.contains(want.as_str()),
        }
    }
}

impl Compound {
    fn is_empty(&self) -> bool {
        self.tag.is_none()
            && self.id.is_none()
            && self.classes.is_empty()
            && self.attrs.is_empty()
            && self.nots.is_empty()
            && !self.never
            && self.pseudo.is_none()
    }

    /// (ids, classes+attrs+pseudo-classes, tags) — `:not` contributes
    /// its argument's counts, not its own.
    fn spec(&self) -> (u32, u32, u32) {
        let mut s = (
            u32::from(self.id.is_some()),
            self.classes.len() as u32 + self.attrs.len() as u32 + self.pseudos,
            u32::from(matches!(&self.tag, Some(t) if t != "*")),
        );
        for n in &self.nots {
            let ns = n.spec();
            s = (s.0 + ns.0, s.1 + ns.1, s.2 + ns.2);
        }
        s
    }
}

impl Complex {
    fn specificity(&self) -> (u32, u32, u32) {
        let mut s = (0, 0, 0);
        for (_, c) in &self.0 {
            let cs = c.spec();
            s = (s.0 + cs.0, s.1 + cs.1, s.2 + cs.2);
        }
        s
    }
}

/// Split on `sep` outside parens, brackets, and quotes — `:not(.a, .b)`
/// and `[title="x,y"]` must survive list splitting.
fn split_top_level(input: &str, sep: char) -> Vec<&str> {
    let mut out = Vec::new();
    let (mut depth, mut start) = (0i32, 0usize);
    let mut quote: Option<char> = None;
    for (i, c) in input.char_indices() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(c),
            (None, '(' | '[') => depth += 1,
            (None, ')' | ']') => depth -= 1,
            (None, c) if c == sep && depth == 0 => {
                out.push(&input[start..i]);
                start = i + c.len_utf8();
            }
            _ => {}
        }
    }
    out.push(&input[start..]);
    out
}

impl SelectorList {
    pub fn parse(input: &str) -> Option<SelectorList> {
        let mut list = Vec::new();
        for part in split_top_level(input, ',') {
            list.push(parse_complex(part.trim())?);
        }
        if list.is_empty() {
            None
        } else {
            Some(SelectorList(list))
        }
    }
}

fn parse_complex(input: &str) -> Option<Complex> {
    let mut parts: Vec<(Combinator, Compound)> = Vec::new();
    let mut chars = input.chars().peekable();
    let mut pending = Combinator::None;
    loop {
        // Inter-compound whitespace / combinators.
        let mut saw_space = false;
        while let Some(&c) = chars.peek() {
            if c.is_ascii_whitespace() {
                saw_space = true;
                chars.next();
            } else if c == '>' {
                pending = Combinator::Child;
                chars.next();
            } else {
                break;
            }
        }
        if chars.peek().is_none() {
            break;
        }
        if pending == Combinator::None && saw_space && !parts.is_empty() {
            pending = Combinator::Descendant;
        }

        let compound = parse_compound(&mut chars)?;
        if compound.is_empty() {
            return None;
        }
        parts.push((std::mem::replace(&mut pending, Combinator::None), compound));
    }
    if parts.is_empty() {
        None
    } else {
        Some(Complex(parts))
    }
}

fn parse_compound(chars: &mut std::iter::Peekable<std::str::Chars>) -> Option<Compound> {
    let mut compound = Compound::default();
    while let Some(&c) = chars.peek() {
        match c {
            '#' => {
                chars.next();
                compound.id = Some(take_name(chars)?);
            }
            '.' => {
                chars.next();
                compound.classes.push(take_name(chars)?);
            }
            '[' => {
                chars.next();
                let inner: String = chars.by_ref().take_while(|&c| c != ']').collect();
                let (name, op, value) = match inner.split_once('=') {
                    Some((n, v)) => {
                        let (n, op) = match n.chars().last() {
                            Some('~') => (&n[..n.len() - 1], AttrOp::Includes),
                            Some('|') => (&n[..n.len() - 1], AttrOp::Dash),
                            Some('^') => (&n[..n.len() - 1], AttrOp::Prefix),
                            Some('$') => (&n[..n.len() - 1], AttrOp::Suffix),
                            Some('*') => (&n[..n.len() - 1], AttrOp::Substring),
                            _ => (n, AttrOp::Exact),
                        };
                        (n, op, Some(v.trim().trim_matches(['"', '\'']).to_string()))
                    }
                    None => (inner.as_str(), AttrOp::Exact, None),
                };
                if name.trim().is_empty() {
                    return None;
                }
                compound.attrs.push(AttrSel {
                    name: name.trim().to_ascii_lowercase(),
                    op,
                    value,
                });
            }
            ':' => {
                chars.next();
                // `::foo` (double colon) marks a pseudo-element; `:before`
                // and `:after` have a legacy single-colon spelling too.
                if chars.peek() == Some(&':') {
                    chars.next();
                }
                let name = take_name(chars)?.to_ascii_lowercase();
                let mut arg = None;
                if chars.peek() == Some(&'(') {
                    chars.next();
                    let mut depth = 1u32;
                    let mut inner = String::new();
                    for c in chars.by_ref() {
                        match c {
                            '(' => depth += 1,
                            ')' => {
                                depth -= 1;
                                if depth == 0 {
                                    break;
                                }
                            }
                            _ => {}
                        }
                        inner.push(c);
                    }
                    if depth != 0 {
                        return None;
                    }
                    arg = Some(inner);
                }
                if name == "not" {
                    // Step-1 :not takes compounds (no combinators) —
                    // anything fancier fails the parse (rule ignored,
                    // fail-open). Specificity comes from the argument.
                    for part in split_top_level(&arg?, ',') {
                        let part = part.trim();
                        if part.is_empty() || part.contains(char::is_whitespace) {
                            return None;
                        }
                        let mut inner_chars = part.chars().peekable();
                        let inner = parse_compound(&mut inner_chars)?;
                        if inner.is_empty() || inner_chars.peek().is_some() {
                            return None;
                        }
                        compound.nots.push(inner);
                    }
                } else if name == "before" || name == "after" {
                    // Generated-content pseudo-element: the compound still
                    // matches the element (tag/class parts), but the rule
                    // targets the element's ::before/::after box.
                    compound.pseudo = Some(if name == "before" {
                        PseudoEl::Before
                    } else {
                        PseudoEl::After
                    });
                    compound.pseudos += 1;
                } else {
                    // Valid CSS we can never satisfy (no pointer, no
                    // focus, no positional matching yet): parse, count
                    // for specificity, never match.
                    compound.never = true;
                    compound.pseudos += 1;
                }
            }
            c if c.is_ascii_whitespace() || c == '>' => break,
            _ => {
                let tag = take_name(chars)?;
                compound.tag = Some(tag.to_ascii_lowercase());
            }
        }
    }
    Some(compound)
}

/// An identifier, `*`, or tag token.
fn take_name(chars: &mut std::iter::Peekable<std::str::Chars>) -> Option<String> {
    let mut out = String::new();
    while let Some(&c) = chars.peek() {
        if c.is_alphanumeric() || matches!(c, '-' | '_' | '*') {
            out.push(c);
            chars.next();
        } else {
            break;
        }
    }
    if out.is_empty() { None } else { Some(out) }
}

// ---- CSS visibility cascade (step 1) ---------------------------------
// A real mini-cascade for exactly two properties, `display` and
// `visibility`, so stylesheet-class hiding (.hidden{display:none}) and
// class-toggle re-showing (.menu.open{display:block}) work. Everything
// unparseable is IGNORED — fail-open always means "visible", never
// "hidden". `:hover`/`:focus` never match; @-blocks are skipped whole.

/// Properties the cascade tracks. Kept deliberately small: the
/// box-layout primitives plus the visibility pair. Everything else is
/// ignored (not stored, fail-open).
const TRACKED: &[&str] = &[
    "display",
    "visibility",
    "margin-top",
    "margin-bottom",
    "margin-left",
    "padding-top",
    "padding-bottom",
    "padding-left",
    "text-align",
    "font-weight",
    "font-style",
    "white-space",
    "text-transform",
    "text-decoration",
    "text-decoration-line",
    "content",
];

fn is_tracked(prop: &str) -> bool {
    TRACKED.contains(&prop)
}

/// Properties baked into serialized HTML so the re-parsed layout arena
/// flows a living page the way the engine (which holds the sheets)
/// computed. `visibility` is omitted: hidden nodes are dropped outright.
const BAKE_PROPS: &[&str] = &[
    "display",
    "margin-top",
    "margin-bottom",
    "margin-left",
    "padding-top",
    "padding-bottom",
    "padding-left",
    "text-align",
    "font-weight",
    "font-style",
    "white-space",
    "text-transform",
    "text-decoration",
    "text-decoration-line",
];

/// Expand a `margin`/`padding` shorthand into its top/right/bottom/left
/// longhands; pass anything else through unchanged.
fn expand_box_shorthand(prop: &str, value: &str) -> Vec<(String, String)> {
    if prop == "margin" || prop == "padding" {
        let p: Vec<&str> = value.split_whitespace().collect();
        let (t, r, b, l) = match p.as_slice() {
            [a] => (*a, *a, *a, *a),
            [a, b] => (*a, *b, *a, *b),
            [a, b, c] => (*a, *b, *c, *b),
            [a, b, c, d] => (*a, *b, *c, *d),
            _ => return Vec::new(),
        };
        return vec![
            (format!("{prop}-top"), t.to_string()),
            (format!("{prop}-right"), r.to_string()),
            (format!("{prop}-bottom"), b.to_string()),
            (format!("{prop}-left"), l.to_string()),
        ];
    }
    vec![(prop.to_string(), value.to_string())]
}

/// One parsed rule, holding its tracked declarations (`(prop, (important,
/// value))`). Rules mentioning no tracked property are never stored.
struct StyleRule {
    selector: Complex,
    specificity: (u32, u32, u32),
    /// Source position across every sheet of the scope.
    order: usize,
    decls: Vec<(String, (bool, String))>,
}

/// (!important, inline, specificity, source order): the cascade key;
/// lexicographic max wins.
type CascadeKey = (bool, bool, (u32, u32, u32), usize);

fn consider(slot: &mut Option<(CascadeKey, String)>, key: CascadeKey, value: &str) {
    if slot.as_ref().is_none_or(|(k, _)| key >= *k) {
        *slot = Some((key, value.to_string()));
    }
}

/// Rules bucketed by tree scope: DOCUMENT for the light DOM, the shadow
/// fragment for each shadow tree. Shadow sheets never leak out;
/// document sheets never reach in.
#[derive(Default)]
struct StyleIndex {
    scopes: std::collections::HashMap<NodeId, Vec<StyleRule>>,
}

/// Parse one `prop: value [!important]` declaration. The value is
/// lowercased (keyword props), EXCEPT `content`, whose text is
/// case-significant (`content:"Read more"`).
fn parse_decl(decl: &str) -> Option<(String, String, bool)> {
    let (k, v) = decl.split_once(':')?;
    let k = k.trim().to_ascii_lowercase();
    let v = v.trim();
    let (v, important) = match v.rsplit_once('!') {
        Some((head, bang)) if bang.trim().eq_ignore_ascii_case("important") => (head, true),
        _ => (v, false),
    };
    let v = v.trim();
    let value = if k == "content" {
        v.to_string()
    } else {
        v.to_ascii_lowercase()
    };
    Some((k, value, important))
}

/// The pseudo-element a rule's subject (last compound) targets, if any.
fn rule_pseudo(rule: &StyleRule) -> Option<PseudoEl> {
    rule.selector.0.last().and_then(|(_, c)| c.pseudo)
}

/// Strip the surrounding quotes from a CSS string and decode its escapes
/// (`\HEX ` codepoints and `\c` literals). `None` if `v` isn't quoted.
fn unquote_css(v: &str) -> Option<String> {
    let quote = v.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    if v.chars().count() < 2 || !v.ends_with(quote) {
        return None;
    }
    let inner: String = {
        let mut it = v.chars();
        it.next();
        it.next_back();
        it.collect()
    };
    let mut out = String::new();
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let mut hex = String::new();
        while hex.len() < 6 && chars.peek().is_some_and(char::is_ascii_hexdigit) {
            hex.push(chars.next().unwrap());
        }
        if !hex.is_empty() {
            // CSS allows one trailing whitespace to delimit the escape.
            if chars.peek() == Some(&' ') {
                chars.next();
            }
            if let Some(ch) = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                out.push(ch);
            }
        } else if let Some(lit) = chars.next() {
            out.push(lit);
        }
    }
    Some(out)
}

fn strip_css_comments(css: &str) -> Cow<'_, str> {
    if !css.contains("/*") {
        return Cow::Borrowed(css);
    }
    let mut out = String::with_capacity(css.len());
    let mut rest = css;
    while let Some(i) = rest.find("/*") {
        out.push_str(&rest[..i]);
        out.push(' ');
        match rest[i + 2..].find("*/") {
            Some(j) => rest = &rest[i + 2 + j + 2..],
            None => return Cow::Owned(out),
        }
    }
    out.push_str(rest);
    Cow::Owned(out)
}

/// Collect a sheet's display/visibility rules into `out`. Whole
/// @-blocks are skipped (no @media in step 1 — media-query hiding is
/// usually mobile-only, and skipping keeps content visible); rules
/// whose selectors don't parse are skipped the same way.
fn parse_sheet(css: &str, order: &mut usize, out: &mut Vec<StyleRule>) {
    let css = strip_css_comments(css);
    let mut rest = css.as_ref();
    loop {
        rest = rest.trim_start();
        if rest.is_empty() {
            return;
        }
        if let Some(after) = rest.strip_prefix('@') {
            // @charset/@import end at ';'; block at-rules at their
            // balanced '}' — whichever comes first.
            rest = match (after.find(';'), after.find('{')) {
                (Some(s), Some(b)) if s < b => &after[s + 1..],
                (_, Some(b)) => take_block(&after[b..]).1,
                (Some(s), None) => &after[s + 1..],
                (None, None) => return,
            };
            continue;
        }
        let Some(brace) = rest.find('{') else { return };
        let selector_text = &rest[..brace];
        let (block, after) = take_block(&rest[brace..]);
        rest = after;
        let mut decls: Vec<(String, (bool, String))> = Vec::new();
        for decl in block.split(';') {
            let Some((k, v, important)) = parse_decl(decl) else {
                continue;
            };
            for (pk, pv) in expand_box_shorthand(&k, &v) {
                if !is_tracked(&pk) {
                    continue;
                }
                // Within one block a later declaration wins unless it
                // would demote !important.
                if let Some(slot) = decls.iter_mut().find(|(n, _)| *n == pk) {
                    if important >= slot.1.0 {
                        slot.1 = (important, pv);
                    }
                } else {
                    decls.push((pk, (important, pv)));
                }
            }
        }
        if decls.is_empty() {
            continue;
        }
        let Some(SelectorList(complexes)) = SelectorList::parse(selector_text.trim()) else {
            continue;
        };
        for selector in complexes {
            out.push(StyleRule {
                specificity: selector.specificity(),
                selector,
                order: *order,
                decls: decls.clone(),
            });
            *order += 1;
        }
    }
}

/// `input` starts at '{'; return (inner text, after-the-matching-'}').
fn take_block(input: &str) -> (&str, &str) {
    let mut depth = 0i32;
    for (i, c) in input.char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return (&input[1..i], &input[i + 1..]);
                }
            }
            _ => {}
        }
    }
    // Unbalanced sheet: everything after the brace is the block.
    (&input[1.min(input.len())..], "")
}

// ---- html5ever integration ------------------------------------------

struct Sink {
    dom: RefCell<Dom>,
}

impl TreeSink for Sink {
    type Handle = NodeId;
    type Output = Dom;
    type ElemName<'a> = Ref<'a, QualName>;

    fn finish(self) -> Dom {
        self.dom.into_inner()
    }

    fn parse_error(&self, _msg: Cow<'static, str>) {}

    fn get_document(&self) -> NodeId {
        DOCUMENT
    }

    fn elem_name<'a>(&'a self, target: &'a NodeId) -> Ref<'a, QualName> {
        Ref::map(self.dom.borrow(), |d| match &d.nodes[*target].data {
            NodeData::Element { name, .. } => name,
            _ => panic!("elem_name on a non-element"),
        })
    }

    fn create_element(&self, name: QualName, attrs: Vec<Attribute>, flags: ElementFlags) -> NodeId {
        let mut dom = self.dom.borrow_mut();
        let contents = flags.template.then(|| dom.new_node(NodeData::Fragment));
        dom.new_node(NodeData::Element {
            name,
            attrs,
            template_contents: contents,
        })
    }

    fn create_comment(&self, text: StrTendril) -> NodeId {
        self.dom.borrow_mut().create_comment(&text)
    }

    fn create_pi(&self, _target: StrTendril, _data: StrTendril) -> NodeId {
        self.dom.borrow_mut().create_comment("")
    }

    fn append(&self, parent: &NodeId, child: NodeOrText<NodeId>) {
        let mut dom = self.dom.borrow_mut();
        match child {
            NodeOrText::AppendNode(n) => dom.append(*parent, n),
            NodeOrText::AppendText(t) => dom.append_text(*parent, &t),
        }
    }

    fn append_based_on_parent_node(
        &self,
        element: &NodeId,
        prev_element: &NodeId,
        child: NodeOrText<NodeId>,
    ) {
        if self.dom.borrow().nodes[*element].parent.is_some() {
            self.append_before_sibling(element, child);
        } else {
            self.append(prev_element, child);
        }
    }

    fn append_doctype_to_document(
        &self,
        _name: StrTendril,
        _public_id: StrTendril,
        _system_id: StrTendril,
    ) {
        let mut dom = self.dom.borrow_mut();
        let dt = dom.new_node(NodeData::Doctype);
        dom.append(DOCUMENT, dt);
    }

    fn get_template_contents(&self, target: &NodeId) -> NodeId {
        match &self.dom.borrow().nodes[*target].data {
            NodeData::Element {
                template_contents: Some(c),
                ..
            } => *c,
            _ => panic!("get_template_contents on a non-template"),
        }
    }

    fn same_node(&self, x: &NodeId, y: &NodeId) -> bool {
        x == y
    }

    fn set_quirks_mode(&self, _mode: QuirksMode) {}

    fn append_before_sibling(&self, sibling: &NodeId, new_node: NodeOrText<NodeId>) {
        let mut dom = self.dom.borrow_mut();
        let Some(parent) = dom.nodes[*sibling].parent else {
            return;
        };
        match new_node {
            NodeOrText::AppendNode(n) => dom.insert_before(parent, n, Some(*sibling)),
            NodeOrText::AppendText(t) => {
                // Merge into the preceding text node when there is one.
                if let Some(prev) = dom.nodes[*sibling].prev_sibling
                    && let NodeData::Text(existing) = &mut dom.nodes[prev].data
                {
                    existing.push_str(&t);
                    return;
                }
                let tn = dom.create_text(&t);
                dom.insert_before(parent, tn, Some(*sibling));
            }
        }
    }

    fn add_attrs_if_missing(&self, target: &NodeId, new_attrs: Vec<Attribute>) {
        let mut dom = self.dom.borrow_mut();
        if let NodeData::Element { attrs, .. } = &mut dom.nodes[*target].data {
            for a in new_attrs {
                if !attrs.iter().any(|e| e.name == a.name) {
                    attrs.push(a);
                }
            }
        }
    }

    fn associate_with_form(
        &self,
        _target: &NodeId,
        _form: &NodeId,
        _nodes: (&NodeId, Option<&NodeId>),
    ) {
    }

    fn remove_from_parent(&self, target: &NodeId) {
        self.dom.borrow_mut().detach(*target);
    }

    fn reparent_children(&self, node: &NodeId, new_parent: &NodeId) {
        let mut dom = self.dom.borrow_mut();
        for c in dom.children(*node) {
            dom.append(*new_parent, c);
        }
    }

    fn mark_script_already_started(&self, _node: &NodeId) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_serializes_a_document() {
        let dom = Dom::parse_document(
            "<html><head><title>T</title></head><body><p id=a>hi <b>there</b></p></body></html>",
        );
        let html = dom.serialize(DOCUMENT);
        assert!(html.contains("<p id=\"a\">hi <b>there</b></p>"), "{html}");
    }

    #[test]
    fn serializer_drops_script_noscript_template_style_and_comments() {
        let dom = Dom::parse_document(
            "<body><script>evil()</script><noscript>no js!</noscript>\
             <template><p>inert</p></template><style>p{color:red}</style>\
             <!-- c -->keep</body>",
        );
        let html = dom.serialize(DOCUMENT);
        assert!(!html.contains("evil"), "{html}");
        assert!(!html.contains("no js"), "{html}");
        assert!(!html.contains("inert"), "{html}");
        assert!(!html.contains("color:red"), "{html}");
        assert!(html.contains("keep"), "{html}");
    }

    #[test]
    fn css_cascade_hides_and_reshows() {
        // Stylesheet-class hiding, and the part a one-way hide-list
        // would get wrong: a MORE SPECIFIC rule re-showing.
        let dom = Dom::parse_document(
            "<head><style>
                .hidden { display: none }
                .menu { display: none }
                .menu.open { display: block }
             </style></head>
             <body><p class=hidden>secret</p>
             <div class=menu>shut menu</div>
             <div class='menu open'>open menu</div></body>",
        );
        let html = dom.serialize(DOCUMENT);
        assert!(!html.contains("secret"), "{html}");
        assert!(!html.contains("shut menu"), "{html}");
        assert!(html.contains("open menu"), "{html}");
    }

    #[test]
    fn css_cascade_inline_and_important_precedence() {
        // Inline style beats sheet rules — except !important.
        let dom = Dom::parse_document(
            "<head><style>
                #a { display: none }
                #b { display: none !important }
             </style></head>
             <body><p id=a style='display:block'>inline wins</p>
             <p id=b style='display:block'>important wins</p></body>",
        );
        let html = dom.serialize(DOCUMENT);
        assert!(html.contains("inline wins"), "{html}");
        assert!(!html.contains("important wins"), "{html}");
    }

    #[test]
    fn css_cascade_fails_open() {
        // :hover can't be true here; @media blocks are skipped whole; a
        // selector list with an unparseable member dies entirely (the
        // spec's rule, and it fails toward VISIBLE).
        let dom = Dom::parse_document(
            "<head><style>
                .x:hover { display: none }
                @media (max-width: 600px) { .x { display: none } }
                .x ~ p, .y { display: none }
                .z { display: none }
             </style></head>
             <body><p class=x>pointer</p><p class=y>comma survivor</p>\
             <p class=z>plain hide</p></body>",
        );
        let html = dom.serialize(DOCUMENT);
        assert!(html.contains("pointer"), "{html}");
        assert!(html.contains("comma survivor"), "{html}");
        assert!(!html.contains("plain hide"), "{html}");
    }

    #[test]
    fn css_not_and_attr_operators_match() {
        let dom = Dom::parse_document(
            "<head><style>
                li:not(.keep) { display: none }
                [data-state^=clos] { visibility: hidden }
             </style></head>
             <body><ul><li class=keep>kept</li><li>dropped</li></ul>
             <div data-state=closed>shut</div><div data-state=open>still open</div></body>",
        );
        let html = dom.serialize(DOCUMENT);
        assert!(html.contains("kept"), "{html}");
        assert!(!html.contains("dropped"), "{html}");
        assert!(!html.contains("shut"), "{html}");
        assert!(html.contains("still open"), "{html}");
    }

    #[test]
    fn css_shadow_scope_is_isolated() {
        // Shadow sheets hide shadow content but never leak into the
        // document; document sheets never reach into shadow trees.
        let mut dom = Dom::parse_document(
            "<head><style>.doc-hidden{display:none}</style></head>
             <body><div id=host></div><p class=sec>light sec stays</p>
             <p class=doc-hidden>doc target</p></body>",
        );
        let host = dom.get_by_id("host").unwrap();
        let root = dom.attach_shadow(host);
        let style = dom.create_element("style");
        let css = dom.create_text(".sec { display: none }");
        dom.append(style, css);
        dom.append(root, style);
        let hidden_span = dom.create_element("span");
        dom.set_attr(hidden_span, "class", "sec");
        let t1 = dom.create_text("shadow secret");
        dom.append(hidden_span, t1);
        dom.append(root, hidden_span);
        let shown_span = dom.create_element("span");
        dom.set_attr(shown_span, "class", "doc-hidden");
        let t2 = dom.create_text("shadow shown");
        dom.append(shown_span, t2);
        dom.append(root, shown_span);
        let html = dom.serialize(DOCUMENT);
        assert!(!html.contains("shadow secret"), "{html}");
        assert!(html.contains("light sec stays"), "{html}");
        assert!(!html.contains("doc target"), "{html}");
        assert!(html.contains("shadow shown"), "{html}");
    }

    #[test]
    fn css_cascade_follows_mutations() {
        // The cached index rebuilds on the mutation epoch: class
        // toggles genuinely show and re-hide.
        let mut dom = Dom::parse_document(
            "<head><style>.menu{display:none}.menu.open{display:block}</style></head>
             <body><div id=m class=menu>payload</div></body>",
        );
        assert!(!dom.serialize(DOCUMENT).contains("payload"));
        let m = dom.get_by_id("m").unwrap();
        dom.set_attr(m, "class", "menu open");
        assert!(dom.serialize(DOCUMENT).contains("payload"));
        dom.set_attr(m, "class", "menu");
        assert!(!dom.serialize(DOCUMENT).contains("payload"));
    }

    #[test]
    fn external_sheets_join_the_cascade() {
        let mut dom = Dom::parse_document(
            "<head><link rel=stylesheet href='/a.css'></head>
             <body><p class=x>linked hide</p></body>",
        );
        assert_eq!(dom.stylesheet_links(), vec![String::from("/a.css")]);
        dom.attach_external_sheets(&[(String::from("/a.css"), String::from(".x{display:none}"))]);
        assert!(!dom.serialize(DOCUMENT).contains("linked hide"));
    }

    #[test]
    fn mutation_appends_inserts_and_detaches() {
        let mut dom = Dom::parse_document("<body><div id=root></div></body>");
        let root = dom.get_by_id("root").unwrap();
        let a = dom.create_element("p");
        let at = dom.create_text("first");
        dom.append(a, at);
        dom.append(root, a);
        let b = dom.create_element("p");
        dom.insert_before(root, b, Some(a));
        assert_eq!(dom.children(root), vec![b, a]);
        dom.detach(b);
        assert_eq!(dom.children(root), vec![a]);
        assert_eq!(dom.text_content(root), "first");
        let html = dom.serialize(DOCUMENT);
        assert!(
            html.contains("<div id=\"root\"><p>first</p></div>"),
            "{html}"
        );
    }

    #[test]
    fn attributes_set_get_remove() {
        let mut dom = Dom::parse_document("<body><a id=x href='/y'>l</a></body>");
        let a = dom.get_by_id("x").unwrap();
        assert_eq!(dom.attr(a, "href"), Some("/y"));
        assert_eq!(dom.attr(a, "HREF"), Some("/y"));
        dom.set_attr(a, "class", "big");
        assert_eq!(dom.attr(a, "class"), Some("big"));
        dom.remove_attr(a, "href");
        assert_eq!(dom.attr(a, "href"), None);
    }

    #[test]
    fn text_escaping_round_trips() {
        let mut dom = Dom::parse_document("<body><p id=t></p></body>");
        let p = dom.get_by_id("t").unwrap();
        dom.set_text(p, "a < b & \"c\"");
        let html = dom.serialize(DOCUMENT);
        assert!(html.contains("a &lt; b &amp; \"c\""), "{html}");
        // And the parser reads it back to the same text.
        let dom2 = Dom::parse_document(&html);
        let p2 = dom2.get_by_id("t").unwrap();
        assert_eq!(dom2.text_content(p2), "a < b & \"c\"");
    }

    #[test]
    fn fragment_parse_transplants_nodes() {
        let mut dom = Dom::parse_document("<body><div id=host></div></body>");
        let host = dom.get_by_id("host").unwrap();
        let nodes = dom.parse_fragment_into("div", "<p class=x>one</p>two");
        for n in &nodes {
            dom.append(host, *n);
        }
        assert_eq!(dom.text_content(host), "onetwo");
        let html = dom.serialize(DOCUMENT);
        assert!(html.contains("<p class=\"x\">one</p>two"), "{html}");
    }

    #[test]
    fn selectors_match_the_workhorse_grammar() {
        let dom = Dom::parse_document(
            "<body><div class='a b'><p id=p1 class=x>1</p><span data-k='v'>2</span></div>\
             <div><p class=x>3</p></div></body>",
        );
        let q = |s: &str| {
            let sel = SelectorList::parse(s).unwrap();
            dom.query(DOCUMENT, &sel, false).len()
        };
        assert_eq!(q("p"), 2);
        assert_eq!(q(".x"), 2);
        assert_eq!(q("#p1"), 1);
        assert_eq!(q("div.a.b p.x"), 1);
        assert_eq!(q("div > p"), 2);
        assert_eq!(q("body > p"), 0);
        assert_eq!(q("[data-k]"), 1);
        assert_eq!(q("[data-k=v]"), 1);
        assert_eq!(q("[data-k=w]"), 0);
        assert_eq!(q("p, span"), 3);
        assert_eq!(q("*"), 8); // html, head, body, div, p, span, div, p
    }

    #[test]
    fn scripts_are_collected_in_document_order() {
        let dom = Dom::parse_document(
            "<head><script src='/a.js'></script></head>\
             <body><script>inline()</script><script type='module'>mod()</script></body>",
        );
        let scripts = dom.scripts();
        assert_eq!(scripts.len(), 3);
        assert_eq!(scripts[0].0.as_deref(), Some("/a.js"));
        assert_eq!(scripts[1].1, "inline()");
        assert_eq!(scripts[2].2.as_deref(), Some("module"));
    }

    #[test]
    fn dirty_bit_tracks_mutations_and_skips_idempotent_writes() {
        let mut dom = Dom::parse_document("<body><p id=a>x</p></body>");
        assert!(dom.take_dirty()); // parsing itself mutates
        assert!(!dom.take_dirty()); // and the take resets
        let a = dom.get_by_id("a").unwrap();
        dom.set_attr(a, "class", "y");
        assert!(dom.take_dirty());
        // Idempotent writes are free: no dirty, no redraw downstream.
        dom.set_attr(a, "class", "y");
        assert!(!dom.take_dirty());
        dom.set_text(a, "x");
        assert!(!dom.take_dirty());
        dom.set_text(a, "z");
        assert!(dom.take_dirty());
        let _ = dom.text_content(a); // reads stay clean
        let _ = dom.serialize(DOCUMENT);
        assert!(!dom.take_dirty());
    }

    #[test]
    fn serialize_live_marks_buttons_and_live_anchors() {
        let dom = Dom::parse_document(
            "<body><button id=b>Push</button>\
             <button id=icon aria-label=menu></button>\
             <a id=plain href='/normal'>plain</a>\
             <a id=hot href='/hot'>hot</a></body>",
        );
        let b = dom.get_by_id("b").unwrap();
        let icon = dom.get_by_id("icon").unwrap();
        let hot = dom.get_by_id("hot").unwrap();
        let clickable = std::collections::HashSet::from([b, icon, hot]);
        let html = dom.serialize_live(DOCUMENT, &clickable);
        // Buttons wrapped; icon-only ones get a readable label.
        assert!(
            html.contains(&format!(
                "<a href=\"x-trust-js:{b}:\"><button id=\"b\">Push</button></a>"
            )),
            "{html}"
        );
        assert!(html.contains("[menu]"), "{html}");
        // The live anchor's href is rewritten with the original kept;
        // the plain one is untouched (the zero-overhead path).
        assert!(
            html.contains(&format!("href=\"x-trust-js:{hot}:/hot\"")),
            "{html}"
        );
        assert!(html.contains("href=\"/normal\""), "{html}");
    }

    #[test]
    fn shadow_trees_flatten_with_slot_projection() {
        let mut dom = Dom::parse_document(
            "<body><my-card><span slot=title>Hello</span>plain text</my-card></body>",
        );
        let host = dom
            .descendants(DOCUMENT)
            .into_iter()
            .find(|&d| dom.tag_name(d) == Some("my-card"))
            .unwrap();
        let root = dom.attach_shadow(host);
        let nodes = dom.parse_fragment_into(
            "div",
            "<h2><slot name=title>untitled</slot></h2><p>body: <slot></slot></p><slot name=missing>fallback</slot>",
        );
        for n in nodes {
            dom.append(root, n);
        }
        let html = dom.serialize(DOCUMENT);
        // Shadow content replaces light children; slots project.
        assert!(
            html.contains("<h2><span slot=\"title\">Hello</span></h2>"),
            "{html}"
        );
        assert!(html.contains("body: plain text"), "{html}");
        // Unassigned slot falls back to its own content.
        assert!(html.contains("fallback"), "{html}");
        // The light children don't ALSO render outside their slots.
        assert_eq!(html.matches("Hello").count(), 1, "{html}");
    }

    #[test]
    fn serialize_bakes_computed_display_into_style() {
        let dom = Dom::parse_document(
            "<html><head><style>li{display:inline}</style></head>\
             <body><ul><li>x</li></ul></body></html>",
        );
        let li = dom
            .descendants(DOCUMENT)
            .into_iter()
            .find(|&id| dom.tag_name(id) == Some("li"))
            .unwrap();
        assert_eq!(dom.computed_display(li).as_deref(), Some("inline"));
        // The serialized HTML carries the computed display so a re-parse
        // (the layout arena) flows it the same way.
        let html = dom.serialize(DOCUMENT);
        assert!(html.contains("display:inline"), "baked display: {html}");
        // Merges into an existing inline style rather than dropping it.
        let dom = Dom::parse_document(
            r#"<body><p style="color:red" class="x">y</p><style>.x{display:inline}</style></body>"#,
        );
        let p = dom
            .descendants(DOCUMENT)
            .into_iter()
            .find(|&id| dom.tag_name(id) == Some("p"))
            .unwrap();
        let html = dom.serialize(p);
        assert!(html.contains("color:red"), "keeps original style: {html}");
        assert!(html.contains("display:inline"), "adds display: {html}");

        // Box properties (margin shorthand → longhands) bake too, so a
        // living page's CSS spacing reaches the re-parsed layout arena.
        let dom =
            Dom::parse_document("<body><p class=x>y</p><style>.x{margin:1em 0}</style></body>");
        let p = dom
            .descendants(DOCUMENT)
            .into_iter()
            .find(|&id| dom.tag_name(id) == Some("p"))
            .unwrap();
        assert_eq!(dom.computed_style(p, "margin-top").as_deref(), Some("1em"));
        let html = dom.serialize(p);
        assert!(html.contains("margin-top:1em"), "bakes margin: {html}");
    }

    #[test]
    fn clone_subtree_is_deep_and_detached() {
        let mut dom = Dom::parse_document("<body><div id=d><p>x</p></div></body>");
        let d = dom.get_by_id("d").unwrap();
        let copy = dom.clone_subtree(d, true);
        assert!(dom.node(copy).parent.is_none());
        assert_eq!(dom.text_content(copy), "x");
    }
}
