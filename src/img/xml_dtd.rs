//! Bounded processing of an SVG image document's internal DTD subset.
//!
//! XML 1.0 (Fifth Edition, W3C Recommendation 26 November 2008,
//! https://www.w3.org/TR/2008/REC-xml-20081126/; not in the local standards
//! library, read from the official REC) lets a document declare entities and
//! attribute defaults in an internal DTD subset (§2.8). A non-validating
//! processor must process every declaration it reads there, up to the first
//! parameter-entity reference it does not read, and use them to include the
//! replacement text of internal entities, normalize attribute values and
//! supply default attribute values (§5.1). Illustrator's SVG export relies on
//! this: `<!ENTITY ns_svg "http://www.w3.org/2000/svg">` and
//! `<svg xmlns="&ns_svg;">`, sometimes `style="&st0;"`.
//!
//! roxmltree (which usvg runs with `allow_dtd`) bounds only nesting depth and
//! the references made below one top-level reference; the number of
//! top-level references is unlimited, so a document can still make its own
//! expansion quadratic or worse. TRust therefore expands the subset itself,
//! with explicit limits on total output, entity nesting, references and
//! declarations, and hands the XML parser a document without a DTD. No
//! external entity or external subset is ever fetched (secure static mode):
//! a reference in content to an external parsed entity is not included
//! (§4.4.3), one in an attribute value is a fatal error (§4.4.4).

use std::borrow::Cow;
use std::collections::HashMap;

/// Bounds on one document's expansion. The defaults cap the expanded
/// document at the SVG source limit.
#[derive(Clone, Copy, Debug)]
pub(super) struct Limits {
    /// Total bytes of the document produced, expansions included.
    pub output_bytes: usize,
    /// Entity references (general and parameter) expanded in total, which
    /// bounds the work of expansions whose replacement text is empty.
    pub references: usize,
    /// Nested entity inclusions.
    pub depth: usize,
    /// Entity and attribute definitions read from the subset.
    pub declarations: usize,
}

impl Limits {
    pub(super) const fn for_svg(output_bytes: usize) -> Self {
        Self {
            output_bytes,
            references: 1 << 20,
            depth: 16,
            declarations: 16_384,
        }
    }
}

/// The document without its document type declaration. An internal subset
/// is processed first: its entities are expanded, attribute values that
/// reference them or whose declared type is not CDATA are normalized, and
/// declared default attributes are supplied (§3.3.2, §3.3.3, §4.4).
/// `Ok(None)` when the document has no document type declaration.
pub(super) fn without_doctype(text: &str, limits: Limits) -> Result<Option<String>, String> {
    // §2.11: line breaks are normalized to #xA before anything else, so
    // attribute normalization below sees one space per line break.
    let normalized: Cow<'_, str> = if text.contains('\r') {
        Cow::Owned(text.replace("\r\n", "\n").replace('\r', "\n"))
    } else {
        Cow::Borrowed(text)
    };
    let text = normalized.as_ref();
    let Some(prologue) = prologue(text)? else {
        return Ok(None);
    };
    let mut cursor = Cursor::new(text, prologue.doctype + "<!DOCTYPE".len());
    if !cursor.space() {
        return Err(String::from("malformed document type declaration"));
    }
    cursor
        .name()
        .ok_or("document type declaration has no name")?;
    let had_space = cursor.space();
    let mut dtd = Dtd {
        standalone: prologue.standalone,
        ..Dtd::default()
    };
    if had_space && (cursor.starts_with("SYSTEM") || cursor.starts_with("PUBLIC")) {
        cursor.external_id(false)?;
        // §2.8: the external subset is never read here (§5.1 permits a
        // non-validating processor to skip it), but its presence changes
        // the Entity Declared constraint (§4.1).
        dtd.external_subset = true;
        cursor.space();
    }
    let budget = Budget {
        output: limits.output_bytes,
        references: limits.references,
        declarations: limits.declarations,
    };
    let mut parser = DtdParser {
        dtd,
        budget,
        limits,
        open_parameters: Vec::new(),
    };
    if cursor.eat("[") {
        let end = parser.declarations(text, cursor.at, Terminator::InternalSubset, 0)?;
        cursor.at = end + 1;
        cursor.space();
    }
    if !cursor.eat(">") {
        return Err(String::from("unterminated document type declaration"));
    }
    let DtdParser {
        dtd, mut budget, ..
    } = parser;
    let mut expander = Expander {
        dtd: &dtd,
        budget: &mut budget,
        limits,
        open: Vec::new(),
    };
    let mut out = String::with_capacity(text.len());
    out.push_str(&text[..prologue.doctype]);
    // Keep prologue and root tokens separated even for compact input such
    // as `<!DOCTYPE svg><svg ...>`.
    out.push('\n');
    expander.content(&text[cursor.at..], 0, &mut out)?;
    Ok(Some(out))
}

/// Where the document type declaration starts, after the XML declaration,
/// comments and processing instructions of the prologue (§2.8 [22]).
struct Prologue {
    doctype: usize,
    /// The XML declaration's `standalone="yes"` (§2.9).
    standalone: bool,
}

fn prologue(text: &str) -> Result<Option<Prologue>, String> {
    let bytes = text.as_bytes();
    let mut at = usize::from(text.starts_with('\u{feff}')) * '\u{feff}'.len_utf8();
    let mut standalone = false;
    loop {
        while bytes.get(at).is_some_and(u8::is_ascii_whitespace) {
            at += 1;
        }
        let tail = &text[at..];
        if let Some(after) = tail.strip_prefix("<!--") {
            let Some(close) = after.find("-->") else {
                return Ok(None); // the XML parser reports the syntax error
            };
            at += 4 + close + 3;
            continue;
        }
        if let Some(after) = tail.strip_prefix("<?") {
            let Some(close) = after.find("?>") else {
                return Ok(None);
            };
            let body = &after[..close];
            if let Some(declaration) = body.strip_prefix("xml")
                && declaration.starts_with(|c: char| c.is_ascii_whitespace())
            {
                standalone = xml_declaration_standalone(declaration);
            }
            at += 2 + close + 2;
            continue;
        }
        if !tail.starts_with("<!DOCTYPE") {
            return Ok(None);
        }
        return Ok(Some(Prologue {
            doctype: at,
            standalone,
        }));
    }
}

