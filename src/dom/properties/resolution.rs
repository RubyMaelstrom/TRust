//! Document-wide registration and CSS Variables dependency resolution.
use super::*;

type Key = (NodeId, Option<PseudoEl>, String);
type AdoptedSource = (std::ops::Range<usize>, Option<url::Url>);

#[derive(Default)]
pub(in crate::dom) struct State {
    pub javascript: FxHashMap<NodeId, Registry>,
    pub document_bases: FxHashMap<NodeId, url::Url>,
    /// Browser-owned cookie context of installed frame documents, independent
    /// of author-mutable URL attributes and CSS/document base URLs.
    pub document_cookie_restrictions: FxHashMap<NodeId, bool>,
    pub adopted_sources: FxHashMap<NodeId, Vec<AdoptedSource>>,
    resolving: RefCell<Resolving>,
}

#[derive(Default)]
struct Resolving {
    active: Vec<Key>,
    cyclic: FxHashMap<NodeId, FxHashSet<Key>>,
    steps: usize,
    stamp: (u64, u64),
    memo: FxHashMap<Key, Option<String>>,
}

impl Resolving {
    fn is_cyclic(&self, key: &Key) -> bool {
        self.cyclic
            .get(&key.0)
            .is_some_and(|keys| keys.contains(key))
    }
}

pub(in crate::dom) struct Guard<'a> {
    state: &'a RefCell<Resolving>,
    key: Key,
}

impl Guard<'_> {
    pub fn cyclic(&self) -> bool {
        self.state.borrow().is_cyclic(&self.key)
    }
}

impl Drop for Guard<'_> {
    fn drop(&mut self) {
        let mut state = self.state.borrow_mut();
        // CSS Values 5 #substitution: the context is guarded only for this
        // invocation. Cleanup must run in release builds as well as debug.
        let completed = state.active.pop();
        debug_assert_eq!(completed.as_ref(), Some(&self.key));
        if state.active.is_empty() {
            state.steps = 0;
            state.memo.clear();
        }
    }
}

impl State {
    pub(in crate::dom) fn remove_node(&mut self, id: NodeId) {
        macro_rules! remove {
            ($($field:ident),+ $(,)?) => {$(
                self.$field.remove(&id);
                if self.$field.capacity() > self.$field.len().saturating_mul(4).max(64) {
                    self.$field.shrink_to(self.$field.len().saturating_mul(2));
                }
            )+};
        }
        remove!(
            javascript,
            document_bases,
            document_cookie_restrictions,
            adopted_sources
        );
        let resolving = self.resolving.get_mut();
        debug_assert!(resolving.active.is_empty() && resolving.memo.is_empty());
        resolving.cyclic.remove(&id);
        if resolving.cyclic.capacity() > resolving.cyclic.len().saturating_mul(4).max(64) {
            resolving
                .cyclic
                .shrink_to(resolving.cyclic.len().saturating_mul(2));
        }
    }

    pub(in crate::dom) fn retain_nodes(&mut self, live: &dyn Fn(NodeId) -> bool) {
        macro_rules! retain {
            ($($field:ident),+ $(,)?) => {$(
                self.$field.retain(|&id, _| live(id));
                if self.$field.capacity() > self.$field.len().saturating_mul(4).max(64) {
                    self.$field.shrink_to(self.$field.len().saturating_mul(2));
                }
            )+};
        }
        retain!(
            javascript,
            document_bases,
            document_cookie_restrictions,
            adopted_sources
        );
        let resolving = self.resolving.get_mut();
        debug_assert!(
            resolving.active.is_empty(),
            "DOM GC cannot run inside style resolution"
        );
        resolving.cyclic.retain(|&node, _| live(node));
        resolving.memo.retain(|key, _| live(key.0));
        if resolving.cyclic.capacity() > resolving.cyclic.len().saturating_mul(4).max(64) {
            resolving
                .cyclic
                .shrink_to(resolving.cyclic.len().saturating_mul(2));
        }
        if resolving.memo.capacity() > resolving.memo.len().saturating_mul(4).max(64) {
            resolving
                .memo
                .shrink_to(resolving.memo.len().saturating_mul(2));
        }
    }

