//! Same-document URL references of serialized inline SVG resources.
//!
//! TRust rasterizes an inline `<svg>` as a standalone image document, but
//! its URL references bind in the host document. SVG 2 linking.html
//! #processingURL-target (svgwg snapshot c403ca4, 2026-09-06) identifies the
//! target of a same-document fragment as the element the document names with
//! that ID, i.e. DOM's first element in tree order with the ID in the
//! reference's tree, and #processingURL-validity then decides whether that
//! element is an acceptable target for the referencing attribute or property.
//! A gradient, pattern, clip path, mask, filter, marker or `<use>` target
//! defined in another (often hidden) `<svg>` of the same document is therefore
//! a valid reference, and so is anything those targets reference in turn.
//!
//! The resource carries every target it needs: elements outside the outer
//! `<svg>` are imported into a `<defs>` of the serialized document,
//! transitively. Because the image decoder resolves fragments only within
//! the serialized markup (and its own duplicate-ID rules differ between
//! `<use>` and paint references), every referenced ID that the markup could
//! bind differently is renamed to a fresh unique ID, and references whose
//! canonical target is invalid point to an ID that no element carries.
//! The decoder then binds each reference exactly as the host document does,
//! and judges the target's type itself.
//!
//! Imported subtrees inherit as their referencing context requires:
//! struct.html #UseStyleInheritance makes a `<use>` (and `feImage`) instance
//! inherit from its host, so imported instance content keeps `currentColor`
//! for the decoder to resolve through the `<use>`. Paint servers, clip paths,
//! masks, markers and filters inherit from their own ancestors (CSS Masking 1
//! #ClipPathElement/#MaskElement: properties do not inherit from the
//! referencing element), so an imported resource root carries its computed
//! inherited presentation values instead of inheriting from `<defs>`.
//! These elements are referencable even when they or their ancestors are
//! `display: none` (SVG 2 pservers.html, painting.html #MarkerElement, CSS
//! Masking 1 #ClipPathElement/#MaskElement, Filter Effects 1
//! #FilterElement).
//!
//! The work is bounded: at most [`MAX_IMPORTS`] imported subtrees with
//! [`MAX_IMPORTED_NODES`] nodes in total; a target beyond the budget is an
//! unresolved (invalid) reference rather than a partial import.

use std::ops::Range;

use rustc_hash::{FxHashMap, FxHashSet};

use super::{Dom, NodeData, NodeId, SvgImageRefs};

const SVG_NS: &str = "http://www.w3.org/2000/svg";
const XLINK_NS: &str = "http://www.w3.org/1999/xlink";
/// Imported subtrees per serialized resource.
pub(super) const MAX_IMPORTS: usize = 256;
/// Nodes across all imported subtrees of one resource.
pub(super) const MAX_IMPORTED_NODES: usize = 32 * 1024;

/// What a reference may name: SVG 2 linking.html #processingURL-validity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum Kind {
    /// `use` `href`: any SVG element except an inclusive ancestor of the
    /// `use` (struct.html #UseElement: a circular reference).
    Use,
    /// `feImage` `href` renders an element like `use` (Filter Effects 1
    /// #feImageElement).
    FeImage,
    /// `fill` / `stroke`: a paint server (painting.html #SpecifyingPaint).
    Paint,
    ClipPath,
    Mask,
    Filter,
    Marker,
    /// Gradient `href` templates: a different gradient element.
    Gradient,
    /// Pattern `href` templates: a different pattern element.
    Pattern,
    /// `textPath` `href`: an element implementing SVGGeometryElement.
    TextPath,
    /// A `url()` in a `<style>` sheet, whose property the decoder resolves.
    Any,
}

impl Kind {
    /// The kind of a `url()` in a property or presentation attribute of this
    /// name, or `None` for a property whose URL does not name an SVG element
    /// the decoder renders.
    fn of_property(name: &str) -> Option<Self> {
        Some(match name {
            "fill" | "stroke" => Self::Paint,
            "clip-path" => Self::ClipPath,
            "mask" => Self::Mask,
            "filter" => Self::Filter,
            "marker" | "marker-start" | "marker-mid" | "marker-end" => Self::Marker,
            _ => return None,
        })
    }

    /// The kind of an SVG element's `href` (linking.html
    /// #processingURL-validity); `a`, `image` and `script` do not name
    /// elements to render.
    fn of_href(tag: &str) -> Option<Self> {
        Some(match tag {
            "use" => Self::Use,
            "feImage" => Self::FeImage,
            "linearGradient" | "radialGradient" => Self::Gradient,
            "pattern" => Self::Pattern,
            "textPath" => Self::TextPath,
            _ => return None,
        })
    }

    /// Whether a `use`-like reference renders a copy of its target, which
    /// then inherits from the referencing element (#UseStyleInheritance).
    fn instantiates(self) -> bool {
        matches!(self, Self::Use | Self::FeImage)
    }

    /// Whether `target` is a valid target of this kind of reference made by
    /// `from`; without `from`, only the target's type is judged (the decoder
    /// detects circular references itself).
    fn accepts(self, dom: &Dom, target: NodeId, from: Option<NodeId>) -> bool {
        if dom.namespace_uri(target) != Some(SVG_NS) {
            return false;
        }
        let tag = dom.tag_name(target).unwrap_or_default();
        let from = from.unwrap_or(NodeId::MAX);
        match self {
            Self::Use => !inclusive_ancestor(dom, target, from),
            Self::FeImage | Self::Any => true,
            Self::Paint => matches!(tag, "linearGradient" | "radialGradient" | "pattern"),
            Self::ClipPath => tag == "clipPath",
            Self::Mask => tag == "mask",
            Self::Filter => tag == "filter",
            Self::Marker => tag == "marker",
            Self::Gradient => target != from && matches!(tag, "linearGradient" | "radialGradient"),
            Self::Pattern => target != from && tag == "pattern",
            Self::TextPath => matches!(
                tag,
                "path" | "rect" | "circle" | "ellipse" | "line" | "polyline" | "polygon"
            ),
        }
    }
}

/// Never-rendered resource elements, which stay referencable even when
/// `display: none` applies to them or an ancestor (see the module comment).
pub(super) fn referencable_when_hidden(tag: &str) -> bool {
    matches!(
        tag,
        "linearGradient"
            | "radialGradient"
            | "pattern"
            | "clipPath"
            | "mask"
            | "marker"
            | "filter"
            | "symbol"
    )
}

fn inclusive_ancestor(dom: &Dom, ancestor: NodeId, node: NodeId) -> bool {
    let mut current = Some(node);
    while let Some(id) = current {
        if id == ancestor {
            return true;
        }
        current = dom.nodes.get(id).and_then(|native| native.parent);
    }
    false
}

