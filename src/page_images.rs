//! A document's image requests, shared with the frontends that present them.
//!
//! WHATWG HTML (local snapshot e5071a20) #updating-the-image-data makes each
//! image request the element's node document's: its fetch uses that
//! document's settings object as the request client and reports its timing to
//! that document's global (Fetch #fetch-finale, Resource Timing
//! #marking-resource-timing). CSS Images 4 #fetching-images likewise fetches a
//! stylesheet image for the declaration's document. TRust's frontends paint
//! those resources, but they must not repeat the document's requests: this
//! per-page store lets the page actor own every request while a frontend
//! joins the actor's in-flight or completed response by URL.
//!
//! Requests are keyed as HTML keys its list of available images (URL, CORS
//! mode, and the requesting origin) plus the cookie restriction RFC 6265bis
//! #document-requests derives from the document's ancestors, so a CORS image
//! never reuses a no-CORS response and a cookie-restricted frame never joins
//! a request that carried cookies. Presentation consumers join any variant: a
//! frontend only paints bytes the document itself fetched.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};

use futures::future::{BoxFuture, FutureExt as _, Shared};
use url::Url;

use crate::http::{CredentialsMode, FetchTiming, PageTaskScope, Request};
use crate::referrer_policy::ReferrerPolicy;

/// Completed compressed bytes retained for presentation consumers. A frontend
/// decodes each resource once and may decode again only after evicting its own
/// decoded pixels; beyond this budget, the oldest responses are dropped and a
/// later presentation of their URLs refetches outside the document.
const RETAINED_BYTES: usize = 256 * 1024 * 1024;

/// Concurrent image fetches per origin. The document starts every image
/// request when HTML says to (often hundreds at parse time); like other user
/// agents, queue the network work per host instead of opening a connection
/// per image (RFC 9112 §9.4 asks clients to limit simultaneous connections).
/// Queued time counts toward the entry's fetch duration, as in a browser.
const FETCHES_PER_ORIGIN: usize = 8;

/// HTML #the-list-of-available-images keys an image by URL, CORS settings
/// attribute mode and, for a CORS request, the document origin.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ImageRequestKey {
    pub(crate) url: String,
    /// `None` is a no-CORS request; otherwise a CORS request's credentials.
    pub(crate) cors: Option<CredentialsMode>,
    pub(crate) origin: url::Origin,
    pub(crate) cookie_restricted: bool,
}

/// One response obtained by a document image request.
#[derive(Debug)]
pub struct FetchedImage {
    pub status: u16,
    pub body: Arc<[u8]>,
    /// Fetch's URL list, including every redirect hop (CORS-same-origin
    /// determination for no-CORS images).
    pub(crate) url_list: Vec<Url>,
    pub(crate) timing: Option<Box<FetchTiming>>,
}

impl FetchedImage {
    /// Fetch #ok-status. HTML #img-determine-type ignores it for element
    /// images; the terminal and inline SVG consumers still consult it.
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

#[derive(Clone, Debug)]
pub(crate) enum ImageFailure {
    /// A network error. A real network attempt reports opaque timing.
    Network(Option<Box<FetchTiming>>),
    /// The document's fetch group was terminated (HTML #abort-a-document).
    Cancelled,
    /// The request failed a precondition before any fetch (policy, URL).
    Refused,
}

pub(crate) type ImageOutcome = Result<Arc<FetchedImage>, ImageFailure>;
pub(crate) type SharedImageFetch = Shared<BoxFuture<'static, ImageOutcome>>;

/// How a request was satisfied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ImageJoin {
    /// This call started the fetch.
    Started,
    /// An equivalent request started by the owner (a settings context id) is
    /// in flight or complete; this caller shares its download.
    Joined { owner: u64 },
}

