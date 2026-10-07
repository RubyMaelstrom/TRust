//! Character encodings of documents created from bytes.
//!
//! A navigated HTML document is decoded with the encoding chosen by HTML's
//! encoding sniffing algorithm, and that encoding becomes the document's
//! character encoding (`document.characterSet`). Sources, from the local
//! standards library:
//!
//! - WHATWG HTML (snapshot e5071a20c856) §13.2.3 "The input byte stream":
//!   #encoding-sniffing-algorithm, #prescan-a-byte-stream-to-determine-its-encoding,
//!   #concept-get-attributes-when-sniffing, #concept-get-xml-encoding-when-sniffing
//!   and #change-the-encoding, which the tree builder's "in head" `meta` rules
//!   invoke (#meta-charset-during-parse); and
//!   #algorithm-for-extracting-a-character-encoding-from-a-meta-element.
//! - WHATWG Encoding (snapshot a985b62a9b45): #concept-encoding-get, #decode and
//!   #bom-sniff. `encoding_rs` implements the label table and every decoder.
//! - WHATWG MIME Sniffing (39aa53511b13) #parse-a-mime-type and Fetch
//!   (394d20d144ed) #concept-header-extract-mime-type for the transport-layer
//!   `charset` parameter.
//! - XML 1.0 Appendix F and RFC 7303 §3 for XML documents, which HTML leaves
//!   to XML (#the-input-byte-stream).
//!
//! The whole response is in memory when TRust decodes it, so "change the
//! encoding" re-decodes from memory instead of restarting the navigation, as
//! the standard recommends.

use std::borrow::Cow;
use std::cell::{Cell, Ref, RefCell};

use encoding_rs::{Encoding, UTF_8, UTF_16BE, UTF_16LE, WINDOWS_1252, X_USER_DEFINED};
use html5ever::interface::{ElementFlags, NodeOrText, QuirksMode, TreeSink};
use html5ever::tendril::StrTendril;
use html5ever::tokenizer::{BufferQueue, Tokenizer, TokenizerOpts};
use html5ever::tree_builder::{TreeBuilder, TreeBuilderOpts};
use html5ever::{Attribute, QualName, TokenizerResult, local_name, ns};

/// HTML #encoding-sniffing-algorithm step 3 encourages a prescan of only the
/// first 1024 bytes (the authoring limit for declarations, #charset1024).
const PRESCAN_BYTES: usize = 1024;

/// HTML #concept-encoding-confidence for a byte stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Confidence {
    Tentative,
    Certain,
}

/// The parser a navigated resource is loaded with (HTML #loading-documents).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DocumentKind {
    /// #read-html: the HTML parser with encoding sniffing.
    Html,
    /// #read-xml: XML's own encoding determination.
    Xml,
    /// #read-text: the HTML parser in PLAINTEXT state.
    Text,
}

impl DocumentKind {
    /// The kind of document a navigation response with this MIME type
    /// essence creates. XML MIME types follow MIME Sniffing #xml-mime-type.
    pub(crate) fn for_essence(essence: &str) -> Self {
        if essence.eq_ignore_ascii_case("text/html") {
            Self::Html
        } else if is_xml_essence(essence) {
            Self::Xml
        } else {
            Self::Text
        }
    }
}

fn is_xml_essence(essence: &str) -> bool {
    let essence = essence.trim();
    essence.eq_ignore_ascii_case("text/xml")
        || essence.eq_ignore_ascii_case("application/xml")
        || essence
            .get(essence.len().saturating_sub(4)..)
            .is_some_and(|suffix| suffix.eq_ignore_ascii_case("+xml"))
}

/// Out-of-band inputs to the encoding sniffing algorithm.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct SniffContext<'a> {
    /// The response's `Content-Type` header value (transport-layer metadata).
    pub content_type: &'a str,
    /// Step 6: the container document's encoding, given only when it is same
    /// origin with the new document.
    pub container: Option<&'static Encoding>,
    /// Step 7 autodetection: a local file may be detected as UTF-8.
    pub local_file: bool,
}

/// A decoded document and the encoding that becomes its character encoding.
#[derive(Debug)]
pub(crate) struct DecodedDocument {
    pub text: String,
    pub encoding: &'static Encoding,
}

/// Decode a navigation response's body for the document `kind` creates.
pub(crate) fn decode_document(
    kind: DocumentKind,
    bytes: &[u8],
    context: SniffContext<'_>,
) -> DecodedDocument {
    match kind {
        DocumentKind::Html => decode_html(bytes, context),
        DocumentKind::Xml => decode_xml(bytes, context.content_type),
        DocumentKind::Text => decode_text(bytes, context),
    }
}

/// HTML #read-html: sniff, decode, and apply a character encoding
/// declaration found by the tree builder while the confidence is tentative.
pub(crate) fn decode_html(bytes: &[u8], context: SniffContext<'_>) -> DecodedDocument {
    let (encoding, confidence) = sniff(bytes, context, true);
    let (text, encoding) = decode(bytes, encoding);
    if confidence == Confidence::Certain || is_utf16(encoding) {
        // #change-the-encoding step 1: a UTF-16 document ignores declarations.
        return DecodedDocument { text, encoding };
    }
    match declared_encoding_while_parsing(&text).map(declared_for_parser) {
        // Step 4: the same encoding only makes the confidence certain.
        Some(declared) if declared != encoding => {
            // Steps 5-6: reparse the bytes, from memory, with the new
            // encoding and certain confidence.
            let (text, encoding) = decode(bytes, declared);
            DecodedDocument { text, encoding }
        }
        _ => DecodedDocument { text, encoding },
    }
}

/// HTML #read-text leaves decoding to the specification of the document's
/// MIME type: RFC 9239 §4.2 (JavaScript) and RFC 8259 §8.1 (JSON) default to
/// UTF-8, CSS Syntax §3.2 has its own fallback, WebVTT is always UTF-8, and
/// other text uses the encoding sniffing algorithm. Plain text has no markup
/// to declare an encoding, so its prescan is skipped (#encoding-sniffing-
/// algorithm step 5 makes it optional).
pub(crate) fn decode_text(bytes: &[u8], context: SniffContext<'_>) -> DecodedDocument {
    let essence = context
        .content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let json =
        essence == "application/json" || essence == "text/json" || essence.ends_with("+json");
    let encoding = if json || crate::http::is_javascript_mime_type(&essence) {
        transport_encoding(context.content_type).unwrap_or(UTF_8)
    } else if essence == "text/css" {
        transport_encoding(context.content_type)
            .or_else(|| charset_rule_encoding(bytes))
            .unwrap_or(UTF_8)
    } else if essence == "text/vtt" {
        UTF_8
    } else {
        sniff(bytes, context, false).0
    };
    let (text, encoding) = decode(bytes, encoding);
    DecodedDocument { text, encoding }
}

