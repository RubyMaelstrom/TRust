//! CSS Conditional 5 size queries. Conditions stay attached to rules: they
//! are evaluated against each subject's eligible ancestor, never the viewport.
use super::*;

/// Observed size reads, including currently false queries and containers
/// whose principal box has not been laid out yet. A size change invalidates
/// its readers and their inheriting descendants. Stale edges are conservative;
/// a broad style revision resets them. Bound the graph and fall back to full
/// invalidation if an adversarial page exceeds the budget.
#[derive(Default)]
pub(super) struct Dependencies {
    epoch: u64,
    readers: FxHashMap<NodeId, FxHashMap<NodeId, u8>>,
    /// Reverse edges make retiring a nursery subject proportional to its own dependencies.
    subjects: FxHashMap<NodeId, FxHashSet<NodeId>>,
    edges: usize,
    broad: bool,
    units: bool,
}

impl Dependencies {
    const MAX_EDGES: usize = 65_536;

    pub(super) fn retain_nodes(&mut self, live: &dyn Fn(NodeId) -> bool) {
        self.edges = 0;
        self.readers.retain(|&container, readers| {
            if !live(container) {
                return false;
            }
            readers.retain(|&subject, _| live(subject));
            if readers.capacity() > readers.len().saturating_mul(4).max(64) {
                readers.shrink_to(readers.len().saturating_mul(2));
            }
            self.edges += readers.len();
            !readers.is_empty()
        });
        if self.readers.capacity() > self.readers.len().saturating_mul(4).max(64) {
            self.readers.shrink_to(self.readers.len().saturating_mul(2));
        }
        self.subjects = FxHashMap::default();
        for (&container, readers) in &self.readers {
            for &subject in readers.keys() {
                self.subjects.entry(subject).or_default().insert(container);
            }
        }
    }

    pub(super) fn remove_node(&mut self, id: NodeId) {
        if let Some(readers) = self.readers.remove(&id) {
            self.edges -= readers.len();
            for subject in readers.keys() {
                if let Some(containers) = self.subjects.get_mut(subject) {
                    containers.remove(&id);
                    if containers.is_empty() {
                        self.subjects.remove(subject);
                    } else if containers.capacity() > containers.len().saturating_mul(4).max(64) {
                        containers.shrink_to(containers.len().saturating_mul(2));
                    }
                }
            }
        }
        if let Some(containers) = self.subjects.remove(&id) {
            for container in containers {
                if let Some(readers) = self.readers.get_mut(&container) {
                    self.edges -= usize::from(readers.remove(&id).is_some());
                    if readers.is_empty() {
                        self.readers.remove(&container);
                    } else if readers.capacity() > readers.len().saturating_mul(4).max(64) {
                        readers.shrink_to(readers.len().saturating_mul(2));
                    }
                }
            }
        }
        if self.readers.capacity() > self.readers.len().saturating_mul(4).max(64) {
            self.readers.shrink_to(self.readers.len().saturating_mul(2));
        }
        if self.subjects.capacity() > self.subjects.len().saturating_mul(4).max(64) {
            self.subjects
                .shrink_to(self.subjects.len().saturating_mul(2));
        }
    }

    fn read(&mut self, epoch: u64, subject: NodeId, container: NodeId, axes: u8) {
        if self.epoch != epoch {
            *self = Self {
                epoch,
                ..Default::default()
            };
        }
        if self.broad {
            return;
        }
        let readers = self.readers.entry(container).or_default();
        match readers.entry(subject) {
            std::collections::hash_map::Entry::Occupied(mut entry) => *entry.get_mut() |= axes,
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(axes);
                self.subjects.entry(subject).or_default().insert(container);
                self.edges += 1;
            }
        }
        if self.edges > Self::MAX_EDGES {
            self.readers = FxHashMap::default();
            self.subjects = FxHashMap::default();
            self.edges = 0;
            self.broad = true;
        }
    }

    pub(super) fn retained_bytes(&self) -> usize {
        self.readers.capacity() * std::mem::size_of::<(NodeId, FxHashMap<NodeId, u8>)>()
            + self.subjects.capacity() * std::mem::size_of::<(NodeId, FxHashSet<NodeId>)>()
            + self
                .subjects
                .values()
                .map(|s| s.capacity() * std::mem::size_of::<NodeId>())
                .sum::<usize>()
            + self
                .readers
                .values()
                .map(|r| r.capacity() * std::mem::size_of::<(NodeId, u8)>())
                .sum::<usize>()
    }
}

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
                dom.record_container_read(subject, node, axes, false);
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
    pub(super) fn record_container_read(
        &self,
        subject: NodeId,
        container: NodeId,
        axes: u8,
        units: bool,
    ) {
        let mut dependencies = self.container_dependencies.borrow_mut();
        dependencies.read(self.style_value_epoch, subject, container, axes);
        dependencies.units |= units;
    }

    pub(crate) fn has_container_unit_dependencies(&self) -> bool {
        self.container_dependencies.borrow().units
    }
}

