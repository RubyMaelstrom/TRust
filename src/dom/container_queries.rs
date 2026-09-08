//! CSS Conditional 5 size queries. Conditions stay attached to rules: they
//! are evaluated against each subject's eligible ancestor, never the viewport.
use super::*;

#[derive(Clone, Debug)]
pub(super) struct Query {
    alternatives: Vec<(Option<String>, Condition)>,
}

#[derive(Clone, Debug)]
enum Condition {
    Not(Box<Self>),
    And(Vec<Self>),
    Or(Vec<Self>),
    Feature(String),
    Unknown,
}

impl Query {
    pub(super) fn parse(text: &str) -> Self {
        Self {
            alternatives: split_top_level(text, ',')
                .into_iter()
                .map(|text| {
                    let text = text.trim();
                    let (name, condition) = if text.starts_with('(') || text.starts_with("not ") {
                        (None, text)
                    } else if let Some(i) = text.find(|c: char| c.is_whitespace() || c == '(') {
                        (Some(text[..i].to_string()), text[i..].trim())
                    } else {
                        (Some(text.to_string()), "")
                    };
                    (name, Condition::parse(condition))
                })
                .collect(),
        }
    }

    pub(super) fn matches(&self, dom: &Dom, subject: NodeId, pseudo: bool) -> bool {
        self.alternatives.iter().any(|(name, condition)| {
            let Some(axes) = condition.axes() else {
                return false;
            };
            let mut ancestor = if pseudo {
                Some(subject)
            } else {
                dom.style_parent(subject)
            };
            while let Some(node) = ancestor {
                ancestor = dom.style_parent(node);
                if name.as_ref().is_some_and(|name| {
                    !dom.computed_value_resolved(node, "container-name")
                        .is_some_and(|names| names.split_whitespace().any(|n| n == name))
                }) {
                    continue;
                }
                let kind = dom.size_container_kind(node);
                if kind == 0 || (axes & 2 != 0 && kind != 2) {
                    continue;
                }
                // No principal box (display:none/contents etc.) is not a size container.
                let size = dom.container_sizes.borrow().get(&node).copied();
                if let Some(size) = size {
                    return condition.eval(dom, node, size) == Some(true);
                }
            }
            false
        })
    }

    pub(super) fn retained_bytes(&self) -> usize {
        self.alternatives.capacity() * std::mem::size_of::<(Option<String>, Condition)>()
            + self
                .alternatives
                .iter()
                .map(|(n, c)| n.as_ref().map_or(0, String::capacity) + c.retained_bytes())
                .sum::<usize>()
    }
}

impl Condition {
    fn parse(text: &str) -> Self {
        let text = text.trim();
        if let Some(rest) = text.strip_prefix("not ") {
            return Self::Not(Box::new(Self::parse(rest)));
        }
        let and = split_supports_kw(text, "and");
        let or = split_supports_kw(text, "or");
        if and.len() > 1 && or.len() > 1 {
            return Self::Unknown;
        }
        if and.len() > 1 {
            return Self::And(and.iter().map(|s| Self::parse(s)).collect());
        }
        if or.len() > 1 {
            return Self::Or(or.iter().map(|s| Self::parse(s)).collect());
        }
        if let Some(inner) = text.strip_prefix('(').and_then(|s| s.strip_suffix(')')) {
            let inner = inner.trim();
            if inner.starts_with('(') || inner.starts_with("not ") {
                Self::parse(inner)
            } else {
                Self::Feature(inner.to_string())
            }
        } else {
            Self::Unknown
        }
    }

    fn axes(&self) -> Option<u8> {
        match self {
            Self::Unknown => None,
            Self::Not(c) => c.axes(),
            Self::And(cs) | Self::Or(cs) => cs.iter().try_fold(0, |mask, c| Some(mask | c.axes()?)),
            Self::Feature(s) => s
                .split(|c: char| c.is_whitespace() || matches!(c, ':' | '<' | '>' | '='))
                .find_map(|t| {
                    feature_axis(t.trim_start_matches("min-").trim_start_matches("max-"))
                }),
        }
    }