/// One same-document fragment reference met while serializing.
struct Reference {
    scope: NodeId,
    fragment: String,
    kind: Kind,
    from: NodeId,
}

#[derive(Default)]
struct Collected {
    references: Vec<Reference>,
    serialized: FxHashSet<NodeId>,
    /// `(id, element)` for every serialized element carrying an `id`.
    ids: Vec<(String, NodeId)>,
}

/// How the serialized markup writes one reference.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Action {
    /// Name this output ID instead: the canonical target's fresh ID, an ID
    /// no element carries, or the empty mask.
    Bind(String),
    /// Write the whole property value as `none`.
    Disable,
}

/// The output names that make the decoder bind like the host document.
/// A reference without an action is written as authored.
#[derive(Default)]
struct Plan {
    renamed: FxHashMap<NodeId, String>,
    actions: FxHashMap<(NodeId, String, Kind), Action>,
    /// The ID of an empty `<mask>` the resource carries for invalid mask
    /// references.
    empty_mask: Option<String>,
    /// `use` elements in error for a circular reference, which name
    /// `missing` instead.
    circular: FxHashSet<NodeId>,
    /// An ID no element in the resource carries.
    missing: Option<String>,
}

impl Plan {
    fn is_empty(&self) -> bool {
        self.renamed.is_empty() && self.actions.is_empty() && self.circular.is_empty()
    }
}

#[derive(Default, PartialEq, Eq)]
enum Pass {
    /// Plain serialization (geometry copies, tests).
    #[default]
    Off,
    Collect,
    Rewrite,
}

/// Serializer state for one inline SVG resource; see the module comment.
#[derive(Default)]
pub(super) struct References {
    pass: Pass,
    collected: Collected,
    plan: Plan,
    /// Imported roots instantiated by `use`/`feImage` only.
    instances: FxHashSet<NodeId>,
    /// Imported roots referenced as resources (paint servers, clip paths, …).
    resources: FxHashSet<NodeId>,
    keep_current_color: bool,
}

impl References {
    fn collecting() -> Self {
        Self {
            pass: Pass::Collect,
            ..Self::default()
        }
    }

    /// Enter `node` while serializing; returns the state [`Self::leave`]
    /// restores. Imported `use` instances keep `currentColor` so the decoder
    /// resolves it through the referencing `use` element.
    pub(super) fn enter(&mut self, node: NodeId) -> bool {
        let previous = self.keep_current_color;
        if self.instances.contains(&node) {
            self.keep_current_color = true;
        } else if self.resources.contains(&node) {
            self.keep_current_color = false;
        }
        previous
    }

    pub(super) fn leave(&mut self, previous: bool) {
        self.keep_current_color = previous;
    }

    pub(super) fn keeps_current_color(&self) -> bool {
        self.keep_current_color
    }

    /// Extra inline style for an imported resource root: its computed
    /// inherited presentation values, which it must not take from the
    /// `<defs>` it is moved into.
    pub(super) fn root_style(&self, dom: &Dom, node: NodeId, color: &str) -> Option<String> {
        self.resources
            .contains(&node)
            .then(|| dom.svg_resource_root_inherited_style(node, color))
    }

    /// Record or rewrite the references of one serialized start tag, whose
    /// attributes `attrs` holds as ` name="value"` pairs.
    pub(super) fn element(&mut self, dom: &Dom, node: NodeId, tag: &str, attrs: String) -> String {
        if self.pass == Pass::Off {
            return attrs;
        }
        let Some(parsed) = serialized_attrs(&attrs) else {
            return attrs;
        };
        let svg = dom.namespace_uri(node) == Some(SVG_NS);
        let dropped = duplicate_hrefs(dom, node, &parsed, &attrs);
        // The reference's tree, found only for an element that refers.
        let mut tree = None;
        let mut scope = || *tree.get_or_insert_with(|| dom.tree_scope(node));
        let collect = self.pass == Pass::Collect;
        // A node serialized again inside a containing import adds nothing.
        let first = collect && self.collected.serialized.insert(node);
        let mut rebuilt = String::with_capacity(attrs.len());
        let mut changed = !dropped.is_empty();
        for (index, (name, value)) in parsed.iter().enumerate() {
            if dropped.contains(&index) {
                continue;
            }
            let name_text = &attrs[name.clone()];
            let escaped = &attrs[value.clone()];
            let mut replacement = None;
            match name_text {
                "id" => {
                    if first {
                        self.collected.ids.push((unescape(escaped), node));
                    } else if !collect && let Some(fresh) = self.plan.renamed.get(&node) {
                        replacement = Some(fresh.clone());
                    }
                }
                "href" if svg => {
                    if let Some(kind) = Kind::of_href(tag) {
                        let text = unescape(escaped);
                        if let Some(fragment) = text.trim().strip_prefix('#')
                            && !fragment.is_empty()
                        {
                            let scope = scope();
                            if collect {
                                self.collected.references.push(Reference {
                                    scope,
                                    fragment: fragment.to_string(),
                                    kind,
                                    from: node,
                                });
                            } else if kind == Kind::Use
                                && self.plan.circular.contains(&node)
                                && let Some(name) = &self.plan.missing
                            {
                                replacement = Some(format!("#{name}"));
                            } else if let Some(Action::Bind(name)) =
                                self.plan.actions.get(&(scope, fragment.to_string(), kind))
                            {
                                replacement = Some(format!("#{name}"));
                            }
                        }
                    }
                }
                "style" => {
                    let text = unescape(escaped);
                    let urls = style_urls(&text);
                    if !urls.is_empty() {
                        replacement = self.css(scope(), node, &text, urls);
                    }
                }
                property => {
                    if let Some(kind) = Kind::of_property(property) {
                        let text = unescape(escaped);
                        let whole = 0..text.len();
                        let urls = css_urls(&text)
                            .into_iter()
                            .map(|(range, fragment)| (kind, range, fragment, whole.clone()))
                            .collect::<Vec<_>>();
                        if !urls.is_empty() {
                            replacement = self.css(scope(), node, &text, urls);
                        }
                    }
                }
            }
            rebuilt.push(' ');
            rebuilt.push_str(name_text);
            rebuilt.push_str("=\"");
            match replacement {
                Some(value) => {
                    changed = true;
                    rebuilt.push_str(&super::escape_attr(&value));
                }
                None => rebuilt.push_str(escaped),
            }
            rebuilt.push('"');
        }
        if changed { rebuilt } else { attrs }
    }

    /// Record or rewrite the `url()` references of a `<style>` sheet's text.
    pub(super) fn style_text(&mut self, dom: &Dom, style: NodeId, text: &str) -> Option<String> {
        if self.pass == Pass::Off {
            return None;
        }
        let whole = 0..text.len();
        let urls: Vec<_> = css_urls(text)
            .into_iter()
            .map(|(range, fragment)| (Kind::Any, range, fragment, whole.clone()))
            .collect();
        if urls.is_empty() {
            return None;
        }
        self.css(dom.tree_scope(style), style, text, urls)
    }

