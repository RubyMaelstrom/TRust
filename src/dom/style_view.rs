//! Read-only style inputs shared by the serial and parallel selector matchers.
//!
//! Selectors 4 #match-against-element is a function of the element tree, its
//! attributes and a few document states: the hover chain, focused areas, the
//! fragment target, popover/modal flags and form-control values. `StyleView`
//! holds plain references to exactly those inputs and the matcher is
//! implemented once, on the view. The DOM's lazily filled `RefCell` caches are
//! deliberately unreachable from it; callers pass per-thread memo storage in
//! `SelectorContext` instead.
//!
//! The view is not `Sync` by itself: the node arena stores attribute values as
//! `StrTendril`s, which are `!Sync`. `SharedView` is the one narrowly scoped
//! exception, used only while `style_pool` runs a parallel pass.

use super::*;

/// Read access to the node arena. This is the only path by which matcher code
/// reaches `Node` data, and it only ever yields ids, booleans, names and
/// `&str` slices: never `&Node`, `&Attribute` or `&StrTendril`. `SharedView`'s
/// safety argument depends on that, so keep new accessors in the same shape.
#[derive(Clone, Copy)]
pub(super) struct NodesRef<'a>(&'a arena::DenseIdMap<Node>);

impl<'a> NodesRef<'a> {
    #[inline]
    pub(super) fn new(nodes: &'a arena::DenseIdMap<Node>) -> Self {
        Self(nodes)
    }

    #[inline]
    pub(super) fn contains(self, id: NodeId) -> bool {
        self.0.get(id).is_some()
    }

    /// Tree parent (DOM parent, not composed). Panics for an unknown id.
    #[inline]
    pub(super) fn parent(self, id: NodeId) -> Option<NodeId> {
        self.0[id].parent
    }

    /// Tree parent, `None` for an unknown id.
    #[inline]
    pub(super) fn parent_of(self, id: NodeId) -> Option<NodeId> {
        self.0.get(id)?.parent
    }

    /// `None` for an unknown id, else its tree parent.
    #[inline]
    pub(super) fn try_parent(self, id: NodeId) -> Option<Option<NodeId>> {
        self.0.get(id).map(|node| node.parent)
    }

    #[inline]
    pub(super) fn first_child(self, id: NodeId) -> Option<NodeId> {
        self.0[id].first_child
    }

    /// First child, `None` for an unknown id.
    #[inline]
    pub(super) fn first_child_of(self, id: NodeId) -> Option<NodeId> {
        self.0.get(id)?.first_child
    }

    #[inline]
    pub(super) fn last_child(self, id: NodeId) -> Option<NodeId> {
        self.0[id].last_child
    }

    #[inline]
    pub(super) fn next_sibling(self, id: NodeId) -> Option<NodeId> {
        self.0[id].next_sibling
    }

    #[inline]
    pub(super) fn prev_sibling(self, id: NodeId) -> Option<NodeId> {
        self.0[id].prev_sibling
    }

    #[inline]
    pub(super) fn owner_document(self, id: NodeId) -> NodeId {
        self.0[id].owner_document
    }

    #[inline]
    pub(super) fn is_document(self, id: NodeId) -> bool {
        matches!(self.0[id].data, NodeData::Document)
    }

    #[inline]
    pub(super) fn is_element(self, id: NodeId) -> bool {
        matches!(self.0[id].data, NodeData::Element { .. })
    }

    /// Whether `id` can have element descendants in its tree: an element, a
    /// Document or a DocumentFragment, rather than character data.
    #[inline]
    pub(super) fn is_container(self, id: NodeId) -> bool {
        matches!(
            self.0[id].data,
            NodeData::Element { .. } | NodeData::Document | NodeData::Fragment
        )
    }