/// CSS Syntax 3 #determine-the-fallback-encoding step 2: the first 1024 bytes
/// begin with `@charset "…";` spelled exactly, quoted label bytes in
/// 0x00-0x21 or 0x23-0x7F. UTF-16 labels mean UTF-8.
fn charset_rule_encoding(body: &[u8]) -> Option<&'static Encoding> {
    let head = &body[..body.len().min(PRESCAN_BYTES)];
    let rest = head.strip_prefix(b"@charset \"")?;
    let length = rest.iter().position(|&byte| byte == b'"')?;
    if rest.get(length + 1) != Some(&b';') || rest[..length].iter().any(|&byte| byte > 0x7F) {
        return None;
    }
    let encoding = Encoding::for_label(&rest[..length])?;
    Some(if is_utf16(encoding) { UTF_8 } else { encoding })
}

/// XML 1.0 §4.3.3 and Appendix F with RFC 7303 §3: the byte order mark,
/// then an external `charset`, then the XML declaration, and otherwise UTF-8.
/// Encoding #decode keeps the BOM most authoritative.
pub(crate) fn decode_xml(bytes: &[u8], content_type: &str) -> DecodedDocument {
    let encoding = Encoding::for_bom(bytes)
        .map(|(encoding, _)| encoding)
        .or_else(|| transport_encoding(content_type))
        .or_else(|| utf16_xml_declaration(bytes))
        .or_else(|| xml_declaration_encoding(bytes))
        .unwrap_or(UTF_8);
    let (text, encoding) = decode(bytes, encoding);
    DecodedDocument { text, encoding }
}

/// XHR #document-response step 5: the final encoding from `label`, else a
/// prescan of the first 1024 received bytes (with the same end condition as
/// navigation), else UTF-8. The HTML parser then runs with that known
/// definite encoding (Encoding #decode still honors a byte order mark), and
/// step 8 makes it the document's encoding.
pub(crate) fn decode_xhr_html(bytes: &[u8], label: Option<&str>) -> DecodedDocument {
    let encoding = label
        .and_then(|label| Encoding::for_label(label.as_bytes()))
        .or_else(|| prescan_byte_stream(bytes))
        .unwrap_or(UTF_8);
    let (text, _) = decode(bytes, encoding);
    DecodedDocument { text, encoding }
}

/// Encoding #decode with the encoding `label` names as the fallback (UTF-8
/// when it names none); the replacement encoding is a valid fallback. XHR
/// #text-response uses this, unlike `TextDecoder`, which refuses replacement.
pub(crate) fn decode_with_label(bytes: &[u8], label: Option<&str>) -> DecodedDocument {
    let fallback = label
        .and_then(|label| Encoding::for_label(label.as_bytes()))
        .unwrap_or(UTF_8);
    let (text, encoding) = decode(bytes, fallback);
    DecodedDocument { text, encoding }
}

/// Encoding #decode: BOM sniffing overrides `encoding`; malformed input
/// decodes to U+FFFD (error mode "replacement").
fn decode(bytes: &[u8], encoding: &'static Encoding) -> (String, &'static Encoding) {
    let (text, used, _) = encoding.decode(bytes);
    (text.into_owned(), used)
}

/// HTML #encoding-sniffing-algorithm. `prescan` selects step 5.
pub(crate) fn sniff(
    bytes: &[u8],
    context: SniffContext<'_>,
    prescan: bool,
) -> (&'static Encoding, Confidence) {
    // Step 1: BOM sniffing (Encoding #bom-sniff).
    if let Some((encoding, _)) = Encoding::for_bom(bytes) {
        return (encoding, Confidence::Certain);
    }
    // Step 2 (a user override) is not offered, and step 3 needs no wait:
    // the response is complete.
    // Step 4: a supported transport-layer encoding.
    if let Some(encoding) = transport_encoding(context.content_type) {
        return (encoding, Confidence::Certain);
    }
    // Step 5: the prescan.
    if prescan && let Some(encoding) = prescan_byte_stream(bytes) {
        return (encoding, Confidence::Tentative);
    }
    // Step 6: a same-origin container document's encoding, unless UTF-16.
    if let Some(encoding) = context.container.filter(|encoding| !is_utf16(encoding)) {
        return (encoding, Confidence::Tentative);
    }
    // Step 7 (optional autodetection): only the local-file UTF-8 check that
    // the standard's note recommends, over the whole file. Network content is
    // never guessed.
    if context.local_file && !bytes.is_ascii() && std::str::from_utf8(bytes).is_ok() {
        return (UTF_8, Confidence::Tentative);
    }
    // Step 8: the default for TRust's fixed en-US locale is windows-1252.
    (WINDOWS_1252, Confidence::Tentative)
}

/// The supported encoding a `Content-Type` value's `charset` parameter labels.
pub(crate) fn transport_encoding(content_type: &str) -> Option<&'static Encoding> {
    Encoding::for_label(content_type_charset(content_type)?.as_bytes())
}

fn is_utf16(encoding: &'static Encoding) -> bool {
    encoding == UTF_16BE || encoding == UTF_16LE
}

/// The prescan and "change the encoding" both refuse to switch to UTF-16 or
/// to x-user-defined from a declaration (#change-the-encoding steps 2-3).
fn declared_for_parser(encoding: &'static Encoding) -> &'static Encoding {
    if is_utf16(encoding) {
        UTF_8
    } else if encoding == X_USER_DEFINED {
        WINDOWS_1252
    } else {
        encoding
    }
}

// ---- HTML #prescan-a-byte-stream-to-determine-its-encoding ----------------

fn is_space(byte: u8) -> bool {
    matches!(byte, 0x09 | 0x0A | 0x0C | 0x0D | 0x20)
}

/// Prescan `bytes`. The end condition is reached when the loop would start a
/// new construct at or beyond the first [`PRESCAN_BYTES`] bytes; a tag or
/// comment that begins inside them is read to its end. Running out of bytes or
/// reaching the end condition returns "get an XML encoding" of the bytes.
pub(crate) fn prescan_byte_stream(bytes: &[u8]) -> Option<&'static Encoding> {
    match prescan_markup(bytes) {
        Some(encoding) => Some(encoding),
        None => xml_declaration_encoding(bytes),
    }
}

