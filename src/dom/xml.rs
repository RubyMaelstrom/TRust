//! HTML #dom-domparser-parsefromstring, XML 1.0 Fifth Edition §2–4,
//! Namespaces in XML 1.0 Third Edition §6. XML is never sent to html5ever.
//!
//! roxmltree validates expanded names, duplicate attributes, entities and
//! document structure before we publish anything. The event reader preserves
//! CDATA boundaries and processing instructions which roxmltree coalesces.
//! Neither parser resolves external resources; entity expansion is bounded.

use super::{Attribute, DocumentTypeInfo, Dom, Namespace, NodeData, NodeId, Prefix, QualName};
use xml::reader::{ParserConfig, XmlEvent};

const HTML: &str = "http://www.w3.org/1999/xhtml";
const XML: &str = "http://www.w3.org/XML/1998/namespace";
const XMLNS: &str = "http://www.w3.org/2000/xmlns/";
const PARSERERROR: &str = "http://www.mozilla.org/newlayout/xml/parsererror.xml";

impl Dom {
    pub fn parse_xml_document_into(&mut self, source: &str, content_type: &str) -> NodeId {
        let doc = self.create_document(content_type);
        self.populate_xml_document_or_error(doc, source);
        doc
    }

    /// HTML #read-xml for a top-level navigation: the page's Document is
    /// built by the XML parser, with `content_type` as its content type.
    pub fn parse_xml_page(source: &str, content_type: &str) -> Dom {
        let mut dom = Dom::new();
        if content_type != "text/html" {
            dom.document_content_types
                .insert(super::DOCUMENT, content_type.to_owned());
        }
        dom.populate_xml_document_or_error(super::DOCUMENT, source);
        // As after an HTML parse (the tree sink's finish): parser-created
        // nodes are connected roots, and the first render is a full one.
        dom.touch();
        dom.clear_gc_allocation_leases();
        dom
    }

    /// Populate `doc`; an XML well-formedness or namespace well-formedness
    /// error leaves only a parsererror element describing it (HTML
    /// #dom-domparser-parsefromstring; #read-xml may report errors inline).
    fn populate_xml_document_or_error(&mut self, doc: NodeId, source: &str) {
        if let Err(error) = self.populate_xml_document(doc, source) {
            let children: Vec<_> = self.child_iter(doc).collect();
            for child in children {
                self.detach(child);
            }
            let root = self.create_element_ns(PARSERERROR, None, "parsererror");
            let text = self.create_text(&error);
            self.append(root, text);
            self.append(doc, root);
        }
    }

    /// DOM #locate-a-namespace for an element (the Element case, which
    /// walks to the parent element).
    pub(crate) fn locate_namespace(&self, element: NodeId, prefix: Option<&str>) -> Option<String> {
        match prefix {
            Some("xml") => return Some(XML.to_owned()),
            Some("xmlns") => return Some(XMLNS.to_owned()),
            _ => {}
        }
        let mut current = Some(element);
        while let Some(id) = current {
            let NodeData::Element { name, attrs, .. } = &self.nodes[id].data else {
                return None;
            };
            if !name.ns.is_empty() && name.prefix.as_deref() == prefix {
                return Some(name.ns.to_string());
            }
            let declaration = attrs.iter().find(|attr| {
                &*attr.name.ns == XMLNS
                    && match prefix {
                        Some(prefix) => {
                            attr.name.prefix.as_deref() == Some("xmlns")
                                && &*attr.name.local == prefix
                        }
                        None => attr.name.prefix.is_none() && &*attr.name.local == "xmlns",
                    }
            });
            if let Some(attr) = declaration {
                return (!attr.value.is_empty()).then(|| attr.value.to_string());
            }
            current = self.nodes[id]
                .parent
                .filter(|&parent| matches!(self.nodes[parent].data, NodeData::Element { .. }));
        }
        None
    }