    fn eval(&self, dom: &Dom, node: NodeId, size: [f32; 2]) -> Option<bool> {
        match self {
            Self::Unknown => None,
            Self::Not(c) => c.eval(dom, node, size).map(|v| !v),
            Self::And(cs) => {
                let values: Vec<_> = cs.iter().map(|c| c.eval(dom, node, size)).collect();
                if values.contains(&Some(false)) {
                    Some(false)
                } else if values.contains(&None) {
                    None
                } else {
                    Some(true)
                }
            }
            Self::Or(cs) => {
                let values: Vec<_> = cs.iter().map(|c| c.eval(dom, node, size)).collect();
                if values.contains(&Some(true)) {
                    Some(true)
                } else if values.contains(&None) {
                    None
                } else {
                    Some(false)
                }
            }
            Self::Feature(text) => feature_eval(dom, node, size, text),
        }
    }

    fn retained_bytes(&self) -> usize {
        match self {
            Self::Unknown => 0,
            Self::Not(c) => std::mem::size_of::<Self>() + c.retained_bytes(),
            Self::Feature(s) => s.capacity(),
            Self::And(cs) | Self::Or(cs) => {
                cs.capacity() * std::mem::size_of::<Self>()
                    + cs.iter().map(Self::retained_bytes).sum::<usize>()
            }
        }
    }
}

fn feature_axis(name: &str) -> Option<u8> {
    match name {
        "width" | "inline-size" => Some(1),
        "height" | "block-size" => Some(2),
        "aspect-ratio" | "orientation" => Some(3),
        _ => None,
    }
}

fn feature_eval(dom: &Dom, node: NodeId, size: [f32; 2], text: &str) -> Option<bool> {
    let feature = |name: &str| match name {
        "width" | "inline-size" => Some(size[0]),
        "height" | "block-size" => Some(size[1]),
        "aspect-ratio" => Some(size[0] / size[1]),
        _ => None,
    };
    let value = |text: &str, ratio: bool| {
        let text = dom.resolve_vars(node, text);
        if ratio {
            media_ratio(&text)
        } else {
            crate::layout2::container_query_length(dom, node, &text)
        }
    };
    if let Some((name, operand)) = text.split_once(':') {
        let name = name.trim();
        if name == "orientation" {
            return match operand.trim() {
                "portrait" => Some(size[1] >= size[0]),
                "landscape" => Some(size[0] > size[1]),
                _ => None,
            };
        }
        let (name, comparison) = if let Some(n) = name.strip_prefix("min-") {
            (n, ">=")
        } else if let Some(n) = name.strip_prefix("max-") {
            (n, "<=")
        } else {
            (name, "=")
        };
        return compare(
            feature(name)?,
            value(operand.trim(), name == "aspect-ratio")?,
            comparison,
        );
    }
    if !text.contains(['<', '>', '=']) {
        return Some(feature(text.trim())? != 0.);
    }
    let mut operands = Vec::new();
    let mut operators = Vec::new();
    let mut start = 0;
    let mut chars = text.char_indices().peekable();
    let mut depth = 0;
    while let Some((i, c)) = chars.next() {
        if c == '(' {
            depth += 1;
        }
        if c == ')' {
            depth -= 1;
        }
        if depth == 0 && matches!(c, '<' | '>' | '=') {
            operands.push(text[start..i].trim());
            let mut end = i + 1;
            if c != '=' && chars.peek().is_some_and(|(_, c)| *c == '=') {
                chars.next();
                end += 1;
            }
            operators.push(&text[i..end]);
            start = end;
        }
    }
    operands.push(text[start..].trim());
    if operators.is_empty() || operators.len() > 2 {
        return None;
    }
    if operators.len() == 2
        && (!operators[0].starts_with(operators[1].chars().next()?) || operators[0] == "=")
    {
        return None;
    }
    let feature_index = operands.iter().position(|n| feature(n).is_some())?;
    if operands.iter().filter(|n| feature(n).is_some()).count() != 1
        || (operators.len() == 2 && feature_index != 1)
    {
        return None;
    }
    let ratio = operands[feature_index] == "aspect-ratio";
    let numbers: Option<Vec<_>> = operands
        .iter()
        .enumerate()
        .map(|(i, text)| {
            if i == feature_index {
                feature(text)
            } else {
                value(text, ratio)
            }
        })
        .collect();
    let numbers = numbers?;
    operators.iter().enumerate().try_fold(true, |hit, (i, op)| {
        Some(hit && compare(numbers[i], numbers[i + 1], op)?)
    })
}