/// The prescan's own steps; `None` means it was aborted.
fn prescan_markup(bytes: &[u8]) -> Option<&'static Encoding> {
    // Step 2: UTF-16 XML declarations (case-sensitive '<?x').
    if bytes.starts_with(&[0x3C, 0x00, 0x3F, 0x00, 0x78, 0x00]) {
        return Some(UTF_16LE);
    }
    if bytes.starts_with(&[0x00, 0x3C, 0x00, 0x3F, 0x00, 0x78]) {
        return Some(UTF_16BE);
    }
    let mut position = 0;
    // Step 3: Loop.
    while position < bytes.len().min(PRESCAN_BYTES) {
        let rest = &bytes[position..];
        if rest.starts_with(b"<!--") {
            // The first '>' preceded by "--" after the '<'; the two hyphens
            // may be those of "<!--" itself.
            let end = bytes[position + 2..]
                .windows(3)
                .position(|window| window == b"-->")?;
            position += 2 + end + 2;
        } else if rest.len() >= 6
            && rest[0] == b'<'
            && rest[1..5].eq_ignore_ascii_case(b"meta")
            && (is_space(rest[5]) || rest[5] == b'/')
        {
            position += 5;
            if let Some(encoding) = prescan_meta(bytes, &mut position)? {
                return Some(encoding);
            }
        } else if rest[0] == b'<'
            && match rest.get(1) {
                Some(b'/') => rest.get(2).is_some_and(u8::is_ascii_alphabetic),
                second => second.is_some_and(u8::is_ascii_alphabetic),
            }
        {
            // An end tag or another start tag: skip its name and attributes.
            position += bytes[position..]
                .iter()
                .position(|&byte| is_space(byte) || byte == b'>')?;
            while get_attribute(bytes, &mut position)?.is_some() {}
        } else if rest.starts_with(b"<!") || rest.starts_with(b"</") || rest.starts_with(b"<?") {
            position += 1 + bytes[position + 1..]
                .iter()
                .position(|&byte| byte == b'>')?;
        }
        // Next byte.
        position += 1;
    }
    None
}

/// The `<meta` branch of the prescan, with `position` at the byte after
/// "meta". `Some(None)` continues at "next byte"; `None` aborts.
fn prescan_meta(bytes: &[u8], position: &mut usize) -> Option<Option<&'static Encoding>> {
    let mut attribute_list: Vec<Vec<u8>> = Vec::new();
    let mut got_pragma = false;
    let mut need_pragma: Option<bool> = None;
    // `None` is the null value; `Some(None)` is failure.
    let mut charset: Option<Option<&'static Encoding>> = None;
    // Attributes.
    while let Some((name, value)) = get_attribute(bytes, position)? {
        if attribute_list.contains(&name) {
            continue;
        }
        match name.as_slice() {
            b"http-equiv" if value == b"content-type" => got_pragma = true,
            b"content" => {
                if let Some(encoding) = extract_meta_encoding(&String::from_utf8_lossy(&value))
                    && charset.is_none()
                {
                    charset = Some(Some(encoding));
                    need_pragma = Some(true);
                }
            }
            b"charset" => {
                charset = Some(Encoding::for_label(&value));
                need_pragma = Some(false);
            }
            _ => {}
        }
        attribute_list.push(name);
    }
    // Processing: a null need pragma, a missing pragma, or a failed charset
    // continue at "next byte".
    let Some(need_pragma) = need_pragma else {
        return Some(None);
    };
    if need_pragma && !got_pragma {
        return Some(None);
    }
    Some(charset.flatten().map(declared_for_parser))
}

/// HTML #concept-get-attributes-when-sniffing. `Some(None)` means there is
/// no further attribute; `None` means the prescan ran out of bytes.
/// Names and values are ASCII-lowercased, as the algorithm specifies.
#[allow(clippy::type_complexity)]
fn get_attribute(bytes: &[u8], position: &mut usize) -> Option<Option<(Vec<u8>, Vec<u8>)>> {
    // Step 1.
    while is_space(*bytes.get(*position)?) || bytes[*position] == b'/' {
        *position += 1;
    }
    // Step 2.
    if bytes[*position] == b'>' {
        return Some(None);
    }
    // Step 3.
    let mut name = Vec::new();
    let mut value = Vec::new();
    // Steps 4-5: the attribute name.
    loop {
        let byte = *bytes.get(*position)?;
        match byte {
            b'=' if !name.is_empty() => {
                *position += 1;
                break;
            }
            byte if is_space(byte) => {
                // Spaces.
                while is_space(*bytes.get(*position)?) {
                    *position += 1;
                }
                if bytes[*position] != b'=' {
                    return Some(Some((name, value)));
                }
                *position += 1;
                break;
            }
            b'/' | b'>' => return Some(Some((name, value))),
            byte => name.push(byte.to_ascii_lowercase()),
        }
        *position += 1;
    }
    // Value.
    while is_space(*bytes.get(*position)?) {
        *position += 1;
    }
    match *bytes.get(*position)? {
        quote @ (b'"' | b'\'') => loop {
            // Quote loop.
            *position += 1;
            let byte = *bytes.get(*position)?;
            if byte == quote {
                *position += 1;
                return Some(Some((name, value)));
            }
            value.push(byte.to_ascii_lowercase());
        },
        b'>' => return Some(Some((name, value))),
        byte => {
            value.push(byte.to_ascii_lowercase());
            *position += 1;
        }
    }
    loop {
        let byte = *bytes.get(*position)?;
        if is_space(byte) || byte == b'>' {
            return Some(Some((name, value)));
        }
        value.push(byte.to_ascii_lowercase());
        *position += 1;
    }
}

/// HTML #concept-get-xml-encoding-when-sniffing, which also determines an
/// XML document's declared encoding here. The `encoding` pseudo-attribute
/// and its value must lie within the declaration, before its first '>'.
fn xml_declaration_encoding(bytes: &[u8]) -> Option<&'static Encoding> {
    // Steps 1-3.
    if !bytes.starts_with(b"<?xml") {
        return None;
    }
    let end = bytes.iter().position(|&byte| byte == b'>')?;
    let declaration = &bytes[..end];
    // Steps 4-5.
    let mut position = declaration
        .windows(8)
        .position(|window| window == b"encoding")?
        + 8;
    // Steps 6-9.
    while *declaration.get(position)? <= 0x20 {
        position += 1;
    }
    if declaration[position] != b'=' {
        return None;
    }
    position += 1;
    while *declaration.get(position)? <= 0x20 {
        position += 1;
    }
    // Steps 10-14.
    let quote = declaration[position];
    if quote != b'"' && quote != b'\'' {
        return None;
    }
    position += 1;
    let length = declaration[position..]
        .iter()
        .position(|&byte| byte == quote)?;
    let potential = &declaration[position..position + length];
    // Step 15.
    if potential.iter().any(|&byte| byte <= 0x20) {
        return None;
    }
    // Steps 16-17.
    let encoding = Encoding::for_label(potential)?;
    Some(if is_utf16(encoding) { UTF_8 } else { encoding })
}