    pub fn invalidate_values(&self) {
        let mut state = self.resolving.borrow_mut();
        debug_assert!(state.active.is_empty());
        state.cyclic.clear();
        state.memo.clear();
    }
    pub fn invalidate(&self, id: NodeId) {
        let mut state = self.resolving.borrow_mut();
        debug_assert!(state.active.is_empty());
        state.cyclic.remove(&id);
        state.memo.clear();
    }
    pub fn enter(
        &self,
        id: NodeId,
        pseudo: Option<PseudoEl>,
        name: &str,
        stamp: (u64, u64),
    ) -> Option<Guard<'_>> {
        let key = (id, pseudo, name.to_owned());
        let mut state = self.resolving.borrow_mut();
        // CSS Properties and Values API #dependency-cycles: dependency edges
        // arise from declarations/relative units, not installed font faces.
        // A font revision expires computed numeric values, but must not erase
        // cycle membership while font-size can still reuse its cached result.
        // Style changes do expire the graph; never discard an active context.
        if state.active.is_empty() {
            if state.stamp.0 != stamp.0 {
                state.cyclic.clear();
            }
            state.stamp = stamp;
        }
        if let Some(start) = state.active.iter().position(|k| *k == key) {
            let cycle = state.active[start..].to_vec();
            for key in cycle {
                state.cyclic.entry(key.0).or_default().insert(key);
            }
            return None;
        }
        state.steps += 1;
        if state.active.len() >= 256 || state.steps > 16384 {
            return None;
        }
        state.active.push(key.clone());
        Some(Guard {
            state: &self.resolving,
            key,
        })
    }

    pub fn retained_bytes(&self) -> usize {
        let mut bytes = self.javascript.capacity() * std::mem::size_of::<(NodeId, Registry)>()
            + self.document_cookie_restrictions.capacity() * std::mem::size_of::<(NodeId, bool)>()
            + self.document_bases.capacity() * std::mem::size_of::<(NodeId, url::Url)>()
            + self
                .document_bases
                .values()
                .map(|u| u.as_str().len())
                .sum::<usize>();
        for registry in self.javascript.values() {
            bytes += registry_bytes(registry);
        }
        bytes +=
            self.adopted_sources.capacity() * std::mem::size_of::<(NodeId, Vec<AdoptedSource>)>();
        for sources in self.adopted_sources.values() {
            bytes += sources.capacity() * std::mem::size_of::<AdoptedSource>()
                + sources
                    .iter()
                    .filter_map(|(_, url)| url.as_ref())
                    .map(|url| url.as_str().len())
                    .sum::<usize>();
        }
        if let Ok(state) = self.resolving.try_borrow() {
            bytes += state.active.capacity() * std::mem::size_of::<Key>()
                + state.cyclic.capacity() * std::mem::size_of::<(NodeId, FxHashSet<Key>)>()
                + state
                    .cyclic
                    .values()
                    .map(|keys| keys.capacity() * std::mem::size_of::<Key>())
                    .sum::<usize>()
                + state
                    .active
                    .iter()
                    .chain(state.cyclic.values().flat_map(|keys| keys.iter()))
                    .map(|k| k.2.capacity())
                    .sum::<usize>();
            bytes += state.memo.capacity() * std::mem::size_of::<(Key, Option<String>)>()
                + state
                    .memo
                    .iter()
                    .map(|(key, value)| {
                        key.2.capacity() + value.as_ref().map_or(0, String::capacity)
                    })
                    .sum::<usize>();
        }
        bytes
    }
}

pub(in crate::dom) fn registry_bytes(registry: &Registry) -> usize {
    let mut seen = FxHashSet::default();
    registry.capacity() * std::mem::size_of::<(String, Arc<Registration>)>()
        + registry
            .iter()
            .map(|(name, reg)| {
                name.capacity()
                    + if seen.insert(Arc::as_ptr(reg)) {
                        std::mem::size_of::<Registration>() + reg.retained_bytes()
                    } else {
                        0
                    }
            })
            .sum::<usize>()
}

