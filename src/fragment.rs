//! HTML fragment identifiers, shared by both presentation adapters.
//! WHATWG HTML snapshot e5071a20 (2026-09-06), #select-the-indicated-part
//! and #find-a-potential-indicated-element; URL #percent-decode.

use std::collections::HashMap;

use crate::dom::{DOCUMENT, Dom, NodeId};

pub fn same_document(left: &url::Url, right: &url::Url) -> bool {
    let mut left = left.clone();
    let mut right = right.clone();
    left.set_fragment(None);
    right.set_fragment(None);
    left == right
}

pub(crate) fn decode(fragment: &str) -> String {
    let bytes = fragment.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (
                (bytes[i + 1] as char).to_digit(16),
                (bytes[i + 2] as char).to_digit(16),
            )
        {
            decoded.push((hi * 16 + lo) as u8);
            i += 3;
        } else {
            decoded.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

/// Raw identifiers take precedence over decoded ones; an element named `top`
/// takes precedence over the special top-of-document fallback.
pub fn position<T: Copy>(fragment: &str, targets: &HashMap<String, T>, top: T) -> Option<T> {
    if fragment.is_empty() {
        return Some(top);
    }
    targets.get(fragment).copied().or_else(|| {
        let decoded = decode(fragment);
        targets
            .get(&decoded)
            .copied()
            .or_else(|| decoded.eq_ignore_ascii_case("top").then_some(top))
    })
}

/// Select before looking up geometry: IDs beat legacy names regardless of
/// their relative positions, and duplicates use DOM tree order, not paint order.
pub(crate) fn targets(dom: &Dom) -> HashMap<String, NodeId> {
    let mut targets = HashMap::new();
    for attribute in ["id", "name"] {
        for node in dom.descendants(DOCUMENT) {
            if dom.owner_document(node) != Some(DOCUMENT) {
                continue;
            }
            if attribute == "name" && dom.tag_name(node) != Some("a") {
                continue;
            }
            if let Some(name) = dom.attr(node, attribute).filter(|name| !name.is_empty()) {
                targets.entry(name.to_owned()).or_insert(node);
            }
        }
    }
    targets
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fragment_lookup_preserves_literal_decoded_and_top_precedence() {
        let targets = HashMap::from([
            ("caf%C3%A9".into(), 10usize),
            ("café".into(), 20),
            ("top".into(), 30),
            ("a+b".into(), 40),
            ("�".into(), 50),
        ]);
        assert_eq!(position("caf%C3%A9", &targets, 0), Some(10));
        assert_eq!(position("caf%c3%a9", &targets, 0), Some(20));
        assert_eq!(position("top", &targets, 0), Some(30));
        assert_eq!(position("%74op", &targets, 0), Some(30));
        assert_eq!(position("ToP", &targets, 0), Some(0));
        assert_eq!(position("", &targets, 0), Some(0));
        assert_eq!(position("a+b", &targets, 0), Some(40));
        assert_eq!(position("%FF", &targets, 0), Some(50));
        assert_eq!(position("missing", &targets, 0), None);
        assert_eq!(decode("%broken%2"), "%broken%2");
    }

    #[test]
    fn fragment_targets_use_document_order_and_id_before_name() {
        let dom = Dom::parse_document(
            "<a name=x></a><div id=x>first</div><p id=x>second</p><input name=y><a name=y></a>",
        );
        let targets = targets(&dom);
        assert_eq!(dom.tag_name(targets["x"]), Some("div"));
        assert_eq!(dom.tag_name(targets["y"]), Some("a"));
    }

    #[test]
    fn fragment_target_styles_follow_navigation_without_changing_on_history_rewrites() {
        let mut dom = Dom::parse_document(
            "<style>:target{color:red}p:not(:target){color:blue}</style><p id=one>One</p><p id=two>Two</p>",
        );
        let one = dom.get_by_id("one").unwrap();
        let two = dom.get_by_id("two").unwrap();
        dom.set_fragment_target(Some("one"));
        assert!(dom.matches(one, &crate::dom::SelectorList::parse(":target").unwrap()));
        assert!(!dom.matches(two, &crate::dom::SelectorList::parse(":target").unwrap()));
        dom.set_doc_url(Some(url::Url::parse("https://example.test/#two").unwrap()));
        assert!(
            dom.matches(one, &crate::dom::SelectorList::parse(":target").unwrap()),
            "history URL writes preserve target state"
        );
        dom.set_fragment_target(Some("two"));
        assert!(dom.matches(two, &crate::dom::SelectorList::parse(":target").unwrap()));
        assert!(dom.matches(
            one,
            &crate::dom::SelectorList::parse("p:not(:target)").unwrap()
        ));
        dom.set_fragment_target(Some("missing"));
        assert!(!dom.matches(two, &crate::dom::SelectorList::parse(":target").unwrap()));
    }
}
