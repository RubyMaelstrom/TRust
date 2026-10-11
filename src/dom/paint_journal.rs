//! The paint journal: what a retained display-list consumer must re-derive.
//!
//! Graphical paint is a pure function of the fragment tree, the Dom's
//! computed styles/attributes/state of the nodes a fragment names, and a few
//! document-wide inputs (viewport, device pixel ratio, base URL, the
//! sheet set). A retained consumer (`layout2::paint_cache`) reuses what it
//! derived for a fragment when none of those changed. This module is the Dom
//! side of that contract: an append-only, bounded log of *dirty roots* and a
//! separate *global stamp*.
//!
//! * `note(node)` says "anything paint reads from `node` or from a flat-tree
//!   descendant of `node` may have changed". Style invalidation reaches
//!   descendants by inheritance (`apply_style_invalidations` records its
//!   roots) and attribute writes are reported on the mutated node; a
//!   consumer expands each such root over its flat-tree subtree
//!   (`Dom::paint_dirty_set`) so reads of an ancestor's attributes (`inert`,
//!   `href`, text-decoration propagation) stay covered.
//! * `note_self(node)` says only `node` itself changed: a child-list or text
//!   change. What it does to other nodes (selector matches, directionality,
//!   counters) is reported by the style invalidation it triggers, and a
//!   descendant's paint reads no ancestor's child list, so the unrelated
//!   descendants of a container that gained a child stay clean.
//! * `global()` says "any node may have changed": a stylesheet change, the
//!   viewport or device pixel ratio, a top-layer/modal change, or a log
//!   overflow.
//!
//! Every Dom mutation that bumps the DOM epoch must reach one of the two.
//! `Dom::mark_dom_revision` is the *global* default; the few mutators that
//! name their nodes use `mark_dom_revision_attributed` and note them
//! themselves, so a new mutator that forgets to say what it changed fails
//! safe: it advances the global stamp.
//!
//! Standards: DOM #concept-tree (flat tree walks follow CSS Shadow 1
//! #flattening); the journal itself is TRust-internal.

use super::*;
use std::cell::{Cell, RefCell};

/// Most dirty roots a consumer may find pending before the log gives up and
/// advances the global stamp instead (every consumer then rebuilds).
const CAPACITY: usize = 4096;

/// A consumer's position in the journal.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct JournalCursor {
    global: u64,
    seq: u64,
}

/// One journal entry: `node`, and its flat-tree descendants when `subtree`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct JournalRoot {
    pub node: NodeId,
    pub subtree: bool,
}

/// What changed since a [`JournalCursor`].
pub(crate) enum JournalDelta {
    /// Nothing is recorded since the cursor.
    Clean,
    /// Exactly these roots may have changed.
    Roots(Vec<JournalRoot>),
    /// Anything may have changed.
    Global,
}

#[derive(Default)]
pub(crate) struct PaintJournal {
    global: Cell<u64>,
    /// Roots recorded since the log's creation; `log` holds the last
    /// `log.len()` of them.
    seq: Cell<u64>,
    log: RefCell<Vec<JournalRoot>>,
    /// How many times the global stamp advanced, by cause, for diagnostics.
    global_causes: RefCell<Vec<(&'static str, u32)>>,
}

impl PaintJournal {
    pub(crate) fn cursor(&self) -> JournalCursor {
        JournalCursor {
            global: self.global.get(),
            seq: self.seq.get(),
        }
    }

    /// Everything may have changed.
    #[track_caller]
    pub(crate) fn global(&self) {
        self.global.set(self.global.get().wrapping_add(1));
        self.log.borrow_mut().clear();
        if super::casc_diag_on() {
            let at = std::panic::Location::caller();
            let cause = format!("{}:{}", at.file(), at.line());
            let mut causes = self.global_causes.borrow_mut();
            // Distinct call sites are few; leak the label once per site.
            if let Some(entry) = causes.iter_mut().find(|(label, _)| **label == *cause) {
                entry.1 += 1;
            } else if causes.len() < 64 {
                causes.push((Box::leak(cause.into_boxed_str()), 1));
            }
        }
    }

    /// `node` and its flat-tree descendants may have changed.
    pub(crate) fn note(&self, node: NodeId) {
        self.record(JournalRoot {
            node,
            subtree: true,
        });
    }

    /// Only `node` itself may have changed (its child list or text).
    pub(crate) fn note_self(&self, node: NodeId) {
        self.record(JournalRoot {
            node,
            subtree: false,
        });
    }

