//! HTML #dom-domparser-parsefromstring, XML 1.0 Fifth Edition §2–4,
//! Namespaces in XML 1.0 Third Edition §6. XML is never sent to html5ever.
//!
//! roxmltree validates expanded names, duplicate attributes, entities and
//! document structure before we publish anything. The event reader preserves
//! CDATA boundaries and processing instructions which roxmltree coalesces.
//! Neither parser resolves external resources; entity expansion is bounded.

use super::{Attribute, DocumentTypeInfo, Dom, Namespace, NodeData, NodeId, Prefix, QualName};
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
