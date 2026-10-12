//! External resource documents that CSS references name: Filter Effects 1
//! `filter: url(doc.svg#f)` (#FilterProperty, #origin-restrictions), CSS
//! Masking 1 `clip-path: url(doc.svg#c)` and `mask-image: url(doc.svg#m)`
//! (#the-clip-path, #the-mask-image, #priv-sec), and the HTML canvas
//! `filter` (#dom-context-2d-filter).
//!
//! Local references: csswg-drafts 81c27f686901 (2026-09-04), WHATWG HTML
//! e5071a20 (2026-09-06), WHATWG Fetch (same library sync).
//!
//! A document is fetched by the referencing element's document with a CORS
//! request in "Anonymous" mode (credentials same-origin, destination
//! "image"), once per (document origin, URL, cookie restriction). A network
//! error, including a failed CORS check, makes the reference behave as if
//! `none` had been specified. Until the document has loaded the reference
//! does not resolve: a filter list or clip-path applies nothing, a mask layer
//! is transparent black, and a canvas draws with no filtering.
//!
//! Fetched text lives in a bounded process-wide store, like external `use`
//! sprite sheets (`dom::prime_sprite_sheet`); each layout thread parses a
//! document into its own arena once (an arena never crosses threads).
//! `file:` documents are never fetched: file documents have opaque origins,
//! and CORS-dependent resources need HTTP(S). `data:` documents need no
//! fetch (Fetch #main-fetch gives them a basic response).

use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::sync::{Arc, LazyLock, Mutex};

use rustc_hash::FxHashSet;

use super::{Dom, NodeId};

/// Documents kept at once; the oldest is dropped beyond this.
const MAX_DOCUMENTS: usize = 16;
/// A larger document is treated as unusable.
const MAX_DOCUMENT_BYTES: usize = 4 * 1024 * 1024;
/// Text kept across all documents.
const MAX_TOTAL_BYTES: usize = 16 * 1024 * 1024;
/// Parsed documents each layout thread keeps.
const MAX_PARSED: usize = 8;

/// One document of the store: the referencing document's origin (as layout
/// saw it), the document's URL without fragment, and whether the
/// referencing element's cookies are restricted (a cross-site frame).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ExternalSvgKey {
    pub(crate) partition: String,
    pub(crate) url: String,
    pub(crate) restricted: bool,
}

/// A document layout needs and nothing has obtained yet, for the page
/// actor to fetch on behalf of `node`'s document.
#[derive(Clone, Debug)]
pub(crate) struct ExternalSvgRequest {
    pub(crate) key: ExternalSvgKey,
    pub(crate) url: url::Url,
    pub(crate) node: NodeId,
}

/// Requests in order, and every key ever requested by the arena.
#[derive(Default)]
pub(super) struct Requests {
    pending: Vec<ExternalSvgRequest>,
    requested: FxHashSet<ExternalSvgKey>,
}

impl Requests {
    /// Retained payload bytes, and whether a hash table's allocation
    /// details are opaque.
    pub(super) fn retained_memory(&self) -> (usize, bool) {
        let bytes = self.pending.capacity() * std::mem::size_of::<ExternalSvgRequest>()
            + self.requested.capacity() * std::mem::size_of::<ExternalSvgKey>()
            + self
                .requested
                .iter()
                .map(|key| key.partition.capacity() + key.url.capacity())
                .sum::<usize>()
            + self
                .pending
                .iter()
                .map(|request| request.url.as_str().len())
                .sum::<usize>();
        (bytes, self.requested.capacity() != 0)
    }
}

#[derive(Clone)]
enum Stored {
    Loaded {
        generation: u64,
        text: Arc<str>,
        xml: bool,
    },
    /// A response that is not a document (an image, say).
    NotDocument,
    Failed,
}

/// What a request obtained.
pub(crate) enum ExternalSvgOutcome {
    /// The decoded text, and whether it is XML.
    Document(String, bool),
    NotDocument,
    /// A network error, or an unusable document.
    Failed,
}

impl ExternalSvgOutcome {
    /// The outcome of a response with this `Content-Type` and body, or of a
    /// network error (`None`). A response whose status is not an ok status
    /// is a network error for a resource document.
    pub(crate) fn of_response(response: Option<(&str, &[u8])>) -> Self {
        let Some((content_type, body)) = response else {
            return Self::Failed;
        };
        match decode_external_svg_document(content_type, body) {
            Some((text, xml)) => Self::Document(text, xml),
            None if body.len() > MAX_DOCUMENT_BYTES => Self::Failed,
            None => Self::NotDocument,
        }
    }
}

