//! Shared cascade and common computed-property tables.
//!
//! CSS Cascade 5 #filtering, #computed, #inheriting and Selectors 4
//! #match-against-element (local CSSWG 81c27f686901): matching remains per
//! element. Equal ordered rule/declaration inputs produce equal cascade maps;
//! common values also require equal parent computation and UA-default context.
//! Unregistered variables are part of that complete cascade/parent context:
//! equal inputs have equal computed token streams, including invalid cycles.
//! Used geometry, transitions and registered-property computations stay local.
//! Weak identities pin allocation addresses. A bounded input-owned graph can
//! also retain completed cascades and common rows across node invalidation:
//! every node still rematches selectors and reconstructs its complete input.
//! CSS Logical 1 #box additionally requires the inherited computation when
//! physicalizing logical declarations, including explicit logical inheritance.

use super::*;
use std::{
    hash::{Hash, Hasher},
    rc::{Rc, Weak},
};

const MAX_ENTRIES: usize = 1024;
const MAX_KEY_BYTES: usize = 2 * 1024 * 1024;
const MAX_CASCADE_BYTES: usize = 4 * 1024 * 1024;
const MAX_ROW_BYTES: usize = 8 * 1024 * 1024;

pub(super) fn persistent_contexts_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("TRUST_PERSISTENT_STYLE_CONTEXTS").as_deref() != Ok("0"))
}

/// Charges follow actual row allocations, including later lazy population.
/// When a write exceeds the bound, its caller drops graph owners before
/// returning. Active node caches remain ordinary semantic owners.
#[derive(Default)]
pub(super) struct RowBudget(Rc<Cell<usize>>);

pub(super) struct RowCharge {
    bytes: usize,
    budget: Rc<Cell<usize>>,
}

impl RowBudget {
    pub(super) fn reserve(&self, bytes: usize) -> Option<RowCharge> {
        let total = self.0.get().checked_add(bytes)?;
        if total > MAX_ROW_BYTES {
            return None;
        }
        self.0.set(total);
        Some(RowCharge {
            bytes,
            budget: self.0.clone(),
        })
    }
}

impl RowCharge {
    pub(super) fn resize(&mut self, added: usize, removed: usize) {
        self.budget.set(self.budget.get() - removed + added);
        self.bytes = self.bytes - removed + added;
    }
}

impl Drop for RowCharge {
    fn drop(&mut self) {
        self.budget.set(self.budget.get() - self.bytes);
    }
}

pub(super) fn enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("TRUST_STYLE_SHARING").as_deref() != Ok("0"))
}

pub(super) fn variable_contexts_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("TRUST_VARIABLE_STYLE_CONTEXTS").as_deref() != Ok("0"))
}

#[derive(Hash, PartialEq, Eq)]
pub(super) struct CascadeInput {
    scope: NodeId,
    parent: Option<Identity<RefCell<computed_cache::Row>>>,
    rules: Rc<Vec<u32>>,
    inline_text: Option<String>,
    inline_declarations: Option<cssom::Declarations>,
    hints: Vec<(&'static str, String)>,
}

impl CascadeInput {
    fn bytes(&self) -> usize {
        self.rules.capacity() * 4
            + self.inline_text.as_ref().map_or(0, String::capacity)
            + self.inline_declarations.as_ref().map_or(0, |ds| {
                ds.iter()
                    .map(|(name, value, _)| name.len() + value.len() + 64)
                    .sum()
            })
            + self
                .hints
                .iter()
                .map(|(_, value)| value.len() + 40)
                .sum::<usize>()
            + std::mem::size_of::<Self>()
    }
}

struct Identity<T>(Weak<T>);
impl<T> PartialEq for Identity<T> {
    fn eq(&self, other: &Self) -> bool {
        self.0.ptr_eq(&other.0)
    }
}
impl<T> Eq for Identity<T> {}
impl<T> Hash for Identity<T> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        (self.0.as_ptr() as usize).hash(state);
    }
}

#[derive(Hash, PartialEq, Eq)]
struct ComputationInput {
    cascade: Identity<CascadedMaps>,
    parent: Option<Identity<RefCell<computed_cache::Row>>>,
    tag: String,
    ua: UaContext,
}

