//! Custom element registry associations of nodes.
//!
//! WHATWG DOM #concept-document custom element registry,
//! #element-custom-element-registry, the ShadowRoot's custom element registry
//! and "keep custom element registry null", #concept-node-adopt step 3,
//! #concept-node-clone ("clone a single node"), #effective-global-custom-
//! element-registry; HTML #look-up-a-custom-element-registry,
//! #dom-customelementregistry-initialize, the parser's declarative shadow
//! root steps (#parsing-main-inhead, `shadowrootcustomelementregistry`) and
//! the HTML fragment serialization algorithm. Consulted local WHATWG
//! snapshots: DOM a2331a45, HTML e5071a20 (2026-09-06).
//!
//! The registry objects and their definitions live in the platform prelude.
//! The arena keeps each node's association, because parser-created elements
//! have no wrapper until script touches them. A registry is `Null`, the
//! `Global` registry of the node document's Window, or a scoped registry
//! identified by a prelude-assigned number. A global registry is always the
//! node document's own: every algorithm that stores one (create, clone,
//! adopt, initialize) stores the node document's registry or its effective
//! global registry.
//!
//! Almost every node has its node document's registry, so only differences
//! are stored. The default of a node in template contents is null: HTML
//! gives template contents an inert "template contents owner document"
//! without a registry, which this arena does not materialize (template
//! contents share the template's node document). Trees that conceptually
//! belong to such a document are tracked by their roots, and moving a node
//! into or out of one runs the registry steps of DOM's adopt algorithm.

use super::*;

/// A custom element registry as seen by the arena.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub(crate) enum CeRegistry {
    #[default]
    Null,
    /// The global registry of the node document's Window.
    Global,
    /// A scoped registry (`new CustomElementRegistry()`), by its number.
    Scoped(u32),
}

impl CeRegistry {
    /// DOM #effective-global-custom-element-registry.
    pub(crate) fn effective_global(self) -> Self {
        match self {
            Self::Global => Self::Global,
            _ => Self::Null,
        }
    }
}

/// A node's conceptual node document: its arena node document, or an inert
/// template contents owner document.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct RegistryContext {
    document: NodeId,
    inert: bool,
}

#[derive(Default)]
pub(crate) struct State {
    /// Elements whose registry differs from their context's default.
    elements: FxHashMap<NodeId, CeRegistry>,
    /// Documents whose registry differs from the default: null, or the
    /// Window's global registry for a Document with a browsing context.
    documents: FxHashMap<NodeId, CeRegistry>,
    /// Roots of trees that belong to an inert template contents owner
    /// document: template contents, and copies cloned from them.
    inert_roots: FxHashSet<NodeId>,
    /// Whether any shadow root has an explicit registry.
    explicit_shadows: bool,
    /// Whether any Document has a scoped registry.
    scoped_documents: bool,
}

impl State {
    pub(super) fn retained_bytes(&self) -> usize {
        self.elements.capacity() * std::mem::size_of::<(NodeId, CeRegistry)>()
            + self.documents.capacity() * std::mem::size_of::<(NodeId, CeRegistry)>()
            + self.inert_roots.capacity() * std::mem::size_of::<NodeId>()
    }

    pub(super) fn inert_roots_remove(&mut self, root: NodeId) {
        self.inert_roots.remove(&root);
    }

    /// Drop the associations of retired identities.
    pub(super) fn retain(&mut self, valid: &dyn Fn(NodeId) -> bool) {
        self.elements.retain(|&id, _| valid(id));
        self.documents.retain(|&id, _| valid(id));
        self.inert_roots.retain(|&id| valid(id));
        macro_rules! shrink {
            ($($map:expr),+) => {$(
                if $map.capacity() > $map.len().saturating_mul(4).max(64) {
                    $map.shrink_to($map.len().saturating_mul(2));
                }
            )+};
        }
        shrink!(self.elements, self.documents, self.inert_roots);
    }
}

impl Dom {
    /// A Document's custom element registry: a Window's associated Document
    /// (the top-level Document, or a child navigable's active Document) has
    /// the Window's registry; other Documents start with null.
    pub(crate) fn document_registry(&self, document: NodeId) -> CeRegistry {
        if let Some(&registry) = self.ce_registries.documents.get(&document) {
            return registry;
        }
        let browsing_context = document == DOCUMENT
            || self.nodes.get(document).is_some_and(|node| {
                node.parent
                    .is_some_and(|frame| matches!(self.tag_name(frame), Some("iframe" | "frame")))
            });
        if browsing_context {
            CeRegistry::Global
        } else {
            CeRegistry::Null
        }
    }

