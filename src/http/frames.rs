//! Inert child navigables use the same navigation reservations as live pages.
//! HTML e5071a20 #the-iframe-element / #create-navigation-params-by-fetching
//! and Service Workers 92aba3be #on-fetch-request-algorithm require one
//! environment per navigable, even when several frames request the same URL.
//! The records below also survive promotion to the page actor: discovering a
//! child script must not cause another navigation or service-worker fetch.

use super::*;
use crate::js::DocumentClient;
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};

type FramePath = Vec<usize>;

pub(super) struct DeferredOptions {
    pub storage: crate::js::WebStorage,
    pub screen_position: (i32, i32),
    pub fullscreen_supported: bool,
    pub traversable: crate::js::TraversableState,
    pub document_creation_time: Option<f64>,
}

/// No root DocumentClient is stored in env: that would form an ownership
/// cycle with the native owner retaining this bundle.
pub(crate) struct DeferredPage {
    pub html: String,
    pub env: crate::js::PageEnv,
}

pub(crate) fn resume_static_page(
    owner: &DocumentClient,
    presentation: crate::js::TraversableState,
) -> Option<LivePage> {
    let DeferredPage { html, mut env } = owner.take_deferred_page()?;
    env.service_client = Some(owner.clone());
    env.traversable = presentation;
    env.resume_static = true;
    let (handle, events) = crate::js::spawn_page(html, env);
    Some(LivePage {
        handle,
        events,
        traversable: presentation,
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Attributes {
    tag: String,
    src: Option<String>,
    srcdoc: Option<String>,
    sandbox: Option<String>,
    referrer_policy: Option<String>,
}

impl Attributes {
    fn of(dom: &crate::dom::Dom, node: usize) -> Self {
        Self {
            tag: dom.tag_name(node).unwrap_or("").into(),
            src: dom.attr(node, "src").map(str::to_owned),
            srcdoc: dom.attr(node, "srcdoc").map(str::to_owned),
            sandbox: dom.attr(node, "sandbox").map(str::to_owned),
            referrer_policy: dom.attr(node, "referrerpolicy").map(str::to_owned),
        }
    }

    fn sandboxed(&self) -> bool {
        self.tag == "iframe"
            && self.sandbox.as_ref().is_some_and(|flags| {
                !flags
                    .split_ascii_whitespace()
                    .any(|flag| flag.eq_ignore_ascii_case("allow-same-origin"))
            })
    }
}

struct FrameDocument {
    requested: Option<Url>,
    url: Url,
    fallback_base: Url,
    base: Url,
    markup: String,
    encoding: &'static encoding_rs::Encoding,
    policy: crate::referrer_policy::ReferrerPolicy,
    initial: Option<DocumentClient>,
    navigation_client: Option<DocumentClient>,
    committed: Mutex<Option<DocumentClient>>,
    sheets: Mutex<Vec<PageSheet>>,
    created: f64,
    // A failed prefetch is a completed navigation attempt as well. The
    // separate flag distinguishes it from a local srcdoc/data navigation.
    network: bool,
    response: Mutex<Option<FetchResponseDetails>>,
    sandboxed: bool,
    restricted: bool,
}

/// A path identifies a particular container in a particular document, not a
/// URL. Arc ownership keeps the same native environments through font/SVG
/// re-layout and through the optional actor handoff.
#[derive(Clone, Default)]
pub(crate) struct FrameDocuments {
    records: BTreeMap<FramePath, Arc<FrameDocument>>,
}

impl FrameDocuments {
    #[cfg(test)]
    pub(super) fn new() -> Self {
        Self::default()
    }

    pub(super) fn scripted(&self) -> bool {
        self.records.values().any(|record| {
            record.markup.to_ascii_lowercase().contains("<script")
                || record
                    .navigation_client
                    .as_ref()
                    .is_some_and(|owner| owner.client().controller().is_some())
        })
    }

    pub(super) fn documents(
        &self,
    ) -> impl Iterator<
        Item = (
            &Url,
            &str,
            &'static encoding_rs::Encoding,
            crate::referrer_policy::ReferrerPolicy,
            Option<DocumentClient>,
            &[usize],
        ),
    > {
        self.records.iter().map(|(path, record)| {
            (
                &record.fallback_base,
                record.markup.as_str(),
                record.encoding,
                record.policy,
                commit(record),
                path.as_slice(),
            )
        })
    }

    pub(super) fn set_sheets(&self, path: &[usize], sheets: Vec<PageSheet>) {
        if let Some(record) = self.records.get(path) {
            *record.sheets.lock().unwrap() = sheets;
        }
    }

    #[cfg(test)]
    pub(super) fn from_metadata(
        entries: impl IntoIterator<
            Item = (
                Vec<usize>,
                (
                    Url,
                    String,
                    &'static encoding_rs::Encoding,
                    crate::referrer_policy::ReferrerPolicy,
                ),
            ),
        >,
    ) -> Self {
        Self {
            records: entries
                .into_iter()
                .map(|(path, (url, markup, encoding, policy))| {
                    let base = base_with_doc_base(&markup, &url);
                    (
                        path,
                        Arc::new(FrameDocument {
                            requested: Some(url.clone()),
                            fallback_base: url.clone(),
                            url,
                            base,
                            markup,
                            encoding,
                            policy,
                            initial: None,
                            navigation_client: None,
                            committed: Mutex::default(),
                            sheets: Mutex::default(),
                            network: false,
                            created: crate::performance::now_ms(),
                            response: Mutex::default(),
                            sandboxed: false,
                            restricted: false,
                        }),
                    )
                })
                .collect(),
        }
    }
}

struct Source {
    ordinal: usize,
    attributes: Attributes,
    url: Option<Url>,
}

fn scan(
    html: &str,
    base: &Url,
    page: &Url,
    ancestors: &[String],
    encoding: &'static encoding_rs::Encoding,
) -> Vec<Source> {
    let dom = crate::dom::Dom::parse_document(html);
    frame_nodes(&dom, dom.document())
        .into_iter()
        .enumerate()
        .map(|(ordinal, node)| {
            let attributes = Attributes::of(&dom, node);
            let url = if attributes.srcdoc.is_none() {
                attributes
                    .src
                    .as_deref()
                    .map(str::trim)
                    .filter(|src| !src.is_empty())
                    .and_then(|src| resolve_frame_src(src, base, page, ancestors, encoding))
            } else {
                None
            };
            Source {
                ordinal,
                attributes,
                url,
            }
        })
        .collect()
}

fn frame_nodes(dom: &crate::dom::Dom, document: usize) -> Vec<usize> {
    dom.flat_descendants(document)
        .into_iter()
        .filter(|&node| {
            dom.owner_document(node) == Some(document)
                && matches!(dom.tag_name(node), Some("iframe" | "frame"))
        })
        .collect()
}

fn storage_key(origin: &url::Origin, cookies: &CookieContext) -> String {
    let partition = cookies.storage_partition();
    if !origin.is_tuple() || partition == "null" {
        "null".into()
    } else if partition.is_empty() {
        origin.ascii_serialization()
    } else {
        format!("{}^{partition}", origin.ascii_serialization())
    }
}

fn window_state(
    parent: &DocumentClient,
    origin: &url::Origin,
    policy: Option<crate::referrer_policy::ReferrerPolicy>,
) -> crate::service_worker::WindowState {
    use crate::referrer_policy::ReferrerPolicy;
    let parent_origin = parent.client().origin();
    let parent_serialized = parent_origin.ascii_serialization();
    let info = parent.client().window_state();
    let mut masked = policy == Some(ReferrerPolicy::NoReferrer)
        || policy == Some(ReferrerPolicy::SameOrigin) && *origin != parent_origin;
    let mut ancestors = vec![if masked {
        "null".into()
    } else {
        parent_serialized.clone()
    }];
    for ancestor in info.ancestor_origins {
        if masked && ancestor == parent_serialized {
            ancestors.push("null".into());
        } else {
            ancestors.push(ancestor);
            masked = false;
        }
    }
    crate::service_worker::WindowState {
        frame_type: "nested".into(),
        visibility_state: info.visibility_state,
        ancestor_origins: ancestors,
        ..Default::default()
    }
}

fn reserve_local(
    parent: Option<&DocumentClient>,
    url: Url,
    origin: url::Origin,
    cookies: &CookieContext,
    local_file_access: bool,
    policy: Option<crate::referrer_policy::ReferrerPolicy>,
) -> Option<DocumentClient> {
    let parent = parent?;
    let registry = parent.client().registry()?;
    let client = registry
        .reserve_client_with_origin(storage_key(&origin, cookies), url, origin.clone(), false)
        .ok()?;
    client.set_secure_context(parent.client().secure_context());
    client.set_local_file_access(local_file_access);
    if origin.is_tuple() && origin == parent.client().origin() {
        client.inherit_controller(&parent.client());
    }
    client.configure(
        crate::service_worker::ClientKind::Window,
        window_state(parent, &origin, policy),
    );
    Some(DocumentClient::new(client))
}

struct Pending {
    path: FramePath,
    markup: String,
    base: Url,
    url: Url,
    ancestors: Vec<String>,
    encoding: &'static encoding_rs::Encoding,
    policy: crate::referrer_policy::ReferrerPolicy,
    owner: Option<DocumentClient>,
    sandboxed: bool,
    restricted: bool,
    top_client: Url,
}

fn client_url(owner: Option<&DocumentClient>, creation: &Url) -> Url {
    owner
        .and_then(crate::trust_js_backend::service_worker_fetch::for_document)
        .and_then(|context| context.client_url(creation))
        .unwrap_or_else(|| {
            owner.map_or_else(
                || creation.clone(),
                |owner| {
                    let client = owner.client();
                    if client.origin().is_tuple() {
                        Url::parse(&client.origin().ascii_serialization()).unwrap()
                    } else if client.local_file_access() {
                        creation.clone()
                    } else {
                        Url::parse("about:blank").unwrap()
                    }
                },
            )
        })
}

/// Fetch a complete finite inert frame tree without sharing the non-Send DOM
/// across an await. Every occurrence owns an initial blank and its reserved
/// navigation environment; no URL de-duplication changes fetch semantics.
pub(super) async fn prefetch_frame_documents(
    html: &str,
    base: &Url,
    page: &Url,
    encoding: &'static encoding_rs::Encoding,
    policy: crate::referrer_policy::ReferrerPolicy,
    owner: Option<&DocumentClient>,
) -> FrameDocuments {
    let mut result = FrameDocuments::default();
    let mut queue = VecDeque::from([Pending {
        path: Vec::new(),
        markup: html.into(),
        base: base.clone(),
        url: page.clone(),
        ancestors: vec![strip_fragment(page.as_str()).into()],
        encoding,
        policy,
        owner: owner.cloned(),
        sandboxed: false,
        restricted: false,
        top_client: client_url(owner, page),
    }]);
    while let Some(parent) = queue.pop_front() {
        let sources = scan(
            &parent.markup,
            &parent.base,
            page,
            &parent.ancestors,
            parent.encoding,
        );
        let fetched = futures::stream::iter(sources.into_iter().map(|source| {
            let parent = &parent;
            async move { fetch_frame(source, parent).await }
        }))
        .buffered(PREFETCH_CONCURRENCY)
        .collect::<Vec<_>>()
        .await;
        for (ordinal, record) in fetched {
            let mut path = parent.path.clone();
            path.push(ordinal);
            let mut ancestors = parent.ancestors.clone();
            if !matches!(record.url.scheme(), "about") {
                ancestors.push(strip_fragment(record.url.as_str()).into());
            }
            queue.push_back(Pending {
                path: path.clone(),
                markup: record.markup.clone(),
                base: record.base.clone(),
                url: record.url.clone(),
                ancestors,
                encoding: record.encoding,
                policy: record.policy,
                owner: record
                    .navigation_client
                    .clone()
                    .or_else(|| record.initial.clone()),
                sandboxed: record.sandboxed,
                restricted: record.restricted,
                top_client: parent.top_client.clone(),
            });
            result.records.insert(path, Arc::new(record));
        }
    }
    result
}

async fn fetch_frame(source: Source, parent: &Pending) -> (usize, FrameDocument) {
    let Source {
        ordinal,
        attributes,
        url: requested,
    } = source;
    let sandboxed = parent.sandboxed || attributes.sandboxed();
    let parent_origin = parent
        .owner
        .as_ref()
        .map_or_else(|| parent.url.origin(), |owner| owner.client().origin());
    let origin = if sandboxed {
        url::Origin::new_opaque()
    } else {
        parent_origin
    };
    let parent_client_url = client_url(parent.owner.as_ref(), &parent.url);
    let mut cookies = CookieContext::subresource(&parent.top_client);
    cookies.client = Some(parent_client_url.clone());
    cookies.cross_site_ancestor = parent.restricted
        || sandboxed
        || !crate::site_storage::same_site(&parent.top_client, &parent_client_url);
    let attribute_policy = attributes
        .referrer_policy
        .as_deref()
        .and_then(crate::referrer_policy::ReferrerPolicy::parse);
    let inherited_file = !sandboxed
        && parent
            .owner
            .as_ref()
            .is_some_and(|owner| owner.client().local_file_access());
    let initial = reserve_local(
        parent.owner.as_ref(),
        Url::parse("about:blank").unwrap(),
        origin.clone(),
        &cookies,
        inherited_file,
        attribute_policy,
    );
    // HTML creates the child's initial Document before starting its first
    // navigation. A worker may inspect replacesClientId during that fetch.
    if let Some(initial) = &initial {
        initial.client().set_execution_ready();
    }
    let mut record = FrameDocument {
        requested: requested.clone(),
        url: Url::parse("about:blank").unwrap(),
        fallback_base: parent.base.clone(),
        base: parent.base.clone(),
        markup: "<!doctype html><html><head></head><body></body></html>".into(),
        encoding: encoding_rs::UTF_8,
        policy: parent.policy,
        initial,
        navigation_client: None,
        committed: Mutex::default(),
        sheets: Mutex::default(),
        created: crate::performance::now_ms(),
        network: false,
        response: Mutex::default(),
        sandboxed,
        restricted: cookies.cross_site_ancestor,
    };
    if let Some(srcdoc) = attributes.srcdoc {
        record.created = crate::performance::now_ms();
        record.url = Url::parse("about:srcdoc").unwrap();
        record.policy = document_referrer_policy(&srcdoc, parent.policy);
        record.markup = srcdoc;
        record.base = base_with_doc_base(&record.markup, &record.fallback_base);
    } else if let Some(url) = requested {
        if url.scheme() == "data" {
            if let Some(markup) = data_frame_document(&url) {
                record.created = crate::performance::now_ms();
                record.fallback_base = url.clone();
                record.base = base_with_doc_base(&markup, &url);
                record.policy = document_referrer_policy(&markup, parent.policy);
                record.markup = markup;
                record.url = url;
            }
        } else {
            record.network = true;
            let policy = attribute_policy.unwrap_or(parent.policy);
            let context = parent
                .owner
                .as_ref()
                .and_then(crate::trust_js_backend::service_worker_fetch::for_document);
            let client_url = parent_client_url;
            let mut request = Request::get(url);
            set_fetch_metadata(
                &mut request,
                &client_url,
                if attributes.tag == "frame" {
                    "frame"
                } else {
                    "iframe"
                },
                "navigate",
            );
            cookies.client = Some(client_url);
            cookies.cross_site_ancestor |=
                !crate::site_storage::same_site(&parent.top_client, &request.url);
            request.cookie_context = Some(cookies.clone());
            request.web.replaces_client_id = record
                .initial
                .as_ref()
                .map_or_else(String::new, |owner| owner.client().id.clone());
            set_request_referrer(&mut request, Some(&parent.url), policy);
            request.service_worker = context.and_then(|context| context.for_navigation(&request));
            if let Ok(mut details) = fetch_with_metadata(&request, policy).await {
                let response = &mut details.response;
                let current = record.initial.as_ref().map(|owner| owner.client());
                settle_no_content_navigation(response, current.as_ref());
                let media = response.content_type.split(';').next().unwrap_or("").trim();
                if !matches!(response.status, 204 | 205)
                    && (media.eq_ignore_ascii_case("text/html")
                        || media.eq_ignore_ascii_case("application/xhtml+xml"))
                {
                    // HTML #initialise-the-document-object creates the
                    // response Document after fetching. The initial blank
                    // above has a separate, earlier creation transition.
                    record.created = crate::performance::now_ms();
                    let response_origin = response
                        .service_client
                        .as_ref()
                        .map_or_else(|| response.url.origin(), |owner| owner.client().origin());
                    let container =
                        (!sandboxed && response_origin == origin).then_some(parent.encoding);
                    let decoded = decode_nested_document_body(
                        &response.url,
                        &response.content_type,
                        &response.body,
                        container,
                    );
                    record.url = response.url.clone();
                    record.fallback_base = response.url.clone();
                    record.base = base_with_doc_base(&decoded.text, &response.url);
                    record.policy =
                        document_referrer_policy(&decoded.text, response_referrer_policy(response));
                    record.markup = decoded.text;
                    record.encoding = decoded.encoding;
                    record.navigation_client = response.service_client.clone();
                }
                *record.response.lock().unwrap() = Some(details);
            }
        }
    }
    // HTML #determining-the-origin creates a new opaque origin for every
    // sandboxed Document, independently of the initial blank's token.
    let sandboxed_origin = sandboxed.then(url::Origin::new_opaque);
    if record.navigation_client.is_none() && record.url.as_str() != "about:blank" {
        let origin = if sandboxed {
            sandboxed_origin.clone().unwrap()
        } else if record.url.scheme() == "about" {
            origin.clone()
        } else {
            record.url.origin()
        };
        record.navigation_client = reserve_local(
            parent.owner.as_ref(),
            record.url.clone(),
            origin,
            &cookies,
            !sandboxed
                && (crate::file::is_local_url(&record.url)
                    || record.url.scheme() == "about" && inherited_file),
            attribute_policy,
        );
    }
    if let Some(owner) = &record.navigation_client {
        if let Some(origin) = sandboxed_origin {
            owner.client().set_document_origin(origin);
        }
        let client = owner.client();
        client.set_local_file_access(
            !sandboxed
                && (crate::file::is_local_url(&record.url)
                    || record.url.scheme() == "about" && inherited_file),
        );
        if let Some(parent) = &parent.owner {
            client.configure(
                crate::service_worker::ClientKind::Window,
                window_state(parent, &client.origin(), attribute_policy),
            );
        }
        let environment_url = client_url(Some(owner), &record.url);
        record.restricted |= !client.origin().is_tuple()
            || !crate::site_storage::same_site(&parent.top_client, &environment_url);
        cookies.cross_site_ancestor = record.restricted;
        client.set_storage_key(storage_key(&client.origin(), &cookies));
    }
    (ordinal, record)
}

fn commit(record: &FrameDocument) -> Option<DocumentClient> {
    let mut committed = record.committed.lock().unwrap();
    if let Some(owner) = committed.as_ref() {
        return Some(owner.clone());
    }
    let owner = match (&record.initial, &record.navigation_client) {
        (Some(initial), Some(navigation))
            if !record.sandboxed && initial.client().origin() == navigation.client().origin() =>
        {
            initial
                .client()
                .adopt_initial_navigation(&navigation.client());
            initial.client().set_document_url(record.url.clone());
            initial.client().configure(
                crate::service_worker::ClientKind::Window,
                navigation.client().window_state(),
            );
            navigation.discard();
            initial.clone()
        }
        (_, Some(navigation)) => {
            if let Some(initial) = &record.initial {
                initial.discard();
            }
            navigation.clone()
        }
        (Some(initial), None) => initial.clone(),
        (None, None) => return None,
    };
    owner.client().set_execution_ready();
    *committed = Some(owner.clone());
    Some(owner)
}

fn attach_sheets(dom: &mut crate::dom::Dom, document: usize, _base: &Url, sheets: &[PageSheet]) {
    // Imported and linked sheets stay scoped to this owning Document. Their
    // original source remains intact when a deferred page gains a CSSOM.
    dom.attach_document_sheets(document, sheets);
}

pub(super) fn install_page_frames_with_clients(
    dom: &mut crate::dom::Dom,
    page: &Url,
    fetched: &FrameDocuments,
    owner: Option<&DocumentClient>,
) {
    let root = dom.document();
    let root_origin = owner.map_or_else(|| page.origin(), |owner| owner.client().origin());
    let base = base_with_doc_base(&dom.serialize(root), page);
    let mut queue = VecDeque::from([(
        root,
        Vec::new(),
        owner.cloned(),
        base,
        root_origin,
        false,
        false,
    )]);
    while let Some((
        document,
        path,
        parent,
        parent_base,
        parent_origin,
        parent_sandboxed,
        parent_restricted,
    )) = queue.pop_front()
    {
        let nodes = frame_nodes(dom, document);
        for (ordinal, node) in nodes.into_iter().enumerate() {
            let mut path = path.clone();
            path.push(ordinal);
            let record = fetched.records.get(&path);
            let attributes = Attributes::of(dom, node);
            let sandboxed = parent_sandboxed || attributes.sandboxed();
            let markup = record
                .map(|record| record.markup.clone())
                .or_else(|| attributes.srcdoc.clone())
                .unwrap_or_else(|| "<!doctype html><html><head></head><body></body></html>".into());
            let fallback_base = record.map_or_else(
                || parent_base.clone(),
                |record| record.fallback_base.clone(),
            );
            let base = record.map_or_else(
                || base_with_doc_base(&markup, &fallback_base),
                |record| record.base.clone(),
            );
            if dom
                .install_frame_document(node, &markup, fallback_base.as_str())
                .is_none()
            {
                continue;
            }
            let Some(child_document) = dom.frame_document(node) else {
                continue;
            };
            dom.set_document_encoding(
                child_document,
                record.map_or(encoding_rs::UTF_8, |record| record.encoding),
            );
            let url = record.map_or_else(
                || {
                    Url::parse(if attributes.srcdoc.is_some() {
                        "about:srcdoc"
                    } else {
                        "about:blank"
                    })
                    .unwrap()
                },
                |record| record.url.clone(),
            );
            let mut origin = if sandboxed {
                url::Origin::new_opaque()
            } else if url.scheme() == "about" {
                parent_origin.clone()
            } else {
                url.origin()
            };
            let restricted = parent_restricted
                || sandboxed
                || url.scheme() != "about" && !crate::site_storage::same_site(page, &url)
                || record.is_some_and(|record| record.restricted);
            dom.set_frame_cookie_restriction(node, restricted);
            if let Some(record) = record {
                attach_sheets(
                    dom,
                    child_document,
                    &record.base,
                    &record.sheets.lock().unwrap(),
                );
            }
            let mut child = record.and_then(|record| commit(record));
            if child.is_none()
                && let Some(parent) = &parent
            {
                let mut cookies = CookieContext::subresource(page);
                cookies.cross_site_ancestor = restricted;
                let policy = attributes
                    .referrer_policy
                    .as_deref()
                    .and_then(crate::referrer_policy::ReferrerPolicy::parse);
                child = parent
                    .static_child(
                        node,
                        url.clone(),
                        origin.clone(),
                        storage_key(&origin, &cookies),
                        !sandboxed
                            && (crate::file::is_local_url(&url)
                                || url.scheme() == "about" && parent.client().local_file_access()),
                        window_state(parent, &origin, policy),
                    )
                    .ok();
            }
            if let (Some(parent), Some(child)) = (&parent, &child) {
                parent.adopt_child((1u64 << 63) | node as u64, child);
                origin = child.client().origin();
            }
            queue.push_back((
                child_document,
                path,
                child,
                base,
                origin,
                sandboxed,
                restricted,
            ));
        }
    }
}

#[cfg(test)]
pub(super) fn install_page_frames(dom: &mut crate::dom::Dom, page: &Url, fetched: &FrameDocuments) {
    install_page_frames_with_clients(dom, page, fetched, None);
}

/// A promoted page binds prefetches to the exact parser-created elements.
/// An inserted/reordered frame can therefore never consume another frame's
/// response, even when its URL is identical. Completed handoffs relinquish
/// native ownership; an unused response cannot keep a retired worker in use.
struct ActorDocument {
    markup: String,
    requested: Option<Url>,
    network: bool,
    initial: Option<DocumentClient>,
    response: Option<FetchResponseDetails>,
    resumed: Option<ResumedDocument>,
    sheets: Vec<PageSheet>,
}

pub(crate) struct ResumedDocument {
    pub owner: Option<DocumentClient>,
    pub url: Url,
    pub base: Url,
    pub markup: String,
    pub encoding: &'static encoding_rs::Encoding,
    pub policy: crate::referrer_policy::ReferrerPolicy,
    pub referrer: String,
    pub timing: Option<Box<FetchTiming>>,
    pub created: f64,
}

#[derive(Default)]
pub(crate) struct ActorFrames {
    records: BTreeMap<FramePath, ActorDocument>,
    bindings: HashMap<usize, (FramePath, Attributes)>,
    navigation_used: HashSet<usize>,
    restoring: HashSet<usize>,
}

impl ActorFrames {
    pub(crate) fn new(documents: FrameDocuments, dom: &crate::dom::Dom, resume: bool) -> Self {
        let records = documents
            .records
            .iter()
            .map(|(path, record)| {
                let response = record.response.lock().unwrap().take();
                let resumed = resume.then(|| ResumedDocument {
                    owner: commit(record),
                    url: record.url.clone(),
                    base: record.fallback_base.clone(),
                    markup: record.markup.clone(),
                    encoding: record.encoding,
                    policy: record.policy,
                    referrer: response
                        .as_ref()
                        .map_or_else(String::new, |response| response.referrer.clone()),
                    timing: response
                        .as_ref()
                        .and_then(|response| response.response.timing.clone()),
                    created: record.created,
                });
                (
                    path.clone(),
                    ActorDocument {
                        markup: record.markup.clone(),
                        requested: record.requested.clone(),
                        network: record.network,
                        initial: if resume { None } else { record.initial.clone() },
                        response: if resume { None } else { response },
                        resumed,
                        sheets: record.sheets.lock().unwrap().clone(),
                    },
                )
            })
            .collect();
        let mut frames = Self {
            records,
            ..Self::default()
        };
        frames.bind(dom, dom.document(), &[]);
        frames
    }

    fn bind(&mut self, dom: &crate::dom::Dom, document: usize, parent: &[usize]) {
        for (ordinal, node) in frame_nodes(dom, document).into_iter().enumerate() {
            let mut path = parent.to_vec();
            path.push(ordinal);
            if self.records.contains_key(&path) {
                self.bindings
                    .insert(node, (path, Attributes::of(dom, node)));
            }
        }
    }

    pub(crate) fn loaded(&mut self, dom: &mut crate::dom::Dom, frame: usize, markup: &str) {
        let Some((path, _)) = self.bindings.get(&frame).cloned() else {
            return;
        };
        let Some(record) = self.records.get(&path) else {
            return;
        };
        if record.markup != markup {
            return;
        }
        if let Some(document) = dom.frame_document(frame) {
            if !record.sheets.is_empty() {
                let base = dom.resource_base_url(document, &Url::parse("about:blank").unwrap());
                attach_sheets(dom, document, &base, &record.sheets);
            }
            self.bind(dom, document, &path);
        }
        // The path binding, rather than the source or owners, is all a later
        // child load needs from this parent.
        if let Some(record) = self.records.get_mut(&path) {
            record.markup.clear();
        }
    }

    pub(crate) fn initial(
        &mut self,
        dom: &crate::dom::Dom,
        frame: usize,
    ) -> Option<DocumentClient> {
        let (path, attributes) = self.bindings.get(&frame)?;
        if *attributes != Attributes::of(dom, frame) {
            return None;
        }
        self.records.get_mut(path)?.initial.take()
    }

    pub(crate) fn navigation(
        &mut self,
        dom: &crate::dom::Dom,
        frame: usize,
        target: &str,
    ) -> Option<Option<FetchResponseDetails>> {
        let (path, attributes) = self.bindings.get(&frame)?;
        let record = self.records.get_mut(path)?;
        if !record.network
            || record.requested.as_ref().map(Url::as_str) != Some(target)
            || *attributes != Attributes::of(dom, frame)
            || !self.navigation_used.insert(frame)
        {
            return None;
        }
        Some(record.response.take())
    }

    pub(crate) fn resume(
        &mut self,
        dom: &crate::dom::Dom,
        frame: usize,
    ) -> Option<ResumedDocument> {
        let (path, attributes) = self.bindings.get(&frame)?;
        if *attributes != Attributes::of(dom, frame) {
            return None;
        }
        let document = self.records.get_mut(path)?.resumed.take()?;
        self.restoring.insert(frame);
        Some(document)
    }

    pub(crate) fn take_restoring(&mut self, frame: usize) -> bool {
        self.restoring.remove(&frame)
    }

    pub(crate) fn removed(&mut self, frame: usize) {
        let Some((path, _)) = self.bindings.remove(&frame) else {
            return;
        };
        self.records
            .retain(|candidate, _| !candidate.starts_with(&path));
        self.bindings
            .retain(|_, (candidate, _)| !candidate.starts_with(&path));
    }
}

#[cfg(test)]
mod tests;