    /// Record `urls` (Collect) or return `text` with its rewritten ones.
    /// Each entry is the reference's kind, the byte range of its `url()`,
    /// its fragment and the byte range of the property value holding it.
    fn css(
        &mut self,
        scope: NodeId,
        from: NodeId,
        text: &str,
        urls: Vec<(Kind, Range<usize>, String, Range<usize>)>,
    ) -> Option<String> {
        if self.pass == Pass::Collect {
            self.collected
                .references
                .extend(urls.into_iter().map(|(kind, _, fragment, _)| Reference {
                    scope,
                    fragment,
                    kind,
                    from,
                }));
            return None;
        }
        let mut edits: Vec<(Range<usize>, String)> = Vec::new();
        for (kind, range, fragment, value) in urls {
            match self.plan.actions.get(&(scope, fragment, kind)) {
                None => {}
                Some(Action::Bind(name)) => edits.push((range, format!("url(#{name})"))),
                Some(Action::Disable) => {
                    edits.retain(|(edit, _)| edit.start < value.start || edit.end > value.end);
                    if !edits.iter().any(|(edit, _)| *edit == value) {
                        edits.push((value, String::from("none")));
                    }
                }
            }
        }
        if edits.is_empty() {
            return None;
        }
        // A url() inside a disabled value is dropped with that value.
        let disabled: Vec<Range<usize>> = edits
            .iter()
            .filter(|(_, replacement)| replacement == "none")
            .map(|(range, _)| range.clone())
            .collect();
        edits.retain(|(range, replacement)| {
            replacement == "none"
                || !disabled
                    .iter()
                    .any(|value| value.start <= range.start && range.end <= value.end)
        });
        edits.sort_by_key(|(range, _)| range.start);
        let mut out = String::with_capacity(text.len());
        let mut last = 0;
        for (range, replacement) in edits {
            if range.start < last {
                continue;
            }
            out.push_str(&text[last..range.start]);
            out.push_str(&replacement);
            last = range.end;
        }
        out.push_str(&text[last..]);
        Some(out)
    }
}

/// Indices (into `parsed`) of `href` attributes to leave out: the serializer
/// writes each attribute under its local name, so a legacy `xlink:href`
/// beside an `href` would otherwise duplicate the attribute and make the
/// resource malformed XML. SVG 2 linking.html #processingURL: the
/// `xlink:href` applies only in the absence of an `href` attribute.
fn duplicate_hrefs(
    dom: &Dom,
    node: NodeId,
    parsed: &[(Range<usize>, Range<usize>)],
    attrs: &str,
) -> Vec<usize> {
    let hrefs: Vec<usize> = parsed
        .iter()
        .enumerate()
        .filter(|(_, (name, _))| &attrs[name.clone()] == "href")
        .map(|(index, _)| index)
        .collect();
    if hrefs.len() < 2 {
        return Vec::new();
    }
    let Some(NodeData::Element { attrs: native, .. }) = dom.nodes.get(node).map(|n| &n.data) else {
        return Vec::new();
    };
    // The serializer writes the element's `href`-named attributes in order.
    let namespaces: Vec<&str> = native
        .iter()
        .filter(|attribute| &*attribute.name.local == "href")
        .map(|attribute| &*attribute.name.ns)
        .collect();
    if namespaces.len() != hrefs.len() {
        return Vec::new();
    }
    let keep = namespaces
        .iter()
        .position(|ns| ns.is_empty())
        .or_else(|| namespaces.iter().position(|ns| *ns == XLINK_NS))
        .unwrap_or(0);
    hrefs
        .into_iter()
        .enumerate()
        .filter(|(position, _)| *position != keep)
        .map(|(_, index)| index)
        .collect()
}

/// Name and value byte ranges of ` name="value"` pairs, or `None` when
/// `attrs` is not entirely in that form.
fn serialized_attrs(attrs: &str) -> Option<Vec<(Range<usize>, Range<usize>)>> {
    let bytes = attrs.as_bytes();
    let mut out = Vec::new();
    let mut at = 0;
    loop {
        while bytes.get(at).is_some_and(u8::is_ascii_whitespace) {
            at += 1;
        }
        if at == bytes.len() {
            return Some(out);
        }
        let name = at;
        while bytes
            .get(at)
            .is_some_and(|&byte| byte != b'=' && !byte.is_ascii_whitespace())
        {
            at += 1;
        }
        if bytes.get(at..at + 2) != Some(&b"=\""[..]) || at == name {
            return None;
        }
        let value = at + 2;
        let end = value + attrs[value..].find('"')?;
        out.push((name..at, value..end));
        at = end + 1;
    }
}