/// `standalone='yes'` in the pseudo-attributes of an XML declaration.
fn xml_declaration_standalone(declaration: &str) -> bool {
    let Some(start) = declaration.find("standalone") else {
        return false;
    };
    let rest = declaration[start + "standalone".len()..].trim_start();
    let Some(rest) = rest.strip_prefix('=') else {
        return false;
    };
    let rest = rest.trim_start();
    rest.starts_with("\"yes\"") || rest.starts_with("'yes'")
}

#[derive(Default)]
struct Dtd {
    general: HashMap<String, Entity>,
    parameter: HashMap<String, Entity>,
    /// Attribute definitions per element type, first definition binding
    /// (§3.3).
    attributes: HashMap<String, Vec<AttributeDefinition>>,
    external_subset: bool,
    /// Whether the internal subset referred to a parameter entity, which
    /// relaxes the Entity Declared well-formedness constraint (§4.1).
    parameter_references: bool,
    /// A parameter entity was referred to but not read: later entity and
    /// attribute-list declarations are not processed (§5.1).
    unread_parameter: bool,
    standalone: bool,
}

impl Dtd {
    /// §4.1 WFC: Entity Declared applies to documents without an external
    /// subset or parameter-entity references, and to standalone ones.
    fn undeclared_is_fatal(&self) -> bool {
        self.standalone || !(self.external_subset || self.parameter_references)
    }

    /// §5.1: whether entity and attribute-list declarations read now are
    /// processed.
    fn processes_declarations(&self) -> bool {
        self.standalone || !self.unread_parameter
    }
}

enum Entity {
    /// An internal entity's replacement text (§4.5): character references
    /// replaced, general-entity references left as they were.
    Internal(String),
    /// An external parsed entity, which is never read.
    External,
    /// An unparsed entity (`NDATA`), which may not be referenced (§4.1 WFC:
    /// Parsed Entity).
    Unparsed,
}

struct AttributeDefinition {
    name: String,
    /// The string type; tokenized and enumerated types are further
    /// normalized (§3.3.3).
    cdata: bool,
    /// The normalized default value, unless `#REQUIRED`/`#IMPLIED`.
    default: Option<String>,
}

struct Budget {
    output: usize,
    references: usize,
    declarations: usize,
}