    /// HTML #xml-fragment-parsing-algorithm: parse `markup` as the content of
    /// a start tag for `context` that declares every namespace prefix in
    /// scope on it (DOM lookupNamespaceURI) and its default namespace (DOM
    /// isDefaultNamespace). Without a context element, the context is a new
    /// HTML body element (HTML #dom-element-insertadjacenthtml). Returns the
    /// new detached children, or None for an XML well-formedness or
    /// namespace well-formedness error (a SyntaxError).
    pub(crate) fn parse_xml_fragment(
        &mut self,
        context: Option<NodeId>,
        markup: &str,
    ) -> Option<Vec<NodeId>> {
        let (parsed, root) = self.xml_fragment_document(context, markup)?;
        let children: Vec<NodeId> = parsed.child_iter(root).collect();
        Some(
            children
                .into_iter()
                .map(|child| self.transplant(&parsed, child))
                .collect(),
        )
    }

    /// Whether `parse_xml_fragment` would succeed, without creating nodes.
    pub(crate) fn xml_fragment_well_formed(&self, context: Option<NodeId>, markup: &str) -> bool {
        self.xml_fragment_document(context, markup).is_some()
    }

    fn xml_fragment_document(
        &self,
        context: Option<NodeId>,
        markup: &str,
    ) -> Option<(Dom, NodeId)> {
        let escape = |value: &str| {
            value
                .replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('"', "&quot;")
        };
        let (qualified, declarations) = match context {
            None => (
                String::from("body"),
                String::from(" xmlns=\"http://www.w3.org/1999/xhtml\""),
            ),
            Some(context) => {
                let NodeData::Element { name, .. } = &self.nodes[context].data else {
                    return None;
                };
                let qualified = match name.prefix.as_deref() {
                    Some(prefix) => format!("{prefix}:{}", name.local),
                    None => name.local.to_string(),
                };
                let mut prefixes: Vec<String> = Vec::new();
                let mut current = Some(context);
                while let Some(id) = current {
                    let NodeData::Element { name, attrs, .. } = &self.nodes[id].data else {
                        break;
                    };
                    prefixes.extend(name.prefix.as_deref().map(str::to_owned));
                    for attr in attrs {
                        if &*attr.name.ns == XMLNS && attr.name.prefix.as_deref() == Some("xmlns") {
                            prefixes.push(attr.name.local.to_string());
                        }
                    }
                    current = self.nodes[id].parent;
                }
                let mut declarations = String::new();
                if let Some(namespace) = self.locate_namespace(context, None) {
                    declarations.push_str(&format!(" xmlns=\"{}\"", escape(&namespace)));
                }
                let mut declared = std::collections::HashSet::new();
                for prefix in prefixes {
                    // The xml prefix is bound implicitly; xmlns may not be declared.
                    if matches!(prefix.as_str(), "xml" | "xmlns")
                        || !declared.insert(prefix.clone())
                    {
                        continue;
                    }
                    if let Some(namespace) = self.locate_namespace(context, Some(&prefix)) {
                        declarations
                            .push_str(&format!(" xmlns:{prefix}=\"{}\"", escape(&namespace)));
                    }
                }
                (qualified, declarations)
            }
        };
        let source = format!(
            "<{qualified}{declarations}>{}</{qualified}>",
            super::replace_lone_surrogates(markup)
        );
        let mut parsed = Dom::new();
        parsed
            .populate_xml_document(super::DOCUMENT, &source)
            .ok()?;
        // The fictional root is the document element and has no siblings.
        let roots: Vec<NodeId> = parsed.child_iter(super::DOCUMENT).collect();
        let [root] = roots[..] else {
            return None;
        };
        Some((parsed, root))
    }

