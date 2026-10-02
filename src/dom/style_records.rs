//! Retained typed style records at the computed/used-value boundary.
//!
//! CSS Cascade 5 #computed / #used / #inheriting (CSSWG 81c27f686901):
//! lengths retain their percentage/auto expressions for layout. Only complete
//! computations are published; a recursive query never observes a partial
//! record. The canonical computed-value rows own invalidation and retirement.

use super::*;
use crate::layout2::{BoxStyle, InlineStyle};
use std::cell::Cell;
use std::rc::Rc;

const MAX_BYTES: usize = 8 * 1024 * 1024;

pub(super) fn enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("TRUST_STYLE_RECORDS").as_deref() != Ok("0"))
}

#[derive(Default)]
pub(super) struct Budget(Rc<Cell<usize>>);

impl Budget {
    pub(super) fn reserve(&self, bytes: usize) -> Option<Lease> {
        let total = self.0.get().checked_add(bytes)?;
        if total > MAX_BYTES {
            return None;
        }
        self.0.set(total);
        Some(Lease {
            bytes,
            budget: self.0.clone(),
        })
    }
}

pub(super) struct Lease {
    bytes: usize,
    budget: Rc<Cell<usize>>,
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.budget.set(self.budget.get() - self.bytes);
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct BoxContext {
    pub dimensions: [u32; 13],
    pub document_element: bool,
    pub display: Option<String>,
}

pub(super) struct BoxRecord {
    pub context: BoxContext,
    pub value: BoxStyle,
    pub lease: Lease,
}

pub(super) struct InlineRecord {
    pub epoch: u64,
    pub clickable: bool,
    pub parent: InlineStyle,
    pub base: url::Url,
    pub value: InlineStyle,
    pub lease: Lease,
}

pub(super) struct DisplayRecord {
    pub value: Option<String>,
    pub lease: Lease,
}

impl DisplayRecord {
    pub(super) fn bytes(&self) -> usize {
        self.lease.bytes
    }
}

struct Computation<'a> {
    dom: &'a Dom,
    stamp: (u64, u64),
    row: Option<computed_cache::SharedRow>,
}

impl Computation<'_> {
    fn can_publish(&self, id: NodeId) -> bool {
        let cache = self.dom.computed_cache.borrow();
        cache.0 == self.stamp
            && self.stamp
                == (
                    self.dom.style_value_epoch,
                    crate::font_system::page_font_epoch(),
                )
            && self.row.as_ref().is_some_and(|original| {
                cache
                    .1
                    .row(id)
                    .is_some_and(|current| Rc::ptr_eq(original, &current))
            })
    }
}

impl Drop for Computation<'_> {
    fn drop(&mut self) {
        self.dom.computed_cache.borrow_mut().1.record_computing = false;
    }
}

impl BoxRecord {
    pub(super) fn bytes(&self) -> usize {
        self.lease.bytes
    }
}

impl InlineRecord {
    pub(super) fn bytes(&self) -> usize {
        self.lease.bytes
    }
}