impl Budget {
    fn reference(&mut self) -> Result<(), String> {
        self.references = self
            .references
            .checked_sub(1)
            .ok_or("XML entity reference limit exceeded")?;
        Ok(())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Terminator {
    /// The internal subset, closed by `]`.
    InternalSubset,
    /// A parameter entity's replacement text, which ends with the text.
    Parameter,
    /// An INCLUDE conditional section, closed by `]]>`.
    Include,
}

struct DtdParser {
    dtd: Dtd,
    budget: Budget,
    limits: Limits,
    /// Parameter entities being included, for the No Recursion constraint.
    open_parameters: Vec<String>,
}

impl DtdParser {
    /// Parse markup declarations from `start` (§2.8 [28b] intSubset and
    /// [31] extSubsetDecl). Returns the position of the terminator (`]` of
    /// the internal subset or of `]]>`) or the end of a parameter entity.
    fn declarations(
        &mut self,
        text: &str,
        start: usize,
        terminator: Terminator,
        depth: usize,
    ) -> Result<usize, String> {
        let mut cursor = Cursor::new(text, start);
        loop {
            cursor.space();
            if cursor.at >= text.len() {
                return if terminator == Terminator::Parameter {
                    Ok(cursor.at)
                } else {
                    Err(String::from("unterminated document type declaration"))
                };
            }
            match terminator {
                Terminator::InternalSubset if cursor.starts_with("]") => return Ok(cursor.at),
                Terminator::Include if cursor.starts_with("]]>") => return Ok(cursor.at),
                _ => {}
            }
            if cursor.eat("%") {
                // §2.8 [28a] DeclSep: a parameter-entity reference between
                // declarations, included as PE (§4.4.8).
                let name = cursor
                    .name()
                    .ok_or("malformed parameter-entity reference")?;
                if !cursor.eat(";") {
                    return Err(String::from("malformed parameter-entity reference"));
                }
                self.dtd.parameter_references = true;
                self.budget.reference()?;
                match self.dtd.parameter.get(name) {
                    Some(Entity::Internal(replacement)) => {
                        if self.open_parameters.iter().any(|open| open == name) {
                            return Err(format!("recursive parameter entity %{name};"));
                        }
                        if depth + 1 > self.limits.depth {
                            return Err(String::from("XML entity nesting limit exceeded"));
                        }
                        let replacement = replacement.clone();
                        self.open_parameters.push(name.to_string());
                        self.declarations(&replacement, 0, Terminator::Parameter, depth + 1)?;
                        self.open_parameters.pop();
                    }
                    // External or undeclared: not read (§5.1).
                    _ => self.dtd.unread_parameter = true,
                }
                continue;
            }
            if cursor.starts_with("<!--") {
                cursor.skip_past("-->").ok_or("unterminated comment")?;
            } else if cursor.starts_with("<?") {
                cursor
                    .skip_past("?>")
                    .ok_or("unterminated processing instruction")?;
            } else if cursor.eat("<!ENTITY") {
                self.entity_declaration(&mut cursor)?;
            } else if cursor.eat("<!ATTLIST") {
                self.attribute_list_declaration(&mut cursor)?;
            } else if cursor.eat("<!ELEMENT") {
                // [45]: a content model has no literals.
                cursor
                    .skip_past(">")
                    .ok_or("unterminated element declaration")?;
            } else if cursor.eat("<!NOTATION") {
                cursor.skip_declaration()?;
            } else if terminator != Terminator::InternalSubset && cursor.eat("<![") {
                // §3.4: conditional sections may occur in parameter-entity
                // replacement text read between declarations, though not
                // directly in the internal subset.
                cursor.space();
                if cursor.eat("INCLUDE") {
                    cursor.space();
                    if !cursor.eat("[") {
                        return Err(String::from("malformed conditional section"));
                    }
                    let end = self.declarations(text, cursor.at, Terminator::Include, depth)?;
                    cursor.at = end + "]]>".len();
                } else if cursor.eat("IGNORE") {
                    cursor.space();
                    if !cursor.eat("[") {
                        return Err(String::from("malformed conditional section"));
                    }
                    cursor.skip_ignored_section()?;
                } else {
                    return Err(String::from("malformed conditional section"));
                }
            } else {
                return Err(String::from("malformed markup declaration"));
            }
        }
    }

    fn declared(&mut self) -> Result<(), String> {
        self.budget.declarations = self
            .budget
            .declarations
            .checked_sub(1)
            .ok_or("XML declaration limit exceeded")?;
        Ok(())
    }

    /// §4.2 [70]-[76].
    fn entity_declaration(&mut self, cursor: &mut Cursor<'_>) -> Result<(), String> {
        if !cursor.space() {
            return Err(String::from("malformed entity declaration"));
        }
        let parameter = cursor.eat("%");
        if parameter && !cursor.space() {
            return Err(String::from("malformed entity declaration"));
        }
        let name = cursor.name().ok_or("entity declaration has no name")?;
        if !cursor.space() {
            return Err(String::from("malformed entity declaration"));
        }
        let entity = if cursor.starts_with("\"") || cursor.starts_with("'") {
            Entity::Internal(self.entity_value(cursor)?)
        } else {
            cursor.external_id(false)?;
            let spaced = cursor.space();
            if spaced && cursor.eat("NDATA") {
                if parameter || !cursor.space() || cursor.name().is_none() {
                    return Err(String::from("malformed unparsed entity declaration"));
                }
                Entity::Unparsed
            } else {
                Entity::External
            }
        };
        cursor.space();
        if !cursor.eat(">") {
            return Err(String::from("malformed entity declaration"));
        }
        self.declared()?;
        if self.dtd.processes_declarations() {
            // §4.2: the first declaration of an entity is binding.
            let table = if parameter {
                &mut self.dtd.parameter
            } else {
                &mut self.dtd.general
            };
            table.entry(name.to_string()).or_insert(entity);
        }
        Ok(())
    }

    /// §2.3 [9] EntityValue and §4.5: character references are replaced
    /// now, general-entity references are bypassed (§4.4.7), and a
    /// parameter-entity reference in the internal subset is a fatal error
    /// (§2.8 WFC: PEs in Internal Subset).
    fn entity_value(&mut self, cursor: &mut Cursor<'_>) -> Result<String, String> {
        let literal = cursor.quoted().ok_or("unterminated entity value")?;
        let mut value = String::with_capacity(literal.len());
        let mut rest = literal;
        while let Some(index) = rest.find(['&', '%']) {
            value.push_str(&rest[..index]);
            let tail = &rest[index..];
            if tail.starts_with('%') {
                return Err(String::from(
                    "parameter-entity reference inside an internal-subset declaration",
                ));
            }
            if let Some((character, length)) = character_reference(tail)? {
                value.push(character);
                rest = &tail[length..];
            } else {
                let length = entity_reference_length(tail).ok_or("malformed entity reference")?;
                value.push_str(&tail[..length]);
                rest = &tail[length..];
            }
        }
        value.push_str(rest);
        Ok(value)
    }

    /// §3.3 [52]-[60].
    fn attribute_list_declaration(&mut self, cursor: &mut Cursor<'_>) -> Result<(), String> {
        if !cursor.space() {
            return Err(String::from("malformed attribute-list declaration"));
        }
        let element = cursor
            .name()
            .ok_or("attribute-list declaration has no element type")?
            .to_string();
        loop {
            let spaced = cursor.space();
            if cursor.eat(">") {
                return Ok(());
            }
            if !spaced {
                return Err(String::from("malformed attribute-list declaration"));
            }
            let name = cursor
                .name()
                .ok_or("malformed attribute definition")?
                .to_string();
            if !cursor.space() {
                return Err(String::from("malformed attribute definition"));
            }
            let cdata = attribute_type(cursor)?;
            if !cursor.space() {
                return Err(String::from("malformed attribute definition"));
            }
            let default = if cursor.eat("#REQUIRED") || cursor.eat("#IMPLIED") {
                None
            } else {
                if cursor.eat("#FIXED") && !cursor.space() {
                    return Err(String::from("malformed attribute default"));
                }
                let raw = cursor.quoted().ok_or("unterminated attribute default")?;
                // §4.4: a default value is a reference in an attribute
                // value; the entities it uses are declared before it (§4.1).
                let mut value = String::new();
                let mut expander = Expander {
                    dtd: &self.dtd,
                    budget: &mut self.budget,
                    limits: self.limits,
                    open: Vec::new(),
                };
                expander.attribute_value(raw, 0, &mut value)?;
                if !cdata {
                    value = collapse_spaces(&value);
                }
                Some(value)
            };
            self.declared()?;
            if self.dtd.processes_declarations() {
                let definitions = self.dtd.attributes.entry(element.clone()).or_default();
                if !definitions.iter().any(|definition| definition.name == name) {
                    definitions.push(AttributeDefinition {
                        name,
                        cdata,
                        default,
                    });
                }
            }
        }
    }
}

/// §3.3.1 [54]-[59]: whether the type is CDATA.
fn attribute_type(cursor: &mut Cursor<'_>) -> Result<bool, String> {
    if cursor.eat("CDATA") {
        return Ok(true);
    }
    for keyword in [
        "IDREFS", "IDREF", "ID", "ENTITIES", "ENTITY", "NMTOKENS", "NMTOKEN",
    ] {
        if cursor.eat(keyword) {
            return Ok(false);
        }
    }
    if cursor.eat("NOTATION") && !cursor.space() {
        return Err(String::from("malformed notation attribute type"));
    }
    if cursor.eat("(") {
        cursor
            .skip_past(")")
            .ok_or("unterminated enumerated attribute type")?;
        return Ok(false);
    }
    Err(String::from("malformed attribute type"))
}

/// Expands general entities in the document after its DTD.
struct Expander<'a> {
    dtd: &'a Dtd,
    budget: &'a mut Budget,
    limits: Limits,
    /// General entities being included (§4.1 WFC: No Recursion).
    open: Vec<&'a str>,
}

impl<'a> Expander<'a> {
    fn push(&mut self, out: &mut String, text: &str) -> Result<(), String> {
        self.budget.output = self
            .budget
            .output
            .checked_sub(text.len())
            .ok_or("XML entity expansion exceeds the document size limit")?;
        out.push_str(text);
        Ok(())
    }