/// What a frontend may present for a URL.
#[derive(Debug)]
pub enum Presentation {
    /// The document fetched these bytes.
    Image(Arc<FetchedImage>),
    /// The document's request failed; HTML renders the element as broken.
    Failed,
    /// The document has no request for this URL and will not make one (its
    /// page is no longer live, or the response was evicted): the frontend
    /// obtains presentation bytes itself.
    Unavailable,
}

struct Entry {
    key: ImageRequestKey,
    fetch: SharedImageFetch,
    owner: u64,
}

#[derive(Default)]
struct Inner {
    entries: HashMap<String, Vec<Entry>>,
    waiters: HashMap<String, Vec<tokio::sync::oneshot::Sender<SharedImageFetch>>>,
    /// Completed responses in completion order, with their retained sizes.
    retained: VecDeque<(ImageRequestKey, usize)>,
    retained_bytes: usize,
    /// URLs the document announced it will request later (lazy images
    /// awaiting their resumption steps). Presentation waits only for these.
    expected: HashSet<String>,
    closed: bool,
}

impl Inner {
    fn publish(&mut self, url: &str, fetch: &SharedImageFetch) {
        self.expected.remove(url);
        for waiter in self.waiters.remove(url).unwrap_or_default() {
            let _ = waiter.send(fetch.clone());
        }
    }

    fn retain(&mut self, key: ImageRequestKey, bytes: usize) {
        self.retained_bytes = self.retained_bytes.saturating_add(bytes);
        self.retained.push_back((key, bytes));
        while self.retained_bytes > RETAINED_BYTES && self.retained.len() > 1 {
            let Some((key, bytes)) = self.retained.pop_front() else {
                break;
            };
            self.retained_bytes = self.retained_bytes.saturating_sub(bytes);
            if let Some(entries) = self.entries.get_mut(&key.url) {
                entries.retain(|entry| entry.key != key);
                if entries.is_empty() {
                    self.entries.remove(&key.url);
                }
            }
        }
    }
}

/// Per-page image request store; see the module documentation.
pub struct PageImages {
    inner: Arc<Mutex<Inner>>,
    tasks: Arc<PageTaskScope>,
    origins: Mutex<HashMap<String, Arc<tokio::sync::Semaphore>>>,
}

impl Default for PageImages {
    fn default() -> Self {
        Self::new(Default::default())
    }
}

impl std::fmt::Debug for PageImages {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = self.inner.lock().map_or(0, |inner| inner.entries.len());
        write!(f, "PageImages({count} URLs)")
    }
}

impl PageImages {
    /// A store whose fetches belong to the document's task scope: aborting the
    /// document (HTML #abort-a-document) aborts its image fetches too.
    pub(crate) fn new(tasks: Arc<PageTaskScope>) -> Self {
        Self {
            inner: Default::default(),
            tasks,
            origins: Default::default(),
        }
    }

    fn origin_slots(&self, url: &Url) -> Arc<tokio::sync::Semaphore> {
        let origin = url.origin().ascii_serialization();
        let mut origins = self.origins.lock().unwrap();
        // An idle origin's semaphore (no queued or running fetch) is dropped.
        origins.retain(|_, slots| {
            Arc::strong_count(slots) > 1 || slots.available_permits() < FETCHES_PER_ORIGIN
        });
        origins
            .entry(origin)
            .or_insert_with(|| Arc::new(tokio::sync::Semaphore::new(FETCHES_PER_ORIGIN)))
            .clone()
    }