fn compare(a: f32, b: f32, op: &str) -> Option<bool> {
    Some(match op {
        "<" => a < b,
        "<=" => a <= b,
        ">" => a > b,
        ">=" => a >= b,
        "=" => a == b,
        _ => return None,
    })
}

impl Dom {
    /// 0: no size container, 1: inline axis, 2: both axes. The layout engine
    /// currently lays out horizontal writing modes; do not claim vertical queries.
    pub(crate) fn size_container_kind(&self, node: NodeId) -> u8 {
        if node == crate::layout2::NO_NODE {
            return 0;
        }
        if matches!(
            self.effective_display(node).as_deref(),
            None | Some(
                "none"
                    | "contents"
                    | "inline"
                    | "table"
                    | "inline-table"
                    | "table-row"
                    | "table-cell"
                    | "table-row-group"
            )
        ) {
            return 0;
        }
        match self
            .computed_value_resolved(node, "container-type")
            .as_deref()
        {
            Some("inline-size") => 1,
            Some("size") => 2,
            _ => 0,
        }
    }

    pub(crate) fn has_container_queries(&self) -> bool {
        self.style_index().has_container_queries
    }

    pub(crate) fn update_container_sizes(&self, sizes: FxHashMap<NodeId, [f32; 2]>) -> bool {
        if *self.container_sizes.borrow() == sizes {
            return false;
        }
        *self.container_sizes.borrow_mut() = sizes;
        // A style/layout interleave is not a DOM mutation. Preserve the parsed
        // rule index and invalidate only values which depend on query results.
        *self.matched_cache.borrow_mut() = NodeCache::default();
        *self.cascaded_cache.borrow_mut() = NodeCache::default();
        self.computed_cache.borrow_mut().1.clear();
        self.custom_prop_cache.borrow_mut().1.clear();
        *self.hidden_cache.borrow_mut() = NodeCache::default();
        *self.font_cache.borrow_mut() = NodeCache::default();
        *self.font_units_cache.borrow_mut() = NodeCache::default();
        *self.decoration_cache.borrow_mut() = NodeCache::default();
        self.layout_cache.borrow_mut().clear();
        self.box_tree_cache.borrow_mut().clear();
        self.serialization_cache.borrow_mut().1.clear();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn size_query_comparisons_and_relative_units() {
        let mut dom = Dom::parse_document(
            r#"<div id="outer" style="container:Page / inline-size;width:800px;font-size:20px"><div id="inner" style="container:Card / inline-size;width:300px;font-size:10px"><b id="subject"></b></div></div>"#,
        );
        dom.set_viewport_px(960., 500.);
        let id = |name| {
            dom.descendants(DOCUMENT)
                .find(|&n| dom.attr(n, "id") == Some(name))
                .unwrap()
        };
        let (outer, inner, subject) = (id("outer"), id("inner"), id("subject"));
        dom.update_container_sizes(
            [(outer, [800., 0.]), (inner, [300., 0.])]
                .into_iter()
                .collect(),
        );
        for text in [
            "Page (width >= 40em)",
            "Card (200px < width <= 30em)",
            "(min-width:300px)",
            "(width:300px)",
            "not (width > 300px)",
        ] {
            let query = Query::parse(text);
            assert!(
                query.matches(&dom, subject, false),
                "{text} {query:?}; kinds {} {} fonts {} {} names {:?} {:?}, feature {:?} length {:?}",
                dom.size_container_kind(outer),
                dom.size_container_kind(inner),
                dom.font_px(outer),
                dom.font_px(inner),
                dom.computed_value_resolved(outer, "container-name"),
                dom.computed_value_resolved(inner, "container-name"),
                feature_eval(&dom, outer, [800., 0.], "width >= 40em"),
                crate::layout2::container_query_length(&dom, outer, "40em")
            );
        }
    }
}