    fn populate_xml_document(&mut self, doc: NodeId, source: &str) -> Result<(), String> {
        if source.len() > 32 * 1024 * 1024 {
            return Err("XML input limit exceeded".into());
        }
        let reader = ParserConfig::new()
            .ignore_comments(false)
            .allow_multiple_root_elements(false)
            // DOMParser receives an already decoded DOMString. An XML encoding
            // declaration cannot reinterpret its UTF-8 Rust representation.
            .override_encoding(Some(xml::Encoding::Utf8))
            .ignore_invalid_encoding_declarations(true)
            .max_data_length(32 * 1024 * 1024)
            .max_attribute_length(8 * 1024 * 1024)
            .create_reader(source.as_bytes());
        // Bound expanded data before running the structural validator. Its
        // node limit alone cannot limit a single enormous entity-expanded Text.
        let mut events = Vec::new();
        let mut expanded = 0usize;
        for event in reader {
            let event = event.map_err(|error| error.to_string())?;
            expanded += match &event {
                XmlEvent::Characters(text)
                | XmlEvent::Whitespace(text)
                | XmlEvent::CData(text)
                | XmlEvent::Comment(text)
                | XmlEvent::Doctype { syntax: text } => text.len(),
                XmlEvent::ProcessingInstruction { name, data } => {
                    name.len() + data.as_ref().map_or(0, String::len)
                }
                XmlEvent::StartElement {
                    name,
                    attributes,
                    namespace,
                } => {
                    name.local_name.len()
                        + name.namespace.as_ref().map_or(0, String::len)
                        + attributes
                            .iter()
                            .map(|attr| attr.name.local_name.len() + attr.value.len())
                            .sum::<usize>()
                        + namespace
                            .0
                            .iter()
                            .map(|(prefix, uri)| prefix.len() + uri.len())
                            .sum::<usize>()
                }
                _ => 0,
            };
            if events.len() >= 2_000_000 || expanded > 32 * 1024 * 1024 {
                return Err("XML expansion limit exceeded".into());
            }
            events.push(event);
        }
        let parsed = roxmltree::Document::parse_with_options(
            source,
            roxmltree::ParsingOptions {
                allow_dtd: true,
                nodes_limit: 1_000_000,
                ..Default::default()
            },
        )
        .map_err(|error| error.to_string())?;
        let mut elements = parsed.descendants().filter(|node| node.is_element());
        let mut parents = vec![doc];
        let mut count = 0usize;
        for event in events {
            let parent = *parents.last().ok_or("XML parent stack is empty")?;
            let node = match event {
                XmlEvent::StartElement { name, .. } => {
                    let source_node = elements.next().ok_or("Inconsistent XML element stream")?;
                    let attrs = xml_attributes(source, source_node)?;
                    // HTML #parsing-xhtml-documents: the XML parser appends a
                    // template element's children to its template contents.
                    let contents = (name.local_name == "template"
                        && name.namespace.as_deref() == Some(HTML))
                    .then(|| self.new_node(NodeData::Fragment));
                    let element = self.new_node(NodeData::Element {
                        name: QualName::new(
                            name.prefix.as_deref().map(Prefix::from),
                            Namespace::from(name.namespace.as_deref().unwrap_or("")),
                            name.local_name.into(),
                        ),
                        attrs,
                        template_contents: contents,
                    });
                    self.append(parent, element);
                    parents.push(contents.unwrap_or(element));
                    Some(element)
                }
                XmlEvent::EndElement { .. } => {
                    parents.pop();
                    None
                }
                XmlEvent::Characters(text) | XmlEvent::Whitespace(text) => {
                    let node = self.create_text(&text);
                    self.append(parent, node);
                    Some(node)
                }
                XmlEvent::CData(text) => {
                    let node = self.new_node(NodeData::CData(text));
                    self.append(parent, node);
                    Some(node)
                }
                XmlEvent::Comment(text) => {
                    let node = self.create_comment(&text);
                    self.append(parent, node);
                    Some(node)
                }
                XmlEvent::ProcessingInstruction { name, data } => {
                    let node = self.new_node(NodeData::ProcessingInstruction {
                        target: name,
                        data: data.unwrap_or_default(),
                    });
                    self.append(parent, node);
                    Some(node)
                }
                // The document's DocumentType child (DOM #concept-doctype).
                XmlEvent::Doctype { syntax } => {
                    let info = doctype_declaration(&syntax).ok_or("Malformed XML DOCTYPE")?;
                    let node = self.create_doctype(&info.name, &info.public_id, &info.system_id);
                    self.append(parent, node);
                    Some(node)
                }
                XmlEvent::StartDocument { .. } | XmlEvent::EndDocument => None,
            };
            if node.is_some() {
                count += 1;
                if count > 1_000_000 {
                    return Err("XML node limit exceeded".into());
                }
            }
        }
        Ok(())
    }
}