    fn push_char(&mut self, out: &mut String, character: char) -> Result<(), String> {
        self.push(out, character.encode_utf8(&mut [0; 4]))
    }

    /// Resolve a general entity reference to the entity to include, or
    /// `None` when nothing is included.
    fn entity(&mut self, name: &str, depth: usize) -> Result<Option<&'a Entity>, String> {
        self.budget.reference()?;
        let Some(entity) = self.dtd.general.get(name) else {
            return if self.dtd.undeclared_is_fatal() {
                Err(format!("undeclared entity &{name};"))
            } else {
                // Declared, if at all, where this processor does not read.
                Ok(None)
            };
        };
        if self.open.contains(&name) {
            return Err(format!("recursive entity &{name};"));
        }
        if depth + 1 > self.limits.depth {
            return Err(String::from("XML entity nesting limit exceeded"));
        }
        Ok(Some(entity))
    }

    /// Copy `text` (document content, or an entity's replacement text being
    /// included in content), including internal entities in place (§4.4.2)
    /// and rewriting start tags. An entity's replacement text must match
    /// `content` (§4.3.2): elements it opens close within it.
    fn content(&mut self, text: &'a str, depth: usize, out: &mut String) -> Result<(), String> {
        let mut open_elements = 0usize;
        let mut rest = text;
        while let Some(index) = rest.find(['<', '&']) {
            self.push(out, &rest[..index])?;
            let tail = &rest[index..];
            if tail.starts_with('&') {
                let length = if tail.starts_with("&#") {
                    let length = tail.find(';').map_or(tail.len(), |end| end + 1);
                    self.push(out, &tail[..length])?;
                    length
                } else if let Some(length) = entity_reference_length(tail) {
                    let name = &tail[1..length - 1];
                    if is_predefined(name) {
                        self.push(out, &tail[..length])?;
                    } else {
                        match self.entity(name, depth)? {
                            Some(Entity::Internal(replacement)) => {
                                self.open.push(name_ref(self.dtd, name));
                                self.content(replacement, depth + 1, out)?;
                                self.open.pop();
                            }
                            // §4.4.3: an external parsed entity need not be
                            // included by a non-validating processor.
                            Some(Entity::External) | None => {}
                            Some(Entity::Unparsed) => {
                                return Err(format!("reference to unparsed entity &{name};"));
                            }
                        }
                    }
                    length
                } else {
                    // Not a reference: the XML parser reports it.
                    self.push(out, "&")?;
                    1
                };
                rest = &tail[length..];
                continue;
            }
            let length = if tail.starts_with("<!--") {
                section_length(tail, "-->")
            } else if tail.starts_with("<![CDATA[") {
                section_length(tail, "]]>")
            } else if tail.starts_with("<?") {
                section_length(tail, "?>")
            } else if tail.starts_with("</") {
                if depth > 0 {
                    open_elements = open_elements
                        .checked_sub(1)
                        .ok_or("entity replacement text closes an element it did not open")?;
                }
                section_length(tail, ">")
            } else if let Some((length, empty)) = self.start_tag(tail, depth, out)? {
                if !empty {
                    open_elements += 1;
                }
                rest = &tail[length..];
                continue;
            } else {
                1
            };
            if depth > 0 && length > tail.len() {
                return Err(String::from("markup crosses an entity boundary"));
            }
            let length = length.min(tail.len());
            self.push(out, &tail[..length])?;
            rest = &tail[length..];
        }
        self.push(out, rest)?;
        if depth > 0 && open_elements != 0 {
            return Err(String::from(
                "entity replacement text leaves an element unclosed",
            ));
        }
        Ok(())
    }