    /// The element's namespace URI, `None` for non-elements or no namespace.
    #[inline]
    pub(super) fn namespace_uri(self, id: NodeId) -> Option<&'a str> {
        match &self.0.get(id)?.data {
            NodeData::Element { name, .. } => {
                let ns = &*name.ns;
                (!ns.is_empty()).then_some(ns)
            }
            _ => None,
        }
    }

    /// The character data of a Text node.
    #[inline]
    pub(super) fn text(self, id: NodeId) -> Option<&'a str> {
        match &self.0[id].data {
            NodeData::Text(text) => Some(text),
            _ => None,
        }
    }

    #[inline]
    pub(super) fn tag_name(self, id: NodeId) -> Option<&'a str> {
        match &self.0.get(id)?.data {
            NodeData::Element { name, .. } => Some(&name.local),
            _ => None,
        }
    }

    /// The renderer's historical ASCII case-insensitive attribute lookup.
    #[inline]
    pub(super) fn attr(self, id: NodeId, name: &str) -> Option<&'a str> {
        match &self.0.get(id)?.data {
            NodeData::Element { attrs, .. } => attrs
                .iter()
                .find(|a| str::eq_ignore_ascii_case(&a.name.local, name))
                .map(|a| &*a.value),
            _ => None,
        }
    }

    /// Every attribute's local name, in source order.
    #[inline]
    pub(super) fn attr_names(self, id: NodeId) -> impl Iterator<Item = &'a str> {
        let attrs = match &self.0[id].data {
            NodeData::Element { attrs, .. } => attrs.as_slice(),
            _ => &[],
        };
        attrs.iter().map(|attribute| attribute.name.local.as_ref())
    }

    #[inline]
    pub(super) fn child_iter(self, id: NodeId) -> impl Iterator<Item = NodeId> + 'a {
        std::iter::successors(self.first_child_of(id), move |&c| self.next_sibling(c))
    }

    #[inline]
    pub(super) fn descendants(self, root: NodeId) -> Descendants<'a> {
        Descendants {
            nodes: self,
            root,
            next: self.first_child_of(root),
        }
    }
}

/// Document state consulted by selector matching, apart from the node arena.
/// Every field must be `Sync` on its own (asserted below), so that
/// `SharedView` needs no argument beyond the arena's.
#[derive(Clone, Copy)]
pub(super) struct ViewState<'a> {
    pub(super) shadow_hosts: &'a FxHashMap<NodeId, NodeId>,
    pub(super) shadow_roots: &'a FxHashMap<NodeId, NodeId>,
    pub(super) shadow_data: &'a FxHashMap<NodeId, shadow::ShadowRootData>,
    pub(super) hover_chain: &'a FxHashSet<NodeId>,
    pub(super) fragment_target: Option<NodeId>,
    pub(super) popover_open: &'a FxHashSet<NodeId>,
    pub(super) modal_dialogs: &'a FxHashSet<NodeId>,
    pub(super) focused_areas: &'a FxHashMap<NodeId, NodeId>,
    pub(super) input_values: &'a FxHashMap<NodeId, input::InputValue>,
    pub(super) render_live: bool,
    pub(super) document_modes: &'a FxHashMap<NodeId, QuirksMode>,
    pub(super) cssom_inline: &'a FxHashMap<NodeId, cssom::Declarations>,
}

const _: () = {
    const fn sync<T: Sync>() {}
    sync::<ViewState<'static>>();
    // `NodesRef` hands out these; they must not need the arena's exemption.
    sync::<QualName>();
    sync::<Complex>();
    sync::<RuleBuckets>();
};

/// The matcher's complete input: the node arena plus document state.
#[derive(Clone, Copy)]
pub(super) struct StyleView<'a> {
    pub(super) nodes: NodesRef<'a>,
    pub(super) state: ViewState<'a>,
}

impl Dom {
    /// The read-only matching inputs of this arena. Cheap: a few references.
    #[inline]
    pub(super) fn style_view(&self) -> StyleView<'_> {
        StyleView {
            nodes: NodesRef(&self.nodes),
            state: ViewState {
                shadow_hosts: &self.shadow_hosts,
                shadow_roots: &self.shadow_roots,
                shadow_data: &self.shadow_data,
                hover_chain: &self.hover_chain,
                fragment_target: self.fragment_target,
                popover_open: &self.popover_open,
                modal_dialogs: &self.modal_dialogs,
                focused_areas: &self.focused_areas,
                input_values: &self.input_values,
                render_live: self.render_live,
                document_modes: &self.document_modes,
                cssom_inline: &self.cssom_inline,
            },
        }
    }
}