/// Undo `escape_attr`.
fn unescape(value: &str) -> String {
    if !value.contains('&') {
        return value.to_string();
    }
    value
        .replace("&quot;", "\"")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

/// Fragment-only `url()` references in CSS text: the byte range of each
/// `url(...)` and its fragment.
fn css_urls(text: &str) -> Vec<(Range<usize>, String)> {
    // Without `url` or an escape there is no url() token to find.
    if !text.contains('\\')
        && !text
            .as_bytes()
            .windows(3)
            .any(|window| window.eq_ignore_ascii_case(b"url"))
    {
        return Vec::new();
    }
    fn walk(parser: &mut cssparser::Parser<'_>, out: &mut Vec<(Range<usize>, String)>) {
        loop {
            let start = parser.position().byte_index();
            let token = match parser.next_including_whitespace_and_comments() {
                Ok(token) => token.clone(),
                Err(_) => return,
            };
            match token {
                cssparser::Token::UnquotedUrl(url) => {
                    if let Some(fragment) = url.strip_prefix('#')
                        && !fragment.is_empty()
                    {
                        out.push((start..parser.position().byte_index(), fragment.to_string()));
                    }
                }
                cssparser::Token::Function(name) if name.eq_ignore_ascii_case("url") => {
                    let url = parser.parse_nested_block(|inner| {
                        inner.skip_whitespace();
                        let url = inner.expect_string()?.as_ref().to_string();
                        inner.skip_whitespace();
                        inner.expect_exhausted()?;
                        Ok::<_, cssparser::ParseError<()>>(url)
                    });
                    if let Ok(url) = url
                        && let Some(fragment) = url.strip_prefix('#')
                        && !fragment.is_empty()
                    {
                        out.push((start..parser.position().byte_index(), fragment.to_string()));
                    }
                }
                cssparser::Token::Function(_)
                | cssparser::Token::ParenthesisBlock
                | cssparser::Token::SquareBracketBlock
                | cssparser::Token::CurlyBracketBlock => {
                    let _ = parser.parse_nested_block(|inner| {
                        walk(inner, out);
                        Ok::<_, cssparser::ParseError<()>>(())
                    });
                }
                _ => {}
            }
        }
    }
    let mut parser = cssparser::Parser::new(text);
    let mut out = Vec::new();
    walk(&mut parser, &mut out);
    out
}

/// The `url()` references of a declaration list whose property names one,
/// with the byte range of each declaration's value.
fn style_urls(text: &str) -> Vec<(Kind, Range<usize>, String, Range<usize>)> {
    let mut out = Vec::new();
    let mut start = 0;
    for end in top_level_semicolons(text).chain(std::iter::once(text.len())) {
        let declaration = &text[start..end];
        if let Some(colon) = declaration.find(':') {
            let property = declaration[..colon].trim().to_ascii_lowercase();
            if let Some(kind) = Kind::of_property(&property) {
                let offset = start + colon + 1;
                out.extend(
                    css_urls(&text[offset..end])
                        .into_iter()
                        .map(|(range, fragment)| {
                            (
                                kind,
                                range.start + offset..range.end + offset,
                                fragment,
                                offset..end,
                            )
                        }),
                );
            }
        }
        start = (end + 1).min(text.len());
    }
    out
}

/// Byte offsets of `;` outside strings, escapes and blocks.
fn top_level_semicolons(text: &str) -> impl Iterator<Item = usize> + '_ {
    let mut depth = 0usize;
    let mut quote = None;
    let mut escaped = false;
    text.char_indices().filter_map(move |(index, c)| {
        if escaped {
            escaped = false;
            return None;
        }
        match (c, quote) {
            ('\\', _) => escaped = true,
            (c, Some(q)) if c == q => quote = None,
            (_, Some(_)) => {}
            ('"' | '\'', None) => quote = Some(c),
            ('(' | '[' | '{', None) => depth += 1,
            (')' | ']' | '}', None) => depth = depth.saturating_sub(1),
            (';', None) if depth == 0 => return Some(index),
            _ => {}
        }
        None
    })
}

/// The `use` references reachable from an outer `<svg>`, for its
/// dependency proof and renderability.
#[derive(Default)]
pub(super) struct UseClosure {
    /// The first `use` in the outer SVG with a valid referenced element.
    pub first_valid: Option<NodeId>,
    /// Referenced elements outside the outer SVG, transitively.
    pub targets: Vec<NodeId>,
}

impl SvgImageRefs<'_> {
    pub(super) fn new(page: Option<&url::Url>) -> SvgImageRefs<'_> {
        SvgImageRefs {
            page,
            urls: Vec::new(),
            references: References::default(),
        }
    }
}

impl Dom {
    /// The legacy-aware `href` of an SVG element: SVG 2 linking.html
    /// #processingURL processes `xlink:href` only without an `href`.
    pub(super) fn svg_href(&self, node: NodeId) -> Option<&str> {
        let NodeData::Element { attrs, .. } = &self.nodes.get(node)?.data else {
            return None;
        };
        let href = |ns: &str| {
            attrs
                .iter()
                .find(|attribute| &*attribute.name.local == "href" && &*attribute.name.ns == ns)
                .map(|attribute| &*attribute.value)
        };
        href("").or_else(|| href(XLINK_NS))
    }

    /// DOM #concept-id lookup for SVG 2 linking.html #processingURL-target:
    /// fill each `None` in `wanted` with the first element in tree order of
    /// `scope` whose ID is the key (getElementById). The walk does not enter
    /// nested documents (frames), whose IDs belong to another document.
    fn svg_resolve_fragments(&self, scope: NodeId, wanted: &mut FxHashMap<String, Option<NodeId>>) {
        let mut missing = wanted.values().filter(|found| found.is_none()).count();
        if missing == 0 {
            return;
        }
        let mut stack = vec![scope];
        while let Some(node) = stack.pop() {
            let Some(native) = self.nodes.get(node) else {
                continue;
            };
            match &native.data {
                NodeData::Document if node != scope => continue,
                NodeData::Element { attrs, .. } => {
                    if let Some(id) = attrs
                        .iter()
                        .find(|a| a.name.ns.is_empty() && &*a.name.local == "id")
                        && let Some(slot) = wanted.get_mut(&*id.value)
                        && slot.is_none()
                    {
                        *slot = Some(node);
                        missing -= 1;
                        if missing == 0 {
                            return;
                        }
                    }
                }
                _ => {}
            }
            let mut child = native.last_child;
            while let Some(id) = child {
                stack.push(id);
                child = self.nodes.get(id).and_then(|native| native.prev_sibling);
            }
        }
    }

    /// The `use` closure of an outer `<svg>`: every element a `use` in it,
    /// or in an element reached that way, validly references outside it
    /// (struct.html #UseElement), bounded like the serialized imports.
    pub(super) fn svg_use_closure(&self, root: NodeId) -> UseClosure {
        let mut closure = UseClosure::default();
        let mut sources = vec![root];
        let mut members: FxHashSet<NodeId> = FxHashSet::default();
        members.insert(root);
        let mut nodes = 0usize;
        let mut index = 0;
        while index < sources.len() {
            let source = sources[index];
            index += 1;
            let mut uses = Vec::new();
            let mut wanted: FxHashMap<NodeId, FxHashMap<String, Option<NodeId>>> =
                FxHashMap::default();
            for node in std::iter::once(source).chain(self.descendants(source)) {
                if self.tag_name(node) != Some("use") || self.namespace_uri(node) != Some(SVG_NS) {
                    continue;
                }
                let Some(fragment) = self
                    .svg_href(node)
                    .and_then(|href| href.trim().strip_prefix('#'))
                    .filter(|fragment| !fragment.is_empty())
                else {
                    continue;
                };
                let scope = self.tree_scope(node);
                wanted
                    .entry(scope)
                    .or_default()
                    .insert(fragment.to_string(), None);
                uses.push((node, scope, fragment.to_string()));
            }
            for (scope, fragments) in &mut wanted {
                self.svg_resolve_fragments(*scope, fragments);
            }
            for (node, scope, fragment) in uses {
                let Some(target) = wanted[&scope][&fragment] else {
                    continue;
                };
                if !Kind::Use.accepts(self, target, Some(node)) {
                    continue;
                }
                if source == root && closure.first_valid.is_none() {
                    closure.first_valid = Some(target);
                }
                let inside = {
                    let mut current = Some(target);
                    let mut inside = false;
                    while let Some(id) = current {
                        if members.contains(&id) {
                            inside = true;
                            break;
                        }
                        current = self.nodes.get(id).and_then(|native| native.parent);
                    }
                    inside
                };
                if inside || sources.len() > MAX_IMPORTS {
                    continue;
                }
                let size = 1 + self.descendants(target).count();
                if nodes + size > MAX_IMPORTED_NODES {
                    continue;
                }
                nodes += size;
                members.insert(target);
                sources.push(target);
                closure.targets.push(target);
            }
        }
        closure
    }