#[derive(Default)]
struct Store {
    entries: HashMap<ExternalSvgKey, Stored>,
    order: VecDeque<ExternalSvgKey>,
    bytes: usize,
    next_generation: u64,
}

static STORE: LazyLock<Mutex<Store>> = LazyLock::new(Default::default);

/// The text of a fetched response as a document: an XML MIME type is
/// parsed as XML (an SVG document), `text/html` as HTML. Other types and
/// oversized bodies are unusable (`None`).
fn decode_external_svg_document(content_type: &str, body: &[u8]) -> Option<(String, bool)> {
    if body.len() > MAX_DOCUMENT_BYTES {
        return None;
    }
    let essence = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if essence == "text/xml" || essence == "application/xml" || essence.ends_with("+xml") {
        Some((
            crate::document_encoding::decode_xml(body, content_type).text,
            true,
        ))
    } else if essence == "text/html" {
        let decoded = crate::document_encoding::decode_html(
            body,
            crate::document_encoding::SniffContext {
                content_type,
                container: None,
                local_file: false,
            },
        );
        Some((decoded.text, false))
    } else {
        None
    }
}

impl Store {
    fn insert(&mut self, key: &ExternalSvgKey, outcome: ExternalSvgOutcome) {
        self.next_generation += 1;
        let stored = match outcome {
            ExternalSvgOutcome::Document(text, xml) if text.len() <= MAX_DOCUMENT_BYTES => {
                Stored::Loaded {
                    generation: self.next_generation,
                    text: text.into(),
                    xml,
                }
            }
            ExternalSvgOutcome::NotDocument => Stored::NotDocument,
            _ => Stored::Failed,
        };
        let size = |stored: &Stored| match stored {
            Stored::Loaded { text, .. } => text.len(),
            Stored::NotDocument | Stored::Failed => 0,
        };
        if let Some(old) = self.entries.remove(key) {
            self.bytes -= size(&old);
            self.order.retain(|entry| entry != key);
        }
        self.bytes += size(&stored);
        self.entries.insert(key.clone(), stored);
        self.order.push_back(key.clone());
        while self.order.len() > MAX_DOCUMENTS || self.bytes > MAX_TOTAL_BYTES {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if let Some(old) = self.entries.remove(&oldest) {
                self.bytes -= size(&old);
            }
        }
    }
}

/// Record the outcome of a request. Rendering that read the store before is
/// invalidated (the external SVG resource revision advances).
pub(crate) fn record_external_svg_document(key: &ExternalSvgKey, outcome: ExternalSvgOutcome) {
    #[cfg(test)]
    let _inputs = crate::layout2::global_layout_input_change();
    STORE.lock().unwrap().insert(key, outcome);
    super::SVG_SPRITE_REVISION.fetch_add(1, std::sync::atomic::Ordering::Release);
}

/// Whether the store holds an outcome for `key`.
pub(crate) fn external_svg_document_known(key: &ExternalSvgKey) -> bool {
    STORE.lock().unwrap().entries.contains_key(key)
}

/// A parsed external document.
pub(crate) struct ExternalSvgDocument {
    pub(crate) dom: Dom,
    /// The document's URL (its resources resolve against it).
    pub(crate) url: url::Url,
}

impl ExternalSvgDocument {
    /// The element a URL fragment names: the first element in tree order
    /// with that ID (SVG 2 #URLReference; the fragment percent-decoded).
    pub(crate) fn target(&self, fragment: &str) -> Option<NodeId> {
        self.dom
            .svg_resource_target(self.dom.document(), &format!("#{fragment}"), None)
    }
}

/// What a reference to an external document finds.
pub(crate) enum ExternalSvg {
    /// Not loaded yet; the arena requested it.
    Pending,
    /// The fetch failed, or the document is unusable.
    Failed,
    /// The resource is not a document (a `mask-image` URL is then an image).
    NotDocument,
    Loaded(Rc<ExternalSvgDocument>),
}

/// Identity of a parsed document on this thread.
#[derive(Clone, PartialEq, Eq)]
enum ParsedKey {
    Stored(u64),
    Data(Rc<str>),
}

thread_local! {
    static PARSED: std::cell::RefCell<VecDeque<(ParsedKey, Rc<ExternalSvgDocument>)>> =
        const { std::cell::RefCell::new(VecDeque::new()) };
}