/// HTML Rendering, Phrasing content / Lists / Tables / Form controls / Details
/// and summary (local HTML e5071a20, source lines 151202/151475/151665/152031/
/// 152908). These are
/// additional UA-cascade inputs, not reasons to make an entire subtree private.
/// Store the resulting bounded UA state, never a node identity or attribute text.
#[derive(Hash, PartialEq, Eq)]
enum UaContext {
    Ordinary,
    Link(bool),
    List(&'static str),
    Input(bool),
    Summary(Option<bool>),
    Align(Option<&'static str>),
}

impl UaContext {
    fn of(dom: &Dom, id: NodeId, tag: &str) -> Self {
        match tag {
            "a" | "area" => Self::Link(dom.attr(id, "href").is_some()),
            "ul" => Self::List(dom.ul_marker_default(id)),
            "ol" => Self::List(dom.ol_marker_default(id)),
            "input" => Self::Input(dom.ua_input_border_box(id)),
            "summary" => Self::Summary(
                dom.is_details_summary(id)
                    .then(|| dom.attr(dom.nodes[id].parent.unwrap(), "open").is_some()),
            ),
            "p" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                Self::Align(dom.ua_paragraph_align(id, tag))
            }
            _ => Self::Ordinary,
        }
    }
}

#[derive(Default)]
pub(super) struct State {
    stamp: Option<(u64, u64, u64)>,
    cascades: FxHashMap<CascadeInput, Weak<CascadedMaps>>,
    computations: FxHashMap<ComputationInput, Weak<RefCell<computed_cache::Row>>>,
    key_bytes: usize,
    cascades_owned: Vec<Rc<CascadedMaps>>,
    cascade_bytes: usize,
    rows_owned: Vec<computed_cache::SharedRow>,
    row_budget: RowBudget,
    preparing_depth: Rc<Cell<usize>>,
}

struct Preparing(Rc<Cell<usize>>);
impl Drop for Preparing {
    fn drop(&mut self) {
        self.0.set(self.0.get() - 1);
    }
}

impl State {
    fn synchronize(&mut self, stamp: (u64, u64, u64)) {
        // Rules, viewport/base URL and page fonts define the environment.
        // A node/value invalidation changes its input, not equal inputs.
        let stamp = if persistent_contexts_enabled() {
            (stamp.0, 0, stamp.2)
        } else {
            stamp
        };
        if self.stamp != Some(stamp) {
            self.clear_graph();
            self.stamp = Some(stamp);
        }
    }

    fn clear_graph(&mut self) {
        self.cascades.clear();
        self.computations.clear();
        self.key_bytes = 0;
        self.cascades_owned.clear();
        self.cascade_bytes = 0;
        self.rows_owned.clear();
        self.row_budget = RowBudget::default();
    }

    pub(super) fn trim_payloads(&mut self) {
        if self.row_budget.0.get() > MAX_ROW_BYTES {
            self.clear_graph();
        }
    }

    pub(super) fn retained_bytes(&self) -> usize {
        let mut bytes = self.key_bytes
            + self.cascades_owned.capacity() * std::mem::size_of::<Rc<CascadedMaps>>()
            + self.rows_owned.capacity() * std::mem::size_of::<computed_cache::SharedRow>()
            // Node-owned payloads are reported by the canonical node caches.
            // Count a graph-only allocation exactly once when those owners leave.
            + self.cascades_owned.iter().filter(|value| Rc::strong_count(value) == 1)
                .map(|value| cascade_bytes(value)).sum::<usize>()
            + self.rows_owned.iter().filter(|value| Rc::strong_count(value) == 1)
                .map(|value| computed_cache::Row::allocation_bytes(&value.borrow())).sum::<usize>()
            + self.cascades.capacity() * std::mem::size_of::<(CascadeInput, Weak<CascadedMaps>)>()
            + self.computations.capacity()
                * std::mem::size_of::<(ComputationInput, Weak<RefCell<computed_cache::Row>>)>()
            + self
                .computations
                .keys()
                .map(|key| key.tag.capacity())
                .sum::<usize>();
        let mut dead_cascades = FxHashSet::default();
        let mut dead_rows = FxHashSet::default();
        for weak in self
            .cascades
            .values()
            .chain(self.computations.keys().map(|key| &key.cascade.0))
        {
            if weak.strong_count() == 0 && dead_cascades.insert(weak.as_ptr()) {
                bytes += std::mem::size_of::<CascadedMaps>() + 2 * std::mem::size_of::<usize>();
            }
        }
        for weak in self.computations.values().chain(
            self.computations
                .keys()
                .filter_map(|key| key.parent.as_ref().map(|parent| &parent.0)),
        ) {
            if weak.strong_count() == 0 && dead_rows.insert(weak.as_ptr()) {
                bytes += std::mem::size_of::<RefCell<computed_cache::Row>>()
                    + 2 * std::mem::size_of::<usize>();
            }
        }
        for weak in self
            .cascades
            .keys()
            .filter_map(|key| key.parent.as_ref().map(|parent| &parent.0))
        {
            if weak.strong_count() == 0 && dead_rows.insert(weak.as_ptr()) {
                bytes += std::mem::size_of::<RefCell<computed_cache::Row>>()
                    + 2 * std::mem::size_of::<usize>();
            }
        }
        bytes
    }
}

fn cascade_bytes(value: &CascadedMaps) -> usize {
    std::mem::size_of::<CascadedMaps>()
        + 2 * std::mem::size_of::<usize>()
        + [&value.elem, &value.before, &value.after]
            .into_iter()
            .map(|map| {
                map.capacity() * std::mem::size_of::<(String, String)>()
                    + map
                        .iter()
                        .map(|(key, value)| key.capacity() + value.capacity())
                        .sum::<usize>()
            })
            .sum::<usize>()
        + value.custom_bases.capacity()
            * std::mem::size_of::<((Option<PseudoEl>, String), Rc<url::Url>)>()
        + value
            .custom_bases
            .keys()
            .map(|(_, key)| key.capacity())
            .sum::<usize>()
}

impl Dom {
    fn sharing_stamp(&self) -> (u64, u64, u64) {
        (
            self.style_epoch,
            self.style_value_epoch,
            crate::font_system::page_font_epoch(),
        )
    }