/// XML 1.0 (Fifth Edition) [28] doctypedecl and [75] ExternalID: the
/// DOCTYPE's name and its public and system literals (empty when absent).
/// The internal subset contributes nothing to the DocumentType node.
fn doctype_declaration(syntax: &str) -> Option<DocumentTypeInfo> {
    // [3] S ::= (#x20 | #x9 | #xD | #xA)+
    let space = |c: char| matches!(c, ' ' | '\t' | '\r' | '\n');
    // [11] SystemLiteral and [12] PubidLiteral: quoted, with no escapes.
    let literal = |input: &str| -> Option<(String, usize)> {
        let quote = input.chars().next().filter(|c| matches!(c, '"' | '\''))?;
        let end = input[1..].find(quote)? + 1;
        Some((input[1..end].to_owned(), end + 1))
    };
    let rest = syntax.strip_prefix("<!DOCTYPE")?.trim_start_matches(space);
    let name_end = rest
        .find(|c: char| space(c) || matches!(c, '[' | '>'))
        .unwrap_or(rest.len());
    let mut info = DocumentTypeInfo {
        name: rest[..name_end].to_owned(),
        ..Default::default()
    };
    let rest = rest[name_end..].trim_start_matches(space);
    if let Some(after) = rest.strip_prefix("SYSTEM") {
        (info.system_id, _) = literal(after.trim_start_matches(space))?;
    } else if let Some(after) = rest.strip_prefix("PUBLIC") {
        let after = after.trim_start_matches(space);
        let (public_id, used) = literal(after)?;
        (info.system_id, _) = literal(after[used..].trim_start_matches(space))?;
        info.public_id = public_id;
    }
    (!info.name.is_empty()).then_some(info)
}

// Preserve the author's prefixes, attribute order and *local* namespace
// declarations, including redundant ones. Both XML parsers otherwise expose
// resolved/in-scope namespaces. This scans only a start tag already validated
// by roxmltree; values come from the parser's normalized attribute data.
fn xml_attributes(
    source: &str,
    element: roxmltree::Node<'_, '_>,
) -> Result<Vec<Attribute>, String> {
    let source = &source[element.range()];
    let bytes = source.as_bytes();
    let mut pos = 1;
    while pos < bytes.len()
        && !bytes[pos].is_ascii_whitespace()
        && !matches!(bytes[pos], b'/' | b'>')
    {
        pos += 1;
    }
    let mut attrs = Vec::new();
    loop {
        while pos < bytes.len() && bytes[pos].is_ascii_whitespace() {
            pos += 1;
        }
        if pos >= bytes.len() || matches!(bytes[pos], b'/' | b'>') {
            break;
        }
        let start = pos;
        while pos < bytes.len() && !bytes[pos].is_ascii_whitespace() && bytes[pos] != b'=' {
            pos += 1;
        }
        let qualified = &source[start..pos];
        while pos < bytes.len() && (bytes[pos].is_ascii_whitespace() || bytes[pos] == b'=') {
            pos += 1;
        }
        let quote = *bytes.get(pos).ok_or("Missing XML attribute quote")?;
        pos += 1;
        while pos < bytes.len() && bytes[pos] != quote {
            pos += 1;
        }
        pos += 1;
        let (prefix, local) = qualified
            .split_once(':')
            .map_or((None, qualified), |(prefix, local)| (Some(prefix), local));
        let (namespace, value) = if qualified == "xmlns" || prefix == Some("xmlns") {
            (
                XMLNS,
                element
                    .lookup_namespace_uri(if qualified == "xmlns" {
                        None
                    } else {
                        Some(local)
                    })
                    .unwrap_or(""),
            )
        } else {
            let namespace = prefix.and_then(|prefix| element.lookup_namespace_uri(Some(prefix)));
            let attr = if let Some(namespace) = namespace {
                element.attribute_node((namespace, local))
            } else {
                element.attribute_node(local)
            }
            .ok_or("Missing normalized XML attribute")?;
            (namespace.unwrap_or(""), attr.value())
        };
        attrs.push(Attribute {
            name: QualName::new(
                prefix.map(Prefix::from),
                Namespace::from(namespace),
                local.into(),
            ),
            value: value.into(),
        });
    }
    Ok(attrs)
}