impl StyleView<'_> {
    /// Selectors 4 #match-against-element for every rule of `id`'s tree
    /// scope that the rule hash admits: the matching rule indices, ascending.
    /// `may_match_ancestors` may reject a candidate only when some key it
    /// requires occurs on none of `id`'s composed ancestors. Returns the
    /// number of candidates tested.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn match_rules<'r>(
        &self,
        id: NodeId,
        buckets: &RuleBuckets,
        selector: impl Fn(u32) -> &'r Complex,
        shadow_host: Option<NodeId>,
        classes: ClassMemo<'_>,
        may_match_ancestors: impl FnMut(&[u16]) -> bool,
        candidates: &mut Vec<u32>,
        out: &mut Vec<u32>,
    ) -> u64 {
        let memo = RefCell::new(rule_index::MatchMemo::default());
        // A logical selector can have several alternative keys. Deduplicate
        // before matching so an element satisfying two alternatives (or
        // repeating a class) tests the rule once.
        candidates.clear();
        buckets.candidates_filtered(self, id, candidates, may_match_ancestors);
        // Amortize a lazy ancestor index on large candidate sets. It is
        // populated only if a descendant combinator needs it.
        let ancestors = (candidates.len() >= 32)
            .then(|| RefCell::new(rule_index::AncestorMatches::new(id, shadow_host)));
        let context = SelectorContext {
            scope: None,
            shadow_host,
            memo: Some(&memo),
            memo_prefixes: false,
            ancestors: ancestors.as_ref(),
            classes,
        };
        for &ri in candidates.iter() {
            if self.matches_complex_uncached(id, &selector(ri).0, context) {
                out.push(ri);
            }
        }
        candidates.len() as u64
    }
}

/// A `StyleView` that style worker threads share during one parallel pass.
pub(super) struct SharedView<'a>(StyleView<'a>);

// SAFETY: `ViewState` is `Sync` (asserted above), so the only non-`Sync`
// component is `NodesRef`, and only because attribute values are
// `StrTendril`s. A tendril is `!Sync` because `&StrTendril` permits `clone()`,
// which writes its non-atomic header (the `Cell` pointer and refcount). Every
// other `Node` field is plain owned data or an atomically refcounted
// `string_cache` atom. `NodesRef` never clones a tendril and never exposes
// `&Node`, `&Attribute` or `&StrTendril`; it only dereferences values to
// `&str`, which reads the header without writing it. Concurrent reads are
// race-free as long as no thread writes a header while the view is shared,
// which is the contract of `SharedView::new`. Arena mutation itself needs
// `&mut Dom`, which the `'a` borrow excludes.
unsafe impl Sync for SharedView<'_> {}

impl<'a> SharedView<'a> {
    /// # Safety
    ///
    /// While a reference to the returned value may be used on another thread,
    /// the arena's owning thread must not clone, or otherwise write through a
    /// shared reference to, any attribute tendril of this arena. Share it only
    /// with a `style_pool::Pool::run` job: workers can reach it only inside
    /// the pass (the job is `Sync`, so it cannot capture `&Dom` either), and
    /// `run` returns only after every worker has left. Meanwhile the owning
    /// thread's `lead` closure must only read the tree (as `NodesRef` does)
    /// and write state outside the node arena.
    pub(super) unsafe fn new(view: StyleView<'a>) -> Self {
        Self(view)
    }

    #[inline]
    pub(super) fn view(&self) -> &StyleView<'a> {
        &self.0
    }
}

/// Where long class lists keep their tokens during one match (see
/// `class_tokens`). The document cache is main-thread state; a style worker
/// brings its own map for one parallel pass.
#[derive(Clone, Copy, Default)]
pub(super) enum ClassMemo<'a> {
    /// Tokenize on every test (allocation-free for short lists).
    #[default]
    None,
    /// The arena's attribute-owned cache, invalidated by class writes.
    Document(&'a RefCell<NodeCache<class_tokens::ClassTokens>>),
    /// A worker-private cache, discarded after the pass.
    Local(&'a RefCell<FxHashMap<NodeId, class_tokens::ClassTokens>>),
}