    fn shareable_cascade_context(&self, id: NodeId) -> bool {
        if !enabled()
            || self.css_transitions_active()
            || self.namespace_uri(id) != Some("http://www.w3.org/1999/xhtml")
            || self.shadow_roots.contains_key(&id)
        {
            return false;
        }
        let index = self.style_index();
        // These paths provisionally install a map and consult node-dependent
        // values while finishing it. Keep their existing computation intact.
        !index.has_revert_layer
            && !index.has_container_queries
            && !index.has_container_units
            && !self
                .attr(id, "style")
                .is_some_and(super::mentions_container_unit)
            && index.slotted_rules.is_empty()
            && !self
                .attr(id, "style")
                .is_some_and(|text| text.to_ascii_lowercase().contains("revert-layer"))
            && !self.cssom_inline.get(&id).is_some_and(|ds| {
                ds.iter()
                    .any(|(_, value, _)| value.to_ascii_lowercase().contains("revert-layer"))
            })
    }

    pub(super) fn shared_cascade_input(&self, id: NodeId) -> Option<CascadeInput> {
        if !self.shareable_cascade_context(id) {
            return None;
        }
        // Parent preparation can itself ask for a cascade before its caller
        // reaches the ordinary depth check. Bound that dependency walk too.
        let depth = self.style_sharing.borrow().preparing_depth.clone();
        if depth.get() >= 128 {
            return None;
        }
        depth.set(depth.get() + 1);
        let _preparing = Preparing(depth);
        if self
            .attr(id, "style")
            .is_some_and(|text| text.len() > MAX_KEY_BYTES)
            || self.cssom_inline.get(&id).is_some_and(|ds| {
                ds.iter()
                    .map(|(name, value, _)| name.len() + value.len() + 64)
                    .sum::<usize>()
                    > MAX_KEY_BYTES
            })
        {
            return None;
        }
        let mut hints = Vec::new();
        self.html_presentational_hints(id, |name, value| hints.push((name, value)));
        Some(CascadeInput {
            scope: self.tree_scope(id),
            // The cascade physicalizes logical declarations before publication.
            // Equal rule IDs alone do not prove equal inherited axes or values.
            parent: self
                .style_parent(id)
                .map(|parent| Identity(Rc::downgrade(&self.prepare_computed_row(parent, 0)))),
            rules: self.matched_rules(id),
            inline_text: self.attr(id, "style").map(str::to_owned),
            inline_declarations: self.cssom_inline.get(&id).cloned(),
            hints,
        })
    }

    pub(super) fn shared_cascade_get(&self, key: &CascadeInput) -> Option<Rc<CascadedMaps>> {
        let mut state = self.style_sharing.borrow_mut();
        state.synchronize(self.sharing_stamp());
        state.cascades.get(key).and_then(Weak::upgrade)
    }

    pub(super) fn shared_cascade_put(&self, key: CascadeInput, maps: &Rc<CascadedMaps>) {
        let bytes = key.bytes();
        if bytes > MAX_KEY_BYTES {
            return;
        }
        let mut state = self.style_sharing.borrow_mut();
        state.synchronize(self.sharing_stamp());
        if state.cascades.len() >= MAX_ENTRIES
            || state.key_bytes.saturating_add(bytes) > MAX_KEY_BYTES
        {
            state.clear_graph();
        }
        // Replacing an expired weak entry must not accumulate phantom bytes.
        if let Some((old, _)) = state.cascades.remove_entry(&key) {
            state.key_bytes -= old.bytes();
        }
        state.key_bytes += bytes;
        state.cascades.insert(key, Rc::downgrade(maps));
        if persistent_contexts_enabled() {
            let bytes = cascade_bytes(maps);
            if bytes <= MAX_CASCADE_BYTES {
                if state.cascade_bytes + bytes > MAX_CASCADE_BYTES {
                    // The input directory is weak; dropping only these owners
                    // cannot invalidate an active node's canonical cache.
                    state.cascades_owned.clear();
                    state.cascade_bytes = 0;
                }
                state.cascade_bytes += bytes;
                state.cascades_owned.push(maps.clone());
            }
        }
    }