#[cfg(test)]
mod tests {
    use super::doctype_declaration;
    use crate::dom::{DOCUMENT, Dom, NodeData, NodeId};

    fn element_children(dom: &Dom, id: NodeId) -> Vec<NodeId> {
        dom.child_iter(id)
            .filter(|&c| matches!(dom.node(c).data, NodeData::Element { .. }))
            .collect()
    }

    #[test]
    fn xml_pages_are_built_by_the_xml_parser() {
        // HTML #read-xml: whitespace and the document element are as written,
        // the DOCTYPE becomes a DocumentType, and (HTML #parsing-xhtml-documents)
        // a template element's children land in its template contents.
        let dom = Dom::parse_xml_page(
            "<!DOCTYPE html>\n<html xmlns=\"http://www.w3.org/1999/xhtml\"><head/>\
             <body><template><p>t</p></template><x:y xmlns:x=\"urn:x\"/></body></html>\n",
            "application/xhtml+xml",
        );
        assert_eq!(dom.document_content_type(DOCUMENT), "application/xhtml+xml");
        let roots = element_children(&dom, DOCUMENT);
        assert_eq!(roots.len(), 1);
        assert!(
            dom.doctype_info(dom.child_iter(DOCUMENT).next().unwrap())
                .is_some()
        );
        let html = roots[0];
        assert_eq!(
            dom.namespace_uri(html),
            Some("http://www.w3.org/1999/xhtml")
        );
        let body = element_children(&dom, html)[1];
        let [template, prefixed] = element_children(&dom, body)[..] else {
            panic!("body children");
        };
        assert!(dom.child_iter(template).next().is_none());
        let contents = dom.content_target(template);
        assert_ne!(contents, template);
        assert_eq!(dom.text_content(contents), "t");
        assert_eq!(dom.namespace_prefix(prefixed), Some("x"));
        assert_eq!(dom.namespace_uri(prefixed), Some("urn:x"));
        // A well-formedness error leaves only the parsererror element.
        let broken = Dom::parse_xml_page("<a><b></a>", "application/xml");
        let roots = element_children(&broken, DOCUMENT);
        assert_eq!(broken.tag_name(roots[0]), Some("parsererror"));
    }