impl Dom {
    fn prepare_style_record(&self, id: NodeId) -> Option<Computation<'_>> {
        if !enabled() || self.css_transitions_active() {
            return None;
        }
        self.flush_style_invalidations();
        // Container/rollback cascades deliberately install provisional maps
        // while resolving their dependencies. They retain the ordinary path.
        let index = self.style_index();
        if index.has_container_queries
            || index.has_container_units
            || index.has_revert_layer
            || self
                .attr(id, "style")
                .is_some_and(super::mentions_container_unit)
        {
            return None;
        }
        {
            let mut cache = self.computed_cache.borrow_mut();
            if cache.1.record_computing {
                return None;
            }
            cache.1.record_computing = true;
        }
        let mut computation = Computation {
            dom: self,
            stamp: (0, 0),
            row: None,
        };
        // This also synchronizes the page-font epoch. The shared row identity
        // proves common values; node context is separately represented below.
        computation.row = Some(self.prepare_computed_row(id, 0));
        computation.stamp = self.computed_cache.borrow().0;
        Some(computation)
    }

    pub(crate) fn retained_box_style(
        &self,
        id: NodeId,
        context: BoxContext,
        compute: impl FnOnce() -> BoxStyle,
    ) -> BoxStyle {
        let Some(computation) = self.prepare_style_record(id) else {
            return compute();
        };
        if let Some(value) = self.computed_cache.borrow().1.box_record(id, &context) {
            return value;
        }
        let value = compute();
        if computation.can_publish(id) {
            self.computed_cache
                .borrow_mut()
                .1
                .put_box_record(id, context, value.clone());
        }
        value
    }

    pub(crate) fn retained_inline_style(
        &self,
        id: NodeId,
        parent: &InlineStyle,
        base: &url::Url,
        compute: impl FnOnce() -> InlineStyle,
    ) -> InlineStyle {
        let Some(computation) = self.prepare_style_record(id) else {
            return compute();
        };
        // Activation metadata is allowed to change independently of CSS.
        // Keep the node's own activation and the inherited formatting context
        // in this node-private record, never in the shared property row.
        let clickable = self.render_clickable(id);
        if let Some(value) = self
            .computed_cache
            .borrow()
            .1
            .inline_record(id, self.epoch, clickable, parent, base)
        {
            return value;
        }
        let value = compute();
        if computation.can_publish(id) {
            self.computed_cache.borrow_mut().1.put_inline_record(
                id,
                self.epoch,
                clickable,
                parent.clone(),
                base.clone(),
                value.clone(),
            );
        }
        value
    }

    pub(super) fn retained_display(
        &self,
        id: NodeId,
        compute: impl FnOnce() -> Option<String>,
    ) -> Option<String> {
        let Some(computation) = self.prepare_style_record(id) else {
            return compute();
        };
        if let Some(value) = self.computed_cache.borrow().1.display_record(id) {
            return value;
        }
        let value = compute();
        if computation.can_publish(id) {
            self.computed_cache
                .borrow_mut()
                .1
                .put_display_record(id, value.clone());
        }
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout2::value::Vp;

    fn vp() -> Vp {
        Vp { w: 800., h: 600. }
    }
    fn base() -> url::Url {
        url::Url::parse("https://example.test/one/").unwrap()
    }

    #[test]
    fn style_contexts_share_variable_dependent_boxes_and_split_on_inherited_changes() {
        let _inputs = crate::layout2::stable_global_layout_inputs();
        // CSS Variables 1 #using-variables: lookup is at the element, with
        // inherited computed streams rather than reinterpreted parent tokens.
        let mut dom = Dom::parse_document(
            r#"<style>
            .theme {--Tone:red;--tone:blue;--unit:2em;--edge:var(--unit);font-size:10px}
            .large {--unit:4em;font-size:20px}
            .box {width:var(--edge);padding:var(--space,3px);color:var(--Tone);background-color:var(--tone)}
            </style><div id=p class=theme><span class=box id=x>x</span><span class=box id=y>y</span></div>
            <div id=q class=theme><span class=box id=z>z</span></div>"#,
        );
        let x = dom.get_by_id("x").unwrap();
        let y = dom.get_by_id("y").unwrap();
        let z = dom.get_by_id("z").unwrap();
        let a = BoxStyle::of(&dom, x, vp());
        let hits = dom.computed_cache.borrow().1.box_hits.get();
        let b = BoxStyle::of(&dom, y, vp());
        let c = BoxStyle::of(&dom, z, vp());
        assert_eq!(a, b);
        assert_eq!(a, c);
        assert_eq!(a.width.resolve(None), Some(20.));
        assert_eq!(
            dom.computed_value_resolved(y, "color").as_deref(),
            Some("red")
        );
        assert_eq!(
            dom.computed_value_resolved(y, "background-color")
                .as_deref(),
            Some("blue")
        );
        if enabled() && style_sharing::enabled() && style_sharing::variable_contexts_enabled() {
            assert!(
                dom.computed_cache.borrow().1.box_hits.get() > hits,
                "variable contexts must admit a complete typed style"
            );
        }
        dom.set_attr(dom.get_by_id("p").unwrap(), "class", "theme large");
        assert_eq!(BoxStyle::of(&dom, x, vp()).width.resolve(None), Some(80.));
        assert_eq!(BoxStyle::of(&dom, y, vp()).width.resolve(None), Some(80.));
        assert_eq!(BoxStyle::of(&dom, z, vp()).width.resolve(None), Some(20.));
        dom.set_attr(y, "style", "--unit:9em");
        // --edge was substituted on the parent; changing --unit here cannot
        // reinterpret that inherited result. Its em remains a consumer unit.
        assert_eq!(BoxStyle::of(&dom, y, vp()).width.resolve(None), Some(80.));
    }

    #[test]
    fn style_contexts_keep_cycles_fallbacks_and_empty_custom_values_distinct() {
        let mut dom = Dom::parse_document(
            r#"<style>
            .cycle {--a:var(--b);--b:var(--a);--empty:;width:var(--a,13px);padding:var(--empty,9px)}
            </style><div><span class=cycle id=x>x</span><span class=cycle id=y>y</span></div>"#,
        );
        let x = dom.get_by_id("x").unwrap();
        let y = dom.get_by_id("y").unwrap();
        let a = BoxStyle::of(&dom, x, vp());
        let b = BoxStyle::of(&dom, y, vp());
        assert_eq!(a, b);
        assert_eq!(a.width.resolve(None), Some(13.));
        assert_eq!(dom.computed_value(y, "--a"), None);
        assert_eq!(dom.computed_value(y, "--empty").as_deref(), Some(""));
        dom.set_attr(y, "style", "--b:17px");
        assert_eq!(BoxStyle::of(&dom, y, vp()).width.resolve(None), Some(17.));
        assert_eq!(BoxStyle::of(&dom, x, vp()).width.resolve(None), Some(13.));
    }

    #[test]
    fn style_contexts_discard_records_when_computation_retires_its_row() {
        let dom = Dom::parse_document("<span id=x style='width:11px'>x</span>");
        let x = dom.get_by_id("x").unwrap();
        let expected = BoxStyle::of(&dom, x, vp());
        dom.computed_cache.borrow_mut().1.clear();
        let context = BoxContext {
            dimensions: [0; 13],
            document_element: false,
            display: None,
        };
        let result = dom.retained_box_style(x, context.clone(), || {
            // The same retirement can occur while resolving a lazy dependency
            // or newly available font. A completed value is usable by this
            // caller, but must not be installed into a replacement context.
            dom.computed_cache.borrow_mut().1.clear();
            dom.computed_cache.borrow_mut().1.ensure_row(x);
            expected.clone()
        });
        assert_eq!(result, expected);
        assert!(
            dom.computed_cache
                .borrow()
                .1
                .box_record(x, &context)
                .is_none()
        );
    }

    #[test]
    fn style_contexts_display_inherit_uses_the_flat_style_parent() {
        let mut dom = Dom::parse_document(
            "<div id=h style='display:flex'><span id=x style='display:inherit'>x</span></div>",
        );
        let h = dom.get_by_id("h").unwrap();
        let x = dom.get_by_id("x").unwrap();
        let shadow = dom.attach_shadow(h);
        let slot = dom.create_element("slot");
        dom.set_attr(slot, "style", "display:grid");
        dom.append(shadow, slot);
        assert_eq!(dom.computed_display(x).as_deref(), Some("grid"));
        dom.set_attr(slot, "style", "display:block");
        assert_eq!(dom.computed_display(x).as_deref(), Some("block"));
    }

    #[test]
    fn style_records_share_typed_boxes_but_preserve_node_and_font_context() {
        // Cache-hit counts: a parallel page-font install would expire rows.
        let _inputs = crate::layout2::stable_global_layout_inputs();
        let mut dom = Dom::parse_document(
            "<style>.same {width:2em;padding:3px}</style><div style='font-size:10px'><span class=same id=x>x</span><span class=same id=y>y</span></div>",
        );
        let x = dom.get_by_id("x").unwrap();
        let y = dom.get_by_id("y").unwrap();
        let a = BoxStyle::of(&dom, x, vp());
        let before = dom.computed_cache.borrow().1.box_hits.get();
        let b = BoxStyle::of(&dom, y, vp());
        assert_eq!(a, b);
        assert_eq!(b.width.resolve(None), Some(20.));
        if enabled() && style_sharing::enabled() {
            assert!(dom.computed_cache.borrow().1.box_hits.get() > before);
        }
        dom.set_attr(y, "style", "font-size:20px");
        assert_eq!(BoxStyle::of(&dom, y, vp()).width.resolve(None), Some(40.));
        assert_eq!(BoxStyle::of(&dom, x, vp()).width.resolve(None), Some(20.));
        dom.set_attr(x, "style", "width:10vw");
        assert_eq!(BoxStyle::of(&dom, x, vp()).width.resolve(None), Some(80.));
        assert_eq!(
            BoxStyle::of(&dom, x, Vp { w: 400., h: 300. })
                .width
                .resolve(None),
            Some(40.)
        );
    }

    #[test]
    fn style_records_keep_inline_links_language_and_parent_alignment_live() {
        let _inputs = crate::layout2::stable_global_layout_inputs();
        let mut dom = Dom::parse_document(
            "<div lang=en><a id=x href=next style='font-size:12px;letter-spacing:1px'>x</a></div>",
        );
        let x = dom.get_by_id("x").unwrap();
        let root = InlineStyle::root();
        let a = InlineStyle::derive(&dom, x, &root, &base());
        let b = InlineStyle::derive(&dom, x, &root, &base());
        assert_eq!(a, b);
        assert_eq!(b.node, x);
        assert_eq!(b.language.as_deref(), Some("en"));
        assert_eq!(b.font_size, 12.);
        assert_eq!(b.letter, 1.);
        if enabled() {
            assert!(dom.computed_cache.borrow().1.inline_hits.get() > 0);
        }
        let other = url::Url::parse("https://example.test/two/").unwrap();
        assert_ne!(InlineStyle::derive(&dom, x, &root, &other).link, a.link);
        dom.set_attr(x, "lang", "de");
        dom.set_attr(x, "style", "font-size:18px;letter-spacing:2px");
        let changed = InlineStyle::derive(&dom, x, &root, &base());
        assert_eq!(changed.language.as_deref(), Some("de"));
        assert_eq!(changed.font_size, 18.);
        assert_eq!(changed.letter, 2.);
        let mut shifted = root.clone();
        shifted.font_family = "monospace".into();
        assert_eq!(
            InlineStyle::derive(&dom, x, &shifted, &base()).font_family,
            "monospace"
        );
    }

    #[test]
    fn style_records_display_tracks_variables_hidden_and_document_roots() {
        let _inputs = crate::layout2::stable_global_layout_inputs();
        let mut dom = Dom::parse_document(
            "<style>html {display:contents}.same {display:var(--mode, inline)}</style><div id=p style='--mode:block'><span id=x class=same>x</span><span id=y class=same hidden>y</span></div>",
        );
        let p = dom.get_by_id("p").unwrap();
        let x = dom.get_by_id("x").unwrap();
        let y = dom.get_by_id("y").unwrap();
        assert_eq!(dom.effective_display(x).as_deref(), Some("block"));
        assert_eq!(dom.effective_display(x).as_deref(), Some("block"));
        assert_eq!(dom.effective_display(y).as_deref(), Some("none"));
        let html = dom
            .children(DOCUMENT)
            .into_iter()
            .find(|&n| dom.tag_name(n) == Some("html"))
            .unwrap();
        assert_eq!(dom.computed_display(html).as_deref(), Some("contents"));
        assert_eq!(dom.effective_display(html).as_deref(), Some("block"));
        if enabled() {
            assert!(dom.computed_cache.borrow().1.display_hits.get() > 0);
        }
        dom.set_attr(p, "style", "--mode:none");
        assert_eq!(dom.effective_display(x).as_deref(), Some("none"));
        dom.remove_attr(y, "hidden");
        dom.set_attr(p, "style", "--mode:flex");
        assert_eq!(dom.effective_display(x).as_deref(), Some("flex"));
        assert_eq!(dom.effective_display(y).as_deref(), Some("flex"));
    }

    #[test]
    fn style_records_follow_running_transitions_and_inheritance() {
        let mut dom = Dom::parse_document(
            "<style>#p {width:10px;transition:width 1s linear} #p.wide {width:30px} #x {width:inherit}</style><div id=p><span id=x>x</span></div>",
        );
        let p = dom.get_by_id("p").unwrap();
        let x = dom.get_by_id("x").unwrap();
        dom.update_css_transitions(0.);
        assert_eq!(BoxStyle::of(&dom, x, vp()).width.resolve(None), Some(10.));
        dom.set_attr(p, "class", "wide");
        dom.update_css_transitions(1.);
        dom.update_css_transitions(1.5);
        assert!(dom.css_transitions_active());
        assert!((BoxStyle::of(&dom, x, vp()).width.resolve(None).unwrap() - 20.).abs() < 0.01);
        dom.update_css_transitions(2.);
        assert_eq!(BoxStyle::of(&dom, x, vp()).width.resolve(None), Some(30.));
    }

    #[test]
    fn style_records_release_budget_and_do_not_publish_reentrant_results() {
        let budget = Budget::default();
        let lease = budget.reserve(MAX_BYTES).unwrap();
        assert!(budget.reserve(1).is_none());
        drop(lease);
        assert_eq!(budget.0.get(), 0);
        assert!(budget.reserve(MAX_BYTES + 1).is_none());
        let dom = Dom::parse_document("<p id=x>x</p>");
        let x = dom.get_by_id("x").unwrap();
        let result = dom.retained_display(x, || {
            assert_eq!(
                dom.retained_display(x, || Some("nested".into())).as_deref(),
                Some("nested")
            );
            Some("outer".into())
        });
        assert_eq!(result.as_deref(), Some("outer"));
        if enabled() {
            assert_eq!(
                dom.retained_display(x, || panic!("cached complete record"))
                    .as_deref(),
                Some("outer")
            );
            assert!(!dom.computed_cache.borrow().1.record_computing);
        }
    }
}