fn parsed(
    key: ParsedKey,
    url: &url::Url,
    text: impl FnOnce() -> ExternalSvgOutcome,
) -> ExternalSvg {
    if let Some(document) = PARSED.with_borrow(|parsed| {
        parsed
            .iter()
            .find(|(candidate, _)| *candidate == key)
            .map(|(_, document)| document.clone())
    }) {
        return ExternalSvg::Loaded(document);
    }
    let (text, xml) = match text() {
        ExternalSvgOutcome::Document(text, xml) => (text, xml),
        ExternalSvgOutcome::NotDocument => return ExternalSvg::NotDocument,
        ExternalSvgOutcome::Failed => return ExternalSvg::Failed,
    };
    let dom = if xml {
        Dom::parse_xml_page(&text, "image/svg+xml")
    } else {
        Dom::parse_document(&text)
    };
    let document = Rc::new(ExternalSvgDocument {
        dom,
        url: url.clone(),
    });
    PARSED.with_borrow_mut(|parsed| {
        if parsed.len() >= MAX_PARSED {
            parsed.pop_front();
        }
        parsed.push_back((key, document.clone()));
    });
    ExternalSvg::Loaded(document)
}

impl Dom {
    /// The external document a CSS `url()` reference from `node` names
    /// (`url`, absolute; its fragment is ignored here). `document_url` is
    /// the URL of `node`'s document as layout knows it: its origin keys the
    /// fetch. An unknown document is requested from this arena.
    pub(crate) fn external_svg_document(
        &self,
        node: NodeId,
        url: &url::Url,
        document_url: &url::Url,
    ) -> ExternalSvg {
        let mut resource = url.clone();
        resource.set_fragment(None);
        match resource.scheme() {
            "data" => {
                let source: Rc<str> = resource.as_str().into();
                return parsed(ParsedKey::Data(source.clone()), &resource, || {
                    let Some((meta, _)) = source
                        .strip_prefix("data:")
                        .and_then(|rest| rest.split_once(','))
                    else {
                        return ExternalSvgOutcome::Failed;
                    };
                    let content_type = meta.split(';').next().unwrap_or("").trim();
                    let content_type = if content_type.is_empty() {
                        "text/plain"
                    } else {
                        content_type
                    };
                    let body = crate::img::decode_data_url(&source);
                    ExternalSvgOutcome::of_response(
                        body.as_deref().map(|body| (content_type, body)),
                    )
                });
            }
            "http" | "https" => {}
            _ => return ExternalSvg::Failed,
        }
        let key = ExternalSvgKey {
            partition: document_url.origin().ascii_serialization(),
            url: resource.to_string(),
            restricted: self.resource_cookies_restricted(node),
        };
        let stored = STORE.lock().unwrap().entries.get(&key).cloned();
        match stored {
            Some(Stored::Loaded {
                generation,
                text,
                xml,
            }) => parsed(ParsedKey::Stored(generation), &resource, || {
                ExternalSvgOutcome::Document(text.to_string(), xml)
            }),
            Some(Stored::NotDocument) => ExternalSvg::NotDocument,
            Some(Stored::Failed) => ExternalSvg::Failed,
            None => {
                let mut requests = self.svg_resource_requests.borrow_mut();
                if requests.requested.insert(key.clone()) {
                    requests.pending.push(ExternalSvgRequest {
                        key,
                        url: resource,
                        node,
                    });
                }
                ExternalSvg::Pending
            }
        }
    }