impl<'a, B: StyleBackend + ?Sized> ComputeView<'a, B> {
    pub(in crate::dom) fn property_guard(
        &self,
        id: NodeId,
        pseudo: Option<PseudoEl>,
        name: &str,
    ) -> Option<Guard<'a>> {
        self.flush_style_invalidations();
        self.0.properties().enter(
            id,
            pseudo,
            name,
            (
                self.style_value_epoch(),
                crate::font_system::page_font_epoch(),
            ),
        )
    }
}

impl<B: StyleBackend + ?Sized> ComputeView<'_, B> {
    /// Shadow roots share their document's registry. Embedded documents have
    /// independent registries even though their arenas share one allocation.
    pub(in crate::dom) fn registration_document(&self, id: NodeId) -> NodeId {
        // DOM a2331a45 #concept-node-document / #concept-node-adopt and CSS
        // Properties & Values 954df531 #determining-registration / #shadow-dom:
        // provenance is maintained when nodes are created/adopted, including
        // detached and shadow descendants. Presentation ancestry is neither
        // necessary nor authoritative: light children of an iframe belong to
        // its outer document, while its content Document owns its own tree.
        self.style_view()
            .nodes
            .owner_document_of(id)
            .unwrap_or(DOCUMENT)
    }
}

impl<'a, B: StyleBackend + ?Sized> ComputeView<'a, B> {
    pub(in crate::dom) fn property_base(&self, id: NodeId) -> Option<&'a url::Url> {
        self.0
            .properties()
            .document_bases
            .get(&self.registration_document(id))
            .or(self.0.doc_url())
    }
}

impl<B: StyleBackend + ?Sized> ComputeView<'_, B> {
    pub(in crate::dom) fn property_registration(
        &self,
        id: NodeId,
        name: &str,
    ) -> Option<Arc<Registration>> {
        let document = self.registration_document(id);
        self.properties()
            .javascript
            .get(&document)
            .and_then(|r| r.get(name))
            .cloned()
            .or_else(|| {
                self.style_index()
                    .properties
                    .get(&document)
                    .and_then(|r| r.get(name))
                    .cloned()
            })
    }
}

impl Dom {
    /// CSSOM #dom-window-getcomputedstyle appends every custom property with
    /// a non-guaranteed-invalid computed value, including registered initials.
    pub(crate) fn cssom_computed_names(&self, id: NodeId, pseudo: Option<PseudoEl>) -> Vec<String> {
        if !self.is_connected(id) {
            return Vec::new();
        }
        let mut names: Vec<_> = PROPS.iter().map(|p| p.name.to_owned()).collect();
        names.sort();
        names.dedup();
        let mut custom = FxHashSet::default();
        let document = self.registration_document(id);
        if let Some(registry) = self.style_index().properties.get(&document) {
            custom.extend(registry.keys().cloned());
        }
        if let Some(registry) = self.properties.javascript.get(&document) {
            custom.extend(registry.keys().cloned());
        }
        if let Some(pseudo) = pseudo {
            custom.extend(
                self.cascaded_maps(id)
                    .pseudo(pseudo)
                    .keys()
                    .filter(|n| n.starts_with("--"))
                    .cloned(),
            );
        }
        let mut current = Some(id);
        while let Some(node) = current {
            custom.extend(
                self.cascaded_maps(node)
                    .elem
                    .keys()
                    .filter(|n| n.starts_with("--"))
                    .cloned(),
            );
            current = self.style_parent(node);
        }
        let mut custom: Vec<_> = custom
            .into_iter()
            .filter(|name| match pseudo {
                Some(pseudo) => self.pseudo_layout_value(id, pseudo, name).is_some(),
                None => self.custom_prop(id, name).is_some(),
            })
            .collect();
        custom.sort();
        names.extend(custom);
        names
    }

