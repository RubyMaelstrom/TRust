//! The XML serialization algorithm (W3C DOM Parsing and Serialization
//! #dfn-xml-serialization, local w3c/DOM-Parsing aefb90d) behind XMLSerializer
//! and the innerHTML/outerHTML getters of XML documents (HTML
//! #fragment-serializing-algorithm-steps). Unlike the HTML serializer it keeps
//! every element's and attribute's namespace, declaring prefixes as needed.
//! Well-formedness checks use XML 1.0 (Fifth Edition) [2] Char, [5] Name and
//! [13] PubidChar.

use super::{Attribute, Dom, NodeData, NodeId, QualName};
use std::collections::{HashMap, HashSet};

const HTML_NS: &str = "http://www.w3.org/1999/xhtml";
const XML_NS: &str = "http://www.w3.org/XML/1998/namespace";
const XMLNS_NS: &str = "http://www.w3.org/2000/xmlns/";

/// The node's serialization would not be well-formed; callers throw an
/// "InvalidStateError" DOMException.
#[derive(Debug, PartialEq, Eq)]
pub struct NotWellFormed;

type Serialized<T> = Result<T, NotWellFormed>;

/// #dfn-namespace-prefix-map: each namespace (None for the null namespace) to
/// the prefixes that map to it, the most recently added last.
#[derive(Clone, Default)]
struct PrefixMap(HashMap<Option<String>, Vec<String>>);

impl PrefixMap {
    /// #dfn-retrieving-a-preferred-prefix-string
    fn preferred(&self, preferred: Option<&str>, namespace: Option<&str>) -> Option<String> {
        let candidates = self.0.get(&namespace.map(str::to_owned))?;
        preferred
            .filter(|preferred| candidates.iter().any(|candidate| candidate == preferred))
            .map(str::to_owned)
            .or_else(|| candidates.last().cloned())
    }

    /// #dfn-found
    fn found(&self, prefix: &str, namespace: Option<&str>) -> bool {
        self.0
            .get(&namespace.map(str::to_owned))
            .is_some_and(|candidates| candidates.iter().any(|candidate| candidate == prefix))
    }

    /// #dfn-add
    fn add(&mut self, prefix: &str, namespace: Option<&str>) {
        self.0
            .entry(namespace.map(str::to_owned))
            .or_default()
            .push(prefix.to_owned());
    }

    /// #dfn-generating-a-prefix
    fn generate(&mut self, namespace: Option<&str>, index: &mut usize) -> String {
        let prefix = format!("ns{index}");
        *index += 1;
        self.add(&prefix, namespace);
        prefix
    }
}

fn namespace(name: &QualName) -> Option<&str> {
    (!name.ns.is_empty()).then_some(&*name.ns)
}

/// XML 1.0 [2] Char.
fn xml_chars(text: &str) -> bool {
    text.chars().all(|c| {
        matches!(c, '\t' | '\n' | '\r' | '\u{20}'..='\u{D7FF}' | '\u{E000}'..='\u{FFFD}')
            || c >= '\u{10000}'
    })
}

/// XML 1.0 [5] Name.
fn xml_name(name: &str) -> bool {
    let start = |c: char| {
        matches!(c, ':' | 'A'..='Z' | '_' | 'a'..='z' | '\u{C0}'..='\u{D6}' | '\u{D8}'..='\u{F6}'
            | '\u{F8}'..='\u{2FF}' | '\u{370}'..='\u{37D}' | '\u{37F}'..='\u{1FFF}'
            | '\u{200C}'..='\u{200D}' | '\u{2070}'..='\u{218F}' | '\u{2C00}'..='\u{2FEF}'
            | '\u{3001}'..='\u{D7FF}' | '\u{F900}'..='\u{FDCF}' | '\u{FDF0}'..='\u{FFFD}'
            | '\u{10000}'..='\u{EFFFF}')
    };
    let mut chars = name.chars();
    chars.next().is_some_and(start)
        && chars.all(|c| {
            start(c)
                || matches!(c, '-' | '.' | '0'..='9' | '\u{B7}' | '\u{300}'..='\u{36F}'
                    | '\u{203F}'..='\u{2040}')
        })
}

/// XML 1.0 [13] PubidChar.
fn pubid_chars(text: &str) -> bool {
    text.chars().all(|c| {
        matches!(c, ' ' | '\r' | '\n' | 'a'..='z' | 'A'..='Z' | '0'..='9')
            || "-'()+,./:=?;!*#@$_%".contains(c)
    })
}