    /// Join an equivalent request or start this one. `owner` identifies the
    /// requesting settings context so a joiner from another document can tell
    /// that the download it shares was not its own.
    pub(crate) fn fetch(
        &self,
        handle: &tokio::runtime::Handle,
        key: ImageRequestKey,
        owner: u64,
        request: Request,
        policy: ReferrerPolicy,
    ) -> (SharedImageFetch, ImageJoin) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(entry) = inner
            .entries
            .get(&key.url)
            .and_then(|entries| entries.iter().find(|entry| entry.key == key))
        {
            return (
                entry.fetch.clone(),
                ImageJoin::Joined { owner: entry.owner },
            );
        }
        if inner.closed {
            return (
                futures::future::ready(Err(ImageFailure::Cancelled))
                    .boxed()
                    .shared(),
                ImageJoin::Started,
            );
        }
        let (abort, registration) = futures::future::AbortHandle::new_pair();
        self.tasks.track_fetch(abort);
        let store = Arc::downgrade(&self.inner);
        let retained_key = key.clone();
        let slots = self.origin_slots(&request.url);
        let fetch = futures::future::Abortable::new(
            async move {
                let _slot = slots.acquire_owned().await;
                match crate::http::fetch_with_timing(&request, policy).await {
                    Ok(details) => {
                        let response = details.response;
                        let image = Arc::new(FetchedImage {
                            status: response.status,
                            body: Arc::from(response.body),
                            url_list: details.url_list,
                            timing: response.timing,
                        });
                        if let Some(store) = store.upgrade()
                            && let Ok(mut store) = store.lock()
                        {
                            store.retain(retained_key, image.body.len());
                        }
                        Ok(image)
                    }
                    Err(error) => Err(ImageFailure::Network(error.timing)),
                }
            },
            registration,
        )
        .map(|result| result.unwrap_or(Err(ImageFailure::Cancelled)))
        .boxed()
        .shared();
        inner.publish(&key.url, &fetch);
        let url = key.url.clone();
        inner.entries.entry(url).or_default().push(Entry {
            key,
            fetch: fetch.clone(),
            owner,
        });
        drop(inner);
        // Drive the fetch now; presentation consumers and the page share it.
        self.tasks.spawn(handle, fetch.clone());
        (fetch, ImageJoin::Started)
    }

    /// Register an equivalent response obtained elsewhere for the document
    /// (HTML #consume-a-preloaded-resource), unless one is already present.
    pub(crate) fn adopt(
        &self,
        key: ImageRequestKey,
        owner: u64,
        response: impl std::future::Future<Output = ImageOutcome> + Send + 'static,
    ) -> (SharedImageFetch, ImageJoin) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(entry) = inner
            .entries
            .get(&key.url)
            .and_then(|entries| entries.iter().find(|entry| entry.key == key))
        {
            return (
                entry.fetch.clone(),
                ImageJoin::Joined { owner: entry.owner },
            );
        }
        let fetch = response.boxed().shared();
        inner.publish(&key.url, &fetch);
        let url = key.url.clone();
        inner.entries.entry(url).or_default().push(Entry {
            key,
            fetch: fetch.clone(),
            owner,
        });
        (fetch, ImageJoin::Started)
    }

    /// Record a request that failed a precondition before fetching (Fetch
    /// #main-fetch's blocked requests), so presentation consumers waiting for
    /// the document's request observe the failure instead of waiting forever.
    pub(crate) fn refuse(&self, key: ImageRequestKey, owner: u64) {
        let mut inner = self.inner.lock().unwrap();
        if inner.closed
            || inner
                .entries
                .get(&key.url)
                .is_some_and(|entries| entries.iter().any(|entry| entry.key == key))
        {
            return;
        }
        let fetch = futures::future::ready(Err(ImageFailure::Refused))
            .boxed()
            .shared();
        inner.publish(&key.url, &fetch);
        let url = key.url.clone();
        inner
            .entries
            .entry(url)
            .or_default()
            .push(Entry { key, fetch, owner });
    }

    /// HTML #lazy-loading-attributes: the document will request `url` when the
    /// element's lazy load resumption steps run; presentation waits for it.
    pub(crate) fn expect(&self, url: &str) {
        let mut inner = self.inner.lock().unwrap();
        if !inner.closed && !inner.entries.contains_key(url) {
            inner.expected.insert(url.to_owned());
        }
    }

    /// The document's response for `url`. The document requests every image
    /// its rendering needs before publishing that rendering, so an unknown URL
    /// is one the document will not request: the frontend fetches it itself.
    /// A lazy image announced by `expect` is awaited until it resumes. A
    /// successful variant wins over a failed one.
    pub async fn presentation(&self, url: &str) -> Presentation {
        let fetches = {
            let mut inner = self.inner.lock().unwrap();
            if let Some(entries) = inner.entries.get(url) {
                Ok(entries
                    .iter()
                    .map(|entry| entry.fetch.clone())
                    .collect::<Vec<_>>())
            } else if inner.closed || !inner.expected.contains(url) {
                return Presentation::Unavailable;
            } else {
                let (sender, receiver) = tokio::sync::oneshot::channel();
                inner
                    .waiters
                    .entry(url.to_owned())
                    .or_default()
                    .push(sender);
                Err(receiver)
            }
        };
        let fetches = match fetches {
            Ok(fetches) => fetches,
            Err(receiver) => match receiver.await {
                Ok(fetch) => vec![fetch],
                Err(_) => return Presentation::Unavailable,
            },
        };
        let mut cancelled = true;
        for fetch in fetches {
            match fetch.await {
                Ok(image) => return Presentation::Image(image),
                Err(ImageFailure::Cancelled) => {}
                Err(_) => cancelled = false,
            }
        }
        if cancelled {
            Presentation::Unavailable
        } else {
            Presentation::Failed
        }
    }

    /// The document no longer makes requests. Waiting consumers fetch for
    /// themselves; completed responses remain available to the frozen
    /// presentation until the store itself is dropped with its page.
    pub(crate) fn close(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.closed = true;
        inner.waiters.clear();
        inner.expected.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(url: &str, cors: Option<CredentialsMode>) -> ImageRequestKey {
        let url = Url::parse(url).unwrap();
        ImageRequestKey {
            url: url.to_string(),
            cors,
            origin: url.origin(),
            cookie_restricted: false,
        }
    }

    #[tokio::test]
    async fn presentation_waits_for_the_document_request_and_close_releases_it() {
        let images = Arc::new(PageImages::default());
        assert!(matches!(
            images.presentation("https://a.test/unknown.png").await,
            Presentation::Unavailable
        ));
        images.expect("https://a.test/late.png");
        images.expect("https://a.test/never.png");
        let waiting = {
            let images = images.clone();
            tokio::spawn(async move { images.presentation("https://a.test/late.png").await })
        };
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());
        images.refuse(key("https://a.test/late.png", None), 0);
        assert!(matches!(waiting.await.unwrap(), Presentation::Failed));
        let pending = {
            let images = images.clone();
            tokio::spawn(async move { images.presentation("https://a.test/never.png").await })
        };
        tokio::task::yield_now().await;
        images.close();
        assert!(matches!(pending.await.unwrap(), Presentation::Unavailable));
        assert!(matches!(
            images.presentation("https://a.test/other.png").await,
            Presentation::Unavailable
        ));
    }

    #[test]
    fn requests_with_different_cors_modes_are_distinct() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let images = PageImages::default();
        let url = "http://127.0.0.1:9/x.png";
        let request = || Request::get(Url::parse(url).unwrap());
        let (_, first) = images.fetch(
            runtime.handle(),
            key(url, None),
            0,
            request(),
            ReferrerPolicy::default(),
        );
        let (_, again) = images.fetch(
            runtime.handle(),
            key(url, None),
            3,
            request(),
            ReferrerPolicy::default(),
        );
        let (_, cors) = images.fetch(
            runtime.handle(),
            key(url, Some(CredentialsMode::SameOrigin)),
            0,
            request(),
            ReferrerPolicy::default(),
        );
        assert_eq!(first, ImageJoin::Started);
        assert_eq!(again, ImageJoin::Joined { owner: 0 });
        assert_eq!(cors, ImageJoin::Started);
        images.tasks.cancel();
    }
}