    /// HTML #initialise-the-document-object: a Window's associated Document
    /// is created with the Window's custom element registry. Its nodes
    /// created before it becomes the active Document of a child navigable
    /// already have that registry.
    pub(crate) fn set_window_document_registry(&mut self, document: NodeId) {
        self.ce_registries
            .documents
            .insert(document, CeRegistry::Global);
    }

    /// Whether `id` is in a tree that belongs to an inert template contents
    /// owner document (following shadow roots to their hosts).
    fn in_inert_tree(&self, id: NodeId) -> bool {
        let roots = &self.ce_registries.inert_roots;
        if roots.is_empty() {
            return false;
        }
        let mut current = id;
        loop {
            if roots.contains(&current) {
                return true;
            }
            let Some(node) = self.nodes.get(current) else {
                return false;
            };
            if matches!(node.data, NodeData::Document) {
                return false;
            }
            match node
                .parent
                .or_else(|| self.shadow_hosts.get(&current).copied())
            {
                Some(next) => current = next,
                None => return false,
            }
        }
    }

    pub(super) fn registry_context(&self, id: NodeId) -> RegistryContext {
        RegistryContext {
            document: self.nodes[id].owner_document,
            inert: self.in_inert_tree(id),
        }
    }

    /// The registry a node in `context` has unless it records another one:
    /// its (conceptual) node document's registry.
    fn default_registry(&self, context: RegistryContext) -> CeRegistry {
        if context.inert {
            CeRegistry::Null
        } else {
            self.document_registry(context.document)
        }
    }

    /// DOM #element-custom-element-registry.
    pub(crate) fn element_registry(&self, element: NodeId) -> CeRegistry {
        match self.ce_registries.elements.get(&element) {
            Some(&registry) => registry,
            None => self.default_registry(self.registry_context(element)),
        }
    }

    /// The custom element registry of the shadow root `root`.
    pub(crate) fn shadow_registry(&self, root: NodeId) -> CeRegistry {
        match self.shadow_data.get(&root).and_then(|data| data.registry) {
            Some(registry) => registry,
            None => self.default_registry(self.registry_context(root)),
        }
    }

    /// HTML #look-up-a-custom-element-registry.
    pub(crate) fn lookup_registry(&self, node: NodeId) -> CeRegistry {
        match self.nodes.get(node).map(|node| &node.data) {
            Some(NodeData::Element { .. }) => self.element_registry(node),
            Some(NodeData::Fragment) if self.shadow_hosts.contains_key(&node) => {
                self.shadow_registry(node)
            }
            Some(NodeData::Document) => self.document_registry(node),
            _ => CeRegistry::Null,
        }
    }

    /// Whether any node records a registry other than its default, in which
    /// case moving nodes between conceptual documents must run the registry
    /// steps of DOM's adopt algorithm.
    pub(super) fn registries_recorded(&self) -> bool {
        let state = &self.ce_registries;
        !state.elements.is_empty() || state.explicit_shadows || state.scoped_documents
    }

    fn store_element_registry(
        &mut self,
        element: NodeId,
        registry: CeRegistry,
        default: CeRegistry,
    ) {
        if registry == default {
            self.ce_registries.elements.remove(&element);
        } else {
            self.ce_registries.elements.insert(element, registry);
        }
    }

    fn store_shadow_registry(&mut self, root: NodeId, registry: CeRegistry, default: CeRegistry) {
        let explicit = (registry != default).then_some(registry);
        if let Some(data) = self.shadow_data.get_mut(&root) {
            data.registry = explicit;
        }
        self.ce_registries.explicit_shadows |= explicit.is_some();
    }

    /// Set an element's custom element registry (DOM #concept-create-element,
    /// #dom-customelementregistry-initialize, the HTML element constructor).
    pub(crate) fn set_element_registry(&mut self, element: NodeId, registry: CeRegistry) {
        if !matches!(
            self.nodes.get(element).map(|node| &node.data),
            Some(NodeData::Element { .. })
        ) {
            return;
        }
        let default = self.default_registry(self.registry_context(element));
        self.store_element_registry(element, registry, default);
    }