/// #dfn-serializing-an-attribute-value. The listed replacements, plus ">"
/// as the specification's note and browsers do.
fn attribute_value(value: Option<&str>, well_formed: bool) -> Serialized<String> {
    let Some(value) = value else {
        return Ok(String::new());
    };
    if well_formed && !xml_chars(value) {
        return Err(NotWellFormed);
    }
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\t' => out.push_str("&#9;"),
            '\n' => out.push_str("&#xA;"),
            '\r' => out.push_str("&#xD;"),
            c => out.push(c),
        }
    }
    Ok(out)
}

/// #dfn-recording-the-namespace-information: record `attrs`' prefix
/// declarations and return the default namespace declaration's value.
fn record_namespaces(
    attrs: &[Attribute],
    map: &mut PrefixMap,
    local_prefixes: &mut HashMap<String, String>,
) -> Option<String> {
    let mut default_namespace = None;
    for attr in attrs {
        if namespace(&attr.name) != Some(XMLNS_NS) {
            continue;
        }
        if attr.name.prefix.is_none() {
            default_namespace = Some(attr.value.to_string());
            continue;
        }
        let prefix = &*attr.name.local;
        let definition = &*attr.value;
        if definition == XML_NS {
            continue;
        }
        let definition = (!definition.is_empty()).then_some(definition);
        if map.found(prefix, definition) {
            continue;
        }
        map.add(prefix, definition);
        local_prefixes.insert(prefix.to_owned(), definition.unwrap_or("").to_owned());
    }
    default_namespace
}

/// The XML serialization algorithm's mutable state.
struct Serializer<'a> {
    dom: &'a Dom,
    prefix_index: usize,
    well_formed: bool,
    out: String,
}

impl Dom {
    /// #dfn-xml-serialization ("produce an XML serialization") of `node`.
    pub fn xml_serialization(&self, node: NodeId, well_formed: bool) -> Serialized<String> {
        let mut serializer = Serializer::new(self, well_formed);
        serializer.node(node, None, &Serializer::initial_map())?;
        Ok(serializer.out)
    }

    /// HTML #fragment-serializing-algorithm-steps for an XML document: the
    /// XML serialization of `node`'s children (a template's contents), as
    /// for a document fragment.
    pub fn xml_fragment_serialization(
        &self,
        node: NodeId,
        well_formed: bool,
    ) -> Serialized<String> {
        let parent = match &self.nodes[node].data {
            NodeData::Element {
                template_contents: Some(contents),
                ..
            } => *contents,
            _ => node,
        };
        let mut serializer = Serializer::new(self, well_formed);
        serializer.children(parent, None, &Serializer::initial_map())?;
        Ok(serializer.out)
    }
}

impl<'a> Serializer<'a> {
    fn new(dom: &'a Dom, well_formed: bool) -> Self {
        Self {
            dom,
            prefix_index: 1,
            well_formed,
            out: String::new(),
        }
    }

    fn initial_map() -> PrefixMap {
        let mut map = PrefixMap::default();
        map.add("xml", Some(XML_NS));
        map
    }

    fn children(
        &mut self,
        parent: NodeId,
        namespace: Option<&str>,
        map: &PrefixMap,
    ) -> Serialized<()> {
        for child in self.dom.child_iter(parent) {
            self.node(child, namespace, map)?;
        }
        Ok(())
    }