    /// CSS Properties and Values API #the-registerproperty-function. Validate
    /// atomically; failed registrations neither reserve a name nor invalidate.
    pub(crate) fn register_property(
        &mut self,
        document: NodeId,
        name: &str,
        syntax: &str,
        inherits: bool,
        initial: Option<String>,
        base: Option<url::Url>,
    ) -> Result<(), &'static str> {
        // This API accepts a property name string, not CSS source text. Names
        // with escapes are handled by CSS parsing before reaching this layer.
        if !name.starts_with("--") || name == "--" {
            return Err("SyntaxError");
        }
        if self
            .properties
            .javascript
            .get(&document)
            .is_some_and(|r| r.contains_key(name))
        {
            return Err("InvalidModificationError");
        }
        let parsed = Syntax::parse(syntax).ok_or("SyntaxError")?;
        if let Some(initial) = &initial
            && parsed.compute(initial, &Context::independent()).is_none()
        {
            return Err("SyntaxError");
        }
        self.properties
            .javascript
            .entry(document)
            .or_default()
            .insert(
                name.to_owned(),
                Arc::new(Registration {
                    syntax_text: syntax.to_owned(),
                    syntax: parsed,
                    inherits,
                    initial,
                    base,
                }),
            );
        self.touch_style();
        Ok(())
    }
}

impl<B: StyleBackend + ?Sized> ComputeView<'_, B> {
    pub(in crate::dom) fn registered_custom_value(
        &self,
        id: NodeId,
        pseudo: Option<PseudoEl>,
        name: &str,
    ) -> VarResult {
        self.flush_style_invalidations();
        let key = (id, pseudo, name.to_owned());
        if let Some(value) = {
            let state = self.properties().resolving.borrow();
            (!state.active.is_empty() && !state.is_cyclic(&key))
                .then(|| state.memo.get(&key).cloned())
                .flatten()
        } {
            return value.map_or(VarResult::Undefined, VarResult::Resolved);
        }
        let Some(guard) = self.property_guard(id, pseudo, name) else {
            return VarResult::Cycle;
        };
        let registration = self.property_registration(id, name);
        let inherits = registration.as_ref().is_none_or(|r| r.inherits);
        let initial = || {
            registration.as_ref().and_then(|r| {
                r.initial.as_deref().and_then(|initial| {
                    r.syntax.compute(
                        initial,
                        &Context {
                            dom: Some(self as &dyn Host),
                            id,
                            pseudo,
                            base: r.base.as_ref().or_else(|| self.property_base(id)),
                            independent: false,
                        },
                    )
                })
            })
        };
        let inherited = || {
            match pseudo {
                Some(_) => self.custom_prop(id, name),
                None => self
                    .style_parent(id)
                    .and_then(|parent| self.custom_prop(parent, name)),
            }
            .or_else(initial)
        };
        let defaulted = || if inherits { inherited() } else { initial() };
        if guard.cyclic() {
            return defaulted().map_or(VarResult::Undefined, VarResult::Resolved);
        }
        let raw = match pseudo {
            Some(which) => self
                .pseudo_style(id, which, name)
                .or_else(|| self.baked_pseudo_value(id, which, name)),
            None => self.cascaded(id, name),
        };
        let computed = match raw
            .as_deref()
            .and_then(ident)
            .as_deref()
            .and_then(wide_keyword)
        {
            Some(WideKeyword::Initial) => initial(),
            Some(WideKeyword::Inherit) => inherited(),
            Some(_) => defaulted(),
            None => {
                if let Some(raw) = raw {
                    // CSS Variables #cycles includes references in unused fallbacks.
                    // Visit those edges before selecting which fallback to substitute.
                    let refs = variable_references(&raw);
                    if let Some(refs) = &refs {
                        for referenced in refs {
                            let _ = self.registered_custom_value(id, pseudo, referenced);
                        }
                    }
                    let value = refs.and_then(|_| substitute(self, id, pseudo, &raw));
                    let value = value.map(|value| {
                        // CSS Values 5 #substitution: a substituted CSS-wide
                        // keyword acts exactly like an authored one.
                        match ident(&value).as_deref().and_then(wide_keyword) {
                            Some(WideKeyword::Initial) => return initial(),
                            Some(WideKeyword::Inherit) => return inherited(),
                            Some(_) => return defaulted(),
                            None => {}
                        }
                        if let Some(r) = &registration {
                            let maps = self.cascaded_maps(id);
                            let base = maps.custom_bases.get(&(pseudo, name.to_owned()));
                            r.syntax
                                .compute(
                                    &value,
                                    &Context {
                                        dom: Some(self as &dyn Host),
                                        id,
                                        pseudo,
                                        base: base
                                            .map(|b| b.as_ref())
                                            .or_else(|| self.property_base(id)),
                                        independent: false,
                                    },
                                )
                                .or_else(defaulted)
                        } else {
                            Some(value)
                        }
                    });
                    if guard.cyclic() {
                        defaulted()
                    } else {
                        value.unwrap_or_else(defaulted)
                    }
                } else {
                    defaulted()
                }
            }
        };
        self.properties()
            .resolving
            .borrow_mut()
            .memo
            .insert(key, computed.clone());
        computed.map_or(VarResult::Undefined, VarResult::Resolved)
    }
}