    fn record(&self, root: JournalRoot) {
        let mut log = self.log.borrow_mut();
        if log.len() >= CAPACITY {
            drop(log);
            self.global();
            return;
        }
        // No de-duplication against the last entry: a consumer whose cursor
        // is past it must still see this note.
        log.push(root);
        self.seq.set(self.seq.get().wrapping_add(1));
    }

    pub(crate) fn note_all(&self, nodes: &[NodeId]) {
        for &node in nodes {
            self.note(node);
        }
    }

    /// What was recorded since `cursor`.
    pub(crate) fn since(&self, cursor: JournalCursor) -> JournalDelta {
        if cursor.global != self.global.get() {
            return JournalDelta::Global;
        }
        let seq = self.seq.get();
        let pending = seq.wrapping_sub(cursor.seq);
        if pending == 0 {
            return JournalDelta::Clean;
        }
        let log = self.log.borrow();
        if pending > log.len() as u64 {
            return JournalDelta::Global;
        }
        JournalDelta::Roots(log[log.len() - pending as usize..].to_vec())
    }

    /// Advances of the global stamp by call site (`TRUST_DIAG_FRAME`).
    pub(crate) fn global_causes(&self) -> Vec<(&'static str, u32)> {
        std::mem::take(&mut *self.global_causes.borrow_mut())
    }
}

impl Dom {
    pub(crate) fn paint_journal(&self) -> &PaintJournal {
        &self.paint_journal
    }

    /// `mark_dom_revision` for a mutation whose caller notes the nodes it
    /// changed in the paint journal. The plain `mark_dom_revision` is
    /// unattributed and therefore global.
    pub(super) fn mark_dom_revision_attributed(&mut self) {
        self.mark_dom_revision_core();
    }