    pub(super) fn prepare_computed_row(
        &self,
        id: NodeId,
        depth: usize,
    ) -> computed_cache::SharedRow {
        let stamp = (
            self.style_value_epoch,
            crate::font_system::page_font_epoch(),
        );
        let original = {
            let mut cache = self.computed_cache.borrow_mut();
            if cache.0 != stamp {
                cache.0 = stamp;
                cache.1.clear();
            }
            if let Some(row) = cache.1.row(id) {
                return row;
            }
            // Install a private row before recursion. A query that returns to
            // this node sees canonical partial computation, never a borrow panic.
            cache.1.ensure_row(id)
        };
        // CSS Animations 1 #animations: animated values change on their own
        // timeline, so an animated element (and thereby its subtree) never
        // shares a computed row with an equal cascade.
        if depth >= 128 || !self.shareable_cascade_context(id) || self.animations.private_row(id) {
            return original;
        }
        let Some(tag) = self.tag_name(id) else {
            return original;
        };
        if tag.len() > 128 {
            return original;
        }
        let maps = self.cascaded_maps(id);
        let document = self.registration_document(id);
        // CSS Variables 1 #using-variables draws the computed custom value
        // from this element. Both its own complete declaration map and the
        // complete parent's computation identity participate in the key.
        // Thus inheritance, local overrides, fallback and cycles need no
        // per-element exclusion. Registered values introduce extra computed
        // unit dependencies; do not conflate them with plain token streams.
        if self
            .properties
            .javascript
            .get(&document)
            .is_some_and(|registry| !registry.is_empty())
            || self
                .style_index()
                .properties
                .get(&document)
                .is_some_and(|registry| !registry.is_empty())
            || (!variable_contexts_enabled()
                && maps.elem.iter().any(|(name, value)| {
                    name.starts_with("--")
                        || pending_shorthand(value).is_some()
                        || find_var_function(value).is_some()
                }))
        {
            return original;
        }
        let parent = self
            .style_parent(id)
            .map(|parent| self.prepare_computed_row(parent, depth + 1));
        let key = ComputationInput {
            cascade: Identity(Rc::downgrade(&maps)),
            parent: parent.as_ref().map(|row| Identity(Rc::downgrade(row))),
            tag: tag.to_owned(),
            ua: UaContext::of(self, id, tag),
        };
        // Cascading/parent preparation may settle a lazy dependency or observe
        // a newly installed font set. It can retire the provisional row. Only
        // publish into the exact row/generation that this preparation began
        // with; restarting as a private row needs no unbounded retry loop.
        let current_stamp = (
            self.style_value_epoch,
            crate::font_system::page_font_epoch(),
        );
        let mut cache = self.computed_cache.borrow_mut();
        if cache.0 != current_stamp {
            cache.0 = current_stamp;
            cache.1.clear();
        }
        if current_stamp != stamp
            || !cache
                .1
                .row(id)
                .is_some_and(|row| Rc::ptr_eq(&row, &original))
        {
            return cache.1.ensure_row(id);
        }
        let shared = {
            let mut state = self.style_sharing.borrow_mut();
            state.synchronize((self.style_epoch, stamp.0, stamp.1));
            if let Some(row) = state.computations.get(&key).and_then(Weak::upgrade) {
                row
            } else {
                if state.computations.len() >= MAX_ENTRIES {
                    state.clear_graph();
                }
                state.computations.insert(key, Rc::downgrade(&original));
                if persistent_contexts_enabled()
                    && original.borrow_mut().retain_in_graph(&state.row_budget)
                {
                    state.rows_owned.push(original.clone());
                }
                original
            }
        };
        cache.1.share_row(id, shared.clone());
        shared
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn value(dom: &Dom, id: &str, name: &str) -> Option<String> {
        dom.computed_value(dom.get_by_id(id).unwrap(), name)
    }

    #[test]
    fn style_sharing_repeated_subtrees_share_values_and_split_on_mutation() {
        // Shared rows are keyed by the process-wide page font epoch.
        let _inputs = crate::layout2::stable_global_layout_inputs();
        let mut dom = Dom::parse_document(
            r#"<style>
            .row {color:red;white-space:pre}.row.done {color:blue}
            .row label {margin-left:3px}
            </style><ul><li id=a class=row><label id=x>one</label></li>
            <li id=b class=row><label id=y>two</label></li></ul>"#,
        );
        assert_eq!(value(&dom, "x", "color").as_deref(), Some("red"));
        assert_eq!(value(&dom, "y", "color").as_deref(), Some("red"));
        let x = dom.get_by_id("x").unwrap();
        let y = dom.get_by_id("y").unwrap();
        if enabled() {
            assert!(Rc::ptr_eq(&dom.cascaded_maps(x), &dom.cascaded_maps(y)));
            let cache = dom.computed_cache.borrow();
            assert!(Rc::ptr_eq(
                &cache.1.row(x).unwrap(),
                &cache.1.row(y).unwrap()
            ));
        }
        let b = dom.get_by_id("b").unwrap();
        dom.set_attr(b, "class", "row done");
        assert_eq!(value(&dom, "y", "color").as_deref(), Some("blue"));
        assert_eq!(value(&dom, "x", "color").as_deref(), Some("red"));
        assert_eq!(value(&dom, "y", "margin-left").as_deref(), Some("3px"));
        dom.set_attr(b, "class", "row");
        assert_eq!(value(&dom, "y", "color").as_deref(), Some("red"));
        if enabled() {
            let cache = dom.computed_cache.borrow();
            assert!(Rc::ptr_eq(
                &cache.1.row(x).unwrap(),
                &cache.1.row(y).unwrap()
            ));
        }
    }

    #[test]
    fn style_sharing_keeps_inheritance_inline_edits_and_presentational_hints_distinct() {
        // Shared rows are keyed by the process-wide page font epoch.
        let _inputs = crate::layout2::stable_global_layout_inputs();
        let mut dom = Dom::parse_document(
            r#"<style>.child{width:inherit;color:inherit}</style>
            <div style='width:10px;color:red'><span id=x class=child>x</span></div>
            <div style='width:20px;color:blue'><span id=y class=child>y</span></div>
            <font id=f color=red>a</font><font id=g color=blue>b</font>
            <a id=link href=x>link</a><a id=plain>plain</a>"#,
        );
        assert_eq!(value(&dom, "x", "width").as_deref(), Some("10px"));
        assert_eq!(value(&dom, "y", "width").as_deref(), Some("20px"));
        assert_eq!(value(&dom, "x", "color").as_deref(), Some("red"));
        assert_eq!(value(&dom, "y", "color").as_deref(), Some("blue"));
        assert_ne!(value(&dom, "f", "color"), value(&dom, "g", "color"));
        assert_eq!(
            value(&dom, "link", "text-decoration-line").as_deref(),
            Some("underline")
        );
        assert_eq!(value(&dom, "plain", "text-decoration-line"), None);
        let y = dom.get_by_id("y").unwrap();
        dom.set_attr(y, "style", "color:green;width:30px");
        assert_eq!(value(&dom, "y", "color").as_deref(), Some("green"));
        assert_eq!(value(&dom, "y", "width").as_deref(), Some("30px"));
        assert_eq!(value(&dom, "x", "width").as_deref(), Some("10px"));
    }

    #[test]
    fn style_sharing_rechecks_structural_selectors_after_tree_changes() {
        // Shared rows are keyed by the process-wide page font epoch.
        let _inputs = crate::layout2::stable_global_layout_inputs();
        let mut dom = Dom::parse_document(
            r#"<style>
            li{color:red}li:nth-child(2n){color:blue}li:has(.flag){color:green}
            </style><ul id=p><li id=a>a</li><li id=b>b</li><li id=c>c</li></ul>"#,
        );
        assert_eq!(value(&dom, "a", "color").as_deref(), Some("red"));
        assert_eq!(value(&dom, "b", "color").as_deref(), Some("blue"));
        assert_eq!(value(&dom, "c", "color").as_deref(), Some("red"));
        let p = dom.get_by_id("p").unwrap();
        let a = dom.get_by_id("a").unwrap();
        let new = dom.create_element("li");
        dom.insert_before(p, new, Some(a));
        assert_eq!(value(&dom, "a", "color").as_deref(), Some("blue"));
        assert_eq!(value(&dom, "b", "color").as_deref(), Some("red"));
        let flag = dom.create_element("span");
        dom.set_attr(flag, "class", "flag");
        let b = dom.get_by_id("b").unwrap();
        dom.append(b, flag);
        assert_eq!(value(&dom, "b", "color").as_deref(), Some("green"));
        assert_eq!(value(&dom, "c", "color").as_deref(), Some("blue"));
    }

    #[test]
    fn style_sharing_preserves_custom_property_and_relative_font_context() {
        // Shared rows are keyed by the process-wide page font epoch.
        let _inputs = crate::layout2::stable_global_layout_inputs();
        let mut dom = Dom::parse_document(
            r#"<style>
            .p{--edge:3px;font-size:10px}.p.large{--edge:9px;font-size:20px}
            span{padding:var(--edge);line-height:2em;font-weight:bolder}
            </style><div id=a class=p><span id=x>x</span></div>
            <div id=b class='p large'><span id=y>y</span></div>"#,
        );
        let x = dom.get_by_id("x").unwrap();
        let y = dom.get_by_id("y").unwrap();
        assert_eq!(
            dom.computed_value_resolved(x, "padding-left").as_deref(),
            Some("3px")
        );
        assert_eq!(
            dom.computed_value_resolved(y, "padding-left").as_deref(),
            Some("9px")
        );
        assert_eq!(value(&dom, "x", "line-height").as_deref(), Some("20px"));
        assert_eq!(value(&dom, "y", "line-height").as_deref(), Some("40px"));
        let a = dom.get_by_id("a").unwrap();
        dom.set_attr(a, "class", "p large");
        assert_eq!(
            dom.computed_value_resolved(x, "padding-left").as_deref(),
            Some("9px")
        );
        assert_eq!(value(&dom, "x", "line-height").as_deref(), Some("40px"));
    }

    #[test]
    fn style_sharing_directories_release_property_payloads_when_graph_is_evicted() {
        // Shared rows are keyed by the process-wide page font epoch.
        let _inputs = crate::layout2::stable_global_layout_inputs();
        let dom = Dom::parse_document(
            "<style>span{color:red}</style><span id=x>x</span><span id=y>y</span>",
        );
        value(&dom, "x", "color");
        value(&dom, "y", "color");
        let x = dom.get_by_id("x").unwrap();
        let maps = Rc::downgrade(&dom.cascaded_maps(x));
        let row = Rc::downgrade(&dom.computed_cache.borrow().1.row(x).unwrap());
        dom.cascaded_cache.borrow_mut().slots.clear();
        dom.computed_cache.borrow_mut().1.clear();
        if enabled() && persistent_contexts_enabled() {
            assert!(maps.upgrade().is_some());
            assert!(row.upgrade().is_some());
        }
        dom.style_sharing.borrow_mut().clear_graph();
        assert!(maps.upgrade().is_none());
        assert!(row.upgrade().is_none());
    }

    #[test]
    fn persistent_styles54_reuse_complete_inputs_across_mutations() {
        // Shared rows are keyed by the process-wide page font epoch.
        let _inputs = crate::layout2::stable_global_layout_inputs();
        let mut dom = Dom::parse_document(
            "<style>.a{--tone:red;color:var(--tone)}.b{--tone:blue;color:var(--tone)}</style>\
             <main id=p class=a><div><span id=x>text</span></div></main>",
        );
        let x = dom.get_by_id("x").unwrap();
        let p = dom.get_by_id("p").unwrap();
        assert_eq!(
            dom.computed_value_resolved(x, "color").as_deref(),
            Some("red")
        );
        let original = Rc::downgrade(&dom.computed_cache.borrow().1.row(x).unwrap());
        let cascade = Rc::downgrade(&dom.cascaded_maps(x));
        dom.set_attr(p, "class", "b");
        assert_eq!(
            dom.computed_value_resolved(x, "color").as_deref(),
            Some("blue")
        );
        let blue = dom.computed_cache.borrow().1.row(x).unwrap();
        assert!(!original.ptr_eq(&Rc::downgrade(&blue)));
        dom.set_attr(p, "class", "a");
        assert_eq!(
            dom.computed_value_resolved(x, "color").as_deref(),
            Some("red")
        );
        if enabled() && persistent_contexts_enabled() {
            assert!(Rc::ptr_eq(
                &original.upgrade().unwrap(),
                &dom.computed_cache.borrow().1.row(x).unwrap()
            ));
            assert!(Rc::ptr_eq(
                &cascade.upgrade().unwrap(),
                &dom.cascaded_maps(x)
            ));
        }
        // Broad value invalidation rematches nodes without changing the style
        // environment. The input-owned graph must remain independently usable.
        dom.invalidate_all_style_values();
        assert_eq!(
            dom.computed_value_resolved(x, "color").as_deref(),
            Some("red")
        );
        if enabled() && persistent_contexts_enabled() {
            assert!(Rc::ptr_eq(
                &original.upgrade().unwrap(),
                &dom.computed_cache.borrow().1.row(x).unwrap()
            ));
        }
        dom.detach(x);
        assert_ne!(
            dom.computed_value_resolved(x, "color").as_deref(),
            Some("red")
        );
        dom.append(p, x);
        assert_eq!(
            dom.computed_value_resolved(x, "color").as_deref(),
            Some("red")
        );
    }

    #[test]
    fn persistent_styles54_lists_retain_descendant_computations() {
        // Shared rows are keyed by the process-wide page font epoch.
        let _inputs = crate::layout2::stable_global_layout_inputs();
        let mut dom = Dom::parse_document(
            "<style>.done{color:blue}li{color:red}</style><body id=body>\
             <ul id=list><li id=a><label id=x>x</label></li>\
             <li id=b><label id=y>y</label><ul id=nested><li id=c>c</li></ul></li></ul>\
             <ol id=lower type=a><li id=d>d</li></ol><ol id=upper type=A><li id=e>e</li></ol>",
        );
        assert_eq!(value(&dom, "x", "color").as_deref(), Some("red"));
        let x = dom.get_by_id("x").unwrap();
        let original = dom.computed_cache.borrow().1.row(x).unwrap();
        dom.set_attr(dom.get_by_id("b").unwrap(), "class", "done");
        assert_eq!(value(&dom, "y", "color").as_deref(), Some("blue"));
        dom.invalidate_all_style_values();
        assert_eq!(value(&dom, "x", "color").as_deref(), Some("red"));
        if enabled() && persistent_contexts_enabled() {
            assert!(Rc::ptr_eq(
                &original,
                &dom.computed_cache.borrow().1.row(x).unwrap()
            ));
        }
        assert_eq!(
            value(&dom, "list", "list-style-type").as_deref(),
            Some("disc")
        );
        assert_eq!(
            value(&dom, "nested", "list-style-type").as_deref(),
            Some("circle")
        );
        assert_eq!(
            value(&dom, "d", "list-style-type").as_deref(),
            Some("lower-alpha")
        );
        assert_eq!(
            value(&dom, "e", "list-style-type").as_deref(),
            Some("upper-alpha")
        );
        dom.set_attr(dom.get_by_id("lower").unwrap(), "type", "I");
        assert_eq!(
            value(&dom, "d", "list-style-type").as_deref(),
            Some("upper-roman")
        );
        assert_eq!(
            value(&dom, "e", "list-style-type").as_deref(),
            Some("upper-alpha")
        );
        let nested = dom.get_by_id("nested").unwrap();
        let body = dom.get_by_id("body").unwrap();
        dom.append(body, nested);
        assert_eq!(
            value(&dom, "nested", "list-style-type").as_deref(),
            Some("disc")
        );
        assert_eq!(value(&dom, "c", "list-style-type").as_deref(), Some("disc"));
    }

    #[test]
    fn persistent_styles54_ua_keys_follow_links_and_input_types() {
        // Shared rows are keyed by the process-wide page font epoch.
        let _inputs = crate::layout2::stable_global_layout_inputs();
        let mut dom = Dom::parse_document(
            "<body style='color:red'><a id=a href=x>a</a><a id=b>b</a>\
             <input id=box type=checkbox><input id=text type=text>",
        );
        assert_eq!(value(&dom, "a", "color").as_deref(), Some("#0000ee"));
        assert_eq!(value(&dom, "b", "color").as_deref(), Some("red"));
        assert_eq!(
            value(&dom, "a", "text-decoration-line").as_deref(),
            Some("underline")
        );
        assert_eq!(value(&dom, "b", "text-decoration-line"), None);
        assert_eq!(
            value(&dom, "box", "box-sizing").as_deref(),
            Some("border-box")
        );
        assert_ne!(
            value(&dom, "text", "box-sizing").as_deref(),
            Some("border-box")
        );
        let b = dom.get_by_id("b").unwrap();
        dom.set_attr(b, "href", "");
        assert_eq!(value(&dom, "b", "color").as_deref(), Some("#0000ee"));
        dom.remove_attr(b, "href");
        assert_eq!(value(&dom, "b", "color").as_deref(), Some("red"));
        let text = dom.get_by_id("text").unwrap();
        dom.set_attr(text, "type", "SeArCh");
        assert_eq!(
            value(&dom, "text", "box-sizing").as_deref(),
            Some("border-box")
        );
        dom.set_attr(text, "type", "text");
        assert_ne!(
            value(&dom, "text", "box-sizing").as_deref(),
            Some("border-box")
        );
    }

    #[test]
    fn persistent_styles54_summary_keys_follow_position_and_open_state() {
        // Shared rows are keyed by the process-wide page font epoch.
        let _inputs = crate::layout2::stable_global_layout_inputs();
        let mut dom = Dom::parse_document(
            "<details id=d><summary id=a>a</summary><summary id=b>b</summary></details>\
             <details open><summary id=c>c</summary></details>",
        );
        assert_eq!(value(&dom, "a", "display").as_deref(), Some("list-item"));
        assert_eq!(
            value(&dom, "a", "list-style-type").as_deref(),
            Some("disclosure-closed")
        );
        assert_eq!(value(&dom, "b", "display").as_deref(), Some("block"));
        assert_eq!(
            value(&dom, "c", "list-style-type").as_deref(),
            Some("disclosure-open")
        );
        let d = dom.get_by_id("d").unwrap();
        dom.set_attr(d, "open", "");
        assert_eq!(
            value(&dom, "a", "list-style-type").as_deref(),
            Some("disclosure-open")
        );
        let a = dom.get_by_id("a").unwrap();
        dom.append(d, a);
        assert_eq!(value(&dom, "a", "display").as_deref(), Some("block"));
        assert_eq!(value(&dom, "b", "display").as_deref(), Some("list-item"));
        assert_eq!(
            value(&dom, "b", "list-style-position").as_deref(),
            Some("inside")
        );
        assert_eq!(
            value(&dom, "b", "counter-increment").as_deref(),
            Some("list-item 0")
        );
        dom.remove_attr(d, "open");
        assert_eq!(
            value(&dom, "b", "list-style-type").as_deref(),
            Some("disclosure-closed")
        );
    }

    #[test]
    fn persistent_styles54_logical_cascades_include_inherited_axes_and_values() {
        // Shared rows are keyed by the process-wide page font epoch.
        let _inputs = crate::layout2::stable_global_layout_inputs();
        let mut dom = Dom::parse_document(
            "<style>.child{margin-inline-start:7px;padding-inline-start:inherit}</style>\
             <div id=a style='direction:ltr;padding-left:11px'><span id=x class=child>x</span></div>\
             <div id=b style='direction:rtl;padding-right:19px'><span id=y class=child>y</span></div>",
        );
        assert_eq!(value(&dom, "x", "margin-left").as_deref(), Some("7px"));
        assert_eq!(value(&dom, "y", "margin-right").as_deref(), Some("7px"));
        assert_eq!(value(&dom, "x", "padding-left").as_deref(), Some("11px"));
        assert_eq!(value(&dom, "y", "padding-right").as_deref(), Some("19px"));
        assert_ne!(value(&dom, "y", "margin-left").as_deref(), Some("7px"));
        let b = dom.get_by_id("b").unwrap();
        dom.set_attr(b, "style", "direction:ltr;padding-left:23px");
        assert_eq!(value(&dom, "y", "padding-left").as_deref(), Some("23px"));
        assert_eq!(value(&dom, "y", "margin-left").as_deref(), Some("7px"));
        dom.set_attr(b, "style", "direction:rtl;padding-right:19px");
        assert_eq!(value(&dom, "y", "padding-right").as_deref(), Some("19px"));
        assert_eq!(value(&dom, "x", "padding-left").as_deref(), Some("11px"));
    }

    #[test]
    fn persistent_styles54_environment_changes_retire_old_graphs() {
        // Shared rows are keyed by the process-wide page font epoch.
        let _inputs = crate::layout2::stable_global_layout_inputs();
        let mut dom = Dom::parse_document(
            "<style id=s>span{color:red}@media(min-width:500px){span{color:blue}}</style><span id=x>x</span>",
        );
        dom.set_viewport_px(300., 200.);
        assert_eq!(value(&dom, "x", "color").as_deref(), Some("red"));
        let x = dom.get_by_id("x").unwrap();
        let red = Rc::downgrade(&dom.computed_cache.borrow().1.row(x).unwrap());
        dom.set_viewport_px(600., 200.);
        assert_eq!(value(&dom, "x", "color").as_deref(), Some("blue"));
        assert!(red.upgrade().is_none());
        let blue = Rc::downgrade(&dom.computed_cache.borrow().1.row(x).unwrap());
        dom.set_doc_url(Some(url::Url::parse("https://example.test/next/").unwrap()));
        assert_eq!(value(&dom, "x", "color").as_deref(), Some("blue"));
        assert!(blue.upgrade().is_none());
    }

    #[test]
    fn persistent_styles54_growing_payloads_evict_graph_owners_before_returning() {
        // Shared rows are keyed by the process-wide page font epoch.
        let _inputs = crate::layout2::stable_global_layout_inputs();
        if !enabled() || !persistent_contexts_enabled() {
            return;
        }
        let dom = Dom::parse_document("<span id=x style='color:red'>x</span>");
        assert_eq!(value(&dom, "x", "color").as_deref(), Some("red"));
        let x = dom.get_by_id("x").unwrap();
        let index = prop_index("color").unwrap();
        assert!(!dom.style_sharing.borrow().rows_owned.is_empty());
        for length in [0, 17, 8192, 2, 10000, 0] {
            dom.computed_cache_put(x, index, Some("x".repeat(length)));
            assert!(!dom.style_sharing.borrow().rows_owned.is_empty());
            assert!(dom.style_sharing.borrow().row_budget.0.get() <= MAX_ROW_BYTES);
        }
        dom.computed_cache_put(x, index, Some("x".repeat(MAX_ROW_BYTES + 1)));
        assert!(dom.style_sharing.borrow().rows_owned.is_empty());
        assert!(dom.style_sharing.borrow().cascades_owned.is_empty());
        // The active semantic value survives eviction.
        assert_eq!(
            dom.computed_cache_get(x, index).unwrap().unwrap().len(),
            MAX_ROW_BYTES + 1
        );
        dom.computed_cache.borrow_mut().1.clear();
        assert_eq!(dom.style_sharing.borrow().row_budget.0.get(), 0);
    }
}
