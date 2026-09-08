//! Attribute-owned class membership, independent of style/layout revisions.
//!
//! Selectors 4 §6.6: a class matches a complete whitespace-separated token,
//! not a substring. Keep the matcher's existing ASCII-whitespace and exact
//! identity semantics. The short-list path remains allocation-free; long
//! lists pay tokenization once until `touch_attr` invalidates a class write.

use super::*;

pub(super) type ClassTokens = FxHashSet<Box<str>>;

// Avoid allocating hash tables for the common one/two-short-class case.
const MEMOIZE_BYTES: usize = 64;

impl Dom {
    pub(super) fn matches_classes(&self, id: NodeId, required: &[String]) -> bool {
        if required.is_empty() {
            return true;
        }
        let classes = self.attr(id, "class").unwrap_or("");
        if classes.len() < MEMOIZE_BYTES {
            return required
                .iter()
                .all(|want| classes.split_ascii_whitespace().any(|token| token == want));
        }
        if let Some(tokens) = self.class_cache.borrow().get(id, 0) {
            return required.iter().all(|want| tokens.contains(want.as_str()));
        }
        let tokens: ClassTokens = classes.split_ascii_whitespace().map(Box::from).collect();
        let matches = required.iter().all(|want| tokens.contains(want.as_str()));
        self.class_cache.borrow_mut().put(id, 0, tokens);
        matches
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn class_tokens_match_streaming_identity_and_whitespace() {
        let mut dom = Dom::new();
        let id = dom.create_element("div");
        // NBSP and vertical tab are NOT ASCII-whitespace token separators.
        let value = format!(
            "{} alpha\tbeta\ngamma\x0cdelta\repsilon alpha Alpha café md:size a\u{a0}b v\x0bt",
            "padding ".repeat(10)
        );
        dom.set_attr(id, "class", &value);
        let checks = [
            "alpha", "beta", "gamma", "delta", "epsilon", "Alpha", "ALPHA", "café", "md:size",
            "a\u{a0}b", "v\x0bt", "a", "b", "v", "t", "padding", "pad", "",
        ];
        for _ in 0..3 {
            for a in checks {
                for b in checks {
                    let required = [a.to_string(), b.to_string()];
                    let expected = required
                        .iter()
                        .all(|want| value.split_ascii_whitespace().any(|token| token == want));
                    assert_eq!(dom.matches_classes(id, &required), expected, "{a:?} {b:?}");
                }
            }
        }
        for selector in [r".md\:size", ".café", ".alpha.beta.gamma.delta.epsilon"] {
            assert!(dom.matches(id, &SelectorList::parse(selector).unwrap()));
        }
        assert!(dom.class_cache.borrow().get(id, 0).is_some());
    }

    #[test]
    fn class_tokens_survive_restyles_but_not_class_mutations() {
        let mut dom = Dom::parse_document(
            "<!doctype html><style>.hit {width:17px} .miss {width:29px}</style><main><div id=x></div></main>",
        );
        let id = dom.get_by_id("x").unwrap();
        let value = format!("{} hit", "padding ".repeat(10));
        dom.set_attr(id, "class", &value);
        assert_eq!(dom.computed_style(id, "width").as_deref(), Some("17px"));
        let retained_token = || {
            dom.class_cache
                .borrow()
                .get(id, 0)
                .unwrap()
                .get("hit")
                .unwrap()
                .as_ptr()
        };
        let original = retained_token();
        let bytes_before = dom.retained_memory().0;
        dom.set_attr(id, "style", "color:red");
        let child = dom.create_element("span");
        dom.append(id, child);
        assert_eq!(dom.computed_style(id, "width").as_deref(), Some("17px"));
        assert_eq!(
            dom.class_cache
                .borrow()
                .get(id, 0)
                .unwrap()
                .get("hit")
                .unwrap()
                .as_ptr(),
            original,
            "unrelated structural/style invalidation must retain parsed tokens"
        );
        dom.set_attr(id, "CLASS", &value);
        assert!(
            dom.class_cache.borrow().get(id, 0).is_some(),
            "idempotent write"
        );
        dom.set_attr(id, "CLASS", &value.replace("hit", "miss"));
        assert!(dom.class_cache.borrow().get(id, 0).is_none());
        assert_eq!(dom.computed_style(id, "width").as_deref(), Some("29px"));
        assert!(dom.class_cache.borrow().get(id, 0).is_some());
        let bytes_with_tokens = dom.retained_memory().0;
        assert!(bytes_with_tokens >= bytes_before);
        dom.remove_attr(id, "CLASS");
        assert!(dom.class_cache.borrow().get(id, 0).is_none());
        assert!(dom.retained_memory().0 < bytes_with_tokens);
        assert!(!dom.matches(id, &SelectorList::parse(".hit, .miss").unwrap()));
        dom.set_attr(id, "class", "hit");
        assert_eq!(dom.computed_style(id, "width").as_deref(), Some("17px"));
        assert!(dom.class_cache.borrow().get(id, 0).is_none());
    }

    #[test]
    fn class_tokens_short_and_unconstrained_matches_do_not_allocate() {
        let mut dom = Dom::new();
        let id = dom.create_element("div");
        for value in ["", "one", "one two\tthree"] {
            dom.set_attr(id, "class", value);
            for want in ["", "one", "two", "missing"] {
                assert_eq!(
                    dom.matches_classes(id, &[want.into()]),
                    value.split_ascii_whitespace().any(|token| token == want)
                );
            }
            assert!(dom.class_cache.borrow().slots.is_empty());
        }
        dom.set_attr(id, "class", &"long-class ".repeat(20));
        assert!(dom.matches_classes(id, &[]));
        assert!(dom.class_cache.borrow().slots.is_empty());
        assert!(!dom.matches_classes(id, &["absent".into()]));
        assert!(
            dom.class_cache.borrow().get(id, 0).is_some(),
            "cache misses too"
        );
    }
}