    /// Rewrite the start tag (§3.1 [40]/[44]) at the start of `tail`:
    /// attribute values are normalized when they reference a declared
    /// entity or have a non-CDATA declared type, and declared defaults are
    /// supplied. Returns the tag's length and whether it is an empty-element
    /// tag, or `None` when `tail` does not begin with a tag the XML parser
    /// would accept (it then reports the error itself).
    fn start_tag(
        &mut self,
        tail: &'a str,
        depth: usize,
        out: &mut String,
    ) -> Result<Option<(usize, bool)>, String> {
        let mut cursor = Cursor::new(tail, 1);
        let Some(element) = cursor.name() else {
            return Ok(None);
        };
        let definitions = self.dtd.attributes.get(element);
        let mut tag = String::new();
        tag.push('<');
        tag.push_str(element);
        let mut specified: Vec<&str> = Vec::new();
        let empty = loop {
            let spaced = cursor.space();
            if cursor.eat("/>") {
                break true;
            }
            if cursor.eat(">") {
                break false;
            }
            if !spaced {
                return Ok(None);
            }
            let Some(name) = cursor.name() else {
                return Ok(None);
            };
            cursor.space();
            if !cursor.eat("=") {
                return Ok(None);
            }
            cursor.space();
            let quote = &tail[cursor.at..cursor.at.saturating_add(1).min(tail.len())];
            let Some(raw) = cursor.quoted() else {
                return Ok(None);
            };
            let definition =
                definitions.and_then(|list| list.iter().find(|definition| definition.name == name));
            let cdata = definition.is_none_or(|definition| definition.cdata);
            tag.push(' ');
            tag.push_str(name);
            tag.push('=');
            if cdata && !references_declared_entity(raw) {
                tag.push_str(quote);
                tag.push_str(raw);
                tag.push_str(quote);
            } else {
                let mut value = String::new();
                self.attribute_value(raw, depth, &mut value)?;
                if !cdata {
                    value = collapse_spaces(&value);
                }
                push_attribute_value(&mut tag, &value);
            }
            specified.push(name);
        };
        // §3.3.2: a declared default is reported for an unspecified attribute.
        for definition in definitions.into_iter().flatten() {
            if let Some(default) = &definition.default
                && !specified.contains(&definition.name.as_str())
            {
                tag.push(' ');
                tag.push_str(&definition.name);
                tag.push('=');
                push_attribute_value(&mut tag, default);
            }
        }
        tag.push_str(if empty { "/>" } else { ">" });
        self.push(out, &tag)?;
        Ok(Some((cursor.at, empty)))
    }

    /// §3.3.3 steps 3-4 for one attribute value or replacement text.
    fn attribute_value(&mut self, raw: &str, depth: usize, out: &mut String) -> Result<(), String> {
        let mut rest = raw;
        while let Some(index) = rest.find(['&', '<', '\t', '\n', '\r', ' ']) {
            self.push(out, &rest[..index])?;
            let tail = &rest[index..];
            let length = match tail.as_bytes()[0] {
                b'<' => return Err(String::from("'<' in an attribute value")),
                b'&' => {
                    if let Some((character, length)) = character_reference(tail)? {
                        self.push_char(out, character)?;
                        length
                    } else {
                        let length =
                            entity_reference_length(tail).ok_or("malformed entity reference")?;
                        let name = &tail[1..length - 1];
                        if let Some(character) = predefined(name) {
                            self.push_char(out, character)?;
                        } else {
                            match self.entity(name, depth)? {
                                Some(Entity::Internal(replacement)) => {
                                    self.open.push(name_ref(self.dtd, name));
                                    self.attribute_value(replacement, depth + 1, out)?;
                                    self.open.pop();
                                }
                                None => {}
                                // §4.4.4: forbidden in attribute values.
                                Some(Entity::External) => {
                                    return Err(format!(
                                        "external entity &{name}; in an attribute value"
                                    ));
                                }
                                Some(Entity::Unparsed) => {
                                    return Err(format!("reference to unparsed entity &{name};"));
                                }
                            }
                        }
                        length
                    }
                }
                // White space becomes a space character.
                _ => {
                    self.push(out, " ")?;
                    1
                }
            };
            rest = &tail[length..];
        }
        self.push(out, rest)
    }
}

/// The DTD's own copy of an entity name, so the open-entity stack borrows
/// from the DTD rather than from a replacement text being scanned.
fn name_ref<'a>(dtd: &'a Dtd, name: &str) -> &'a str {
    dtd.general
        .get_key_value(name)
        .map_or("", |(key, _)| key.as_str())
}

/// Whether an attribute value refers to an entity other than the predefined
/// ones, which the XML parser can no longer resolve once the DTD is gone.
fn references_declared_entity(raw: &str) -> bool {
    let mut rest = raw;
    while let Some(index) = rest.find('&') {
        let tail = &rest[index..];
        match entity_reference_length(tail) {
            Some(length) if !tail.starts_with("&#") && !is_predefined(&tail[1..length - 1]) => {
                return true;
            }
            _ => rest = &tail[1..],
        }
    }
    false
}

/// Write a normalized value as a double-quoted attribute value that the XML
/// parser normalizes back to the same string: references keep `&`, `<`,
/// `"` and the white space characters other than space.
fn push_attribute_value(out: &mut String, value: &str) {
    out.push('"');
    for character in value.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '"' => out.push_str("&quot;"),
            '\t' => out.push_str("&#9;"),
            '\n' => out.push_str("&#10;"),
            '\r' => out.push_str("&#13;"),
            other => out.push(other),
        }
    }
    out.push('"');
}

/// §3.3.3: tokenized values drop leading and trailing spaces and collapse
/// runs of spaces.
fn collapse_spaces(value: &str) -> String {
    value
        .split(' ')
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// §4.6: the five entities every processor recognizes.
fn predefined(name: &str) -> Option<char> {
    Some(match name {
        "lt" => '<',
        "gt" => '>',
        "amp" => '&',
        "apos" => '\'',
        "quot" => '"',
        _ => return None,
    })
}

fn is_predefined(name: &str) -> bool {
    predefined(name).is_some()
}

/// §4.1 [66] CharRef at the start of `text`, with its length; `Ok(None)`
/// when `text` does not start with `&#`. WFC: Legal Character.
fn character_reference(text: &str) -> Result<Option<(char, usize)>, String> {
    let Some(digits) = text.strip_prefix("&#") else {
        return Ok(None);
    };
    let end = digits.find(';').ok_or("malformed character reference")?;
    let (digits, radix) = match digits[..end].strip_prefix('x') {
        Some(hex) => (hex, 16),
        None => (&digits[..end], 10),
    };
    if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
        return Err(String::from("malformed character reference"));
    }
    let character = u32::from_str_radix(digits, radix)
        .ok()
        .and_then(char::from_u32)
        .filter(|&c| is_xml_char(c))
        .ok_or("character reference to an illegal character")?;
    Ok(Some((character, 2 + end + 1)))
}

