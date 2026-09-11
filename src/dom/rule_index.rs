//! Necessary subject keys for rule-hash candidate rejection.
//!
//! Selectors 4 §§4.2, 4.4, 19.3: `:is()` and `:where()` match the union of
//! their arguments; each argument's rightmost compound matches the subject.
//! Every argument must supply a necessary key before that union is indexed.
//! This does not expand/rewrite selectors or change specificity. Negation,
//! relational/positional selectors and uncertain alternatives keep the full
//! matcher fallback. The index can admit extra candidates, never omit a match.

use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Key<'a> {
    Id(&'a str),
    Class(&'a str),
    Tag(&'a str),
}

// Bound both retained bucket fanout and extra analysis of adversarial nesting.
// Exceeding any budget makes only the optimization unavailable.
const MAX_KEYS: usize = 64;
const MAX_DEPTH: usize = 16;
const MAX_VISITS: usize = 256;

pub(super) fn subject_keys(compound: &Compound) -> Option<Vec<Key<'_>>> {
    let mut remaining = MAX_VISITS;
    keys(compound, 0, &mut remaining)
}

fn keys<'a>(compound: &'a Compound, depth: usize, remaining: &mut usize) -> Option<Vec<Key<'a>>> {
    if depth >= MAX_DEPTH || *remaining == 0 {
        return None;
    }
    *remaining -= 1;
    if let Some(id) = &compound.id {
        return Some(vec![Key::Id(id)]);
    }
    if let Some(class) = compound.classes.first() {
        return Some(vec![Key::Class(class)]);
    }
    if let Some(tag) = compound.tag.as_deref().filter(|tag| *tag != "*") {
        return Some(vec![Key::Tag(tag)]);
    }
    // Multiple logical pseudo-classes in one compound are conjunctive. One
    // fully indexable group's union is a sufficient necessary condition.
    for (group, _) in &compound.selects {
        if *remaining == 0 {
            break;
        }
        if group.is_empty() {
            continue;
        }
        let mut union = Vec::new();
        let complete = group.iter().all(|selector| {
            let Some((_, subject)) = selector.0.last() else {
                return false;
            };
            let Some(alternatives) = keys(subject, depth + 1, remaining) else {
                return false;
            };
            for key in alternatives {
                if !union.contains(&key) {
                    if union.len() == MAX_KEYS {
                        return false;
                    }
                    union.push(key);
                }
            }
            true
        });
        if complete {
            return Some(union);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_candidates_cover_full_scan(dom: &Dom) {
        let index = dom.style_index();
        for node in 0..dom.node_count() {
            if dom.tag_name(node).is_none() {
                continue;
            }
            let scope = dom.tree_scope(node);
            let Some(rules) = index.scopes.get(&scope) else {
                continue;
            };
            let mut candidates = Vec::new();
            index.buckets[&scope].candidates(dom, node, &mut candidates);
            assert!(
                candidates.windows(2).all(|pair| pair[0] < pair[1]),
                "candidate union must be unique"
            );
            let actual = candidates
                .into_iter()
                .filter(|ri| dom.matches_complex(node, &rules[*ri as usize].selector.0, None))
                .collect::<Vec<_>>();
            let expected = rules
                .iter()
                .enumerate()
                .filter(|(_, rule)| dom.matches_complex(node, &rule.selector.0, None))
                .map(|(ri, _)| ri as u32)
                .collect::<Vec<_>>();
            assert_eq!(
                actual, expected,
                "candidate rejection omitted a match on node {node}"
            );
            assert_eq!(
                *dom.matched_rules(node),
                expected,
                "cached matcher disagrees on node {node}"
            );
        }
    }

    #[test]
    fn cursor_candidates_preserve_cascade_and_avoid_unrelated_elements() {
        let mut dom = Dom::parse_document(
            r#"<style>
            * { box-sizing:border-box; margin:0 }
            .active :is(.target, #alternate) { cursor:pointer }
            [data-cursor] { cursor:var(--cursor) }
            .target:hover { cursor:crosshair }
            </style><main id=root class=active><a id=target class=target>one</a>
            <span id=alternate>two</span><span id=inline style='cursor:pointer'>three</span>
            <span id=variable data-cursor style='--cursor:pointer'>four</span>
            <p id=unrelated>unrelated</p></main>"#,
        );
        // Detached feature probes must not disable candidate rejection for
        // the whole document merely because a shadow root exists somewhere.
        let probe = dom.create_element("div");
        dom.attach_shadow(probe);
        for active in ["active", ""] {
            dom.set_attr(dom.get_by_id("root").unwrap(), "class", active);
            let nodes = dom.composed_descendants(DOCUMENT);
            let candidates = dom.cursor_style_candidates(&nodes);
            let pointers = |ids: &[NodeId]| {
                ids.iter()
                    .copied()
                    .filter(|&node| {
                        dom.computed_style(node, "cursor").as_deref() == Some("pointer")
                    })
                    .collect::<Vec<_>>()
            };
            assert_eq!(pointers(&candidates), pointers(&nodes));
            assert!(!candidates.contains(&dom.get_by_id("unrelated").unwrap()));
        }
        let unrelated = dom.get_by_id("unrelated").unwrap();
        dom.set_attr(unrelated, "style", "cursor:pointer");
        assert!(
            dom.cursor_style_candidates(&dom.composed_descendants(DOCUMENT))
                .contains(&unrelated)
        );
    }

    #[test]
    fn rule_index_logical_subjects_preserve_full_matching_and_specificity() {
        let mut dom = Dom::parse_document(
            r#"<style>
            .prose :where(p, h2, h3):not(:where(.not-prose *)) { margin:2px }
            .prose :where(a strong, thead th strong, .emphasis) { padding:3px }
            :is(#one, .red, span):where(.active, .another) { top:4px }
            :where(:is(.red, .blue), :where(.green, .yellow)) { left:5px }
            :where(.m\:thing) { right:6px }
            :is(:not(.blocked), .red) { bottom:7px }
            :not(:is(.hidden, .disabled)) { min-height:8px }
            :has(> .mark) { min-width:9px }
            :nth-child(2 of :is(.red, .blue)) { max-height:10px }
            :where([data-any], .red) { max-width:11px }
            :is(.red, *) { border-width:1px }
            .red { color:red; height:10px }
            :where(.red) { color:blue }
            :is(#absent, .red) { height:20px }
        </style><main class="prose"><p id="one" class="red red active m:thing"><strong class="mark">one</strong></p>
            <div class="not-prose"><p id="two" data-any="yes" class="blue">two</p></div>
            <a><strong>three</strong></a><span class="emphasis green another"></span></main>"#,
        );
        assert_candidates_cover_full_scan(&dom);
        let one = dom.get_by_id("one").unwrap();
        assert_eq!(dom.computed_style(one, "color").as_deref(), Some("red"));
        assert_eq!(dom.computed_style(one, "height").as_deref(), Some("20px"));
        dom.set_attr(one, "class", "green another");
        assert_candidates_cover_full_scan(&dom);
        dom.remove_attr(dom.get_by_id("two").unwrap(), "data-any");
        assert_candidates_cover_full_scan(&dom);
    }

    #[test]
    fn rule_index_rejects_impossible_functional_candidates_before_matching() {
        let mut css = String::new();
        for index in 0..500 {
            css.push_str(&format!(".scope :where(.target-{index}) {{ color:red }}"));
        }
        let dom = Dom::parse_document(&format!(
            "<style>{css}</style><main class=scope><span id=target class=target-123></span><span id=unrelated></span></main>"
        ));
        assert_candidates_cover_full_scan(&dom);
        let index = dom.style_index();
        let buckets = &index.buckets[&DOCUMENT];
        assert!(
            buckets.universal.is_empty(),
            "functional class subjects must not enter the universal bucket"
        );
        let mut candidates = Vec::new();
        buckets.candidates(&dom, dom.get_by_id("target").unwrap(), &mut candidates);
        assert_eq!(
            candidates.len(),
            1,
            "499 impossible rules must not reach the full matcher"
        );
        candidates.clear();
        buckets.candidates(&dom, dom.get_by_id("unrelated").unwrap(), &mut candidates);
        assert!(candidates.is_empty());
    }

    #[test]
    fn rule_index_fanout_and_depth_limits_preserve_universal_fallback() {
        let wide = format!(
            ":is({})",
            (0..MAX_KEYS + 1)
                .map(|n| format!(".c{n}"))
                .collect::<Vec<_>>()
                .join(",")
        );
        let deep = format!(
            "{}.c0{}",
            ":is(".repeat(MAX_DEPTH + 2),
            ")".repeat(MAX_DEPTH + 2)
        );
        for selector in [&wide, &deep] {
            let parsed = SelectorList::parse(selector).unwrap();
            assert!(subject_keys(&parsed.0[0].0.last().unwrap().1).is_none());
        }
        let dom = Dom::parse_document(&format!(
            "<style>{wide} {{ color:red }} {deep} {{ height:5px }}</style><p class=c0>match</p>"
        ));
        assert_candidates_cover_full_scan(&dom);
        assert_eq!(dom.style_index().buckets[&DOCUMENT].universal.len(), 2);
    }
}