    /// #dfn-xml-serialization-algorithm
    fn node(&mut self, node: NodeId, namespace: Option<&str>, map: &PrefixMap) -> Serialized<()> {
        let well_formed = self.well_formed;
        match &self.dom.nodes[node].data {
            NodeData::Element {
                name,
                attrs,
                template_contents,
            } => self.element(node, name, attrs, *template_contents, namespace, map),
            NodeData::Document => {
                let has_element = self
                    .dom
                    .child_iter(node)
                    .any(|child| matches!(self.dom.nodes[child].data, NodeData::Element { .. }));
                if well_formed && !has_element {
                    return Err(NotWellFormed);
                }
                self.children(node, namespace, map)
            }
            NodeData::Fragment => self.children(node, namespace, map),
            NodeData::Comment(data) => {
                if well_formed && (!xml_chars(data) || data.contains("--") || data.ends_with('-')) {
                    return Err(NotWellFormed);
                }
                self.out.push_str("<!--");
                self.out.push_str(data);
                self.out.push_str("-->");
                Ok(())
            }
            NodeData::CData(data) => {
                self.out.push_str("<![CDATA[");
                self.out.push_str(data);
                self.out.push_str("]]>");
                Ok(())
            }
            NodeData::Text(data) => {
                if well_formed && !xml_chars(data) {
                    return Err(NotWellFormed);
                }
                for c in data.chars() {
                    match c {
                        '&' => self.out.push_str("&amp;"),
                        '<' => self.out.push_str("&lt;"),
                        '>' => self.out.push_str("&gt;"),
                        c => self.out.push(c),
                    }
                }
                Ok(())
            }
            NodeData::Doctype(info) => {
                if well_formed
                    && (!pubid_chars(&info.public_id)
                        || !xml_chars(&info.system_id)
                        || (info.system_id.contains('"') && info.system_id.contains('\'')))
                {
                    return Err(NotWellFormed);
                }
                // #dfn-serialization-of-the-id
                let id = |id: &str| {
                    let quote = if id.contains('"') { '\'' } else { '"' };
                    format!("{quote}{id}{quote}")
                };
                self.out.push_str("<!DOCTYPE ");
                self.out.push_str(&info.name);
                if !info.public_id.is_empty() {
                    self.out.push_str(" PUBLIC ");
                    self.out.push_str(&id(&info.public_id));
                } else if !info.system_id.is_empty() {
                    self.out.push_str(" SYSTEM");
                }
                if !info.system_id.is_empty() {
                    self.out.push(' ');
                    self.out.push_str(&id(&info.system_id));
                }
                self.out.push('>');
                Ok(())
            }
            NodeData::ProcessingInstruction { target, data } => {
                if well_formed
                    && (target.contains(':')
                        || target.eq_ignore_ascii_case("xml")
                        || !xml_chars(data)
                        || data.contains("?>"))
                {
                    return Err(NotWellFormed);
                }
                self.out.push_str("<?");
                self.out.push_str(target);
                self.out.push(' ');
                self.out.push_str(data);
                self.out.push_str("?>");
                Ok(())
            }
        }
    }

    /// #dfn-xml-serializing-an-element-node
    fn element(
        &mut self,
        node: NodeId,
        name: &QualName,
        attrs: &[Attribute],
        template_contents: Option<NodeId>,
        namespace: Option<&str>,
        map: &PrefixMap,
    ) -> Serialized<()> {
        let well_formed = self.well_formed;
        let local = &*name.local;
        if well_formed && (local.contains(':') || !xml_name(local)) {
            return Err(NotWellFormed);
        }
        self.out.push('<');
        let mut ignore_namespace_definition = false;
        let mut map = map.clone();
        let mut local_prefixes = HashMap::new();
        let local_default_namespace = record_namespaces(attrs, &mut map, &mut local_prefixes);
        let mut inherited: Option<String> = namespace.map(str::to_owned);
        let ns = self::namespace(name);
        let qualified;
        if inherited.as_deref() == ns {
            if local_default_namespace.is_some() {
                ignore_namespace_definition = true;
            }
            qualified = if ns == Some(XML_NS) {
                format!("xml:{local}")
            } else {
                local.to_owned()
            };
            self.out.push_str(&qualified);
        } else {
            let prefix = name.prefix.as_deref();
            let mut candidate = map.preferred(prefix, ns);
            if prefix == Some("xmlns") {
                if well_formed {
                    return Err(NotWellFormed);
                }
                candidate = Some("xmlns".to_owned());
            }
            if let Some(candidate) = candidate {
                // Found a suitable namespace prefix.
                qualified = format!("{candidate}:{local}");
                if let Some(default) = &local_default_namespace
                    && default != XML_NS
                {
                    inherited = (!default.is_empty()).then(|| default.clone());
                }
                self.out.push_str(&qualified);
            } else if let Some(prefix) = prefix {
                let prefix = if local_prefixes.contains_key(prefix) {
                    map.generate(ns, &mut self.prefix_index)
                } else {
                    prefix.to_owned()
                };
                map.add(&prefix, ns);
                qualified = format!("{prefix}:{local}");
                self.out.push_str(&qualified);
                self.out.push_str(" xmlns:");
                self.out.push_str(&prefix);
                self.out.push_str("=\"");
                self.out.push_str(&attribute_value(ns, well_formed)?);
                self.out.push('"');
                if let Some(default) = &local_default_namespace {
                    inherited = (!default.is_empty()).then(|| default.clone());
                }
            } else if local_default_namespace.is_none() || local_default_namespace.as_deref() != ns
            {
                // Declare (or replace) the default namespace for this element.
                ignore_namespace_definition = true;
                qualified = local.to_owned();
                inherited = ns.map(str::to_owned);
                self.out.push_str(&qualified);
                self.out.push_str(" xmlns=\"");
                self.out.push_str(&attribute_value(ns, well_formed)?);
                self.out.push('"');
            } else {
                qualified = local.to_owned();
                inherited = ns.map(str::to_owned);
                self.out.push_str(&qualified);
            }
        }
        self.attributes(
            attrs,
            &mut map,
            &mut local_prefixes,
            ignore_namespace_definition,
        )?;
        let childless = self.dom.nodes[node].first_child.is_none();
        let html = ns == Some(HTML_NS);
        let void = matches!(
            local,
            "area"
                | "base"
                | "basefont"
                | "bgsound"
                | "br"
                | "col"
                | "embed"
                | "frame"
                | "hr"
                | "img"
                | "input"
                | "keygen"
                | "link"
                | "menuitem"
                | "meta"
                | "param"
                | "source"
                | "track"
                | "wbr"
        );
        if childless && html && void {
            self.out.push_str(" />");
            return Ok(());
        }
        if childless && !html {
            self.out.push_str("/>");
            return Ok(());
        }
        self.out.push('>');
        match template_contents {
            Some(contents) if html && local == "template" => {
                self.children(contents, inherited.as_deref(), &map)?;
            }
            _ => self.children(node, inherited.as_deref(), &map)?,
        }
        self.out.push_str("</");
        self.out.push_str(&qualified);
        self.out.push('>');
        Ok(())
    }