/// §2.2 [2] Char.
fn is_xml_char(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\r' | '\u{20}'..='\u{d7ff}' | '\u{e000}'..='\u{fffd}' | '\u{10000}'..='\u{10ffff}')
}

/// The length of a `&Name;` reference at the start of `text`.
fn entity_reference_length(text: &str) -> Option<usize> {
    let name = name_length(text.strip_prefix('&')?)?;
    text[1 + name..].starts_with(';').then_some(1 + name + 1)
}

/// The length of the XML Name (§2.3 [5]) at the start of `text`.
fn name_length(text: &str) -> Option<usize> {
    let mut chars = text.char_indices();
    let (_, first) = chars.next()?;
    if !is_name_start(first) {
        return None;
    }
    Some(
        chars
            .find(|&(_, c)| !is_name_char(c))
            .map_or(text.len(), |(index, _)| index),
    )
}

/// §2.3 [4] NameStartChar.
fn is_name_start(c: char) -> bool {
    matches!(c,
        ':' | 'A'..='Z' | '_' | 'a'..='z'
        | '\u{c0}'..='\u{d6}' | '\u{d8}'..='\u{f6}' | '\u{f8}'..='\u{2ff}'
        | '\u{370}'..='\u{37d}' | '\u{37f}'..='\u{1fff}' | '\u{200c}'..='\u{200d}'
        | '\u{2070}'..='\u{218f}' | '\u{2c00}'..='\u{2fef}' | '\u{3001}'..='\u{d7ff}'
        | '\u{f900}'..='\u{fdcf}' | '\u{fdf0}'..='\u{fffd}' | '\u{10000}'..='\u{effff}')
}

/// §2.3 [4a] NameChar.
fn is_name_char(c: char) -> bool {
    is_name_start(c)
        || matches!(c, '-' | '.' | '0'..='9' | '\u{b7}' | '\u{300}'..='\u{36f}' | '\u{203f}'..='\u{2040}')
}

/// The length of a markup construct opened at the start of `text` and
/// closed by `close`, or more than `text.len()` when it is unterminated.
fn section_length(text: &str, close: &str) -> usize {
    text[1..]
        .find(close)
        .map_or(usize::MAX, |index| 1 + index + close.len())
}

/// A position in DTD text.
struct Cursor<'t> {
    text: &'t str,
    at: usize,
}

impl<'t> Cursor<'t> {
    fn new(text: &'t str, at: usize) -> Self {
        Self { text, at }
    }