    /// The flat-tree closure of `roots`: each root and, for a subtree root,
    /// everything below it (light children, shadow roots, slot-assigned
    /// nodes), the same walk
    /// `apply_style_invalidations` uses for inherited values. `None` once it
    /// would exceed `budget` nodes, which a consumer treats as global.
    pub(crate) fn paint_dirty_set(
        &self,
        roots: &[JournalRoot],
        budget: usize,
    ) -> Option<FxHashSet<NodeId>> {
        let mut seen = FxHashSet::default();
        let mut pending: Vec<NodeId> = roots
            .iter()
            .filter(|root| root.subtree)
            .map(|root| root.node)
            .collect();
        // Own-change roots join afterwards: a node noted both ways must
        // still expand.
        let own = || {
            roots
                .iter()
                .filter(|root| !root.subtree && self.nodes.get(root.node).is_some())
                .map(|root| root.node)
        };
        while let Some(node) = pending.pop() {
            if self.nodes.get(node).is_none() || !seen.insert(node) {
                continue;
            }
            if seen.len() > budget {
                return None;
            }
            pending.extend(self.child_iter(node));
            if let Some(shadow) = self.shadow_root(node) {
                pending.push(shadow);
            }
            if self.tag_name(node) == Some("slot") {
                pending.extend(self.slot_assigned_nodes(node));
            }
        }
        seen.extend(own());
        Some(seen)
    }
}

impl Dom {
    /// Epochs whose advance means "any style may differ": the style value,
    /// sheet-set and selector epochs, and the process-wide font and SVG
    /// sprite revisions that length units and sprite references read.
    pub(crate) fn paint_epochs(&self) -> [u64; 5] {
        [
            self.style_value_epoch,
            self.style_epoch,
            self.selector_epoch,
            crate::font_system::page_font_epoch(),
            svg_sprite_revision(),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The nodes of the roots of the given kind.
    fn nodes(delta: JournalDelta, subtree: bool) -> Vec<NodeId> {
        roots(delta)
            .into_iter()
            .filter(|root| root.subtree == subtree)
            .map(|root| root.node)
            .collect()
    }

    fn roots(delta: JournalDelta) -> Vec<JournalRoot> {
        match delta {
            JournalDelta::Roots(roots) => roots,
            JournalDelta::Clean => panic!("clean"),
            JournalDelta::Global => panic!("global"),
        }
    }

    #[test]
    fn a_mutation_that_names_its_node_notes_only_that_node() {
        let mut dom = Dom::parse_document("<div id=a><p id=b>x</p></div><p id=c>y</p>");
        let (a, b) = (dom.get_by_id("a").unwrap(), dom.get_by_id("b").unwrap());
        // A style index makes the mutations below provably local.
        dom.computed_value_resolved(a, "color");
        let cursor = dom.paint_journal().cursor();
        assert!(matches!(
            dom.paint_journal().since(cursor),
            JournalDelta::Clean
        ));
        dom.set_attr(a, "title", "t");
        assert_eq!(nodes(dom.paint_journal().since(cursor), true), vec![a]);
        let text = dom.child_iter(b).next().unwrap();
        let attribute_changes = roots(dom.paint_journal().since(cursor)).len();
        dom.set_text(text, "z");
        // A text change is the parent's and the text node's own, and
        // reaches no descendant of a container.
        let noted: Vec<_> = roots(dom.paint_journal().since(cursor))
            .into_iter()
            .skip(attribute_changes)
            .collect();
        assert!(noted.iter().all(|root| !root.subtree), "{noted:?}");
        let mut own: Vec<_> = noted.iter().map(|root| root.node).collect();
        own.sort_unstable();
        own.dedup();
        let mut expected = vec![b, text];
        expected.sort_unstable();
        assert_eq!(own, expected);
    }

    #[test]
    fn style_invalidation_notes_its_roots_when_applied() {
        let mut dom = Dom::parse_document(
            "<style>.x{color:red}</style><div id=a><p id=b>x</p></div><p id=c>y</p>",
        );
        let a = dom.get_by_id("a").unwrap();
        // Settle the sheet and the style index.
        dom.computed_value_resolved(a, "color");
        let cursor = dom.paint_journal().cursor();
        dom.set_attr(a, "class", "x");
        dom.flush_style_invalidations();
        let noted = nodes(dom.paint_journal().since(cursor), true);
        assert!(noted.contains(&a), "{noted:?}");
    }

    #[test]
    fn a_node_noted_again_after_a_consumer_looked_is_seen_again() {
        let mut dom = Dom::parse_document("<p id=a>x</p>");
        let a = dom.get_by_id("a").unwrap();
        dom.computed_value_resolved(a, "color");
        dom.set_attr(a, "title", "1");
        let cursor = dom.paint_journal().cursor();
        dom.set_attr(a, "title", "2");
        assert_eq!(nodes(dom.paint_journal().since(cursor), true), vec![a]);
    }

    #[test]
    fn an_unattributed_revision_is_global() {
        let mut dom = Dom::parse_document("<p id=a>x</p>");
        let cursor = dom.paint_journal().cursor();
        dom.mark_dom_revision();
        assert!(matches!(
            dom.paint_journal().since(cursor),
            JournalDelta::Global
        ));
        let cursor = dom.paint_journal().cursor();
        dom.touch();
        assert!(matches!(
            dom.paint_journal().since(cursor),
            JournalDelta::Global
        ));
    }

    #[test]
    fn an_overflowing_log_is_global() {
        let dom = Dom::parse_document("<p id=a>x</p>");
        let cursor = dom.paint_journal().cursor();
        for node in 0..CAPACITY + 1 {
            dom.paint_journal().note(node);
        }
        assert!(matches!(
            dom.paint_journal().since(cursor),
            JournalDelta::Global
        ));
    }

    #[test]
    fn dirty_set_covers_the_flat_tree_below_a_root() {
        let mut dom =
            Dom::parse_document("<div id=a><p id=b>x<i id=i>y</i></p></div><p id=c>z</p>");
        let (a, b, c, i) = (
            dom.get_by_id("a").unwrap(),
            dom.get_by_id("b").unwrap(),
            dom.get_by_id("c").unwrap(),
            dom.get_by_id("i").unwrap(),
        );
        let subtree = |node| JournalRoot {
            node,
            subtree: true,
        };
        let own_root = |node| JournalRoot {
            node,
            subtree: false,
        };
        let set = dom.paint_dirty_set(&[subtree(b)], 100).unwrap();
        assert!(set.contains(&b) && set.contains(&i));
        assert!(!set.contains(&a) && !set.contains(&c));
        assert!(dom.paint_dirty_set(&[subtree(a)], 2).is_none(), "budget");
        // A node noted both ways still expands.
        let both = [own_root(a), subtree(a)];
        let set = dom.paint_dirty_set(&both, 100).unwrap();
        assert!(set.contains(&b) && set.contains(&i));
        // A node's own change leaves its descendants clean.
        let set = dom.paint_dirty_set(&[own_root(a)], 100).unwrap();
        assert!(set.contains(&a) && !set.contains(&b) && !set.contains(&i));
        // A shadow host's tree and slotted light children are below it.
        let host = dom.create_element("div");
        let root = dom.attach_shadow(host);
        let slot = dom.create_element("slot");
        dom.insert_before(root, slot, None);
        let light = dom.create_element("span");
        dom.insert_before(host, light, None);
        dom.insert_before(a, host, None);
        let set = dom.paint_dirty_set(&[subtree(host)], 100).unwrap();
        assert!(set.contains(&root) && set.contains(&slot) && set.contains(&light));
    }
}
