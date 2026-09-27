//! Bounded recency metadata for optional layout caches.
//!
//! No timestamp wraparound, whole-cache victim scan, or append-only access
//! journal. A lookup, unlink, or victim choice takes constant expected work.
//! Keys are stable DOM identities, never recycled arena offsets.

use crate::dom::NodeId;
use rustc_hash::FxHashMap;

#[derive(Default)]
struct Links {
    older: Option<NodeId>,
    newer: Option<NodeId>,
}

#[derive(Default)]
pub(super) struct Recency {
    links: FxHashMap<NodeId, Links>,
    oldest: Option<NodeId>,
    newest: Option<NodeId>,
}

impl Recency {
    pub(super) fn clear(&mut self) {
        self.links.clear();
        self.oldest = None;
        self.newest = None;
    }

    pub(super) fn remove(&mut self, node: NodeId) {
        let Some(links) = self.links.remove(&node) else {
            return;
        };
        if let Some(older) = links.older {
            self.links.get_mut(&older).unwrap().newer = links.newer;
        } else {
            self.oldest = links.newer;
        }
        if let Some(newer) = links.newer {
            self.links.get_mut(&newer).unwrap().older = links.older;
        } else {
            self.newest = links.older;
        }
    }

    pub(super) fn touch(&mut self, node: NodeId) {
        if self.newest == Some(node) {
            return;
        }
        self.remove(node);
        self.links.insert(
            node,
            Links {
                older: self.newest,
                newer: None,
            },
        );
        if let Some(newest) = self.newest {
            self.links.get_mut(&newest).unwrap().newer = Some(node);
        } else {
            self.oldest = Some(node);
        }
        self.newest = Some(node);
    }

    pub(super) fn oldest(&self) -> Option<NodeId> {
        self.oldest
    }

    pub(super) fn retained_bytes(&self) -> usize {
        self.links.capacity() * std::mem::size_of::<(NodeId, Links)>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recency_matches_reference_through_hits_eviction_and_replacement() {
        let mut order = Recency::default();
        let mut reference = std::collections::VecDeque::new();
        let mut random = 17u64;
        for i in 0..20_000 {
            random = random.wrapping_mul(6364136223846793005).wrapping_add(1);
            let node = (random >> 32) as usize % 64;
            reference.retain(|&n| n != node);
            if i % 4 == 0 {
                order.remove(node);
            } else {
                order.touch(node);
                reference.push_back(node);
            }
            if i % 101 == 0 {
                order.clear();
                reference.clear();
            }
            assert_eq!(order.oldest(), reference.front().copied());
            assert_eq!(order.newest, reference.back().copied());
            assert_eq!(order.links.len(), reference.len());
            let mut previous = None;
            for &node in &reference {
                assert_eq!(order.links[&node].older, previous);
                if let Some(previous) = previous {
                    assert_eq!(order.links[&previous].newer, Some(node));
                }
                previous = Some(node);
            }
        }
    }
}