    #[test]
    fn xml_fragments_declare_in_scope_namespaces() {
        // HTML #xml-fragment-parsing-algorithm: the context start tag declares
        // the default namespace and every prefix in scope on the context.
        let mut dom = Dom::new();
        let doc = dom.parse_xml_document_into(
            "<r xmlns=\"urn:d\" xmlns:p=\"urn:p\"><p:c xmlns:q=\"urn:q\"/></r>",
            "application/xml",
        );
        let root = element_children(&dom, doc)[0];
        let context = element_children(&dom, root)[0];
        assert_eq!(
            dom.locate_namespace(context, None).as_deref(),
            Some("urn:d")
        );
        assert_eq!(
            dom.locate_namespace(context, Some("q")).as_deref(),
            Some("urn:q")
        );
        assert_eq!(
            dom.locate_namespace(context, Some("xml")).as_deref(),
            Some("http://www.w3.org/XML/1998/namespace")
        );
        assert_eq!(dom.locate_namespace(context, Some("z")), None);
        let nodes = dom
            .parse_xml_fragment(Some(context), "<e/>t<p:e/><q:e/><![CDATA[<]]>")
            .unwrap();
        let names: Vec<_> = nodes
            .iter()
            .map(|&n| (dom.tag_name(n), dom.namespace_uri(n)))
            .collect();
        assert_eq!(
            names,
            [
                (Some("e"), Some("urn:d")),
                (None, None),
                (Some("e"), Some("urn:p")),
                (Some("e"), Some("urn:q")),
                (None, None),
            ]
        );
        assert!(matches!(dom.node(nodes[4]).data, NodeData::CData(_)));
        // Undeclared prefixes, stray end tags and HTML entities are errors.
        for markup in ["<z:e/>", "</c>", "&nbsp;", "<a>"] {
            assert!(
                !dom.xml_fragment_well_formed(Some(context), markup),
                "{markup}"
            );
            assert!(
                dom.parse_xml_fragment(Some(context), markup).is_none(),
                "{markup}"
            );
        }
        // Without a context element the context is an XHTML body element.
        let nodes = dom.parse_xml_fragment(None, "<p/>").unwrap();
        assert_eq!(
            dom.namespace_uri(nodes[0]),
            Some("http://www.w3.org/1999/xhtml")
        );
    }

    #[test]
    fn xml_parsing_replaces_lone_surrogates() {
        // A DOMString's lone surrogate (engine text: one private-use scalar)
        // reaches the XML parser as U+FFFD; a pair is kept.
        let pair = "\u{10F83C}\u{10FF00}";
        let text = format!("a\u{10F83C}b{pair}");
        assert_eq!(
            crate::dom::replace_lone_surrogates(&text),
            format!("a\u{FFFD}b{pair}")
        );
        assert!(matches!(
            crate::dom::replace_lone_surrogates("plain"),
            std::borrow::Cow::Borrowed(_)
        ));
    }

    #[test]
    fn doctype_declarations_follow_xml_productions() {
        // XML 1.0 (Fifth Edition) [28] doctypedecl, [75] ExternalID, [11]
        // SystemLiteral and [12] PubidLiteral; the internal subset is ignored.
        let parts = |syntax| {
            doctype_declaration(syntax).map(|info| (info.name, info.public_id, info.system_id))
        };
        let owned = |name: &str, public: &str, system: &str| {
            Some((name.to_owned(), public.to_owned(), system.to_owned()))
        };
        assert_eq!(parts("<!DOCTYPE note>"), owned("note", "", ""));
        assert_eq!(
            parts("<!DOCTYPE note SYSTEM \"a.dtd\">"),
            owned("note", "", "a.dtd")
        );
        assert_eq!(
            parts("<!DOCTYPE\nhtml\tPUBLIC '-//W3C//DTD XHTML 1.0 Strict//EN'\n'x.dtd' >"),
            owned("html", "-//W3C//DTD XHTML 1.0 Strict//EN", "x.dtd")
        );
        assert_eq!(
            parts("<!DOCTYPE r [<!ENTITY e \"SYSTEM\">]>"),
            owned("r", "", "")
        );
        assert_eq!(
            parts("<!DOCTYPE x:y SYSTEM 'it\"s'>"),
            owned("x:y", "", "it\"s")
        );
        assert_eq!(parts("<!DOCTYPE note SYSTEM>"), None);
        assert_eq!(parts("<!ELEMENT note>"), None);
    }
}
