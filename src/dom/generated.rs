//! CSS Lists 3 #creating-counters and CSS Content 3 #quote-values.
//! Counters follow the flattened tree, including generated first/last children.
//! CSSWG snapshot 81c27f686901 (2026-09-06).
use super::*;
use counter_styles::{identifier, valid_name};

#[derive(Default)]
pub(super) struct Generated {
    pub content: FxHashMap<(NodeId, u8), String>,
    pub list_items: FxHashMap<NodeId, i64>,
    pub dynamic: bool,
}
impl Generated {
    pub fn retained_bytes(&self) -> usize {
        self.content.capacity() * std::mem::size_of::<((NodeId, u8), String)>()
            + self.content.values().map(String::capacity).sum::<usize>()
            + self.list_items.capacity() * std::mem::size_of::<(NodeId, i64)>()
    }
}
#[derive(Clone, Debug)]
struct Change {
    name: String,
    value: Option<i64>,
    reversed: bool,
}
fn changes(value: &str, property: &str) -> Option<Vec<Change>> {
    if value.trim().eq_ignore_ascii_case("none") {
        return Some(vec![]);
    }
    let tokens = split_top_level_ws(value);
    let mut i = 0;
    let mut result = Vec::new();
    while i < tokens.len() {
        let token = tokens[i];
        let reversed = token
            .strip_prefix("reversed(")
            .and_then(|s| s.strip_suffix(')'));
        if reversed.is_some() && property != "counter-reset" {
            return None;
        }
        let name = reversed.unwrap_or(token).trim();
        if !valid_name(name) {
            return None;
        }
        let name = identifier(name)?;
        i += 1;
        let explicit = tokens.get(i).and_then(|v| v.parse::<i64>().ok());
        if explicit.is_some() {
            i += 1;
        }
        let value = explicit.or_else(|| {
            if reversed.is_some() {
                None
            } else {
                Some(if property == "counter-increment" {
                    1
                } else {
                    0
                })
            }
        });
        if property == "counter-reset" {
            result.retain(|c: &Change| c.name != name);
        }
        result.push(Change {
            name,
            value,
            reversed: reversed.is_some(),
        });
    }
    (!result.is_empty()).then_some(result)
}
pub(super) fn valid_changes(value: &str, property: &str) -> bool {
    changes(value, property).is_some()
}
#[derive(Clone)]
enum Token {
    Text(String),
    Attribute(String, String),
    Counter {
        name: String,
        separator: Option<String>,
        style: String,
    },
    Quote {
        open: bool,
        emit: bool,
    },
}
fn content(raw: &str) -> Option<Vec<Token>> {
    let visual = split_top_level_slash(raw).map_or(raw, |(v, _)| v).trim();
    if matches!(visual, "none" | "normal" | "") || !cssom::valid_value(visual) {
        return None;
    }
    split_top_level_ws(visual)
        .into_iter()
        .map(|token| {
            if let Some(text) = unquote_css(token) {
                return Some(Token::Text(text));
            }
            match token {
                "open-quote" | "close-quote" | "no-open-quote" | "no-close-quote" => {
                    return Some(Token::Quote {
                        open: token.ends_with("open-quote"),
                        emit: !token.starts_with("no-"),
                    });
                }
                _ => {}
            }
            let (func, body) = token.split_once('(')?;
            let args = split_top_level(body.strip_suffix(')')?, ',');
            match func {
                "attr" if (1..=2).contains(&args.len()) => {
                    let attr = identifier(args[0].trim())?;
                    let fallback = args
                        .get(1)
                        .map(|s| unquote_css(s.trim()))
                        .unwrap_or(Some(String::new()))?;
                    Some(Token::Attribute(attr, fallback))
                }
                "counter" | "counters" => {
                    let plural = func == "counters";
                    if args.len() < if plural { 2 } else { 1 }
                        || args.len() > if plural { 3 } else { 2 }
                        || !valid_name(args[0].trim())
                    {
                        return None;
                    }
                    let separator = if plural {
                        Some(unquote_css(args[1].trim())?)
                    } else {
                        None
                    };
                    let style = args
                        .get(if plural { 2 } else { 1 })
                        .map_or("decimal", |v| v.trim());
                    if !valid_name(style) && style != "none" && !style.starts_with("symbols(") {
                        return None;
                    }
                    Some(Token::Counter {
                        name: identifier(args[0].trim())?,
                        separator,
                        style: style.to_string(),
                    })
                }
                _ => None,
            }
        })
        .collect()
}
pub(super) fn simple_content(dom: &Dom, id: NodeId, raw: &str) -> Option<String> {
    let mut out = String::new();
    for token in content(raw)? {
        match token {
            Token::Text(s) => out.push_str(&s),
            Token::Attribute(name, fallback) => {
                out.push_str(dom.attr(id, &name).unwrap_or(&fallback))
            }
            _ => return None,
        }
    }
    Some(out)
}
struct Event {
    node: NodeId,
    pseudo: Option<PseudoEl>,
    parent: Option<usize>,
    previous: Option<usize>,
    reset: Vec<Change>,
    increment: Vec<Change>,
    set: Vec<Change>,
    list_item: bool,
    tokens: Option<Vec<Token>>,
}
fn events(dom: &Dom) -> Vec<Event> {
    let mut result = Vec::new();
    let mut previous: FxHashMap<Option<usize>, usize> = FxHashMap::default();
    let mut stack = vec![(DOCUMENT, None, None)];
    while let Some((id, pseudo, parent)) = stack.pop() {
        let display = match pseudo {
            Some(p) => dom.pseudo_layout_value(id, p, "display"),
            None => dom.effective_display(id),
        };
        if display.as_deref() == Some("none")
            || (pseudo.is_none()
                && (dom.attr(id, "hidden").is_some()
                    || (dom.tag_name(id) == Some("dialog")
                        && dom.attr(id, "open").is_none()
                        && !dom.author_declares(id, "display"))
                    || (dom.attr(id, "popover").is_some() && !dom.is_popover_showing(id))))
        {
            continue;
        }
        let value = |p| match pseudo {
            Some(which) => dom.pseudo_layout_value(id, which, p),
            None => dom.computed_value_resolved(id, p),
        };
        let tokens = pseudo.and_then(|which| {
            value("content")
                .or_else(|| {
                    (dom.tag_name(id) == Some("q")).then(|| {
                        if which == PseudoEl::Before {
                            "open-quote"
                        } else {
                            "close-quote"
                        }
                        .to_string()
                    })
                })
                .as_deref()
                .and_then(content)
        });
        if pseudo.is_some() && tokens.is_none() {
            continue;
        }
        let boxed = display.as_deref() != Some("contents")
            && (dom.tag_name(id).is_some() || pseudo.is_some());
        let list_item = boxed
            && display
                .as_deref()
                .is_some_and(|s| s.split_whitespace().any(|v| v == "list-item"));
        let parse = |p| {
            if boxed {
                value(p)
                    .as_deref()
                    .and_then(|v| changes(v, p))
                    .unwrap_or_default()
            } else {
                vec![]
            }
        };
        let mut reset = parse("counter-reset");
        let increment = parse("counter-increment");
        let mut set = parse("counter-set");
        if pseudo.is_none() && boxed {
            if matches!(dom.tag_name(id), Some("ol" | "ul" | "menu"))
                && !dom.author_declares(id, "counter-reset")
            {
                let reversed = dom.tag_name(id) == Some("ol") && dom.attr(id, "reversed").is_some();
                let start = dom
                    .attr(id, "start")
                    .and_then(|v| v.trim().parse::<i64>().ok());
                reset.push(Change {
                    name: "list-item".into(),
                    reversed,
                    value: if reversed {
                        start.map(|n| n.saturating_add(1))
                    } else {
                        Some(start.unwrap_or(1).saturating_sub(1))
                    },
                });
            }
            if dom.tag_name(id) == Some("li")
                && !dom.author_declares(id, "counter-set")
                && let Some(n) = dom.attr(id, "value").and_then(|v| v.trim().parse().ok())
            {
                set.push(Change {
                    name: "list-item".into(),
                    value: Some(n),
                    reversed: false,
                });
            }
        }
        let index = result.len();
        result.push(Event {
            node: id,
            pseudo,
            parent,
            previous: parent.and_then(|_| previous.insert(parent, index)),
            reset,
            increment,
            set,
            list_item,
            tokens,
        });
        if pseudo.is_none() {
            stack.push((id, Some(PseudoEl::After), Some(index)));
            for child in dom
                .flat_children(id)
                .into_iter()
                .rev()
                .filter(|&c| dom.tag_name(c).is_some())
            {
                // Separate navigable documents start new counter/quote scopes.
                stack.push((
                    child,
                    None,
                    if matches!(dom.tag_name(id), Some("iframe" | "frame")) {
                        None
                    } else {
                        Some(index)
                    },
                ));
            }
            stack.push((id, Some(PseudoEl::Before), Some(index)));
        }
    }
    result
}
#[derive(Default)]
struct AutoStart {
    sum: i64,
    last: i64,
    stopped: bool,
}
struct Counter {
    name: String,
    creator: usize,
    reversed: bool,
    value: i64,
    auto: Option<AutoStart>,
}
fn instantiate(
    state: &mut std::rc::Rc<Vec<usize>>,
    counters: &mut Vec<Counter>,
    events: &[Event],
    creator: usize,
    c: &Change,
    starts: Option<&[i64]>,
) -> usize {
    let state = std::rc::Rc::make_mut(state);
    if let Some(index) = state.iter().rposition(|&i| counters[i].name == c.name) {
        let origin = counters[state[index]].creator;
        if origin == creator || events[origin].parent == events[creator].parent {
            state.remove(index);
        }
    }
    let index = counters.len();
    counters.push(Counter {
        name: c.name.clone(),
        creator,
        reversed: c.reversed,
        value: c
            .value
            .unwrap_or_else(|| starts.and_then(|v| v.get(index)).copied().unwrap_or(0)),
        auto: c.value.is_none().then(AutoStart::default),
    });
    state.push(index);
    index
}
fn ensure(
    state: &mut std::rc::Rc<Vec<usize>>,
    counters: &mut Vec<Counter>,
    events: &[Event],
    creator: usize,
    name: &str,
    starts: Option<&[i64]>,
) -> usize {
    if let Some(&i) = state.iter().rev().find(|&&i| counters[i].name == name) {
        return i;
    }
    instantiate(
        state,
        counters,
        events,
        creator,
        &Change {
            name: name.into(),
            value: Some(0),
            reversed: false,
        },
        starts,
    )
}
fn quotes(dom: &Dom, event: &Event) -> Vec<(String, String)> {
    let raw = event
        .pseudo
        .and_then(|p| dom.pseudo_layout_value(event.node, p, "quotes"))
        .or_else(|| dom.computed_value_resolved(event.node, "quotes"))
        .unwrap_or_else(|| "auto".into());
    if raw == "none" {
        return vec![];
    }
    let strings: Option<Vec<_>> = split_top_level_ws(&raw)
        .into_iter()
        .map(unquote_css)
        .collect();
    if let Some(strings) = strings.filter(|v| !v.is_empty() && v.len() % 2 == 0) {
        return strings
            .chunks(2)
            .map(|p| (p[0].clone(), p[1].clone()))
            .collect();
    }
    let lang = dom
        .inherited_lang(event.node)
        .unwrap_or("en")
        .split('-')
        .next()
        .unwrap_or("en");
    let pairs = match lang {
        "de" => [("„", "“"), ("‚", "‘")],
        "fr" => [("«\u{a0}", "\u{a0}»"), ("‹\u{a0}", "\u{a0}›")],
        "ja" | "zh" => [("「", "」"), ("『", "』")],
        _ => [("“", "”"), ("‘", "’")],
    };
    pairs
        .into_iter()
        .map(|(a, b)| (a.into(), b.into()))
        .collect()
}
fn run(dom: &Dom, events: &[Event], starts: Option<&[i64]>) -> (Generated, Vec<i64>) {
    let mut out = Generated::default();
    let mut counters: Vec<Counter> = vec![];
    let mut states: Vec<std::rc::Rc<Vec<usize>>> = vec![];
    let mut scopes = Vec::new();
    let mut quote_depths: FxHashMap<usize, usize> = FxHashMap::default();
    let index = dom.style_index();
    for (event_id, event) in events.iter().enumerate() {
        let scope = event.parent.map_or(event_id, |p| scopes[p]);
        scopes.push(scope);
        let depth = quote_depths.entry(scope).or_default();
        let source = event.previous.or(event.parent);
        let mut state = source.map(|i| states[i].clone()).unwrap_or_default();
        for change in &event.reset {
            instantiate(&mut state, &mut counters, events, event_id, change, starts);
        }
        let mut increments = event.increment.clone();
        if event.list_item && !increments.iter().any(|c| c.name == "list-item") {
            let i = ensure(
                &mut state,
                &mut counters,
                events,
                event_id,
                "list-item",
                starts,
            );
            increments.push(Change {
                name: "list-item".into(),
                value: Some(if counters[i].reversed { -1 } else { 1 }),
                reversed: false,
            });
        }
        let mut current_increments: FxHashMap<usize, i64> = FxHashMap::default();
        for change in &increments {
            let i = ensure(
                &mut state,
                &mut counters,
                events,
                event_id,
                &change.name,
                starts,
            );
            let delta = change.value.unwrap_or(1);
            counters[i].value = counters[i].value.saturating_add(delta);
            let current = current_increments.entry(i).or_default();
            *current = current.saturating_add(delta.saturating_neg());
        }
        for (&i, &negative) in &current_increments {
            if let Some(auto) = &mut counters[i].auto
                && !auto.stopped
            {
                auto.sum = auto.sum.saturating_add(negative);
                if negative != 0 {
                    auto.last = negative;
                }
            }
        }
        for change in &event.set {
            let i = ensure(
                &mut state,
                &mut counters,
                events,
                event_id,
                &change.name,
                starts,
            );
            let value = change.value.unwrap_or(0);
            counters[i].value = value;
            if let Some(auto) = &mut counters[i].auto
                && !auto.stopped
            {
                auto.sum = auto
                    .sum
                    .saturating_sub(current_increments.get(&i).copied().unwrap_or(0))
                    .saturating_add(value);
                auto.stopped = true;
            }
        }
        if event.list_item
            && let Some(&i) = state
                .iter()
                .rev()
                .find(|&&i| counters[i].name == "list-item")
        {
            out.list_items.insert(event.node, counters[i].value);
        }
        if let Some(tokens) = &event.tokens {
            let styles = index.counter_styles.get(&dom.tree_scope(event.node));
            let empty = counter_styles::Styles::default();
            let styles = styles.unwrap_or(&empty);
            let marks = quotes(dom, event);
            let mut text = String::new();
            for token in tokens {
                match token {
                    Token::Text(s) => text.push_str(s),
                    Token::Attribute(name, fallback) => {
                        text.push_str(dom.attr(event.node, name).unwrap_or(fallback))
                    }
                    Token::Counter {
                        name,
                        separator,
                        style,
                    } => {
                        let inner =
                            ensure(&mut state, &mut counters, events, event_id, name, starts);
                        if let Some(separator) = separator {
                            for (n, &i) in state
                                .iter()
                                .filter(|&&i| counters[i].name == *name)
                                .enumerate()
                            {
                                if n > 0 {
                                    text.push_str(separator);
                                }
                                text.push_str(&counter_styles::representation(
                                    style,
                                    counters[i].value,
                                    styles,
                                    false,
                                ));
                            }
                        } else {
                            text.push_str(&counter_styles::representation(
                                style,
                                counters[inner].value,
                                styles,
                                false,
                            ));
                        }
                    }
                    Token::Quote { open, emit } => {
                        if !open && *depth == 0 {
                            continue;
                        }
                        if !open {
                            *depth -= 1;
                        }
                        if *emit
                            && let Some((a, b)) =
                                marks.get((*depth).min(marks.len().saturating_sub(1)))
                        {
                            text.push_str(if *open { a } else { b });
                        }
                        if *open {
                            *depth = depth.saturating_add(1);
                        }
                    }
                }
            }
            if let Some(p) = event.pseudo {
                out.content.insert(
                    (event.node, if p == PseudoEl::Before { 0 } else { 1 }),
                    text,
                );
            }
        }
        states.push(state);
    }
    let starts = counters
        .iter()
        .map(|c| {
            c.auto
                .as_ref()
                .map_or(c.value, |a| a.sum.saturating_add(a.last))
        })
        .collect();
    (out, starts)
}
pub(super) fn build(dom: &Dom) -> Generated {
    let events = events(dom);
    let reversed = events
        .iter()
        .any(|e| e.reset.iter().any(|c| c.reversed && c.value.is_none()));
    let mut result = if reversed {
        let (_, starts) = run(dom, &events, None);
        run(dom, &events, Some(&starts)).0
    } else {
        run(dom, &events, None).0
    };
    result.dynamic = events.iter().any(|event| {
        event.tokens.as_ref().is_some_and(|tokens| {
            tokens
                .iter()
                .any(|token| matches!(token, Token::Counter { .. } | Token::Quote { .. }))
        }) || ["counter-reset", "counter-increment", "counter-set"]
            .iter()
            .any(|p| {
                if let Some(which) = event.pseudo {
                    dom.pseudo_style(event.node, which, p).is_some()
                } else {
                    dom.author_declares(event.node, p)
                }
            })
    });
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn generated_counters_preserve_scope_order_visibility_and_mutations() {
        let mut dom = Dom::parse_document(
            r#"<style>
          body{counter-reset:Chapter 0} section{counter-increment:Chapter}
          section::before{content:counter(Chapter,upper-roman) ':'}
          ol{counter-reset:item} li{display:block} li::before{counter-increment:item;content:counters(item,'.') ' '}
          .hidden{visibility:hidden} .none{display:none} .contents{display:contents}
        </style><section id=a></section><section class=hidden id=b></section><section class=none></section><section class=contents></section><section id=c></section>
        <ol><li id=x><ol><li id=y></li></ol></li><li id=z></li></ol>"#,
        );
        for (id, text) in [
            ("a", "I:"),
            ("b", "II:"),
            ("c", "III:"),
            ("x", "1 "),
            ("y", "1.1 "),
            ("z", "2 "),
        ] {
            assert_eq!(
                dom.pseudo_content(dom.get_by_id(id).unwrap(), PseudoEl::Before)
                    .as_deref(),
                Some(text),
                "{id}"
            );
        }
        dom.set_attr(
            dom.get_by_id("a").unwrap(),
            "style",
            "counter-increment:Chapter 5",
        );
        assert_eq!(
            dom.pseudo_content(dom.get_by_id("c").unwrap(), PseudoEl::Before)
                .as_deref(),
            Some("VII:")
        );
        assert!(
            dom.take_dirty_targets().is_none(),
            "counter dependencies require full relayout"
        );
    }
    #[test]
    fn generated_reversed_counters_quotes_and_custom_styles() {
        let dom = Dom::parse_document(
            r#"<style>
          @counter-style Steps {system:extends upper-alpha;suffix:') ';}
          ol {list-style-type:Steps}
          .reverse{counter-reset:reversed(n)} .reverse span{counter-increment:n -1}
          .reverse span::before{content:counter(n)}
          q{quotes:'[' ']' '{' '}'}
        </style><ol reversed><li id=a></li><li id=b></li><li id=c></li></ol>
        <div class=reverse><span id=x></span><span id=y></span></div><q id=q1><q id=q2></q></q>"#,
        );
        for (id, text) in [("a", "C) "), ("b", "B) "), ("c", "A) ")] {
            assert_eq!(
                dom.css_list_marker(dom.get_by_id(id).unwrap(), "Steps"),
                text
            );
        }
        for (id, text) in [("x", "2"), ("y", "1"), ("q1", "["), ("q2", "{")] {
            assert_eq!(
                dom.pseudo_content(dom.get_by_id(id).unwrap(), PseudoEl::Before)
                    .as_deref(),
                Some(text),
                "{id}"
            );
        }
        assert_eq!(
            dom.pseudo_content(dom.get_by_id("q2").unwrap(), PseudoEl::After)
                .as_deref(),
            Some("}")
        );
        assert_eq!(
            dom.pseudo_content(dom.get_by_id("q1").unwrap(), PseudoEl::After)
                .as_deref(),
            Some("]")
        );
    }
}