    fn rest(&self) -> &'t str {
        self.text.get(self.at..).unwrap_or_default()
    }

    fn starts_with(&self, prefix: &str) -> bool {
        self.rest().starts_with(prefix)
    }

    fn eat(&mut self, prefix: &str) -> bool {
        let found = self.starts_with(prefix);
        if found {
            self.at += prefix.len();
        }
        found
    }

    /// §2.3 [3] S; whether any white space was consumed.
    fn space(&mut self) -> bool {
        let skipped = self
            .rest()
            .find(|c: char| !matches!(c, ' ' | '\t' | '\n' | '\r'))
            .unwrap_or(self.rest().len());
        self.at += skipped;
        skipped > 0
    }

    fn name(&mut self) -> Option<&'t str> {
        let length = name_length(self.rest())?;
        let name = &self.rest()[..length];
        self.at += length;
        Some(name)
    }

    /// A single- or double-quoted literal; returns its content.
    fn quoted(&mut self) -> Option<&'t str> {
        let rest = self.rest();
        let quote = rest.chars().next().filter(|c| matches!(c, '"' | '\''))?;
        let end = rest[1..].find(quote)?;
        self.at += 1 + end + 1;
        Some(&rest[1..1 + end])
    }

    fn skip_past(&mut self, close: &str) -> Option<()> {
        let index = self.rest().find(close)?;
        self.at += index + close.len();
        Some(())
    }

    /// §4.2.2 [75] ExternalID (or [83] PublicID when `public_only` allows a
    /// missing system literal).
    fn external_id(&mut self, public_only: bool) -> Result<(), String> {
        if self.eat("SYSTEM") {
            if !self.space() || self.quoted().is_none() {
                return Err(String::from("malformed system identifier"));
            }
        } else if self.eat("PUBLIC") {
            if !self.space() || self.quoted().is_none() {
                return Err(String::from("malformed public identifier"));
            }
            let at = self.at;
            if !(self.space() && self.quoted().is_some()) {
                if !public_only {
                    return Err(String::from("public identifier without a system literal"));
                }
                self.at = at;
            }
        } else {
            return Err(String::from("malformed external identifier"));
        }
        Ok(())
    }

    /// Skip a declaration whose literals may contain `>`.
    fn skip_declaration(&mut self) -> Result<(), String> {
        loop {
            let rest = self.rest();
            let Some(index) = rest.find(['>', '"', '\'']) else {
                return Err(String::from("unterminated markup declaration"));
            };
            self.at += index;
            if self.eat(">") {
                return Ok(());
            }
            self.quoted()
                .ok_or("unterminated literal in a markup declaration")?;
        }
    }

    /// §3.4 [63]-[65]: an ignored section's contents, nested sections
    /// included, through its closing `]]>`.
    fn skip_ignored_section(&mut self) -> Result<(), String> {
        let mut depth = 1usize;
        loop {
            let rest = self.rest();
            let open = rest.find("<![");
            let close = rest.find("]]>").ok_or("unterminated conditional section")?;
            match open {
                Some(open) if open < close => {
                    self.at += open + 3;
                    depth += 1;
                }
                _ => {
                    self.at += close + 3;
                    depth -= 1;
                    if depth == 0 {
                        return Ok(());
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expand(text: &str) -> Result<Option<String>, String> {
        without_doctype(text, Limits::for_svg(1 << 20))
    }

    fn parsed(text: &str) -> String {
        let expanded = expand(text).unwrap().unwrap();
        roxmltree::Document::parse(&expanded).expect(&expanded);
        expanded
    }

    #[test]
    fn documents_without_a_doctype_are_untouched() {
        assert_eq!(expand("<svg/>").unwrap(), None);
        assert_eq!(expand("<?xml version='1.0'?><!--x--><svg/>").unwrap(), None);
    }

    #[test]
    fn illustrator_namespace_and_style_entities_expand() {
        let text = r#"<?xml version="1.0" encoding="utf-8"?>
<!-- Generator: Adobe Illustrator 24.0.0, SVG Export Plug-In  -->
<!DOCTYPE svg PUBLIC "-//W3C//DTD SVG 1.1//EN" "http://www.w3.org/Graphics/SVG/1.1/DTD/svg11.dtd" [
	<!ENTITY ns_extend "http://ns.adobe.com/Extensibility/1.0/">
	<!ENTITY ns_svg "http://www.w3.org/2000/svg">
	<!ENTITY ns_xlink "http://www.w3.org/1999/xlink">
	<!ENTITY st0 "fill:#FF0000;stroke:#000000;">
]>
<svg version="1.1" xmlns:x="&ns_extend;" xmlns="&ns_svg;" xmlns:xlink="&ns_xlink;" width="10" height="10">
<rect style="&st0;" width="10" height="10"/></svg>"#;
        let expanded = parsed(text);
        assert!(!expanded.contains("<!DOCTYPE"), "{expanded}");
        let document = roxmltree::Document::parse(&expanded).unwrap();
        let root = document.root_element();
        assert_eq!(
            root.tag_name().namespace(),
            Some("http://www.w3.org/2000/svg")
        );
        let rect = root.first_element_child().unwrap();
        assert_eq!(
            rect.attribute("style"),
            Some("fill:#FF0000;stroke:#000000;")
        );
    }

    #[test]
    fn content_entities_follow_the_appendix_d_example() {
        // XML 1.0 Appendix D: character references in the literal are
        // replaced at declaration, the rest when the entity is included.
        let text = r#"<!DOCTYPE test [
<!ENTITY example "<p>An ampersand (&#38;#38;) may be escaped
numerically (&#38;#38;#38;) or with a general entity
(&amp;amp;).</p>" >
]><test>&example;</test>"#;
        let expanded = parsed(text);
        let document = roxmltree::Document::parse(&expanded).unwrap();
        let p = document.root_element().first_element_child().unwrap();
        assert_eq!(
            p.text(),
            Some(
                "An ampersand (&) may be escaped\nnumerically (&#38;) or with a general entity\n(&amp;)."
            )
        );
        // §4.5: parameter entities in a DeclSep are included as declarations.
        let tricky = r#"<?xml version='1.0'?>
<!DOCTYPE test [
<!ELEMENT test (#PCDATA) >
<!ENTITY % xx '&#37;zz;'>
<!ENTITY % zz '&#60;!ENTITY tricky "error-prone" >' >
%xx;
]>
<test>This sample shows a &tricky; method.</test>"#;
        let expanded = parsed(tricky);
        assert!(
            expanded.contains("shows a error-prone method"),
            "{expanded}"
        );
    }

    #[test]
    fn attribute_values_are_normalized_with_their_declared_type() {
        // §3.3.3's examples: an entity's white space becomes spaces, while
        // character references keep the referenced character.
        let text = "<!DOCTYPE t [\n<!ENTITY d \"&#xD;\">\n<!ENTITY a \"&#xA;\">\n<!ENTITY da \"&#xD;&#xA;\">\n\
            <!ATTLIST t n NMTOKENS #IMPLIED>\n]>\
            <t c=\"&d;&d;A&a;&#x20;&a;B&da;\" n=\"&d;&d;A&a;&#x20;&a;B&da;\" q='&#34;'/>";
        let expanded = parsed(text);
        let document = roxmltree::Document::parse(&expanded).unwrap();
        let root = document.root_element();
        assert_eq!(root.attribute("c"), Some("  A   B  "));
        assert_eq!(root.attribute("n"), Some("A B"));
        assert_eq!(root.attribute("q"), Some("\""));
        // §4.6: `<` reaches an attribute value only double-escaped; a
        // replacement text containing `<` itself is fatal there.
        let entity_quote =
            parsed("<!DOCTYPE t [<!ENTITY q 'say \"hi\" &amp; &#38;#60;'>]><t a=\"&q;\"/>");
        let document = roxmltree::Document::parse(&entity_quote).unwrap();
        assert_eq!(
            document.root_element().attribute("a"),
            Some("say \"hi\" & <")
        );
    }

    #[test]
    fn declared_defaults_are_supplied() {
        // §3.3.2: the first definition binds; specified attributes win.
        let text = r#"<!DOCTYPE svg [
<!ENTITY ns "http://www.w3.org/2000/svg">
<!ATTLIST svg xmlns CDATA #FIXED "&ns;" fill CDATA "red">
<!ATTLIST svg fill CDATA "blue" id ID #IMPLIED>
<!ATTLIST rect fill CDATA 'green'>
]><svg><rect/><rect fill="black"/></svg>"#;
        let expanded = parsed(text);
        let document = roxmltree::Document::parse(&expanded).unwrap();
        let root = document.root_element();
        assert_eq!(
            root.tag_name().namespace(),
            Some("http://www.w3.org/2000/svg")
        );
        assert_eq!(root.attribute("fill"), Some("red"));
        assert_eq!(root.attribute("id"), None);
        let fills: Vec<_> = root
            .children()
            .filter(|node| node.is_element())
            .map(|node| node.attribute("fill"))
            .collect();
        assert_eq!(fills, [Some("green"), Some("black")]);
    }

    #[test]
    fn external_entities_are_never_read() {
        // §4.4.3: an external entity in content is not included; §4.4.4:
        // in an attribute value it is fatal.
        let text = r#"<!DOCTYPE t [<!ENTITY ext SYSTEM "file:///etc/passwd">]><t>a&ext;b</t>"#;
        let expanded = parsed(text);
        assert!(expanded.contains("<t>ab</t>"), "{expanded}");
        assert!(expand(r#"<!DOCTYPE t [<!ENTITY ext SYSTEM "x">]><t a="&ext;"/>"#).is_err());
        assert!(expand(r#"<!DOCTYPE t [<!ENTITY u SYSTEM "x" NDATA gif>]><t>&u;</t>"#).is_err());
        // §5.1: declarations after an unread parameter entity are skipped,
        // and with it undeclared references are no longer fatal.
        let text = r#"<!DOCTYPE t [<!ENTITY % ext SYSTEM "x.dtd"> %ext; <!ENTITY late "no">]><t>&late;</t>"#;
        assert!(parsed(text).contains("<t></t>"));
        let standalone = r#"<?xml version="1.0" standalone="yes"?><!DOCTYPE t [<!ENTITY % ext SYSTEM "x.dtd"> %ext; <!ENTITY late "yes">]><t>&late;</t>"#;
        assert!(parsed(standalone).contains("<t>yes</t>"));
    }

    #[test]
    fn well_formedness_errors_are_reported() {
        for text in [
            // §4.1 WFC: Entity Declared (internal subset only).
            "<!DOCTYPE t []><t>&nope;</t>",
            // WFC: No Recursion, direct and indirect.
            "<!DOCTYPE t [<!ENTITY a '&a;'>]><t>&a;</t>",
            "<!DOCTYPE t [<!ENTITY a '&b;'><!ENTITY b '&a;'>]><t a='&a;'/>",
            // WFC: No < in Attribute Values.
            "<!DOCTYPE t [<!ENTITY lt2 '<'>]><t a='&lt2;'/>",
            // WFC: PEs in Internal Subset.
            "<!DOCTYPE t [<!ENTITY % p 'x'><!ENTITY e '%p;'>]><t/>",
            // §4.3.2: replacement text must be properly nested content.
            "<!DOCTYPE t [<!ENTITY s '<a>'><!ENTITY e '</a>'>]><t>&s;&e;</t>",
            // WFC: Legal Character.
            "<!DOCTYPE t [<!ENTITY z '&#0;'>]><t/>",
            "<!DOCTYPE t [<!ENTITY x 'y'>",
        ] {
            assert!(expand(text).is_err(), "{text}");
        }
        // An external subset relaxes Entity Declared: the reference may be
        // declared in the subset this processor does not read.
        let text = r#"<!DOCTYPE t SYSTEM "t.dtd" [<!ENTITY a "x">]><t>&a;&b;</t>"#;
        assert!(parsed(text).contains("<t>x</t>"));
    }

    #[test]
    fn entity_expansion_attacks_are_bounded() {
        // Billion laughs: exponential expansion from nested references.
        let mut text = String::from("<!DOCTYPE lolz [<!ENTITY lol0 \"lol\">");
        for level in 1..10 {
            let previous = format!("&lol{};", level - 1);
            text.push_str(&format!("<!ENTITY lol{level} \"{}\">", previous.repeat(10)));
        }
        text.push_str("]><lolz>&lol9;</lolz>");
        let error = expand(&text).unwrap_err();
        assert!(
            error.contains("limit") || error.contains("exceeds"),
            "{error}"
        );
        // Quadratic blowup: one large entity, referenced many times at the
        // top level (roxmltree's per-reference limits do not bound this).
        let big = "x".repeat(64 * 1024);
        let text = format!(
            "<!DOCTYPE t [<!ENTITY big \"{big}\">]><t>{}</t>",
            "&big;".repeat(4096)
        );
        assert!(expand(&text).is_err());
        // Empty expansions still count references.
        let mut text = String::from("<!DOCTYPE t [<!ENTITY e0 \"\">");
        for level in 1..8 {
            let previous = format!("&e{};", level - 1);
            text.push_str(&format!("<!ENTITY e{level} \"{}\">", previous.repeat(10)));
        }
        text.push_str("]><t>&e7;</t>");
        assert!(expand(&text).unwrap_err().contains("reference limit"));
        // Deep chains stop at the nesting limit.
        let mut text = String::from("<!DOCTYPE t [<!ENTITY d0 \"x\">");
        for level in 1..40 {
            text.push_str(&format!("<!ENTITY d{level} \"&d{};\">", level - 1));
        }
        text.push_str("]><t>&d39;</t>");
        assert!(expand(&text).unwrap_err().contains("nesting"));
    }

    #[test]
    fn markup_sections_and_line_breaks_survive() {
        let text = "<!DOCTYPE t [\r\n<!ENTITY e 'v'>\r\n<!-- ] > -->\r\n<?pi ]>?>\r\n]>\r\n\
                    <t><![CDATA[&e; <x>]]><!-- &e; --><?p &e;?>&e;&lt;</t>";
        let expanded = parsed(text);
        assert!(expanded.contains("<![CDATA[&e; <x>]]>"), "{expanded}");
        assert!(expanded.contains("<!-- &e; -->"), "{expanded}");
        assert!(expanded.ends_with("v&lt;</t>"), "{expanded}");
        assert!(!expanded.contains('\r'));
    }
}