/// XML 1.0 Appendix F.1: a UTF-16 document without a byte order mark that
/// begins with an XML declaration (`<?` as 16-bit units).
fn utf16_xml_declaration(bytes: &[u8]) -> Option<&'static Encoding> {
    if bytes.starts_with(&[0x3C, 0x00, 0x3F, 0x00]) {
        Some(UTF_16LE)
    } else if bytes.starts_with(&[0x00, 0x3C, 0x00, 0x3F]) {
        Some(UTF_16BE)
    } else {
        None
    }
}

/// HTML #algorithm-for-extracting-a-character-encoding-from-a-meta-element.
/// `None` is "nothing" and also a label that is not an encoding.
pub(crate) fn extract_meta_encoding(s: &str) -> Option<&'static Encoding> {
    let bytes = s.as_bytes();
    let mut position = 0;
    loop {
        // Loop: the next ASCII case-insensitive "charset".
        position += bytes
            .get(position..)?
            .windows(7)
            .position(|window| window.eq_ignore_ascii_case(b"charset"))?
            + 7;
        while bytes.get(position).is_some_and(u8::is_ascii_whitespace) {
            position += 1;
        }
        if bytes.get(position) == Some(&b'=') {
            position += 1;
            break;
        }
    }
    while bytes.get(position).is_some_and(u8::is_ascii_whitespace) {
        position += 1;
    }
    let label = match *bytes.get(position)? {
        quote @ (b'"' | b'\'') => {
            let length = bytes[position + 1..]
                .iter()
                .position(|&byte| byte == quote)?;
            &bytes[position + 1..position + 1 + length]
        }
        _ => {
            let length = bytes[position..]
                .iter()
                .position(|&byte| byte.is_ascii_whitespace() || byte == b';')
                .unwrap_or(bytes.len() - position);
            &bytes[position..position + length]
        }
    };
    Encoding::for_label(label)
}

// ---- #change-the-encoding --------------------------------------------------

/// Run the HTML tree builder over a tentatively decoded document until it
/// inserts the first `meta` element whose character encoding declaration
/// would change the encoding (#meta-charset-during-parse). Elements created
/// by `document.write` are outside the decoded bytes and are not seen here.
fn declared_encoding_while_parsing(text: &str) -> Option<&'static Encoding> {
    let builder = TreeBuilder::new(DeclarationSink::default(), TreeBuilderOpts::default());
    let tokenizer = Tokenizer::new(builder, TokenizerOpts::default());
    let input = BufferQueue::default();
    input.push_back(StrTendril::from(text));
    loop {
        // html5ever pauses after each `meta` the "in head" rules process
        // (`EncodingIndicator`) and after each script end tag.
        let done = matches!(tokenizer.feed(&input), TokenizerResult::Done);
        if let Some(encoding) = tokenizer.sink.sink.declared.get() {
            return Some(encoding);
        }
        if done {
            break;
        }
    }
    tokenizer.end();
    tokenizer.sink.sink.declared.get()
}

/// A tree sink that keeps only what the tree builder must query (element
/// names and integration points) and records the first effective `meta`
/// character encoding declaration.
#[derive(Default)]
struct DeclarationSink {
    /// Names by handle; non-elements hold an empty name.
    names: RefCell<Vec<QualName>>,
    annotation_xml_integration_points: RefCell<Vec<usize>>,
    declared: Cell<Option<&'static Encoding>>,
}

impl DeclarationSink {
    fn push(&self, name: QualName) -> usize {
        let mut names = self.names.borrow_mut();
        names.push(name);
        names.len()
    }

    fn placeholder(&self) -> usize {
        self.push(QualName::new(None, ns!(), local_name!("")))
    }
}

/// #meta-charset-during-parse steps 1-2 for a `meta` start tag's attributes.
fn meta_declaration(attributes: &[Attribute]) -> Option<&'static Encoding> {
    let attribute = |name| {
        attributes
            .iter()
            .find(|attribute| attribute.name.ns == ns!() && attribute.name.local == name)
            .map(|attribute| &*attribute.value)
    };
    attribute(local_name!("charset"))
        .and_then(|label| Encoding::for_label(label.as_bytes()))
        .or_else(|| {
            attribute(local_name!("http-equiv"))
                .filter(|value| value.eq_ignore_ascii_case("content-type"))
                .and(attribute(local_name!("content")))
                .and_then(extract_meta_encoding)
        })
}

impl TreeSink for DeclarationSink {
    /// Handle 0 is the document; element `n` is `names[n - 1]`.
    type Handle = usize;
    type Output = ();
    type ElemName<'a> = Ref<'a, QualName>;

    fn finish(self) {}