    /// Set a shadow root's custom element registry and its keep custom
    /// element registry null flag.
    pub(crate) fn set_shadow_registry(
        &mut self,
        root: NodeId,
        registry: CeRegistry,
        keep_null: bool,
    ) {
        if !self.shadow_hosts.contains_key(&root) {
            return;
        }
        let default = self.default_registry(self.registry_context(root));
        self.store_shadow_registry(root, registry, default);
        if let Some(data) = self.shadow_data.get_mut(&root) {
            data.keep_registry_null = keep_null;
        }
        // Adoption must keep a null registry the flag protects.
        self.ce_registries.explicit_shadows |= keep_null;
    }

    /// An element's recorded registry, if it differs from its default.
    pub(super) fn recorded_element_registry(&self, element: NodeId) -> Option<CeRegistry> {
        self.ce_registries.elements.get(&element).copied()
    }

    /// Record a parsed element's registry when transferring it from a parse
    /// arena; a later insertion reconciles it with its new context.
    pub(super) fn record_element_registry(&mut self, element: NodeId, registry: CeRegistry) {
        self.ce_registries.elements.insert(element, registry);
    }

    /// A shadow root's data was copied wholesale from a parse arena.
    pub(super) fn note_copied_shadow_data(&mut self, data: &shadow::ShadowRootData) {
        self.ce_registries.explicit_shadows |= data.registry.is_some() || data.keep_registry_null;
    }

    /// Template contents (HTML #template-contents) belong to an inert
    /// document without a custom element registry.
    pub(super) fn note_template_contents(&mut self, contents: NodeId) {
        self.ce_registries.inert_roots.insert(contents);
    }

    /// A copy of a node from an inert tree is a detached root in that inert
    /// document (DOM #concept-node-clone with the node's node document).
    fn note_inert_copy(&mut self, root: NodeId) {
        self.ce_registries.inert_roots.insert(root);
    }

    /// `root` was adopted into a Document (DOM #concept-node-adopt).
    pub(super) fn leave_inert_document(&mut self, root: NodeId) {
        self.ce_registries.inert_roots.remove(&root);
    }

    /// Before `child` is inserted: the conceptual document whose registry
    /// steps may have to run, when any node records a registry.
    pub(super) fn registry_context_before_insertion(
        &self,
        child: NodeId,
    ) -> Option<RegistryContext> {
        (!matches!(self.nodes[child].data, NodeData::Document) && self.registries_recorded())
            .then(|| self.registry_context(child))
    }

    /// After `child` was inserted: a detached copy's inert root is adopted
    /// into its new parent's document, and a node that changed conceptual
    /// document runs the registry steps of DOM #concept-node-adopt.
    pub(super) fn registries_after_insertion(
        &mut self,
        child: NodeId,
        before: Option<RegistryContext>,
    ) {
        if !self.ce_registries.inert_roots.is_empty() && self.nodes[child].parent.is_some() {
            self.ce_registries.inert_roots.remove(&child);
        }
        if let Some(old) = before {
            let new = self.registry_context(child);
            if new != old {
                self.adopt_registries(child, old, new);
            }
        }
    }

    /// DOM #concept-node-adopt step 3's registry steps for the shadow-
    /// including inclusive descendants of `root`, which moved from conceptual
    /// document `old` to `new`. The adopted root's parent is null (adopt
    /// removes it first); every descendant's parent has already been
    /// processed in shadow-including tree order. Template contents stay in
    /// their own inert document.
    pub(super) fn adopt_registries(
        &mut self,
        root: NodeId,
        old: RegistryContext,
        new: RegistryContext,
    ) {
        let old_default = self.default_registry(old);
        let new_document = self.default_registry(new);
        // (node, the registry its parent looks up; None for the root and an
        // exclusive DocumentFragment parent, which use the document's).
        let mut stack: Vec<(NodeId, Option<CeRegistry>)> = vec![(root, None)];
        while let Some((id, parent_registry)) = stack.pop() {
            let lookup = match &self.nodes[id].data {
                NodeData::Element { .. } => {
                    let current = self
                        .ce_registries
                        .elements
                        .get(&id)
                        .copied()
                        .unwrap_or(old_default);
                    let registry = match current {
                        CeRegistry::Scoped(_) => current,
                        CeRegistry::Null | CeRegistry::Global => {
                            let source = if current != CeRegistry::Null {
                                new_document
                            } else {
                                parent_registry.unwrap_or(new_document)
                            };
                            source.effective_global()
                        }
                    };
                    self.store_element_registry(id, registry, new_document);
                    Some(registry)
                }
                NodeData::Fragment if self.shadow_hosts.contains_key(&id) => {
                    let data = self.shadow_data.get(&id).copied().unwrap_or_default();
                    let current = data.registry.unwrap_or(old_default);
                    let registry = if (current == CeRegistry::Null && !data.keep_registry_null)
                        || current == CeRegistry::Global
                    {
                        new_document.effective_global()
                    } else {
                        current
                    };
                    self.store_shadow_registry(id, registry, new_document);
                    Some(registry)
                }
                NodeData::Document => continue,
                _ => None,
            };
            let start = stack.len();
            let mut child = self.nodes[id].first_child;
            while let Some(next) = child {
                stack.push((next, lookup));
                child = self.nodes[next].next_sibling;
            }
            if let Some(&shadow) = self.shadow_roots.get(&id) {
                stack.push((shadow, None));
            }
            stack[start..].reverse();
        }
    }