    /// Serialize the outer `<svg>` `root` as a standalone resource whose
    /// same-document references bind as in this document; see the module
    /// comment.
    pub(super) fn svg_resource_markup(
        &self,
        root: NodeId,
        images: &mut SvgImageRefs<'_>,
    ) -> String {
        images.references = References::collecting();
        let root_markup = self.serialize_svg_for_image_with(root, images);
        // Imported roots in discovery order, with their collected markup.
        let mut imports: Vec<(NodeId, Option<String>)> = Vec::new();
        let mut resolved: FxHashMap<(NodeId, String), Option<NodeId>> = FxHashMap::default();
        let mut imported_nodes = 0usize;
        let mut scanned = 0;
        let mut mode_changed = false;
        loop {
            let pending = images.references.collected.references.len();
            if scanned == pending {
                break;
            }
            let mut wanted: FxHashMap<NodeId, FxHashMap<String, Option<NodeId>>> =
                FxHashMap::default();
            for reference in &images.references.collected.references[scanned..pending] {
                let key = (reference.scope, reference.fragment.clone());
                if !resolved.contains_key(&key) {
                    wanted
                        .entry(reference.scope)
                        .or_default()
                        .insert(reference.fragment.clone(), None);
                }
            }
            for (scope, mut fragments) in wanted {
                self.svg_resolve_fragments(scope, &mut fragments);
                for (fragment, target) in fragments {
                    resolved.insert((scope, fragment), target);
                }
            }
            // Targets to import, in reference order, with whether every
            // reference to them so far instantiates them.
            let mut new: Vec<(NodeId, bool)> = Vec::new();
            for index in scanned..pending {
                let reference = &images.references.collected.references[index];
                let Some(Some(target)) = resolved
                    .get(&(reference.scope, reference.fragment.clone()))
                    .copied()
                else {
                    continue;
                };
                if !reference.kind.accepts(self, target, Some(reference.from)) {
                    continue;
                }
                if images.references.collected.serialized.contains(&target) {
                    // An imported `use` instance referenced as a resource
                    // too inherits as a resource.
                    if !reference.kind.instantiates() && images.references.instances.remove(&target)
                    {
                        images.references.resources.insert(target);
                        mode_changed = true;
                    }
                    continue;
                }
                let instance = reference.kind.instantiates();
                match new.iter_mut().find(|(node, _)| *node == target) {
                    Some((_, all)) => *all &= instance,
                    None => new.push((target, instance)),
                }
            }
            scanned = pending;
            for (target, instance) in new {
                // An element inside the outer SVG or an import that was not
                // serialized (it is hidden) stays unresolved.
                let contained = {
                    let mut current = Some(target);
                    let mut contained = false;
                    while let Some(id) = current {
                        if id == root || imports.iter().any(|(import, _)| *import == id) {
                            contained = true;
                            break;
                        }
                        current = self.nodes.get(id).and_then(|native| native.parent);
                    }
                    contained
                };
                if contained || imports.len() >= MAX_IMPORTS {
                    continue;
                }
                let size = 1 + self.descendants(target).count();
                if imported_nodes + size > MAX_IMPORTED_NODES {
                    continue;
                }
                imported_nodes += size;
                // An earlier import inside this one is serialized with it.
                imports.retain(|(import, _)| !inclusive_ancestor(self, target, *import));
                if instance {
                    images.references.instances.insert(target);
                } else {
                    images.references.resources.insert(target);
                }
                let markup = self.serialize_svg_for_image_with(target, images);
                imports.push((target, Some(markup)));
            }
        }
        let plan = self.svg_reference_plan(&images.references.collected, &resolved);
        let empty_mask = plan.empty_mask.clone();
        if !plan.is_empty() || mode_changed {
            images.urls.clear();
            let References {
                instances,
                resources,
                ..
            } = std::mem::take(&mut images.references);
            images.references = References {
                pass: Pass::Rewrite,
                plan,
                instances,
                resources,
                ..References::default()
            };
            let markup = self.serialize_svg_for_image_with(root, images);
            for (import, slot) in &mut imports {
                *slot = Some(self.serialize_svg_for_image_with(*import, images));
            }
            images.references = References::default();
            return assemble(markup, &imports, empty_mask.as_deref());
        }
        images.references = References::default();
        assemble(root_markup, &imports, None)
    }

    /// Decide how the markup writes each reference so that the decoder
    /// binds it as this document does. A valid reference keeps its ID when
    /// its target is the only serialized element carrying it, and otherwise
    /// names the target's fresh ID. An invalid one is written with the
    /// behavior its specification gives it, which the decoder would not
    /// reproduce from a dangling or mistyped ID: no clipping (CSS Masking 1
    /// #the-clip-path) and no filter (Filter Effects 1 #FilterProperty) are
    /// a `none` value, a mask layer of transparent black (#the-mask-image)
    /// names an empty `<mask>`, and the others (paint fallback, `use`,
    /// templates, markers) name an ID no element carries.
    fn svg_reference_plan(
        &self,
        collected: &Collected,
        resolved: &FxHashMap<(NodeId, String), Option<NodeId>>,
    ) -> Plan {
        let mut bearers: FxHashMap<&str, Vec<NodeId>> = FxHashMap::default();
        for (id, node) in &collected.ids {
            bearers.entry(id.as_str()).or_default().push(*node);
        }
        let mut keys: Vec<(NodeId, &str, Kind)> = Vec::new();
        let mut seen = FxHashSet::default();
        for reference in &collected.references {
            let key = (reference.scope, reference.fragment.as_str(), reference.kind);
            if seen.insert(key) {
                keys.push(key);
            }
        }
        let mut taken: FxHashSet<&str> = bearers.keys().copied().collect();
        taken.extend(keys.iter().map(|(_, fragment, _)| *fragment));
        let mut counter = 0usize;
        let mut fresh = || loop {
            let name = format!("trust-ref-{counter}");
            counter += 1;
            if !taken.contains(name.as_str()) {
                return name;
            }
        };
        let mut plan = Plan::default();
        let mut missing: Option<String> = None;
        for (scope, fragment, kind) in keys {
            let carriers = bearers.get(fragment).map_or(&[][..], Vec::as_slice);
            let target = resolved
                .get(&(scope, fragment.to_string()))
                .copied()
                .flatten()
                .filter(|&target| {
                    collected.serialized.contains(&target) && kind.accepts(self, target, None)
                });
            let action = match target {
                Some(target) if carriers == [target] => continue,
                Some(target) => Action::Bind(
                    plan.renamed
                        .entry(target)
                        .or_insert_with(&mut fresh)
                        .clone(),
                ),
                None => match kind {
                    Kind::ClipPath | Kind::Filter => Action::Disable,
                    Kind::Mask => {
                        Action::Bind(plan.empty_mask.get_or_insert_with(&mut fresh).clone())
                    }
                    _ if carriers.is_empty() => continue,
                    _ => Action::Bind(missing.get_or_insert_with(&mut fresh).clone()),
                },
            };
            plan.actions
                .insert((scope, fragment.to_string(), kind), action);
        }
        plan.circular = self.svg_circular_uses(collected, resolved);
        if !plan.circular.is_empty() {
            missing.get_or_insert_with(&mut fresh);
        }
        plan.missing = missing;
        plan
    }