    fn parse_error(&self, _msg: Cow<'static, str>) {}

    fn get_document(&self) -> usize {
        0
    }

    fn elem_name<'a>(&'a self, target: &'a usize) -> Ref<'a, QualName> {
        Ref::map(self.names.borrow(), |names| &names[target - 1])
    }

    fn create_element(&self, name: QualName, attrs: Vec<Attribute>, flags: ElementFlags) -> usize {
        if self.declared.get().is_none()
            && name.ns == ns!(html)
            && name.local == local_name!("meta")
        {
            self.declared.set(meta_declaration(&attrs));
        }
        let handle = self.push(name);
        if flags.template {
            // The template contents fragment is the next handle.
            self.placeholder();
        }
        if flags.mathml_annotation_xml_integration_point {
            self.annotation_xml_integration_points
                .borrow_mut()
                .push(handle);
        }
        handle
    }

    fn create_comment(&self, _text: StrTendril) -> usize {
        self.placeholder()
    }

    fn create_pi(&self, _target: StrTendril, _data: StrTendril) -> usize {
        self.placeholder()
    }

    fn append(&self, _parent: &usize, _child: NodeOrText<usize>) {}

    fn append_based_on_parent_node(
        &self,
        _element: &usize,
        _prev_element: &usize,
        _child: NodeOrText<usize>,
    ) {
    }

    fn append_doctype_to_document(
        &self,
        _name: StrTendril,
        _public_id: StrTendril,
        _system_id: StrTendril,
    ) {
    }

    fn get_template_contents(&self, target: &usize) -> usize {
        target + 1
    }

    fn same_node(&self, x: &usize, y: &usize) -> bool {
        x == y
    }

    fn set_quirks_mode(&self, _mode: QuirksMode) {}

    fn append_before_sibling(&self, _sibling: &usize, _new_node: NodeOrText<usize>) {}

    fn add_attrs_if_missing(&self, _target: &usize, _attrs: Vec<Attribute>) {}

    fn remove_from_parent(&self, _target: &usize) {}

    fn reparent_children(&self, _node: &usize, _new_parent: &usize) {}

    fn is_mathml_annotation_xml_integration_point(&self, handle: &usize) -> bool {
        self.annotation_xml_integration_points
            .borrow()
            .contains(handle)
    }
}

// ---- Content-Type charset --------------------------------------------------

fn is_http_whitespace(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\r' | ' ')
}

fn is_http_token(c: char) -> bool {
    c.is_ascii_alphanumeric() || "!#$%&'*+-.^_`|~".contains(c)
}

fn is_http_quoted_string_token(c: char) -> bool {
    matches!(c, '\t' | ' '..='~' | '\u{80}'..='\u{ff}')
}

/// Fetch #collect-an-http-quoted-string from `input[*position..]`, which
/// starts with U+0022.
fn collect_http_quoted_string(input: &str, position: &mut usize, extract_value: bool) -> String {
    let start = *position;
    let mut value = String::new();
    *position += 1;
    loop {
        let rest = &input[*position..];
        let run = rest.find(['"', '\\']).unwrap_or(rest.len());
        value.push_str(&rest[..run]);
        *position += run;
        let Some(quote_or_backslash) = input[*position..].chars().next() else {
            break;
        };
        *position += 1;
        if quote_or_backslash == '\\' {
            let Some(escaped) = input[*position..].chars().next() else {
                value.push('\\');
                break;
            };
            value.push(escaped);
            *position += escaped.len_utf8();
        } else {
            break;
        }
    }
    if extract_value {
        value
    } else {
        input[start..*position].to_owned()
    }
}

/// MIME Sniffing #parse-a-mime-type: the essence and the `charset`
/// parameter, or `None` for failure.
fn parse_mime_type(input: &str) -> Option<(String, Option<String>)> {
    let input = input.trim_matches(is_http_whitespace);
    let (kind, rest) = input.split_once('/')?;
    if kind.is_empty() || !kind.chars().all(is_http_token) {
        return None;
    }
    let subtype_end = rest.find(';').unwrap_or(rest.len());
    let subtype = rest[..subtype_end].trim_end_matches(is_http_whitespace);
    if subtype.is_empty() || !subtype.chars().all(is_http_token) {
        return None;
    }
    let essence = format!(
        "{}/{}",
        kind.to_ascii_lowercase(),
        subtype.to_ascii_lowercase()
    );
    let parameters = &rest[subtype_end..];
    let mut charset = None;
    let mut position = 0;
    while position < parameters.len() {
        // Skip past ';' and HTTP whitespace.
        position += 1;
        position += parameters[position..]
            .find(|c| !is_http_whitespace(c))
            .unwrap_or(parameters.len() - position);
        let name_end = parameters[position..]
            .find([';', '='])
            .map_or(parameters.len(), |offset| position + offset);
        let name = parameters[position..name_end].to_ascii_lowercase();
        position = name_end;
        if position < parameters.len() {
            if parameters[position..].starts_with(';') {
                continue;
            }
            position += 1;
        }
        if position >= parameters.len() {
            break;
        }
        let value = if parameters[position..].starts_with('"') {
            let value = collect_http_quoted_string(parameters, &mut position, true);
            position = parameters[position..]
                .find(';')
                .map_or(parameters.len(), |offset| position + offset);
            value
        } else {
            let end = parameters[position..]
                .find(';')
                .map_or(parameters.len(), |offset| position + offset);
            let value = parameters[position..end].trim_end_matches(is_http_whitespace);
            position = end;
            if value.is_empty() {
                continue;
            }
            value.to_owned()
        };
        if name == "charset" && charset.is_none() && value.chars().all(is_http_quoted_string_token)
        {
            charset = Some(value);
        }
    }
    Some((essence, charset))
}

/// Fetch #concept-header-list-get-decode-split for a combined header value.
fn split_header_value(input: &str) -> Vec<String> {
    let mut values = Vec::new();
    let mut position = 0;
    let mut temporary = String::new();
    loop {
        let rest = &input[position..];
        let run = rest.find(['"', ',']).unwrap_or(rest.len());
        temporary.push_str(&rest[..run]);
        position += run;
        if input[position..].starts_with('"') {
            temporary.push_str(&collect_http_quoted_string(input, &mut position, false));
            if position < input.len() {
                continue;
            }
        }
        values.push(temporary.trim_matches(['\t', ' ']).to_owned());
        temporary.clear();
        if position >= input.len() {
            break;
        }
        // Skip the ','.
        position += 1;
    }
    values
}

/// The `charset` parameter of the MIME type Fetch's "extract a MIME type"
/// computes from a (possibly combined) `Content-Type` value.
pub(crate) fn content_type_charset(content_type: &str) -> Option<String> {
    let mut charset: Option<String> = None;
    let mut essence: Option<String> = None;
    let mut mime_charset: Option<String> = None;
    let mut found = false;
    for value in split_header_value(content_type) {
        let Some((temporary_essence, temporary_charset)) = parse_mime_type(&value) else {
            continue;
        };
        if temporary_essence == "*/*" {
            continue;
        }
        found = true;
        mime_charset = temporary_charset;
        if essence.as_deref() != Some(temporary_essence.as_str()) {
            charset = mime_charset.clone();
            essence = Some(temporary_essence);
        } else if mime_charset.is_none() {
            mime_charset = charset.clone();
        }
    }
    if found { mime_charset } else { None }
}

#[cfg(test)]
mod tests {
    use super::*;
    use encoding_rs::{
        EUC_JP, GBK, ISO_8859_2, ISO_8859_5, KOI8_R, REPLACEMENT, SHIFT_JIS, WINDOWS_1250,
        WINDOWS_1251, WINDOWS_1253,
    };