    /// #dfn-xml-serialization-of-the-attributes
    fn attributes(
        &mut self,
        attrs: &[Attribute],
        map: &mut PrefixMap,
        local_prefixes: &mut HashMap<String, String>,
        ignore_namespace_definition: bool,
    ) -> Serialized<()> {
        let well_formed = self.well_formed;
        let mut local_names = HashSet::new();
        for attr in attrs {
            let attr_ns = namespace(&attr.name);
            let local = &*attr.name.local;
            let value = &*attr.value;
            if !local_names.insert((attr_ns, local)) && well_formed {
                return Err(NotWellFormed);
            }
            let mut candidate = None;
            if let Some(attr_ns) = attr_ns {
                let prefix = attr.name.prefix.as_deref();
                candidate = map.preferred(prefix, Some(attr_ns));
                if attr_ns == XMLNS_NS {
                    let redeclared = prefix.is_some()
                        && local_prefixes
                            .get(local)
                            .is_none_or(|defined| defined != value)
                        && map.found(local, Some(value));
                    if value == XML_NS
                        || (prefix.is_none() && ignore_namespace_definition)
                        || redeclared
                    {
                        continue;
                    }
                    if well_formed && (value == XMLNS_NS || value.is_empty()) {
                        return Err(NotWellFormed);
                    }
                    if prefix == Some("xmlns") {
                        candidate = Some("xmlns".to_owned());
                    }
                } else if candidate.is_none() {
                    let generated = match prefix {
                        Some(prefix) if !local_prefixes.contains_key(prefix) => prefix.to_owned(),
                        _ => map.generate(Some(attr_ns), &mut self.prefix_index),
                    };
                    map.add(&generated, Some(attr_ns));
                    local_prefixes.insert(generated.clone(), attr_ns.to_owned());
                    self.out.push_str(" xmlns:");
                    self.out.push_str(&generated);
                    self.out.push_str("=\"");
                    self.out
                        .push_str(&attribute_value(Some(attr_ns), well_formed)?);
                    self.out.push('"');
                    candidate = Some(generated);
                }
            }
            self.out.push(' ');
            if let Some(candidate) = &candidate {
                self.out.push_str(candidate);
                self.out.push(':');
            }
            if well_formed
                && (local.contains(':')
                    || !xml_name(local)
                    || (local == "xmlns" && attr_ns.is_none()))
            {
                return Err(NotWellFormed);
            }
            self.out.push_str(local);
            self.out.push_str("=\"");
            self.out
                .push_str(&attribute_value(Some(value), well_formed)?);
            self.out.push('"');
        }
        Ok(())
    }
}