impl Dom {
    pub(crate) fn set_adopted_sheets(
        &mut self,
        scope: NodeId,
        sheets: Vec<(String, Option<url::Url>)>,
    ) {
        let mut text = String::new();
        let mut sources = Vec::new();
        for (sheet, base) in sheets {
            let start = text.len();
            text.push_str(&sheet);
            sources.push((start..text.len(), base));
            text.push('\n');
        }
        if self.adopted_styles.get(&scope) == Some(&text)
            && self.properties.adopted_sources.get(&scope) == Some(&sources)
        {
            return;
        }
        self.adopted_styles.insert(scope, text);
        self.properties.adopted_sources.insert(scope, sources);
        self.touch_style_at(scope);
    }
}

/// Every var() edge, including nested and unused fallback arguments.
fn variable_references(text: &str) -> Option<Vec<String>> {
    fn scan<'i>(
        p: &mut Parser<'i, '_>,
        depth: usize,
        refs: &mut Vec<String>,
    ) -> ParseResult<'i, ()> {
        if depth > MAX_DEPTH || refs.len() > MAX_COMPONENTS {
            return Err(p.new_custom_error(()));
        }
        while !p.is_exhausted() {
            let token = p.next()?.clone();
            if token.is_parse_error() {
                return Err(p.new_custom_error(()));
            }
            if let Token::Function(name) = &token
                && name.eq_ignore_ascii_case("var")
            {
                p.parse_nested_block(|p| {
                    let name = p.expect_ident_cloned()?.to_string();
                    if !name.starts_with("--") || name == "--" {
                        return Err(p.new_custom_error(()));
                    }
                    refs.push(name);
                    if !p.is_exhausted() {
                        p.expect_comma()?;
                        scan(p, depth + 1, refs)?;
                    }
                    Ok(())
                })?;
            } else if matches!(
                token,
                Token::Function(_)
                    | Token::ParenthesisBlock
                    | Token::SquareBracketBlock
                    | Token::CurlyBracketBlock
            ) {
                p.parse_nested_block(|p| scan(p, depth + 1, refs))?;
            }
        }
        Ok(())
    }
    let mut refs = Vec::new();
    scan(&mut Parser::new(&mut ParserInput::new(text)), 0, &mut refs).ok()?;
    refs.sort();
    refs.dedup();
    Some(refs)
}