    fn prescan(source: &str) -> Option<&'static str> {
        prescan_byte_stream(source.as_bytes()).map(Encoding::name)
    }

    fn html(bytes: &[u8], content_type: &str) -> (String, &'static str) {
        let decoded = decode_html(
            bytes,
            SniffContext {
                content_type,
                ..Default::default()
            },
        );
        (decoded.text, decoded.encoding.name())
    }

    #[test]
    fn meta_charset_forms_are_found_by_the_prescan() {
        assert_eq!(prescan("<meta charset=koi8-r>"), Some("KOI8-R"));
        assert_eq!(prescan("<meta charset='koi8-r'>"), Some("KOI8-R"));
        assert_eq!(prescan("<META CHARSET=\"KOI8-R\">"), Some("KOI8-R"));
        assert_eq!(prescan("<meta/charset=koi8-r>"), Some("KOI8-R"));
        assert_eq!(prescan("<meta charset = \t koi8-r >"), Some("KOI8-R"));
        assert_eq!(prescan("<meta charset=\" koi8-r \">"), Some("KOI8-R"));
        assert_eq!(prescan("<meta name=x charset=gbk>"), Some("GBK"));
        // "<meta" must be followed by a space or slash.
        assert_eq!(prescan("<metacharset=koi8-r>"), None);
        assert_eq!(prescan("<meta>"), None);
    }

    #[test]
    fn the_prescan_requires_the_http_equiv_pragma_for_content() {
        let pragma = |source| prescan(source);
        assert_eq!(
            pragma("<meta http-equiv=content-type content='text/html; charset=iso-8859-5'>"),
            Some("ISO-8859-5")
        );
        // Attribute order does not matter.
        assert_eq!(
            pragma("<meta content=\"text/html;charset=ISO-8859-5\" http-equiv=\"Content-Type\"/>"),
            Some("ISO-8859-5")
        );
        assert_eq!(pragma("<meta content='text/html; charset=koi8-r'>"), None);
        assert_eq!(
            pragma("<meta http-equiv=refresh content='text/html; charset=koi8-r'>"),
            None
        );
        // `charset` overrides `content` and needs no pragma.
        assert_eq!(
            pragma("<meta content='charset=koi8-r' charset=gbk>"),
            Some("GBK")
        );
        assert_eq!(
            pragma("<meta charset=gbk content='charset=koi8-r' http-equiv=content-type>"),
            Some("GBK")
        );
        // A repeated attribute is ignored: the first http-equiv wins.
        assert_eq!(
            pragma("<meta http-equiv=refresh http-equiv=content-type content='charset=koi8-r'>"),
            None
        );
        // The extraction finds "charset" anywhere and honors quoting.
        assert_eq!(
            pragma("<meta http-equiv=content-type content='charset; charset=\"gbk\"'>"),
            Some("GBK")
        );
    }

    #[test]
    fn the_prescan_skips_comments_and_other_tags() {
        assert_eq!(
            prescan("<!-- <meta charset=gbk> --><meta charset=koi8-r>"),
            Some("KOI8-R")
        );
        assert_eq!(prescan("<!--><meta charset=koi8-r>"), Some("KOI8-R"));
        // "<!--->" is a whole comment: its "-->" reuses the opening hyphens.
        assert_eq!(
            prescan("<!---><meta charset=gbk>--><meta charset=koi8-r>"),
            Some("GBK")
        );
        assert_eq!(
            prescan("<div title='<meta charset=gbk>'><meta charset=koi8-r>"),
            Some("KOI8-R")
        );
        assert_eq!(
            prescan("<!doctype html><meta charset=koi8-r>"),
            Some("KOI8-R")
        );
        assert_eq!(
            prescan("</p foo='<meta charset=gbk>'><meta charset=koi8-r>"),
            Some("KOI8-R")
        );
        assert_eq!(
            prescan("<?pi <meta charset=gbk>?><meta charset=koi8-r>"),
            Some("KOI8-R")
        );
        // `<![CDATA[` is a `<!` construct ending at the first '>'.
        assert_eq!(prescan("<![CDATA[<meta charset=gbk>]]>"), None);
        // The prescan does not know raw text elements.
        assert_eq!(
            prescan("<title><meta charset=koi8-r></title>"),
            Some("KOI8-R")
        );
        // An unclosed comment runs out of bytes.
        assert_eq!(prescan("<!-- <meta charset=koi8-r>"), None);
    }

    #[test]
    fn declarations_of_utf16_and_x_user_defined_are_remapped() {
        assert_eq!(prescan("<meta charset=utf-16>"), Some("UTF-8"));
        assert_eq!(prescan("<meta charset=utf-16be>"), Some("UTF-8"));
        assert_eq!(
            prescan("<meta charset=x-user-defined>"),
            Some("windows-1252")
        );
        assert_eq!(prescan("<meta charset=iso-2022-kr>"), Some("replacement"));
        // A bad label is failure; the scan continues to the next declaration.
        assert_eq!(
            prescan("<meta charset=bogus><meta charset=gbk>"),
            Some("GBK")
        );
        assert_eq!(prescan("<meta charset=\"&#119;indows-1251\">"), None);
    }

    #[test]
    fn the_prescan_stops_at_the_first_kilobyte() {
        let pad = " ".repeat(PRESCAN_BYTES - 1);
        assert_eq!(
            prescan(&format!("{pad}<meta charset=koi8-r>")),
            Some("KOI8-R")
        );
        assert_eq!(prescan(&format!("{pad} <meta charset=koi8-r>")), None);
        // A comment beginning inside the first kilobyte is skipped to its end.
        let comment = format!("<!--{}--> <meta charset=koi8-r>", " ".repeat(2000));
        assert_eq!(prescan(&comment), None);
    }