    /// External documents layout found unknown since the last call, each
    /// returned once, for the page actor to fetch.
    pub(crate) fn take_svg_resource_requests(&self) -> Vec<ExternalSvgRequest> {
        std::mem::take(&mut self.svg_resource_requests.borrow_mut().pending)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(url: &str) -> ExternalSvgKey {
        ExternalSvgKey {
            partition: String::from("https://page.test"),
            url: url.to_string(),
            restricted: false,
        }
    }

    #[test]
    fn documents_are_requested_once_and_resolve_after_they_are_recorded() {
        let dom = Dom::parse_document("<div id=a></div>");
        let node = dom.get_by_id("a").unwrap();
        let page = url::Url::parse("https://page.test/index.html").unwrap();
        let url = url::Url::parse("https://cdn.test/external-store-test.svg#f").unwrap();
        assert!(matches!(
            dom.external_svg_document(node, &url, &page),
            ExternalSvg::Pending
        ));
        assert!(matches!(
            dom.external_svg_document(node, &url, &page),
            ExternalSvg::Pending
        ));
        let requests = dom.take_svg_resource_requests();
        assert_eq!(requests.len(), 1, "requested once");
        assert_eq!(
            requests[0].key,
            key("https://cdn.test/external-store-test.svg")
        );
        assert_eq!(
            requests[0].url.as_str(),
            "https://cdn.test/external-store-test.svg"
        );
        let revision = super::super::svg_sprite_revision();
        record_external_svg_document(
            &requests[0].key,
            ExternalSvgOutcome::of_response(Some((
                "image/svg+xml; charset=utf-8",
                br#"<svg xmlns="http://www.w3.org/2000/svg"><filter id="f"/></svg>"#,
            ))),
        );
        assert!(super::super::svg_sprite_revision() > revision);
        let ExternalSvg::Loaded(document) = dom.external_svg_document(node, &url, &page) else {
            panic!("loaded");
        };
        let filter = document.target("f").unwrap();
        assert_eq!(document.dom.tag_name(filter), Some("filter"));
        assert!(document.target("missing").is_none());
        // Another origin's documents are its own.
        let other = url::Url::parse("https://other.test/").unwrap();
        assert!(matches!(
            dom.external_svg_document(node, &url, &other),
            ExternalSvg::Pending
        ));
        // A network error is remembered as a failure.
        record_external_svg_document(
            &key("https://cdn.test/external-store-fail.svg"),
            ExternalSvgOutcome::of_response(None),
        );
        // A response that is no document (an image) is remembered as such.
        record_external_svg_document(
            &key("https://cdn.test/external-store-image.png"),
            ExternalSvgOutcome::of_response(Some(("image/png", b"\x89PNG"))),
        );
        let image = url::Url::parse("https://cdn.test/external-store-image.png#f").unwrap();
        assert!(matches!(
            dom.external_svg_document(node, &image, &page),
            ExternalSvg::NotDocument
        ));
        let failed = url::Url::parse("https://cdn.test/external-store-fail.svg#f").unwrap();
        assert!(matches!(
            dom.external_svg_document(node, &failed, &page),
            ExternalSvg::Failed
        ));
    }

    #[test]
    fn data_and_file_documents_need_no_fetch() {
        let dom = Dom::parse_document("<div id=a></div>");
        let node = dom.get_by_id("a").unwrap();
        let page = url::Url::parse("https://page.test/").unwrap();
        let data = url::Url::parse(
            "data:image/svg+xml,%3Csvg%20xmlns%3D%22http%3A%2F%2Fwww.w3.org%2F2000%2Fsvg%22%3E%3CclipPath%20id%3D%22c%22%2F%3E%3C%2Fsvg%3E#c",
        )
        .unwrap();
        let ExternalSvg::Loaded(document) = dom.external_svg_document(node, &data, &page) else {
            panic!("a data: document loads synchronously");
        };
        assert!(document.target("c").is_some());
        let file = url::Url::parse("file:///tmp/defs.svg#c").unwrap();
        assert!(matches!(
            dom.external_svg_document(node, &file, &page),
            ExternalSvg::Failed
        ));
        let text = url::Url::parse("data:text/plain,hello#c").unwrap();
        assert!(matches!(
            dom.external_svg_document(node, &text, &page),
            ExternalSvg::NotDocument
        ));
        assert!(dom.take_svg_resource_requests().is_empty());
    }

    #[test]
    fn the_store_is_bounded() {
        let mut store = Store::default();
        for index in 0..MAX_DOCUMENTS + 4 {
            store.insert(
                &key(&format!("https://cdn.test/bounded-{index}.svg")),
                ExternalSvgOutcome::Document(
                    String::from("<svg xmlns=\"http://www.w3.org/2000/svg\"/>"),
                    true,
                ),
            );
        }
        assert_eq!(store.entries.len(), MAX_DOCUMENTS);
        assert_eq!(store.order.len(), MAX_DOCUMENTS);
        assert!(
            !store
                .entries
                .contains_key(&key("https://cdn.test/bounded-0.svg"))
        );
        let large = "x".repeat(MAX_DOCUMENT_BYTES / 2);
        for index in 0..8 {
            store.insert(
                &key(&format!("https://cdn.test/large-{index}.svg")),
                ExternalSvgOutcome::Document(large.clone(), true),
            );
        }
        assert!(store.bytes <= MAX_TOTAL_BYTES);
        // Too large a document is unusable.
        store.insert(
            &key("https://cdn.test/huge.svg"),
            ExternalSvgOutcome::Document("x".repeat(MAX_DOCUMENT_BYTES + 1), true),
        );
        assert!(matches!(
            store.entries.get(&key("https://cdn.test/huge.svg")),
            Some(Stored::Failed)
        ));
    }
}
