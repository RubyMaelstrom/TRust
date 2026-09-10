//! HTML #dom-domparser-parsefromstring, XML 1.0 Fifth Edition §2–4,
//! Namespaces in XML 1.0 Third Edition §6. XML is never sent to html5ever.
//!
//! roxmltree validates expanded names, duplicate attributes, entities and
//! document structure before we publish anything. The event reader preserves
//! CDATA boundaries and processing instructions which roxmltree coalesces.
//! Neither parser resolves external resources; entity expansion is bounded.

use super::{Attribute, Dom, Namespace, NodeData, NodeId, Prefix, QualName};
use xml::reader::{ParserConfig, XmlEvent};

const XMLNS: &str = "http://www.w3.org/2000/xmlns/";
const PARSERERROR: &str = "http://www.mozilla.org/newlayout/xml/parsererror.xml";

impl Dom {
    pub fn parse_xml_document_into(&mut self, source: &str, content_type: &str) -> NodeId {
        let doc = self.create_document(content_type);
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
        doc
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
                | XmlEvent::Comment(text) => text.len(),
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
                    let element = self.new_node(NodeData::Element {
                        name: QualName::new(
                            name.prefix.as_deref().map(Prefix::from),
                            Namespace::from(name.namespace.as_deref().unwrap_or("")),
                            name.local_name.into(),
                        ),
                        attrs,
                        template_contents: None,
                    });
                    self.append(parent, element);
                    parents.push(element);
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