    /// HTML #dom-customelementregistry-initialize steps 2–4 with `registry`,
    /// whose preconditions (step 1) the platform checked. Returns the custom
    /// element candidates among `root`'s inclusive descendants that now have
    /// `registry`, in tree order, for the platform to try to upgrade.
    pub(crate) fn initialize_registry(
        &mut self,
        root: NodeId,
        registry: CeRegistry,
    ) -> Vec<NodeId> {
        if !self.is_valid(root) {
            return Vec::new();
        }
        match self.nodes[root].data {
            // Step 2.
            NodeData::Document if self.document_registry(root) == CeRegistry::Null => {
                self.set_document_registry(root, registry);
            }
            // Step 3.
            NodeData::Fragment
                if self.shadow_hosts.contains_key(&root)
                    && self.shadow_registry(root) == CeRegistry::Null =>
            {
                let keep_null = self
                    .shadow_data
                    .get(&root)
                    .is_some_and(|data| data.keep_registry_null);
                self.set_shadow_registry(root, registry, keep_null);
            }
            _ => {}
        }
        // Step 4, over inclusive descendants (not shadow-including).
        let default = self.default_registry(self.registry_context(root));
        let mut elements = Vec::new();
        if matches!(self.nodes[root].data, NodeData::Element { .. }) {
            elements.push(root);
        }
        self.style_view().visit_tree_descendants(root, |id| {
            if matches!(self.nodes[id].data, NodeData::Element { .. }) {
                elements.push(id);
            }
            true
        });
        let mut candidates = Vec::new();
        for element in elements {
            let mut current = self
                .ce_registries
                .elements
                .get(&element)
                .copied()
                .unwrap_or(default);
            if current == CeRegistry::Null {
                self.store_element_registry(element, registry, default);
                current = registry;
            }
            if current == registry && self.is_custom_element_candidate(element) {
                candidates.push(element);
            }
        }
        candidates
    }

    /// Set a Document's registry, which until now was null. Nodes of that
    /// Document outside its tree (detached nodes and shadow trees) keep their
    /// null registry: the initialize() steps only change its inclusive
    /// descendants.
    fn set_document_registry(&mut self, document: NodeId, registry: CeRegistry) {
        let outside: Vec<NodeId> = self
            .nodes
            .ids()
            .filter(|&id| {
                id != document
                    && self.nodes[id].owner_document == document
                    && self.tree_scope(id) != document
                    && !self.in_inert_tree(id)
            })
            .collect();
        for id in outside {
            match self.nodes[id].data {
                NodeData::Element { .. } => {
                    self.ce_registries
                        .elements
                        .entry(id)
                        .or_insert(CeRegistry::Null);
                }
                NodeData::Fragment if self.shadow_hosts.contains_key(&id) => {
                    if let Some(data) = self.shadow_data.get_mut(&id)
                        && data.registry.is_none()
                    {
                        data.registry = Some(CeRegistry::Null);
                        self.ce_registries.explicit_shadows = true;
                    }
                }
                _ => {}
            }
        }
        self.ce_registries.scoped_documents |= matches!(registry, CeRegistry::Scoped(_));
        self.ce_registries.documents.insert(document, registry);
    }