    #[test]
    fn utf16_xml_declarations_and_bogus_xml_declarations() {
        assert_eq!(
            prescan_byte_stream(b"<\0?\0x\0m\0l\0").map(Encoding::name),
            Some("UTF-16LE")
        );
        assert_eq!(
            prescan_byte_stream(b"\0<\0?\0x\0m\0l").map(Encoding::name),
            Some("UTF-16BE")
        );
        assert_eq!(
            prescan_byte_stream(b"<\0?\0x><meta charset=windows-1253>").map(Encoding::name),
            Some("windows-1253")
        );
        // HTML #concept-get-xml-encoding-when-sniffing.
        assert_eq!(
            prescan("<?xml version=\"1.0\" encoding=\"windows-1251\"?>"),
            Some("windows-1251")
        );
        assert_eq!(prescan("<?xml encoding = 'koi8-r'?>"), Some("KOI8-R"));
        assert_eq!(
            prescan("<?xml version=\"1.0\" encoding=\"UTF-16\"?>"),
            Some("UTF-8")
        );
        assert_eq!(
            prescan("<?xml version=\"1.0\" encoding=\"ISO-2022-KR\"?>"),
            Some("replacement")
        );
        assert_eq!(prescan("<?xml>encoding=\"windows-1251\"?>"), None);
        assert_eq!(prescan(" <?xml encoding=\"windows-1251\"?>"), None);
        assert_eq!(prescan("<?XML encoding=\"windows-1251\"?>"), None);
        assert_eq!(prescan("<?xml encoding=windows-1251?>"), None);
        assert_eq!(prescan("<?xml encoding=\" windows-1251\"?>"), None);
        assert_eq!(prescan("<?xml encoding=\"windows-1251?>"), None);
        assert_eq!(prescan("<?xml encodingencoding=\"windows-1251\"?>"), None);
        // A meta inside the declaration is skipped with it.
        assert_eq!(
            prescan("<?xml <meta charset=\"windows-1253\" encoding=\"windows-1251\"?>"),
            Some("windows-1251")
        );
        // A meta after the declaration takes precedence.
        assert_eq!(
            prescan("<?xml encoding=\"windows-1251\"?><meta charset=\"windows-1253\">"),
            Some("windows-1253")
        );
        // The declaration may extend past the first kilobyte.
        let long = format!("<?xml{}encoding=\"windows-1251\"?>", " ".repeat(1100));
        assert_eq!(prescan(&long), Some("windows-1251"));
    }

    #[test]
    fn meta_content_extraction_follows_the_html_algorithm() {
        let extract = |s| extract_meta_encoding(s).map(Encoding::name);
        assert_eq!(extract("text/html; charset=koi8-r"), Some("KOI8-R"));
        assert_eq!(extract("text/html;CHARSET = \"koi8-r\""), Some("KOI8-R"));
        assert_eq!(extract("charset='koi8-r' trailing"), Some("KOI8-R"));
        assert_eq!(extract("charset=koi8-r;foo"), Some("KOI8-R"));
        assert_eq!(extract("charsetcharset=koi8-r"), Some("KOI8-R"));
        assert_eq!(extract("charset x; charset=gbk"), Some("GBK"));
        assert_eq!(extract("charset=\"koi8-r"), None);
        assert_eq!(extract("charset="), None);
        assert_eq!(extract("text/html"), None);
        assert_eq!(extract("charset=bogus"), None);
    }

    #[test]
    fn content_type_charset_follows_fetch_and_mime_sniffing() {
        let charset = |s| content_type_charset(s);
        assert_eq!(charset("text/html;charset=gbk").as_deref(), Some("gbk"));
        assert_eq!(
            charset("text/html; Charset=\"Shift_JIS\"").as_deref(),
            Some("Shift_JIS")
        );
        assert_eq!(
            charset("text/html;charset=\"shift_jis\"iso-2022-jp").as_deref(),
            Some("shift_jis")
        );
        assert_eq!(charset("text/html;charset=").as_deref(), None);
        assert_eq!(
            charset("text/html;charset;charset=gbk").as_deref(),
            Some("gbk")
        );
        assert_eq!(
            charset("text/html;charset=gbk;charset=koi8-r").as_deref(),
            Some("gbk")
        );
        assert_eq!(charset("text/html").as_deref(), None);
        assert_eq!(charset("charset=gbk").as_deref(), None);
        // Fetch: later values replace earlier ones; a same-essence value
        // without a charset keeps the earlier charset.
        assert_eq!(
            charset("text/html;charset=gbk, text/html").as_deref(),
            Some("gbk")
        );
        assert_eq!(
            charset("text/html;charset=gbk, text/plain").as_deref(),
            None
        );
        assert_eq!(
            charset("text/plain;charset=gbk, */*").as_deref(),
            Some("gbk")
        );
        assert_eq!(
            charset("text/html;charset=\"a,b\", text/html").as_deref(),
            Some("a,b")
        );
    }

    #[test]
    fn sniffing_orders_bom_transport_prescan_container_and_default() {
        let context = |content_type| SniffContext {
            content_type,
            ..Default::default()
        };
        let sniffed = |bytes: &[u8], content_type| sniff(bytes, context(content_type), true);
        assert_eq!(
            sniffed(
                b"\xEF\xBB\xBF<meta charset=gbk>",
                "text/html;charset=koi8-r"
            ),
            (UTF_8, Confidence::Certain)
        );
        assert_eq!(
            sniffed(b"\xFF\xFE<\0", "text/html"),
            (UTF_16LE, Confidence::Certain)
        );
        assert_eq!(
            sniffed(b"<meta charset=gbk>", "text/html;charset=koi8-r"),
            (KOI8_R, Confidence::Certain)
        );
        // An unsupported transport label is ignored.
        assert_eq!(
            sniffed(b"<meta charset=gbk>", "text/html;charset=bogus"),
            (GBK, Confidence::Tentative)
        );
        assert_eq!(
            sniffed(b"<p>", "text/html"),
            (WINDOWS_1252, Confidence::Tentative)
        );
        let inherited = SniffContext {
            content_type: "text/html",
            container: Some(WINDOWS_1250),
            local_file: false,
        };
        assert_eq!(
            sniff(b"<p>", inherited, true),
            (WINDOWS_1250, Confidence::Tentative)
        );
        let from_utf16 = SniffContext {
            container: Some(UTF_16BE),
            ..inherited
        };
        assert_eq!(
            sniff(b"<p>", from_utf16, true),
            (WINDOWS_1252, Confidence::Tentative)
        );
        // Only local files are autodetected, and only as UTF-8.
        let file = SniffContext {
            local_file: true,
            ..Default::default()
        };
        assert_eq!(
            sniff("<p>é".as_bytes(), file, true),
            (UTF_8, Confidence::Tentative)
        );
        assert_eq!(
            sniff(b"<p>\xE9", file, true),
            (WINDOWS_1252, Confidence::Tentative)
        );
        assert_eq!(sniff("<p>é".as_bytes(), context(""), true).0, WINDOWS_1252);
    }