    /// struct.html #UseShadowTree: "a set of references that directly or
    /// indirectly reference a element to create a circular dependency is an
    /// invalid circular reference", and the `use` whose shadow tree would
    /// create it is in error and not rendered. The decoder detects only a
    /// direct cycle, so cut every cycle of the serialized `use` graph where
    /// instantiation, entered from the referenced elements in reference
    /// order, would first repeat a referenced element. The cut is per
    /// element: an element instantiated along several paths is cut on all.
    fn svg_circular_uses(
        &self,
        collected: &Collected,
        resolved: &FxHashMap<(NodeId, String), Option<NodeId>>,
    ) -> FxHashSet<NodeId> {
        let mut uses: Vec<(NodeId, NodeId)> = Vec::new();
        let mut seen = FxHashSet::default();
        for reference in &collected.references {
            if reference.kind != Kind::Use || !seen.insert(reference.from) {
                continue;
            }
            if let Some(target) = resolved
                .get(&(reference.scope, reference.fragment.clone()))
                .copied()
                .flatten()
                .filter(|&target| {
                    collected.serialized.contains(&target) && Kind::Use.accepts(self, target, None)
                })
            {
                uses.push((reference.from, target));
            }
        }
        let targets: FxHashSet<NodeId> = uses.iter().map(|&(_, target)| target).collect();
        // Instantiating a referenced element instantiates every `use` in it.
        let mut edges: FxHashMap<NodeId, Vec<(NodeId, NodeId)>> = FxHashMap::default();
        for &(element, target) in &uses {
            let mut current = Some(element);
            while let Some(id) = current {
                if targets.contains(&id) {
                    edges.entry(id).or_default().push((element, target));
                }
                current = self.nodes.get(id).and_then(|native| native.parent);
            }
        }
        // Depth-first search over referenced elements; a back edge is a
        // `use` that would instantiate an element already being instantiated.
        let mut circular = FxHashSet::default();
        let mut finished: FxHashMap<NodeId, bool> = FxHashMap::default();
        for &(_, start) in &uses {
            if finished.contains_key(&start) {
                continue;
            }
            finished.insert(start, false);
            let mut stack = vec![(start, 0usize)];
            while let Some(&(element, next)) = stack.last() {
                let edge = edges.get(&element).and_then(|list| list.get(next)).copied();
                let Some((user, target)) = edge else {
                    finished.insert(element, true);
                    stack.pop();
                    continue;
                };
                if let Some(top) = stack.last_mut() {
                    top.1 += 1;
                }
                match finished.get(&target) {
                    Some(false) => {
                        circular.insert(user);
                    }
                    Some(true) => {}
                    None => {
                        finished.insert(target, false);
                        stack.push((target, 0));
                    }
                }
            }
        }
        circular
    }

    /// The inherited presentation values an imported resource root takes
    /// from its own ancestors (it declares none of them itself): for each
    /// property, the nearest ancestor's CSS declaration or, failing that,
    /// its presentation attribute (SVG 2 styling.html #PresentationAttributes:
    /// an author declaration that any other author rule overrides), else the
    /// initial value. `currentColor` resolves against the root's `color`.
    fn svg_resource_root_inherited_style(&self, node: NodeId, color: &str) -> String {
        let presentation = |element: NodeId, property: &str| -> Option<String> {
            if self.namespace_uri(element) != Some(SVG_NS) {
                return None;
            }
            let NodeData::Element { attrs, .. } = &self.nodes.get(element)?.data else {
                return None;
            };
            attrs
                .iter()
                .find(|a| a.name.ns.is_empty() && &*a.name.local == property)
                .map(|a| a.value.trim().to_string())
                .filter(|value| !value.is_empty())
        };
        let mut out = String::new();
        for &property in super::SVG_INHERITED_PRESENTATION_PROPERTIES {
            if self.cascaded(node, property).is_some() || presentation(node, property).is_some() {
                continue;
            }
            let mut value = None;
            let mut ancestor = self.style_parent(node);
            while let Some(element) = ancestor {
                let declared = self
                    .cascaded(element, property)
                    .and_then(|_| self.computed_value_resolved(element, property))
                    .or_else(|| presentation(element, property));
                if let Some(declared) = declared.filter(|value| {
                    !value.trim().is_empty() && !value.trim().eq_ignore_ascii_case("inherit")
                }) {
                    value = Some(declared);
                    break;
                }
                ancestor = self.style_parent(element);
            }
            let value = value.unwrap_or_else(|| initial_presentation_value(property).to_string());
            let value = super::replace_css_current_color(&value, color);
            if value.contains([';', '"']) {
                continue;
            }
            out.push_str(property);
            out.push(':');
            out.push_str(value.trim());
            out.push(';');
        }
        out
    }
}

/// SVG 2 property-table initial values of the inherited presentation
/// properties (painting.html, render.html #ClipRuleProperty).
fn initial_presentation_value(property: &str) -> &'static str {
    match property {
        "fill" => "black",
        "stroke" | "stroke-dasharray" => "none",
        "fill-opacity" | "stroke-opacity" => "1",
        "fill-rule" | "clip-rule" => "nonzero",
        "stroke-width" => "1px",
        "stroke-linecap" => "butt",
        "stroke-linejoin" => "miter",
        "stroke-miterlimit" => "4",
        "stroke-dashoffset" => "0",
        "paint-order" => "normal",
        _ => "auto",
    }
}