    /// HTML #look-up-a-custom-element-definition can only find a definition
    /// for a valid custom element name or an element with an `is` value.
    fn is_custom_element_candidate(&self, id: NodeId) -> bool {
        self.tag_name(id).is_some_and(|tag| {
            input::is_valid_custom_element_name(tag)
                || self.attr(id, "is").is_some()
                || self.is_value(id).is_some()
        })
    }

    /// HTML #upgrade-particular-elements-within-a-document's candidates:
    /// the shadow-including descendants of `document` named `local_name`
    /// whose custom element registry is `registry`.
    pub(crate) fn upgrade_candidates(
        &self,
        document: NodeId,
        local_name: &str,
        registry: CeRegistry,
    ) -> Vec<NodeId> {
        let mut candidates = self.elements_by_tag_composed(document, local_name);
        if matches!(
            self.nodes.get(document).map(|node| &node.data),
            Some(NodeData::Document)
        ) {
            // Shadow-including descendants of a Document share its context.
            let default = self.document_registry(document);
            candidates.retain(|&id| {
                self.ce_registries
                    .elements
                    .get(&id)
                    .copied()
                    .unwrap_or(default)
                    == registry
            });
        } else {
            candidates.retain(|&id| self.element_registry(id) == registry);
        }
        candidates
    }

    /// The registries of the custom element candidates in `root`'s composed
    /// subtree (see `custom_elements_composed`), which share one context.
    pub(crate) fn custom_element_candidates_with_registries(
        &self,
        root: NodeId,
    ) -> Vec<(NodeId, CeRegistry)> {
        let candidates = self.custom_elements_composed(root);
        if candidates.is_empty() {
            return Vec::new();
        }
        let default = self.default_registry(self.registry_context(root));
        candidates
            .into_iter()
            .map(|id| {
                let registry = self
                    .ce_registries
                    .elements
                    .get(&id)
                    .copied()
                    .unwrap_or(default);
                (id, registry)
            })
            .collect()
    }

    /// The fragment parsing algorithm creates each element with the
    /// registry looked up for the element it is inserted into, starting
    /// from the target (HTML #html-fragment-parsing-algorithm, #create-an-
    /// element-for-the-token). Parsed elements are inserted into elements
    /// with that same registry, so the whole parsed tree takes the target's
    /// registry; template contents and declarative shadow trees keep theirs.
    ///
    /// The local snapshot's root insertion target makes the intended parent
    /// of the top-level nodes the new DocumentFragment, for which the lookup
    /// returns null; the root `html` element is still created with the
    /// target's registry for this purpose, and a null registry would leave
    /// every custom element created by innerHTML unable to upgrade. This
    /// follows that evident intent.
    pub(crate) fn apply_parsed_registry(&mut self, target: NodeId, nodes: &[NodeId]) {
        let Some(&first) = nodes.first() else {
            return;
        };
        let registry = self.lookup_registry(target);
        let default = self.default_registry(self.registry_context(first));
        if registry == default {
            return;
        }
        let mut elements = Vec::new();
        for &node in nodes {
            if matches!(self.nodes[node].data, NodeData::Element { .. }) {
                elements.push(node);
            }
            self.style_view().visit_tree_descendants(node, |id| {
                if matches!(self.nodes[id].data, NodeData::Element { .. }) {
                    elements.push(id);
                }
                true
            });
        }
        for element in elements {
            self.store_element_registry(element, registry, default);
        }
    }

    /// After a document or fragment parse: the parser created the content of
    /// a declarative shadow root whose template had a
    /// `shadowrootcustomelementregistry` attribute with the shadow root's null
    /// registry (its intended parents look up null).
    pub(super) fn settle_parsed_registries(&mut self, roots: &[NodeId]) {
        for &root in roots {
            if !self
                .shadow_data
                .get(&root)
                .is_some_and(|data| data.keep_registry_null)
            {
                continue;
            }
            let default = self.default_registry(self.registry_context(root));
            let mut elements = Vec::new();
            self.style_view().visit_tree_descendants(root, |id| {
                if matches!(self.nodes[id].data, NodeData::Element { .. }) {
                    elements.push(id);
                }
                true
            });
            for element in elements {
                self.store_element_registry(element, CeRegistry::Null, default);
            }
        }
    }

