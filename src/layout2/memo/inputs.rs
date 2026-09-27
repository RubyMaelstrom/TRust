//! Observed resource inputs of a retained layout computation.
//!
//! CSS Images 3 #default-sizing and HTML
//! #density-corrected-intrinsic-width-and-height make the selected image's
//! natural dimensions an input, including their absence before a load. A
//! different image's arrival is not an input to this computation.
//!
//! Nested computations share immutable read sets. A cache hit replays its
//! dependencies into the enclosing computation just like executing it would.
//! This includes the within-pass intrinsic cache, anonymous generated boxes,
//! and failed/missing image reads. Validation is memoized per resource revision;
//! both validation and retained-memory accounting visit shared dependencies
//! once, rather than flattening the same reads into every ancestor.

use super::{Dom, ImageSizes, LayoutCache};
use rustc_hash::FxHashMap;
use std::{cell::Cell, cell::RefCell, mem::size_of, rc::Rc};

pub(in crate::layout2) type Inputs = Option<Rc<Reads>>;

pub(in crate::layout2) struct Reads {
    images: Vec<(String, Option<(u32, u32)>)>,
    children: Vec<Rc<Reads>>,
    validated: Cell<(u64, bool)>,
}

impl Reads {
    fn valid(&self, revision: u64, images: &ImageSizes) -> bool {
        let (previous, valid) = self.validated.get();
        if previous == revision {
            return valid;
        }
        let valid = self
            .images
            .iter()
            .all(|(url, size)| images.get(url).copied() == *size)
            && self
                .children
                .iter()
                .all(|read| read.valid(revision, images));
        self.validated.set((revision, valid));
        valid
    }

    fn bytes(&self) -> usize {
        size_of::<Self>()
            + 2 * size_of::<usize>() // Rc allocation's reference counts
            + self.images.capacity() * size_of::<(String, Option<(u32, u32)>)>()
            + self.images.iter().map(|(url, _)| url.capacity()).sum::<usize>()
            + self.children.capacity() * size_of::<Rc<Reads>>()
    }
}

#[derive(Default)]
struct Frame {
    images: FxHashMap<String, Option<(u32, u32)>>,
    children: FxHashMap<usize, Rc<Reads>>,
}

#[derive(Default)]
pub(super) struct Tracker {
    revision: u64,
    frames: Vec<Frame>,
    /// Only cache-root / dependency edges count here. Transient read scopes
    /// and the per-pass intrinsic memo own their own short-lived references.
    retained: FxHashMap<usize, usize>,
    bytes: usize,
}

impl Tracker {
    pub(super) fn changed(&mut self) {
        debug_assert!(self.frames.is_empty());
        self.revision = self
            .revision
            .checked_add(1)
            .expect("layout resource revision exhausted");
    }

    pub(super) fn valid(&self, reads: &Inputs, images: &ImageSizes) -> bool {
        reads
            .as_ref()
            .is_none_or(|read| read.valid(self.revision, images))
    }

    pub(super) fn replay(&mut self, reads: &Inputs) {
        if let Some(read) = reads
            && let Some(frame) = self.frames.last_mut()
        {
            frame
                .children
                .entry(Rc::as_ptr(read) as usize)
                .or_insert_with(|| read.clone());
        }
    }

    pub(super) fn observe_image(&mut self, url: &str, size: Option<(u32, u32)>) {
        if let Some(frame) = self.frames.last_mut() {
            // The input map is immutable throughout this transaction. Avoid
            // allocating again for repeated intrinsic/definite reads of a URL.
            if !frame.images.contains_key(url) {
                frame.images.insert(url.to_owned(), size);
            }
        }
    }

    fn begin(&mut self) -> usize {
        let depth = self.frames.len();
        self.frames.push(Frame::default());
        depth
    }

    fn finish(&mut self, depth: usize) -> Inputs {
        assert_eq!(self.frames.len(), depth + 1, "nested layout input scopes");
        let frame = self.frames.pop().unwrap();
        let inputs = if frame.images.is_empty() && frame.children.len() <= 1 {
            // A wrapper which only forwarded another computation introduces
            // no new dependency allocation or validation edge.
            frame.children.into_values().next()
        } else {
            Some(Rc::new(Reads {
                images: frame.images.into_iter().collect(),
                children: frame.children.into_values().collect(),
                validated: Cell::new((self.revision, true)),
            }))
        };
        self.replay(&inputs);
        if self.frames.is_empty() && self.frames.capacity() > 64 {
            self.frames.shrink_to(16);
        }
        inputs
    }

    pub(super) fn retain(&mut self, inputs: &Inputs) {
        let mut pending: Vec<_> = inputs.iter().cloned().collect();
        while let Some(read) = pending.pop() {
            let references = self.retained.entry(Rc::as_ptr(&read) as usize).or_default();
            *references += 1;
            if *references == 1 {
                self.bytes += read.bytes();
                pending.extend(read.children.iter().cloned());
            }
        }
    }