impl<B: StyleBackend + ?Sized> ComputeView<'_, B> {
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
}

impl Dom {
    /// Container rules and observed container-relative lengths require a
    /// settled layout before computed styles or incremental patches escape.
    pub(crate) fn style_depends_on_layout(&self) -> bool {
        self.style_index().has_container_queries || self.has_container_unit_dependencies()
    }

    pub(crate) fn update_container_sizes(&self, sizes: FxHashMap<NodeId, [f32; 2]>) -> bool {
        if *self.container_sizes.borrow() == sizes {
            return false;
        }
        let previous = std::mem::replace(&mut *self.container_sizes.borrow_mut(), sizes);
        let mut affected = FxHashSet::default();
        let broad = {
            let sizes = self.container_sizes.borrow();
            let dependencies = self.container_dependencies.borrow();
            for (&container, readers) in &dependencies.readers {
                let changed_axes = match (previous.get(&container), sizes.get(&container)) {
                    (Some(a), Some(b)) => u8::from(a[0] != b[0]) | (u8::from(a[1] != b[1]) << 1),
                    (None, None) => 0,
                    _ => 3,
                };
                affected.extend(
                    readers
                        .iter()
                        .filter_map(|(&node, &axes)| (axes & changed_axes != 0).then_some(node)),
                );
            }
            dependencies.broad || dependencies.epoch != self.style_value_epoch
        };
        if !broad {
            if affected.is_empty() {
                return false;
            }
            // Computed values inherit through the flat tree, including slots.
            let mut pending: Vec<_> = affected.iter().copied().collect();
            while let Some(node) = pending.pop() {
                let mut children = Vec::new();
                self.push_composed_children(node, &mut children);
                if self.tag_name(node) == Some("slot") {
                    children.extend(self.flat_slot_nodes(node));
                }
                pending.extend(children.into_iter().filter(|&child| affected.insert(child)));
            }
            let mut computed = self.computed_cache.borrow_mut();
            let mut custom = self.custom_prop_cache.borrow_mut();
            let mut matched = self.matched_cache.borrow_mut();
            let mut cascaded = self.cascaded_cache.borrow_mut();
            let mut hidden = self.hidden_cache.borrow_mut();
            let mut fonts = self.font_cache.borrow_mut();
            let mut font_units = self.font_units_cache.borrow_mut();
            let mut decorations = self.decoration_cache.borrow_mut();
            for &node in &affected {
                self.transitions.invalidate(node);
                self.animations.invalidate(node);
                computed.1.remove_node(node);
                custom.1.remove(&node);
                matched.invalidate(node);
                cascaded.invalidate(node);
                hidden.invalidate(node);
                fonts.invalidate(node);
                font_units.invalidate(node);
                decorations.invalidate(node);
            }
            // Formatting constraints flow from ancestors; size changes flow
            // back to them. Expire both chains without restyling ancestors.
            let mut pending: Vec<_> = affected.iter().copied().collect();
            while let Some(node) = pending.pop() {
                for parent in [self.parent_composed(node), self.parent_flat(node)]
                    .into_iter()
                    .flatten()
                {
                    if affected.insert(parent) {
                        pending.push(parent);
                    }
                }
            }
            let mut layout = self.layout_cache.borrow_mut();
            let mut tree = self.box_tree_cache.borrow_mut();
            // Generated counter/quote state may reach later siblings. Until
            // it has its own read graph, retain the general geometry fallback.
            if self.generated_cache.borrow_mut().take().is_some() {
                layout.clear();
                tree.clear();
            } else {
                for node in affected {
                    layout.invalidate(node);
                    tree.invalidate(node);
                }
            }
            self.properties.invalidate_values();
            self.serialization_cache.borrow_mut().1.clear();
            self.svg_image_memo.borrow_mut().clear();
            return true;
        }
        // Every dependent value is about to expire. Start a fresh observed
        // graph, so one overflow does not permanently disable selective reuse.
        let units = self.container_dependencies.borrow().units;
        *self.container_dependencies.borrow_mut() = Dependencies {
            epoch: self.style_value_epoch,
            units,
            ..Default::default()
        };
        // A style/layout interleave is not a DOM mutation. Preserve the parsed
        // rule index and invalidate only values which depend on query results.
        self.transitions.invalidate_all();
        self.animations.invalidate_all();
        *self.matched_cache.borrow_mut() = NodeCache::default();
        *self.cascaded_cache.borrow_mut() = NodeCache::default();
        self.computed_cache.borrow_mut().1.clear();
        self.custom_prop_cache.borrow_mut().1.clear();
        self.properties.invalidate_values();
        *self.hidden_cache.borrow_mut() = NodeCache::default();
        *self.font_cache.borrow_mut() = NodeCache::default();
        *self.font_units_cache.borrow_mut() = NodeCache::default();
        *self.decoration_cache.borrow_mut() = NodeCache::default();
        self.layout_cache.borrow_mut().clear();
        self.box_tree_cache.borrow_mut().clear();
        *self.generated_cache.borrow_mut() = None;
        self.serialization_cache.borrow_mut().1.clear();
        self.svg_image_memo.borrow_mut().clear();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nursery_dependency_retirement_repairs_both_indexes_without_other_reads() {
        let mut dependencies = Dependencies::default();
        dependencies.read(1, 10, 20, 1);
        dependencies.read(1, 10, 20, 2);
        dependencies.read(1, 10, 21, 1);
        dependencies.read(1, 11, 20, 1);
        dependencies.read(1, 20, 21, 1);
        assert_eq!(dependencies.edges, 4);
        assert_eq!(dependencies.readers[&20][&10], 3);
        dependencies.remove_node(20);
        assert_eq!(dependencies.edges, 1);
        assert_eq!(dependencies.readers.len(), 1);
        assert_eq!(dependencies.readers[&21].len(), 1);
        assert_eq!(dependencies.subjects[&10], [21].into_iter().collect());
        assert!(!dependencies.subjects.contains_key(&11));
        dependencies.remove_node(10);
        assert_eq!(dependencies.edges, 0);
        assert!(dependencies.readers.is_empty());
        assert!(dependencies.subjects.is_empty());
        dependencies.read(1, 30, 40, 1);
        dependencies.read(1, 31, 41, 1);
        dependencies.retain_nodes(&|id| id != 31);
        dependencies.remove_node(40);
        assert_eq!(dependencies.edges, 0);
        assert!(dependencies.subjects.is_empty());
    }

    #[test]
    fn size_changes_invalidate_readers_including_false_queries_and_units() {
        let dom = Dom::parse_document(
            r#"<style>
            .container { container-type:size; width:200px; height:100px }
            @property --size { syntax:"<length>"; inherits:true; initial-value:0px }
            b { color:red; --size:10cqw; width:var(--size) }
            @container (width > 250px) { b { color:blue } }
            @container (height > 150px) { i { color:green } }
        </style><div id=a class=container><b id=x></b><i id=z></i></div>
        <div id=b class=container><b id=y></b></div>"#,
        );
        let id = |name| dom.get_by_id(name).unwrap();
        let (a, b, x, y, z) = (id("a"), id("b"), id("x"), id("y"), id("z"));
        dom.update_container_sizes([(a, [200., 100.]), (b, [200., 100.])].into_iter().collect());
        assert_eq!(
            dom.computed_value_resolved(x, "color").as_deref(),
            Some("red")
        );
        assert_eq!(
            dom.computed_value_resolved(x, "width").as_deref(),
            Some("20px")
        );
        let unrelated = dom.matched_rules(y);
        let other_axis = dom.matched_rules(z);
        assert!(
            dom.update_container_sizes(
                [(a, [300., 100.]), (b, [200., 100.])].into_iter().collect()
            )
        );
        assert_eq!(
            dom.computed_value_resolved(x, "color").as_deref(),
            Some("blue")
        );
        assert_eq!(
            dom.computed_value_resolved(x, "width").as_deref(),
            Some("30px")
        );
        assert!(std::rc::Rc::ptr_eq(&unrelated, &dom.matched_rules(y)));
        assert!(std::rc::Rc::ptr_eq(&other_axis, &dom.matched_rules(z)));
        assert!(
            dom.update_container_sizes(
                [(a, [200., 100.]), (b, [200., 100.])].into_iter().collect()
            )
        );
        assert_eq!(
            dom.computed_value_resolved(x, "color").as_deref(),
            Some("red")
        );
    }

    #[test]
    fn dependency_budget_has_a_bounded_conservative_fallback() {
        let mut dependencies = Dependencies::default();
        for node in 0..=Dependencies::MAX_EDGES {
            dependencies.read(1, node, 0, 1);
        }
        assert!(dependencies.broad);
        assert!(dependencies.readers.is_empty());
        dependencies.read(1, 1, 2, 2);
        assert!(dependencies.readers.is_empty());
        dependencies.read(2, 1, 2, 2);
        assert!(!dependencies.broad);
        assert_eq!(dependencies.edges, 1);
    }

    #[test]
    fn query_changes_expire_both_box_generation_consumers_without_dom_mutation() {
        let dom = Dom::parse_document(
            r#"<style>
                #container { container-type:size }
                #child { display:contents }
                @container (width > 100px) { #child { display:block } }
                @container (width > 200px) { #child { display:none } }
            </style><div id=container><div id=child>text</div></div>"#,
        );
        let container = dom.get_by_id("container").unwrap();
        let child = dom.get_by_id("child").unwrap();
        let epoch = dom.epoch;
        for (width, principal, hidden) in [
            (50., false, false),
            (150., true, false),
            (250., false, true),
            (50., false, false),
        ] {
            dom.update_container_sizes([(container, [width, 100.])].into_iter().collect());
            assert_eq!(dom.epoch, epoch);
            assert_eq!(dom.generates_principal_box(child), principal);
            assert_eq!(dom.is_hidden(child), hidden);
        }
    }

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