    /// Scoped registry numbers still associated with a node.
    pub(crate) fn live_scoped_registries(&self) -> FxHashSet<u32> {
        let mut live = FxHashSet::default();
        let state = &self.ce_registries;
        let shadows = self.shadow_data.values().filter_map(|data| data.registry);
        for registry in state
            .elements
            .values()
            .chain(state.documents.values())
            .copied()
            .chain(shadows)
        {
            if let CeRegistry::Scoped(id) = registry {
                live.insert(id);
            }
        }
        live
    }

    /// Whether the serialization of `root` needs a
    /// `shadowrootcustomelementregistry` attribute (HTML #serialising-html-
    /// fragments, shouldAppendRegistryAttribute).
    pub(super) fn serialize_registry_attribute(&self, root: NodeId) -> bool {
        let document = self.default_registry(self.registry_context(root));
        let shadow = self.shadow_registry(root);
        !(document == CeRegistry::Null && shadow == CeRegistry::Null)
            && !(document == CeRegistry::Global && shadow == CeRegistry::Global)
    }
}

/// The registry a copy receives in DOM #concept-node-clone.
#[derive(Clone, Copy)]
pub(super) struct CloneRegistries {
    /// The registry the source nodes have unless they record another one.
    pub(super) source_default: CeRegistry,
    /// The copy's (conceptual) document's registry, its default.
    pub(super) document: CeRegistry,
    /// The fallback registry for a source element with a null registry.
    pub(super) fallback: CeRegistry,
}

impl Dom {
    pub(super) fn clone_registries(
        &self,
        source: NodeId,
        document: Option<NodeId>,
        fallback: CeRegistry,
    ) -> (CloneRegistries, bool) {
        let source_context = self.registry_context(source);
        let copy_inert = document.is_none() && source_context.inert;
        let copy_document = document.unwrap_or(source_context.document);
        let document_registry = if copy_inert {
            CeRegistry::Null
        } else {
            self.document_registry(copy_document)
        };
        (
            CloneRegistries {
                source_default: self.default_registry(source_context),
                document: document_registry,
                fallback,
            },
            copy_inert,
        )
    }

    /// DOM #clone-a-single-node steps 2.1–2.3 for an element.
    pub(super) fn clone_element_registry(
        &mut self,
        source: NodeId,
        copy: NodeId,
        registries: CloneRegistries,
    ) {
        let registry = match self
            .ce_registries
            .elements
            .get(&source)
            .copied()
            .unwrap_or(registries.source_default)
        {
            CeRegistry::Null => registries.fallback,
            registry => registry,
        };
        let registry = match registry {
            CeRegistry::Global => registries.document.effective_global(),
            registry => registry,
        };
        self.store_element_registry(copy, registry, registries.document);
    }

    /// DOM #concept-node-clone step 6.2–6.3 and 6.6 for a clonable shadow
    /// root's copy.
    pub(super) fn clone_shadow_registry(
        &mut self,
        source_root: NodeId,
        copy_root: NodeId,
        registries: CloneRegistries,
    ) {
        let data = self
            .shadow_data
            .get(&source_root)
            .copied()
            .unwrap_or_default();
        let registry = match data.registry.unwrap_or(registries.source_default) {
            CeRegistry::Global => registries.document.effective_global(),
            registry => registry,
        };
        self.store_shadow_registry(copy_root, registry, registries.document);
        if let Some(copy) = self.shadow_data.get_mut(&copy_root) {
            copy.keep_registry_null = data.keep_registry_null;
        }
        self.ce_registries.explicit_shadows |= data.keep_registry_null;
    }

    /// A copy of an inert tree's node stays in the inert document.
    pub(super) fn note_clone_root(&mut self, copy: NodeId, inert: bool) {
        if inert {
            self.note_inert_copy(copy);
        }
    }

