//! Native live tag-name and class-name collection views. DOM a2331a45
//! #concept-getelementsbytagname, #concept-getelementsbytagnamens,
//! #concept-getelementsbyclassname and #concept-collection-live.
//! Every observation reflects the current tree: membership is reused only
//! while the DOM epoch (bumped by every tree and attribute mutation) is
//! unchanged. Node identities are never recycled (see `arena`), so a cached
//! root cannot designate a different node.

use super::*;
use std::rc::Rc;

/// Indexed loops read one collection to completion; a few entries cover
/// interleaved reads without letting retained members grow with the page.
const MAX_ENTRIES: usize = 8;

/// The membership query of one collection view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CollectionQuery<'a> {
    /// `namespace` is None for getElementsByTagName and `Some("")` for the
    /// null namespace of getElementsByTagNameNS.
    Tag {
        qualified: &'a str,
        namespace: Option<&'a str>,
    },
    Class {
        names: &'a str,
        document_root: bool,
    },
}

impl<'a> CollectionQuery<'a> {
    /// A self-describing key naming `root` and this query. The engine keeps
    /// it as the view's membership state; it carries no object references.
    pub(crate) fn key(&self, root: NodeId) -> String {
        match *self {
            Self::Tag {
                qualified,
                namespace: None,
            } => format!("{root}:t:{qualified}"),
            Self::Tag {
                qualified,
                namespace: Some(namespace),
            } => format!("{root}:n{}:{namespace}{qualified}", namespace.len()),
            Self::Class {
                names,
                document_root,
            } => format!("{root}:{}:{names}", if document_root { 'd' } else { 'c' }),
        }
    }

    pub(crate) fn parse(key: &'a str) -> Option<(NodeId, Self)> {
        let (root, rest) = key.split_once(':')?;
        let root = root.parse().ok()?;
        let (kind, rest) = rest.split_once(':')?;
        let query = match kind {
            "t" => Self::Tag {
                qualified: rest,
                namespace: None,
            },
            "c" | "d" => Self::Class {
                names: rest,
                document_root: kind == "d",
            },
            _ => {
                let length: usize = kind.strip_prefix('n')?.parse().ok()?;
                let namespace = rest.get(..length)?;
                Self::Tag {
                    qualified: rest.get(length..)?,
                    namespace: Some(namespace),
                }
            }
        };
        Some((root, query))
    }
}

struct Entry {
    key: Box<str>,
    epoch: u64,
    members: Rc<[NodeId]>,
}

#[derive(Default)]
pub(super) struct State {
    /// Most recently used last.
    entries: Vec<Entry>,
}

impl State {
    pub(super) fn retained_bytes(&self) -> usize {
        self.entries.capacity() * std::mem::size_of::<Entry>()
            + self
                .entries
                .iter()
                .map(|entry| entry.key.len() + std::mem::size_of_val(&*entry.members))
                .sum::<usize>()
    }
}

impl Dom {
    /// The current members of the collection view named by `key` (see
    /// [`CollectionQuery::key`]), in tree order. An unparsable key or an
    /// invalid root has no members.
    pub(crate) fn query_collection(&self, key: &str) -> Rc<[NodeId]> {
        let mut state = self.query_lists.borrow_mut();
        if let Some(position) = state.entries.iter().rposition(|entry| &*entry.key == key) {
            let entry = state.entries.remove(position);
            if entry.epoch == self.epoch {
                let members = entry.members.clone();
                state.entries.push(entry);
                return members;
            }
        }
        let members: Rc<[NodeId]> = match CollectionQuery::parse(key) {
            Some((
                root,
                CollectionQuery::Tag {
                    qualified,
                    namespace,
                },
            )) => self.elements_by_tag_name(root, qualified, namespace).into(),
            Some((
                root,
                CollectionQuery::Class {
                    names,
                    document_root,
                },
            )) => self
                .elements_by_class_name(root, names, document_root)
                .into(),
            None => Rc::from([]),
        };
        if state.entries.len() >= MAX_ENTRIES {
            state.entries.remove(0);
        }
        state.entries.push(Entry {
            key: key.into(),
            epoch: self.epoch,
            members: members.clone(),
        });
        members
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_collection_keys_round_trip_every_query_shape() {
        for query in [
            CollectionQuery::Tag {
                qualified: "*",
                namespace: None,
            },
            CollectionQuery::Tag {
                qualified: "svg:a:b",
                namespace: Some("http://www.w3.org/2000/svg"),
            },
            CollectionQuery::Tag {
                qualified: "x",
                namespace: Some(""),
            },
            CollectionQuery::Tag {
                qualified: "",
                namespace: Some("a:1:b"),
            },
            CollectionQuery::Class {
                names: " a  b:c ",
                document_root: false,
            },
            CollectionQuery::Class {
                names: "",
                document_root: true,
            },
        ] {
            let key = query.key(42);
            assert_eq!(CollectionQuery::parse(&key), Some((42, query)), "{key}");
        }
        assert_eq!(CollectionQuery::parse("x:t:a"), None);
        assert_eq!(CollectionQuery::parse("1:n9:short"), None);
    }

    #[test]
    fn query_collections_follow_tree_and_attribute_mutations() {
        let mut dom = Dom::parse_document(
            "<main id=r><p class='a b'></p><span class=a></span><p></p></main>",
        );
        let root = dom.get_by_id("r").unwrap();
        let paragraphs = CollectionQuery::Tag {
            qualified: "p",
            namespace: None,
        }
        .key(root);
        let class_a = CollectionQuery::Class {
            names: "a",
            document_root: false,
        }
        .key(root);
        let check = |dom: &Dom| {
            assert_eq!(
                &*dom.query_collection(&paragraphs),
                dom.elements_by_tag_name(root, "p", None).as_slice()
            );
            assert_eq!(
                &*dom.query_collection(&class_a),
                dom.elements_by_class_name(root, "a", false).as_slice()
            );
        };
        check(&dom);
        assert_eq!(dom.query_collection(&paragraphs).len(), 2);
        assert_eq!(dom.query_collection(&class_a).len(), 2);
        let added = dom.create_element("p");
        dom.set_attr(added, "class", "a");
        dom.append(root, added);
        check(&dom);
        assert_eq!(dom.query_collection(&paragraphs).len(), 3);
        dom.set_attr(added, "class", "b");
        check(&dom);
        assert_eq!(dom.query_collection(&class_a).len(), 2);
        dom.detach(added);
        check(&dom);
        // More views than the cache holds still answer from the live tree.
        for name in ["main", "p", "span", "b", "i", "q", "s", "u", "em", "p"] {
            let key = CollectionQuery::Tag {
                qualified: name,
                namespace: None,
            }
            .key(root);
            assert_eq!(
                &*dom.query_collection(&key),
                dom.elements_by_tag_name(root, name, None).as_slice()
            );
        }
        check(&dom);
        assert!(dom.query_collection("garbage").is_empty());
    }
}