    pub(super) fn release(&mut self, inputs: &Inputs) {
        let mut pending: Vec<_> = inputs.iter().cloned().collect();
        while let Some(read) = pending.pop() {
            let key = Rc::as_ptr(&read) as usize;
            let references = self
                .retained
                .get_mut(&key)
                .expect("retained resource inputs");
            *references -= 1;
            if *references == 0 {
                self.retained.remove(&key);
                self.bytes -= read.bytes();
                pending.extend(read.children.iter().cloned());
            }
        }
        if self.retained.capacity() > self.retained.len().saturating_mul(4).max(64) {
            self.retained
                .shrink_to(self.retained.len().saturating_mul(2));
        }
    }

    pub(super) fn clear_retained(&mut self) {
        self.retained.clear();
        self.bytes = 0;
    }

    pub(super) fn retained_bytes(&self) -> usize {
        self.bytes
            + self.retained.capacity() * size_of::<(usize, usize)>()
            + self.frames.capacity() * size_of::<Frame>()
    }
}

/// A scope never holds a RefCell borrow while running layout. Unwinding also
/// closes the frame so a contained layout panic cannot leave stale inputs on
/// the stack for the next transaction.
pub(in crate::layout2) struct ReadScope<'a> {
    cache: &'a RefCell<LayoutCache>,
    depth: Option<usize>,
}

impl<'a> ReadScope<'a> {
    pub(in crate::layout2) fn new(dom: &'a Dom, enabled: bool) -> Self {
        Self {
            cache: &dom.layout_cache,
            depth: enabled.then(|| dom.layout_cache.borrow_mut().inputs.begin()),
        }
    }

    pub(in crate::layout2) fn finish(mut self) -> Inputs {
        self.depth
            .take()
            .and_then(|depth| self.cache.borrow_mut().inputs.finish(depth))
    }
}

impl Drop for ReadScope<'_> {
    fn drop(&mut self) {
        if let Some(depth) = self.depth.take() {
            self.cache.borrow_mut().inputs.finish(depth);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negative_reads_and_cache_hit_replay_survive_other_resource_changes() {
        let mut tracker = Tracker::default();
        let scope = tracker.begin();
        tracker.observe_image("image", None);
        let child = tracker.finish(scope);
        tracker.retain(&child);
        let scope = tracker.begin();
        tracker.replay(&child);
        tracker.replay(&child);
        let parent = tracker.finish(scope);
        assert!(Rc::ptr_eq(
            child.as_ref().unwrap(),
            parent.as_ref().unwrap()
        ));
        let bytes = tracker.bytes;
        tracker.retain(&parent);
        assert_eq!(tracker.bytes, bytes);
        let mut images = ImageSizes::from([("unrelated".into(), (10, 20))]);
        tracker.changed();
        assert!(tracker.valid(&parent, &images));
        images.insert("image".into(), (40, 30));
        tracker.changed();
        assert!(!tracker.valid(&parent, &images));
        images.remove("image");
        tracker.changed();
        assert!(tracker.valid(&parent, &images));
        tracker.release(&child);
        assert_eq!(tracker.bytes, bytes);
        tracker.release(&parent);
        assert_eq!(tracker.bytes, 0);
        assert!(tracker.retained.is_empty());
    }

    #[test]
    fn shared_nested_inputs_are_charged_once_until_the_last_root_retires() {
        let mut tracker = Tracker::default();
        let scope = tracker.begin();
        tracker.observe_image("shared", Some((20, 30)));
        let child = tracker.finish(scope);
        let mut roots = Vec::new();
        for index in 0..256 {
            let scope = tracker.begin();
            tracker.replay(&child);
            tracker.observe_image(&format!("image-{index}"), None);
            let root = tracker.finish(scope);
            tracker.retain(&root);
            roots.push(root);
        }
        assert_eq!(tracker.retained.len(), 257);
        let expected = child.as_ref().unwrap().bytes()
            + roots
                .iter()
                .map(|root| root.as_ref().unwrap().bytes())
                .sum::<usize>();
        assert_eq!(tracker.bytes, expected);
        for root in roots {
            tracker.release(&root);
        }
        assert!(tracker.retained.is_empty());
        assert_eq!(tracker.bytes, 0);
    }

    #[test]
    fn contained_panic_closes_the_dependency_transaction() {
        let dom = Dom::parse_document("<p>text</p>");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _outer = ReadScope::new(&dom, true);
            let _inner = ReadScope::new(&dom, true);
            super::super::image_size(&dom, &ImageSizes::new(), "missing");
            panic!("contained layout failure");
        }));
        assert!(result.is_err());
        let cache = dom.layout_cache.borrow();
        assert!(cache.inputs.frames.is_empty());
        assert!(cache.inputs.retained.is_empty());
    }
}