    #[test]
    fn html_documents_decode_with_their_sniffed_encoding() {
        assert_eq!(
            html(b"<p>caf\xE9", "text/html"),
            ("<p>café".into(), "windows-1252")
        );
        assert_eq!(
            html(b"<p>\x80", "text/html"),
            ("<p>€".into(), "windows-1252")
        );
        assert_eq!(
            html("<p>café".as_bytes(), "text/html; charset=utf-8"),
            ("<p>café".into(), "UTF-8")
        );
        assert_eq!(
            html(b"<meta charset=koi8-r><p>\xC1", "text/html"),
            ("<meta charset=koi8-r><p>а".into(), "KOI8-R")
        );
        assert_eq!(
            html(
                b"\xEF\xBB\xBF<p>\xC3\xA9",
                "text/html; charset=windows-1252"
            ),
            ("<p>é".into(), "UTF-8")
        );
        assert_eq!(
            html(b"<p>\x82\xA0", "text/html;charset=Shift_JIS").1,
            SHIFT_JIS.name()
        );
        assert_eq!(
            html(b"<p>abc", "text/html;charset=iso-2022-kr"),
            ("\u{FFFD}".into(), REPLACEMENT.name())
        );
    }

    #[test]
    fn a_declaration_after_the_prescan_changes_the_encoding() {
        let pad = format!("<!doctype html><head><title>t</title>{}", " ".repeat(1100));
        let mut late = format!("{pad}<meta charset=windows-1251></head><p>").into_bytes();
        late.push(0xE6);
        let (text, encoding) = html(&late, "text/html");
        assert_eq!(encoding, WINDOWS_1251.name());
        assert!(text.ends_with("<p>ж"));
        // A character reference in the label is decoded by the tokenizer.
        let (text, encoding) = html(b"<meta charset=\"&#119;indows-1251\"><p>\xE6", "text/html");
        assert_eq!(encoding, "windows-1251");
        assert!(text.ends_with("<p>ж"));
        // Raw text, comments and scripts do not declare an encoding.
        for hidden in [
            "<script>'<meta charset=windows-1251>'</script>",
            "<style><meta charset=windows-1251></style>",
            "<noscript><meta charset=windows-1251></noscript>",
            "<textarea><meta charset=windows-1251></textarea>",
        ] {
            let source = format!("{pad}{hidden}");
            assert_eq!(
                html(source.as_bytes(), "text/html").1,
                "windows-1252",
                "{hidden}"
            );
        }
        // The first effective declaration wins; a bad label is skipped.
        let source = format!(
            "{pad}<meta charset=bogus><meta http-equiv=content-type content='charset=koi8-r'><meta charset=gbk>"
        );
        assert_eq!(html(source.as_bytes(), "text/html").1, "KOI8-R");
        // A charset that fails falls back to the pragma in the same element.
        let source =
            format!("{pad}<meta charset=bogus http-equiv=content-type content='charset=koi8-r'>");
        assert_eq!(html(source.as_bytes(), "text/html").1, "KOI8-R");
        // Body metas are processed by the "in head" rules too; a meta in SVG
        // breaks out of foreign content.
        let source = format!("{pad}</head><body><p><svg><meta charset=gbk></svg>");
        assert_eq!(html(source.as_bytes(), "text/html").1, "GBK");
        // A certain transport encoding is never changed.
        let source = format!("{pad}<meta charset=gbk>");
        assert_eq!(
            html(source.as_bytes(), "text/html;charset=koi8-r").1,
            "KOI8-R"
        );
        // Declarations of UTF-16 mean UTF-8.
        let source = format!("{pad}<meta charset=utf-16le><p>é");
        assert_eq!(
            html(source.as_bytes(), "text/html"),
            (source.clone(), "UTF-8")
        );
    }

    #[test]
    fn xml_documents_use_xml_encoding_rules() {
        let xml = |bytes: &[u8], content_type| decode_xml(bytes, content_type).encoding.name();
        assert_eq!(xml(b"<x/>", "application/xhtml+xml"), "UTF-8");
        assert_eq!(xml(b"<meta charset=gbk/>", "application/xml"), "UTF-8");
        assert_eq!(
            xml(b"<?xml version='1.0' encoding='EUC-JP'?><x/>", "text/xml"),
            EUC_JP.name()
        );
        assert_eq!(
            xml(
                b"<?xml version='1.0' encoding='EUC-JP'?><x/>",
                "text/xml;charset=iso-8859-2"
            ),
            ISO_8859_2.name()
        );
        assert_eq!(xml(b"<\0?\0x\0m\0l\0>\0", "application/xml"), "UTF-16LE");
        assert_eq!(
            xml(b"\xFE\xFF\0<", "application/xml;charset=windows-1253"),
            "UTF-16BE"
        );
    }

    #[test]
    fn text_documents_skip_the_prescan() {
        let text = |bytes: &[u8], content_type| {
            decode_text(
                bytes,
                SniffContext {
                    content_type,
                    container: Some(ISO_8859_5),
                    local_file: false,
                },
            )
            .encoding
            .name()
        };
        assert_eq!(text(b"<meta charset=gbk>", "text/plain"), "ISO-8859-5");
        assert_eq!(
            text(b"x", "text/plain;charset=windows-1253"),
            WINDOWS_1253.name()
        );
        assert_eq!(text(b"\xEF\xBB\xBFx", "text/plain;charset=gbk"), "UTF-8");
        // Scripts, JSON, WebVTT and CSS have their own UTF-8 defaults.
        assert_eq!(text(b"x", "text/javascript"), "UTF-8");
        assert_eq!(text(b"x", "application/json"), "UTF-8");
        assert_eq!(text(b"x", "application/ld+json;charset=gbk"), "GBK");
        assert_eq!(text(b"x", "text/vtt;charset=gbk"), "UTF-8");
        assert_eq!(text(b"x", "text/css"), "UTF-8");
        assert_eq!(text(b"@charset \"gbk\";", "text/css"), "GBK");
    }

    #[test]
    fn document_kinds_follow_the_mime_type_essence() {
        assert_eq!(DocumentKind::for_essence("text/html"), DocumentKind::Html);
        assert_eq!(
            DocumentKind::for_essence("application/xhtml+xml"),
            DocumentKind::Xml
        );
        assert_eq!(
            DocumentKind::for_essence("image/svg+xml"),
            DocumentKind::Xml
        );
        assert_eq!(DocumentKind::for_essence("text/xml"), DocumentKind::Xml);
        assert_eq!(DocumentKind::for_essence("text/plain"), DocumentKind::Text);
        assert_eq!(
            DocumentKind::for_essence("application/json"),
            DocumentKind::Text
        );
    }
}