/// The outer markup with the imported subtrees (and the empty mask of
/// invalid mask references) in a `<defs>` after its start tag.
fn assemble(
    mut outer: String,
    imports: &[(NodeId, Option<String>)],
    empty_mask: Option<&str>,
) -> String {
    let mut defs: String = imports
        .iter()
        .filter_map(|(_, markup)| markup.as_deref())
        .collect();
    if let Some(id) = empty_mask {
        defs.push_str(&format!("<mask id=\"{id}\"/>"));
    }
    if defs.is_empty() {
        return outer;
    }
    if let Some(open) = outer.find('>') {
        outer.insert_str(open + 1, &format!("<defs>{defs}</defs>"));
    }
    outer
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raster(dom: &Dom, id: &str) -> crate::render::ImageResource {
        let (data, _) = dom
            .svg_image_data(dom.get_by_id(id).unwrap(), None)
            .expect("paintable SVG");
        let bytes = crate::img::decode_data_url(&data).unwrap();
        crate::img::decode_graphical(&bytes).unwrap()
    }

    fn pixel(image: &crate::render::ImageResource, x: u32, y: u32) -> [u8; 4] {
        let at = ((y * image.width + x) * 4) as usize;
        image.rgba[at..at + 4].try_into().unwrap()
    }

    #[test]
    fn css_url_scanner_finds_fragments_in_any_form() {
        let text =
            r##"fill:url(#a);stroke:url("#b") red;filter:blur(1px) url('#c');mask:url(x.svg#d)"##;
        let urls = style_urls(text);
        let fragments: Vec<_> = urls.iter().map(|(_, _, f, _)| f.as_str()).collect();
        assert_eq!(fragments, ["a", "b", "c"]);
        assert_eq!(&text[urls[1].1.clone()], r##"url("#b")"##);
        assert_eq!(urls[2].0, Kind::Filter);
        // Only properties that name a rendered SVG element are references.
        assert!(style_urls("background:url(#a);content:'url(#b);'").is_empty());
        assert_eq!(css_urls("url(#x) url(#)").len(), 1);
    }

    #[test]
    fn serialized_attribute_parser_round_trips_escaped_values() {
        let attrs = r##" id="a" style="fill:url(&quot;#g&quot;)" href="#u&amp;v""##;
        let parsed = serialized_attrs(attrs).unwrap();
        assert_eq!(parsed.len(), 3);
        assert_eq!(unescape(&attrs[parsed[1].1.clone()]), r##"fill:url("#g")"##);
        assert_eq!(unescape(&attrs[parsed[2].1.clone()]), "#u&v");
        assert!(serialized_attrs(" broken").is_none());
    }

    #[test]
    fn paint_servers_clip_paths_masks_markers_and_filters_resolve_across_svgs() {
        // SVG 2 linking.html #processingURL-target: same-document
        // references bind in the host document, so resources in a hidden
        // sprite SVG apply to every other inline SVG.
        let dom = Dom::parse_document(
            r##"<svg style="display:none" width="0" height="0">
              <linearGradient id="base"><stop offset="0" stop-color="#0000ff"/></linearGradient>
              <linearGradient id="grad" href="#base"/>
              <clipPath id="clip"><rect width="5" height="10"/></clipPath>
              <mask id="mask"><rect width="10" height="5" fill="white"/></mask>
              <marker id="dot" markerWidth="4" markerHeight="4" markerUnits="userSpaceOnUse"
                refX="2" refY="2"><rect width="4" height="4" fill="#ff0000"/></marker>
              <filter id="flood" x="0" y="0" width="1" height="1">
                <feFlood flood-color="#00ff00"/></filter>
            </svg>
            <svg id="paint" width="10" height="10"><rect width="10" height="10" fill="url(#grad) red"/></svg>
            <svg id="clipped" width="10" height="10"><rect width="10" height="10" fill="#000" clip-path="url(#clip)"/></svg>
            <svg id="masked" width="10" height="10"><rect width="10" height="10" fill="#000" style="mask:url(#mask)"/></svg>
            <svg id="marked" width="10" height="10"><path d="M5 5h1" stroke="none" marker-start="url(#dot)"/></svg>
            <svg id="filtered" width="10" height="10"><rect width="10" height="10" fill="#000" filter="url(#flood)"/></svg>"##,
        );
        assert_eq!(pixel(&raster(&dom, "paint"), 5, 5), [0, 0, 255, 255]);
        let clipped = raster(&dom, "clipped");
        assert_eq!(pixel(&clipped, 2, 5), [0, 0, 0, 255]);
        assert_eq!(pixel(&clipped, 7, 5)[3], 0);
        let masked = raster(&dom, "masked");
        assert_eq!(pixel(&masked, 5, 2), [0, 0, 0, 255]);
        assert_eq!(pixel(&masked, 5, 7)[3], 0);
        let marked = raster(&dom, "marked");
        assert_eq!(pixel(&marked, 4, 4), [255, 0, 0, 255]);
        assert_eq!(pixel(&marked, 8, 8)[3], 0);
        assert_eq!(pixel(&raster(&dom, "filtered"), 5, 5), [0, 255, 0, 255]);
    }

    #[test]
    fn first_element_with_the_id_in_the_document_wins_over_a_local_duplicate() {
        // DOM getElementById semantics: copy-pasted icons that repeat a
        // gradient ID all paint with the first one in the document.
        let dom = Dom::parse_document(
            r##"<svg width="0" height="0"><linearGradient id="g"><stop stop-color="#0000ff"/></linearGradient></svg>
            <svg id="icon" width="10" height="10"><defs><linearGradient id="g"><stop stop-color="#ff0000"/></linearGradient></defs>
            <rect width="10" height="10" fill="url(#g)"/></svg>"##,
        );
        assert_eq!(pixel(&raster(&dom, "icon"), 5, 5), [0, 0, 255, 255]);
    }

    #[test]
    fn invalid_target_types_and_missing_ids_use_the_paint_fallback() {
        // painting.html #SpecifyingPaint: an invalid paint server reference
        // uses the fallback color; a `<div>` with the ID is not a paint
        // server even though a later SVG element has the same ID.
        let dom = Dom::parse_document(
            r##"<div id="g"></div><svg width="0" height="0"><linearGradient id="g"><stop stop-color="#0000ff"/></linearGradient></svg>
            <svg id="icon" width="10" height="10"><rect width="10" height="10" fill="url(#g) #00ff00"/></svg>"##,
        );
        assert_eq!(pixel(&raster(&dom, "icon"), 5, 5), [0, 255, 0, 255]);
    }

    #[test]
    fn invalid_clip_filter_and_mask_references_follow_their_specifications() {
        // CSS Masking 1 #the-clip-path: no clipping; Filter Effects 1
        // #FilterProperty: no filter; CSS Masking 1 #the-mask-image: a layer
        // of transparent black. Here the first element with each ID is an
        // HTML element, the later SVG resources do not bind.
        let dom = Dom::parse_document(
            r##"<p id="c"></p><p id="f"></p><p id="m"></p>
            <svg width="0" height="0"><clipPath id="c"><rect width="1" height="1"/></clipPath>
            <filter id="f"><feFlood flood-color="#ff0000"/></filter>
            <mask id="m"><rect width="10" height="10" fill="white"/></mask></svg>
            <svg id="clip" width="10" height="10"><rect width="10" height="10" fill="#000" clip-path="url(#c)"/></svg>
            <svg id="filter" width="10" height="10"><rect width="10" height="10" fill="#000" style="filter:url(#f)"/></svg>
            <svg id="mask" width="10" height="10"><rect width="10" height="10" fill="#000" mask="url(#m)"/>
              <rect width="5" height="5" fill="#0000ff" mask="url(#missing)"/></svg>
            <svg id="missing" width="10" height="10"><rect width="10" height="10" fill="#000" filter="url(#nothing)" clip-path="url(#none)"/></svg>"##,
        );
        assert_eq!(pixel(&raster(&dom, "clip"), 8, 8), [0, 0, 0, 255]);
        assert_eq!(pixel(&raster(&dom, "filter"), 5, 5), [0, 0, 0, 255]);
        let masked = raster(&dom, "mask");
        assert_eq!(pixel(&masked, 2, 2)[3], 0);
        assert_eq!(pixel(&masked, 8, 8)[3], 0);
        assert_eq!(pixel(&raster(&dom, "missing"), 5, 5), [0, 0, 0, 255]);
    }

    #[test]
    fn use_instances_inherit_current_color_from_their_use() {
        // struct.html #UseStyleInheritance: an instance inherits from its
        // host `use`, not from the hidden sprite around its source.
        let dom = Dom::parse_document(
            r##"<div style="color:#0000ff"><svg style="display:none"><symbol id="s" viewBox="0 0 10 10">
              <rect width="10" height="10" fill="currentColor"/></symbol></svg></div>
            <svg id="icon" width="20" height="10" style="color:#ff0000"><use href="#s" width="10" height="10"/>
              <g style="color:#00ff00"><use href="#s" x="10" width="10" height="10"/></g></svg>"##,
        );
        let image = raster(&dom, "icon");
        assert_eq!(pixel(&image, 5, 5), [255, 0, 0, 255]);
        assert_eq!(pixel(&image, 15, 5), [0, 255, 0, 255]);
    }

    #[test]
    fn imported_resources_keep_their_own_inherited_properties() {
        // CSS Masking 1 #ClipPathElement: clip path content inherits from
        // the clipPath's ancestors, not from the referencing element.
        let dom = Dom::parse_document(
            r##"<svg width="0" height="0" clip-rule="evenodd"><clipPath id="c">
              <path d="M0 0h10v10H0zM3 3h4v4H3z"/></clipPath></svg>
            <svg id="icon" width="10" height="10" clip-rule="nonzero">
              <rect width="10" height="10" fill="#000" clip-path="url(#c)"/></svg>"##,
        );
        let image = raster(&dom, "icon");
        assert_eq!(pixel(&image, 1, 1), [0, 0, 0, 255]);
        assert_eq!(pixel(&image, 5, 5)[3], 0, "evenodd hole from the source");
        // pservers.html: pattern content inherits from the pattern, whose own
        // presentation attribute is kept, and otherwise from its ancestors.
        let dom = Dom::parse_document(
            r##"<svg width="0" height="0" fill="#ff0000">
              <pattern id="own" width="10" height="10" patternUnits="userSpaceOnUse" fill="#0000ff">
                <rect width="10" height="10"/></pattern>
              <pattern id="inherited" width="10" height="10" patternUnits="userSpaceOnUse">
                <rect width="10" height="10"/></pattern></svg>
            <svg id="icon" width="20" height="10" fill="#00ff00">
              <rect width="10" height="10" fill="url(#own)"/>
              <rect x="10" width="10" height="10" fill="url(#inherited)"/></svg>"##,
        );
        let image = raster(&dom, "icon");
        assert_eq!(pixel(&image, 5, 5), [0, 0, 255, 255]);
        assert_eq!(pixel(&image, 15, 5), [255, 0, 0, 255]);
    }

    #[test]
    fn transitive_references_and_cycles_stay_bounded() {
        let dom = Dom::parse_document(
            r##"<svg width="0" height="0">
              <symbol id="a" viewBox="0 0 10 10"><use href="#b"/></symbol>
              <symbol id="b" viewBox="0 0 10 10"><rect width="10" height="10" fill="url(#p)"/><use href="#a"/></symbol>
              <pattern id="p" width="10" height="10" patternUnits="userSpaceOnUse" href="#q"/>
              <pattern id="q" width="10" height="10" patternUnits="userSpaceOnUse">
                <rect width="10" height="10" fill="#0000ff"/></pattern>
            </svg>
            <svg id="icon" width="20" height="10"><use href="#a" width="10" height="10"/>
              <use href="#b" x="10" width="10" height="10"/></svg>"##,
        );
        // struct.html #UseShadowTree: the `use` that would instantiate `a`
        // inside its own instance is in error; everything else renders,
        // including the imported pattern's template chain.
        let image = raster(&dom, "icon");
        assert_eq!(pixel(&image, 5, 5), [0, 0, 255, 255]);
        assert_eq!(pixel(&image, 15, 5), [0, 0, 255, 255]);
    }

    #[test]
    fn imports_respect_the_node_budget() {
        let mut html = String::from(r#"<svg width="0" height="0">"#);
        for index in 0..MAX_IMPORTS + 8 {
            html.push_str(&format!(
                r#"<symbol id="s{index}"><rect width="1" height="1"/></symbol>"#
            ));
        }
        html.push_str(r#"</svg><svg id="icon" width="10" height="10">"#);
        for index in 0..MAX_IMPORTS + 8 {
            html.push_str(&format!(r##"<use href="#s{index}"/>"##));
        }
        html.push_str("</svg>");
        let dom = Dom::parse_document(&html);
        let mut images = SvgImageRefs::new(None);
        let markup = dom.svg_resource_markup(dom.get_by_id("icon").unwrap(), &mut images);
        assert_eq!(markup.matches("<symbol").count(), MAX_IMPORTS);
        let closure = dom.svg_use_closure(dom.get_by_id("icon").unwrap());
        assert!(closure.targets.len() <= MAX_IMPORTS + 1);
    }

    #[test]
    fn legacy_xlink_href_beside_href_does_not_duplicate_the_attribute() {
        // linking.html #processingURL: `href` wins; the resource must stay
        // well-formed XML.
        let dom = Dom::parse_document(
            r##"<svg width="0" height="0"><rect id="blue" width="10" height="10" fill="#0000ff"/>
            <rect id="red" width="10" height="10" fill="#ff0000"/></svg>
            <svg id="icon" width="10" height="10"><use xlink:href="#red" href="#blue"/></svg>"##,
        );
        assert_eq!(pixel(&raster(&dom, "icon"), 5, 5), [0, 0, 255, 255]);
    }
}
