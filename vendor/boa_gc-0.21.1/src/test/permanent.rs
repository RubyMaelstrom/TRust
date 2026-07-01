use boa_macros::{Finalize, Trace};

use super::{Harness, run_test};
use crate::{Gc, GcRefCell, force_collect, retenure_permanent};

// The permanent generation (see `crate::retenure_permanent`) tenures the live
// set so it is skipped as a source in the `trace_non_roots` ref-counting pass.
// These pin the two properties that keep it safe: it never frees a live object,
// and — crucially — it never *retains* a dead one (no forced retention, so a
// reused thread can't leak a prior context's platform).

#[test]
fn tenured_objects_survive_and_stay_accessible() {
    run_test(|| {
        let keep = Gc::new(GcRefCell::new(1u64));
        force_collect();
        let n = retenure_permanent();
        assert!(n >= 1, "the live object should be tenured");
        // Churn young garbage; collecting it must not disturb the tenured set.
        for _ in 0..1000 {
            let _g = Gc::new(GcRefCell::new(9u64));
        }
        force_collect();
        assert_eq!(*keep.borrow(), 1);
    });
}

#[test]
fn a_tenured_object_is_still_collected_once_unreachable() {
    run_test(|| {
        let victim = Gc::new(GcRefCell::new(7u64));
        force_collect();
        retenure_permanent(); // `victim` is now permanent
        // Permanent must NOT mean immortal-when-dead: dropping the last handle
        // and collecting has to reclaim it, or a reused page thread would leak
        // every prior page's platform graph.
        drop(victim);
        force_collect();
        Harness::assert_empty_gc();
    });
}

#[test]
fn a_child_reachable_only_through_a_tenured_object_is_retained() {
    run_test(|| {
        #[derive(Finalize, Trace)]
        struct Node {
            child: GcRefCell<Option<Gc<u64>>>,
        }
        // `parent` (stack-rooted) holds the only handle to `child`.
        let parent = Gc::new(Node {
            child: GcRefCell::new(Some(Gc::new(42u64))),
        });
        force_collect();
        // Tenures parent + child; parent is now skipped as a trace_non_roots
        // source, so `child`'s in-heap ref from it is no longer counted.
        retenure_permanent();
        // Collecting must still keep `child`, reached only via the skipped
        // permanent parent (marking is ordinary reachability, unaffected).
        force_collect();
        force_collect();
        assert_eq!(**parent.child.borrow().as_ref().unwrap(), 42);
    });
}

#[test]
fn retenure_is_self_cleaning_across_generations() {
    run_test(|| {
        // First generation: one object, tenured, then dropped.
        let first = Gc::new(GcRefCell::new(1u64));
        force_collect();
        assert_eq!(retenure_permanent(), 1);
        drop(first);

        // Second generation: a fresh object. `retenure_permanent` clears the
        // old flag and collects the (now-dead) first generation before tenuring
        // the survivor, so the count reflects only what's live now — not an
        // ever-growing immortal pile.
        let second = Gc::new(GcRefCell::new(2u64));
        force_collect();
        assert_eq!(
            retenure_permanent(),
            1,
            "the dead first generation must not still be counted"
        );
        assert_eq!(*second.borrow(), 2);
    });
}