pub(in crate::dom) fn substitute<B: StyleBackend + ?Sized>(
    dom: &ComputeView<'_, B>,
    id: NodeId,
    pseudo: Option<PseudoEl>,
    text: &str,
) -> Option<String> {
    use cssparser::TokenSerializationType;
    fn scan<'i, B: StyleBackend + ?Sized>(
        p: &mut Parser<'i, '_>,
        dom: &ComputeView<'_, B>,
        id: NodeId,
        pseudo: Option<PseudoEl>,
        depth: usize,
    ) -> ParseResult<'i, String> {
        if depth > MAX_DEPTH {
            return Err(p.new_custom_error(()));
        }
        let mut out = String::new();
        let mut previous = TokenSerializationType::Nothing;
        loop {
            let start = p.position();
            let Ok(token) = p.next_including_whitespace_and_comments().cloned() else {
                break;
            };
            let literal = p.slice_from(start).to_owned();
            if token.is_parse_error() {
                return Err(p.new_custom_error(()));
            }
            let text = if let Token::Function(name) = &token
                && name.eq_ignore_ascii_case("var")
            {
                p.parse_nested_block(|p| {
                    let name = p.expect_ident_cloned()?.to_string();
                    if !name.starts_with("--") || name == "--" {
                        return Err(p.new_custom_error(()));
                    }
                    let fallback = if p.is_exhausted() {
                        None
                    } else {
                        p.expect_comma()?;
                        let start = p.position();
                        p.expect_no_error_token()?;
                        Some(p.slice_from(start).trim().to_owned())
                    };
                    match dom.registered_custom_value(id, pseudo, &name) {
                        VarResult::Resolved(v) => Ok(v),
                        VarResult::Undefined => {
                            let fallback = fallback.ok_or_else(|| p.new_custom_error(()))?;
                            scan(
                                &mut Parser::new(&mut ParserInput::new(&fallback)),
                                dom,
                                id,
                                pseudo,
                                depth + 1,
                            )
                            .map_err(|_| p.new_custom_error(()))
                        }
                        VarResult::Cycle => Err(p.new_custom_error(())),
                    }
                })?
            } else if matches!(
                token,
                Token::Function(_)
                    | Token::ParenthesisBlock
                    | Token::SquareBracketBlock
                    | Token::CurlyBracketBlock
            ) {
                let closing = match token {
                    Token::SquareBracketBlock => ']',
                    Token::CurlyBracketBlock => '}',
                    _ => ')',
                };
                let inside = p.parse_nested_block(|p| scan(p, dom, id, pseudo, depth + 1))?;
                format!("{literal}{inside}{closing}")
            } else {
                literal
            };
            // Substitution preserves token identity: var(--number)px cannot
            // accidentally turn a number followed by an ident into a length.
            let mut input = ParserInput::new(&text);
            let mut tokens = Parser::new(&mut input);
            if let Ok(first) = tokens.next_including_whitespace_and_comments() {
                let kind = first.serialization_type();
                if previous.needs_separator_when_before(kind) {
                    out.push_str("/**/");
                }
                previous = kind;
                while let Ok(t) = tokens.next_including_whitespace_and_comments() {
                    previous = t.serialization_type();
                }
            }
            out.push_str(&text);
            if out.len() > 2 * 1024 * 1024 {
                return Err(p.new_custom_error(()));
            }
        }
        Ok(out)
    }
    scan(
        &mut Parser::new(&mut ParserInput::new(text)),
        dom,
        id,
        pseudo,
        0,
    )
    .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registered_dependency_cycles_survive_font_registry_revisions() {
        // CSS Properties and Values API #dependency-cycles: these edges
        // come from property references and units, not from available faces.
        for unit in ["em", "rem", "lh", "rlh"] {
            let html = format!(
                "<style>@property --x {{syntax:'<length>';initial-value:3px;inherits:false}} \
                 html {{--x:2{unit};font-size:var(--x);line-height:var(--x)}}</style>"
            );
            let mut dom = Dom::parse_document(&html);
            let root = dom.document_element().unwrap();
            assert_eq!(dom.font_px(root), FONT_SIZE_INITIAL, "{unit}");
            assert_eq!(dom.custom_prop(root, "--x").as_deref(), Some("3px"));

            // Simulate retained dependencies/values predating a font-registry
            // update, without changing process-global fonts in parallel tests.
            let old_font_epoch = crate::font_system::page_font_epoch().wrapping_sub(1);
            dom.properties.resolving.borrow_mut().stamp.1 = old_font_epoch;
            dom.custom_prop_cache.borrow_mut().0.1 = old_font_epoch;
            dom.computed_cache.borrow_mut().0.1 = old_font_epoch;
            assert_eq!(
                dom.custom_prop(root, "--x").as_deref(),
                Some("3px"),
                "font revision must not hide the {unit} cycle behind cached font-size"
            );
            assert_eq!(dom.font_px(root), FONT_SIZE_INITIAL, "{unit}");

            // Real dependency changes still invalidate the retained cycle.
            dom.set_attr(root, "style", "font-size:10px;line-height:20px;--x:2em");
            assert_eq!(dom.custom_prop(root, "--x").as_deref(), Some("20px"));
        }
    }
}