    /// A cloned Document keeps a scoped registry (DOM #clone-a-single-node,
    /// Document).
    pub(super) fn clone_document_registry(&mut self, source: NodeId, copy: NodeId) {
        if let Some(&registry @ CeRegistry::Scoped(_)) = self.ce_registries.documents.get(&source) {
            self.ce_registries.documents.insert(copy, registry);
            self.ce_registries.scoped_documents = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dom::SelectorList;
    use CeRegistry::{Global, Null, Scoped};

    fn query(dom: &Dom, root: NodeId, selector: &str) -> NodeId {
        dom.query(root, &SelectorList::parse(selector).unwrap(), true)[0]
    }

    #[test]
    fn declarative_null_registries_are_kept_by_the_parsed_tree() {
        // HTML #parsing-main-inhead: shadowrootcustomelementregistry gives a
        // null registry that the parser's intended parents look up.
        let dom = Dom::parse_document(
            r#"<div id=keep><template shadowrootmode=open shadowrootserializable
                 shadowrootcustomelementregistry><x-a id=a><x-b id=b></x-b></x-a>
                 <p id=nested><template shadowrootmode=open><x-c id=c></x-c></template></p>
               </template></div>
               <div id=plain><template shadowrootmode=open><x-d id=d></x-d></template></div>
               <template id=t><x-e id=e></x-e></template><x-f id=f></x-f>"#,
        );
        let keep = dom.shadow_root(dom.get_by_id("keep").unwrap()).unwrap();
        let plain = dom.shadow_root(dom.get_by_id("plain").unwrap()).unwrap();
        assert_eq!(dom.shadow_registry(keep), Null);
        assert!(dom.shadow_data[&keep].keep_registry_null);
        for id in ["#a", "#b"] {
            assert_eq!(dom.element_registry(query(&dom, keep, id)), Null, "{id}");
        }
        // A nested declaration without the attribute has its document's.
        let nested = dom.shadow_root(query(&dom, keep, "#nested")).unwrap();
        assert_eq!(dom.shadow_registry(nested), Global);
        assert_eq!(dom.element_registry(query(&dom, nested, "#c")), Global);
        assert_eq!(dom.shadow_registry(plain), Global);
        assert_eq!(dom.element_registry(query(&dom, plain, "#d")), Global);
        assert_eq!(dom.element_registry(dom.get_by_id("f").unwrap()), Global);
        // Template contents belong to an inert document without a registry.
        let contents = dom.content_target(dom.get_by_id("t").unwrap());
        assert_eq!(dom.element_registry(dom.children(contents)[0]), Null);
        let markup = dom.html_with_shadow_roots(dom.get_by_id("keep").unwrap(), true, Vec::new());
        assert!(
            markup.contains(r#"shadowrootcustomelementregistry="""#),
            "{markup}"
        );
    }

    #[test]
    fn insertion_and_adoption_run_the_registry_steps() {
        // DOM #concept-node-adopt step 3 through the conceptual documents.
        let mut dom =
            Dom::parse_document("<body id=body><template id=t><x-a></x-a></template></body>");
        let body = dom.get_by_id("body").unwrap();
        let contents = dom.content_target(dom.get_by_id("t").unwrap());
        let source = dom.children(contents)[0];
        // A clone of template contents stays in the inert document until
        // inserted into the document.
        let copy = dom.clone_subtree(source, true);
        assert_eq!(dom.element_registry(copy), Null);
        dom.append(body, copy);
        assert_eq!(dom.element_registry(copy), Global);
        dom.detach(copy);
        assert_eq!(dom.element_registry(copy), Global, "adoption is permanent");
        // With recorded registries: a scoped registry is kept, a global one
        // follows the new document, a null one with a parent looks it up.
        let scoped = dom.create_element("x-s");
        dom.set_element_registry(scoped, Scoped(7));
        let null_child = dom.create_element("x-n");
        dom.append(scoped, null_child);
        dom.set_element_registry(null_child, Null);
        let other = dom.parse_document_into("<p>");
        dom.adopt_node(other, scoped).unwrap();
        assert_eq!(dom.element_registry(scoped), Scoped(7));
        assert_eq!(dom.element_registry(null_child), Null);
        dom.append(body, scoped);
        assert_eq!(dom.element_registry(scoped), Scoped(7));
        // A null element under a scoped parent looks up the parent's, whose
        // effective global registry is null.
        assert_eq!(dom.element_registry(null_child), Null);
        let global = dom.create_element("x-g");
        dom.append(scoped, global);
        dom.adopt_node(other, global).unwrap();
        assert_eq!(
            dom.element_registry(global),
            Null,
            "a Document without a registry"
        );
        dom.append(body, global);
        assert_eq!(dom.element_registry(global), Global);
        // A shadow root keeping its null registry.
        let host = dom.create_element("div");
        dom.append(body, host);
        let root = dom.attach_shadow(host);
        dom.set_shadow_registry(root, Null, true);
        dom.adopt_node(other, host).unwrap();
        dom.append(body, host);
        assert_eq!(dom.shadow_registry(root), Null);
    }

    #[test]
    fn clones_take_registries_from_the_source_and_fallback() {
        // DOM #clone-a-single-node steps 2.1–2.3 and #concept-node-clone.
        let mut dom = Dom::parse_document(
            "<body id=body><template id=t><x-a><x-b></x-b></x-a></template></body>",
        );
        let contents = dom.content_target(dom.get_by_id("t").unwrap());
        let imported = dom.clone_subtree_into(contents, true, DOCUMENT, Scoped(3));
        let a = dom.children(imported)[0];
        let b = dom.children(a)[0];
        assert_eq!(
            (dom.element_registry(a), dom.element_registry(b)),
            (Scoped(3), Scoped(3))
        );
        let fallback_global = dom.clone_subtree_into(contents, true, DOCUMENT, Global);
        assert_eq!(
            dom.element_registry(dom.children(fallback_global)[0]),
            Global
        );
        // A global source keeps its (the document's) registry.
        let body = dom.get_by_id("body").unwrap();
        let element = dom.create_element("x-c");
        dom.append(body, element);
        let copy = dom.clone_subtree_into(element, false, DOCUMENT, Scoped(3));
        assert_eq!(dom.element_registry(copy), Global);
        // A clonable shadow root keeps a scoped registry; its children get
        // no fallback.
        let host = dom.create_element("div");
        let root = dom
            .attach_shadow_with_options(
                host,
                shadow::ShadowRootData {
                    clonable: true,
                    ..Default::default()
                },
            )
            .unwrap();
        dom.set_shadow_registry(root, Scoped(5), false);
        let copy = dom.clone_subtree(host, false);
        let copy_root = dom.shadow_root(copy).unwrap();
        assert_eq!(dom.shadow_registry(copy_root), Scoped(5));
    }

    #[test]
    fn initialize_sets_null_registries_and_returns_candidates() {
        // HTML #dom-customelementregistry-initialize steps 2–4.
        let mut dom = Dom::parse_document(
            r#"<div id=keep><template shadowrootmode=open shadowrootcustomelementregistry>
                 <x-a id=a></x-a><p id=p></p></template></div>"#,
        );
        let root = dom.shadow_root(dom.get_by_id("keep").unwrap()).unwrap();
        let a = query(&dom, root, "#a");
        let p = query(&dom, root, "#p");
        assert_eq!(dom.initialize_registry(root, Scoped(9)), vec![a]);
        assert_eq!(dom.shadow_registry(root), Scoped(9));
        assert_eq!(dom.element_registry(p), Scoped(9));
        // Registries, once set, are not replaced.
        assert_eq!(dom.initialize_registry(root, Global), Vec::<NodeId>::new());
        assert_eq!(dom.element_registry(a), Scoped(9));
        // A Document without a registry: its tree takes the registry, its
        // other nodes keep null.
        let document = dom.parse_document_into("<x-b id=b></x-b>");
        let detached = dom.create_element("x-c");
        dom.adopt_node(document, detached).unwrap();
        assert_eq!(dom.element_registry(detached), Null);
        let candidates = dom.initialize_registry(document, Scoped(4));
        assert_eq!(dom.document_registry(document), Scoped(4));
        assert_eq!(candidates.len(), 1);
        assert_eq!(dom.element_registry(candidates[0]), Scoped(4));
        assert_eq!(dom.element_registry(detached), Null);
    }

    #[test]
    fn fragment_parsing_uses_the_target_registry() {
        let mut dom = Dom::parse_document("<body id=body></body>");
        let body = dom.get_by_id("body").unwrap();
        let host = dom.create_element("div");
        dom.append(body, host);
        let root = dom.attach_shadow(host);
        dom.set_shadow_registry(root, Scoped(2), false);
        let nodes = dom
            .fragment_parse(root, Some(root), "<x-a><x-b></x-b></x-a>", false)
            .unwrap();
        dom.replace_all_children(root, nodes);
        let a = dom.children(root)[0];
        assert_eq!(dom.element_registry(a), Scoped(2));
        assert_eq!(dom.element_registry(dom.children(a)[0]), Scoped(2));
        assert_eq!(dom.upgrade_candidates(DOCUMENT, "x-a", Scoped(2)), vec![a]);
        assert!(dom.upgrade_candidates(DOCUMENT, "x-a", Global).is_empty());
        assert_eq!(dom.live_scoped_registries(), [2].into_iter().collect());
    }
}
