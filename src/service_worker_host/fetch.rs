//! Service Workers 92aba3be #on-fetch-request-algorithm and
//! #fetch-event-respondwith; Fetch 394d20d #concept-http-fetch.
//! Request ownership identifies the client; URLs alone never select a profile.
//! The response head, event lifetime, and demand-driven body have separate
//! completion paths. Dropping a request cancels its reader without terminating
//! unrelated events in the same worker.
use super::*;
use crate::http::{BodyStream, ExternalBody, Request, Response};
use tokio::sync::oneshot;

#[derive(Clone)]
pub(crate) struct Context {
    profile: Arc<Profile>,
    client: Option<Arc<sw::Client>>,
    reserved: Option<sw::DocumentClient>,
}
impl std::fmt::Debug for Context {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServiceWorkerFetchContext")
            .field("client", &self.client.as_ref().map(|c| &c.id))
            .field("navigation", &self.reserved.is_some())
            .finish()
    }
}

pub(crate) fn for_document(document: &sw::DocumentClient) -> Option<Context> {
    let registry = document.client().registry()?;
    let profile = PROFILES
        .lock()
        .unwrap()
        .iter()
        .find(|(_, profile)| Arc::ptr_eq(&profile.registry, &registry))?
        .1
        .clone();
    Some(Context {
        profile,
        client: Some(document.client()),
        reserved: None,
    })
}

pub(in crate::trust_js_backend) fn for_context(state: &HostState, context: u64) -> Option<Context> {
    // Fetch #concept-request-service-workers-mode: requests initiated by a
    // ServiceWorkerGlobalScope always bypass interception.
    if state.service_scope.is_some() {
        return None;
    }
    Some(Context {
        profile: profile(state)?,
        client: Some(client_for_context(state, context)?),
        reserved: None,
    })
}

pub(crate) fn navigation(
    storage: &crate::js::WebStorage,
    handle: tokio::runtime::Handle,
    url: &url::Url,
    initiator: Option<Context>,
) -> Option<Context> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let blobs = crate::js::BlobMap::default();
    let profile = profile_for(storage, &blobs, handle, None);
    let client = profile
        .registry
        .reserve_client(url.origin().ascii_serialization(), url.clone(), true)
        .ok()?;
    Some(Context {
        profile,
        client: initiator.and_then(|c| c.client),
        reserved: Some(sw::DocumentClient::new(client)),
    })
}

impl Context {
    /// HTML Document creation stores the response origin separately from
    /// creationURL. A navigation context's client remains its initiator;
    /// the reserved destination is not the request's origin.
    pub(crate) fn client_url(&self, creation: &url::Url) -> Option<url::Url> {
        let client = self.client.as_ref()?;
        let origin = client.origin();
        // Fetch leaves file transport to the UA; its native local-document
        // permission is not a tuple-origin grant. Keep that provenance while
        // sandboxed/data origins remain ordinary opaque clients.
        Some(super::super::request_origin_url(
            &origin,
            creation,
            client.local_file_access(),
        ))
    }

    /// Fetch #populate-request-from-client, before CORS/credential checks.
    /// Preserve the creation URL's referrer and navigation cookie flags.
    pub(crate) fn apply_client_origin(&self, request: &mut Request) {
        let creation = request
            .timing_client
            .as_ref()
            .or_else(|| request.fetch_policy.as_ref().map(|policy| &policy.origin))
            .unwrap_or(&request.url);
        let Some(origin) = self.client_url(creation) else {
            return;
        };
        let cookies = request.cookie_context.clone();
        if let Some(metadata) = request.fetch_metadata {
            crate::http::set_fetch_metadata(request, &origin, metadata.destination, metadata.mode);
            request.fetch_metadata.as_mut().unwrap().user_activation = metadata.user_activation;
        } else {
            request.timing_client = Some(origin.clone());
        }
        if let Some(policy) = request.fetch_policy.as_mut() {
            policy.origin = origin.clone();
        }
        if let Some(mut cookies) = cookies {
            cookies.client = Some(origin.clone());
            // RFC6265bis #dedicated-and-shared-requests: a worker keeps
            // its owner's top-level site, including with an opaque origin.
            if self
                .client
                .as_ref()
                .is_some_and(|client| client.kind() == sw::ClientKind::Window)
                && !cookies.top_level
                && !cookies.cross_site_ancestor
            {
                cookies.top_level_site =
                    crate::http::CookieContext::subresource(&origin).top_level_site;
            }
            request.cookie_context = Some(cookies);
        }
    }

    pub(crate) fn cache_key(&self) -> String {
        self.client.as_ref().map_or_else(String::new, |c| {
            format!("{}:{}", c.id, c.controller().map_or(0, |w| w.id))
        })
    }
    pub(crate) fn document(&self) -> Option<sw::DocumentClient> {
        self.reserved.clone()
    }
    pub(crate) fn controlled(&self) -> bool {
        self.reserved
            .as_ref()
            .map(sw::DocumentClient::client)
            .or_else(|| self.client.clone())
            .is_some_and(|c| c.controller().is_some())
    }
    pub(crate) fn for_navigation(&self, request: &Request) -> Option<Self> {
        let origin = request.url.origin();
        let partition = request
            .cookie_context
            .as_ref()
            .map_or("", |c| c.storage_partition());
        let key = if partition == "null" {
            "null".to_owned()
        } else if partition.is_empty() {
            origin.ascii_serialization()
        } else {
            format!("{}^{partition}", origin.ascii_serialization())
        };
        let secure = self.client.as_ref().is_none_or(|c| c.secure_context())
            && crate::http::potentially_trustworthy(&request.url);
        let client = self
            .profile
            .registry
            .reserve_client_with_origin(key, request.url.clone(), origin, secure)
            .ok()?;
        client.set_secure_context(secure);
        Some(Self {
            profile: self.profile.clone(),
            client: self.client.clone(),
            reserved: Some(sw::DocumentClient::new(client)),
        })
    }
    pub(crate) fn redirected(&self, request: &Request) -> Option<Self> {
        let previous = self.reserved.as_ref()?;
        if previous.client().origin() == request.url.origin() {
            self.profile
                .registry
                .select_navigation(&previous.client(), &request.url);
            return Some(self.clone());
        }
        let next = self.for_navigation(request)?;
        next.reserved
            .as_ref()?
            .navigation_reply(previous.take_navigation_reply());
        previous.discard();
        Some(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn local_file_context_preserves_native_resources_without_origin_access() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("resource.txt");
        std::fs::write(&path, "local context resource").unwrap();
        let target = url::Url::from_file_path(&path).unwrap();
        let page = target.join("index.html").unwrap();
        let storage = crate::js::WebStorage::default();
        let blobs = crate::js::BlobMap::default();
        let document = super::super::document(
            &storage,
            &blobs,
            tokio::runtime::Handle::current(),
            &page,
            crate::js::TraversableState::SHOWN,
        )
        .unwrap();
        let context = for_document(&document).unwrap();
        assert!(document.client().local_file_access());
        assert_eq!(context.client_url(&page), Some(page.clone()));
        assert!(!crate::http::same_origin_for_host(&page, &target));
        let mut request = Request::subresource(target.clone(), &page, "script", None);
        request.service_worker = Some(context.clone());
        context.apply_client_origin(&mut request);
        assert_eq!(request.timing_client, Some(page.clone()));
        assert_eq!(
            crate::http::fetch(&request).await.unwrap().body,
            b"local context resource"
        );
        // The same native provenance never turns file origins into tuple
        // origins. Fetch same-origin/CORS modes still fail before file I/O.
        for mode in [
            crate::http::RequestMode::Cors,
            crate::http::RequestMode::SameOrigin,
        ] {
            request.fetch_policy = Some(crate::http::FetchPolicy {
                origin: page.clone(),
                mode,
                credentials: crate::http::CredentialsMode::Omit,
            });
            let error = crate::http::fetch(&request).await.unwrap_err();
            assert!(
                error.contains("same-origin") || error.contains("CORS"),
                "{error}"
            );
        }
        // Initial Window reuse transfers native document metadata, including
        // this permission, without transferring the reserved client id.
        let initial = document
            .client()
            .registry()
            .unwrap()
            .reserve_client_with_origin(
                "null".into(),
                url::Url::parse("about:blank").unwrap(),
                document.client().origin(),
                false,
            )
            .unwrap();
        initial.adopt_initial_navigation(&document.client());
        assert!(initial.local_file_access());
        initial.discard();
        assert!(!initial.local_file_access());
        document
            .client()
            .set_document_origin(url::Origin::new_opaque());
        assert!(!document.client().local_file_access());
        assert_eq!(context.client_url(&page).unwrap().scheme(), "about");
    }

    #[tokio::test]
    async fn local_file_context_denies_sandboxed_and_other_opaque_documents() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("resource.txt");
        std::fs::write(&path, "local context resource").unwrap();
        let target = url::Url::from_file_path(path).unwrap();
        let page = target.join("index.html").unwrap();
        let dom = Rc::new(RefCell::new(Dom::parse_document(
            "<iframe id=plain></iframe><iframe id=sandboxed sandbox=allow-scripts></iframe>\
             <iframe id=data></iframe><iframe id=blank></iframe>",
        )));
        let mut state = HostState::new(dom.clone(), Rc::new(RealmClock::new()));
        let (events, _received) = tokio::sync::mpsc::unbounded_channel();
        state.enable_network(
            page.clone(),
            tokio::runtime::Handle::current(),
            Arc::new(crate::http::PageCache::default()),
            events,
        );
        let parent = super::super::document(
            &state.storage,
            &state.blobs,
            tokio::runtime::Handle::current(),
            &page,
            crate::js::TraversableState::SHOWN,
        )
        .unwrap();
        state.realm_origins.insert(0, parent.client().origin());
        state
            .service_workers
            .document(0, &parent, state.blobs.clone(), state.blob_agent);
        for (context, id, document_url, sandboxed, allowed) in [
            (1, "plain", page.clone(), false, true),
            (2, "sandboxed", page.clone(), true, false),
            (
                3,
                "data",
                url::Url::parse("data:text/html,opaque").unwrap(),
                false,
                false,
            ),
            (
                4,
                "blank",
                url::Url::parse("about:blank").unwrap(),
                false,
                true,
            ),
        ] {
            let node = dom.borrow().get_by_id(id).unwrap();
            let creation = if id == "blank" {
                page.clone()
            } else {
                document_url.clone()
            };
            let origin = if id == "blank" {
                parent.client().origin()
            } else {
                url::Origin::new_opaque()
            };
            state.window_request_urls.insert(context, creation);
            state.window_parents.insert(context, 0);
            state.realm_origins.insert(context, origin);
            if sandboxed {
                state.sandboxed_origins.insert(context);
            }
            super::super::child_document(&mut state, context, 0, node, document_url);
            let context = for_context(&state, context).unwrap();
            let mut request = Request::subresource(target.clone(), &page, "image", None);
            request.service_worker = Some(context.clone());
            context.apply_client_origin(&mut request);
            let response = crate::http::fetch(&request).await;
            if allowed {
                assert_eq!(context.client_url(&page), Some(page.clone()), "{id}");
                assert_eq!(response.unwrap().body, b"local context resource", "{id}");
            } else {
                assert_eq!(context.client_url(&page).unwrap().scheme(), "about", "{id}");
                assert!(response.unwrap_err().contains("non-file document"), "{id}");
            }
        }
    }

    #[tokio::test]
    async fn worker_context_keeps_owner_cookie_site_for_data_and_blob_scripts() {
        let owner_url = url::Url::parse("https://cookie-owner.test/page").unwrap();
        let storage = crate::js::WebStorage::default();
        let blobs = crate::js::BlobMap::default();
        let owner = super::super::document(
            &storage,
            &blobs,
            tokio::runtime::Handle::current(),
            &owner_url,
            crate::js::TraversableState::SHOWN,
        )
        .unwrap();
        let registry = owner.client().registry().unwrap();
        for (kind, script_url, worker) in [
            (
                sw::ClientKind::Worker,
                "data:text/javascript,void%200",
                true,
            ),
            (
                sw::ClientKind::Worker,
                "blob:https://cookie-owner.test/script",
                true,
            ),
            (
                sw::ClientKind::Sharedworker,
                "blob:https://cookie-owner.test/shared",
                true,
            ),
            (sw::ClientKind::Window, "data:text/html,opaque", false),
            (
                sw::ClientKind::Window,
                "blob:https://cookie-owner.test/document",
                false,
            ),
        ] {
            let script = url::Url::parse(script_url).unwrap();
            let origin = if script.scheme() == "data" {
                url::Origin::new_opaque()
            } else {
                owner_url.origin()
            };
            let client = registry
                .reserve_client_with_origin(
                    origin.ascii_serialization(),
                    script.clone(),
                    origin.clone(),
                    false,
                )
                .unwrap();
            client.configure(kind, sw::WindowState::default());
            let document = sw::DocumentClient::new(client);
            let context = for_document(&document).unwrap();
            let mut request =
                Request::subresource(owner_url.join("asset").unwrap(), &script, "empty", None);
            let cookies = crate::http::CookieContext::subresource(&owner_url);
            let owner_site = cookies.top_level_site.clone();
            // Even an incoming false ancestor flag cannot make a worker a
            // top-level Window and erase its owner's site during population.
            assert!(!cookies.cross_site_ancestor);
            request.cookie_context = Some(cookies);
            request.fetch_policy = Some(crate::http::FetchPolicy {
                origin: script,
                mode: crate::http::RequestMode::Cors,
                credentials: crate::http::CredentialsMode::Include,
            });
            context.apply_client_origin(&mut request);
            let cookies = request.cookie_context.unwrap();
            assert_eq!(
                cookies.top_level_site,
                if worker || origin.is_tuple() {
                    owner_site
                } else {
                    None
                },
                "{kind:?} {script_url}"
            );
            let effective = request.fetch_policy.unwrap().origin;
            if origin.is_tuple() {
                assert_eq!(effective.scheme(), "https", "{script_url}");
                assert_eq!(effective.origin(), owner_url.origin());
            } else {
                assert_eq!(effective.scheme(), "about", "{script_url}");
            }
            assert_eq!(cookies.client, Some(effective));
        }
    }
}

type Head = Result<Option<(Response, Option<BodyStream>)>, String>;
type Piece = Result<Option<Vec<u8>>, String>;
type PreloadResult = Result<(Response, Option<BodyStream>), crate::http::TimedFetchError>;
struct Preload(tokio::task::JoinHandle<PreloadResult>);
impl Drop for Preload {
    fn drop(&mut self) {
        self.0.abort();
    }
}
pub(super) struct Exchange {
    runtime: Arc<Runtime>,
    id: u64,
    request: Request,
    started: f64,
    head: Mutex<Option<oneshot::Sender<Head>>>,
    pull: Mutex<Option<oneshot::Sender<Piece>>>,
    ended: AtomicBool,
    _body_lease: sw::EventLease,
    preload: Mutex<Option<Preload>>,
    preload_abort: Option<tokio::task::AbortHandle>,
}
impl Exchange {
    fn control(&self, op: &'static str) {
        let _ = self
            .runtime
            .handle
            .ctl
            .sender
            .send(TrustJsWorkerCtl::ServiceControl { id: self.id, op });
    }
    pub(super) fn failed(&self) {
        if let Some(tx) = self.head.lock().unwrap().take() {
            let _ = tx.send(Err("Service worker stopped before responding".into()));
        }
        if let Some(tx) = self.pull.lock().unwrap().take() {
            let _ = tx.send(Err("Service worker stopped while streaming".into()));
        }
    }
    fn settle(&self, head: Head) {
        let ok = head.is_ok();
        if let Some(tx) = self.head.lock().unwrap().take() {
            let _ = tx.send(head);
        }
        self.control(if ok { "handled" } else { "failed" });
    }
}
impl Drop for Exchange {
    fn drop(&mut self) {
        self.runtime.fetches.lock().unwrap().remove(&self.id);
        if !self.ended.load(Ordering::Acquire) {
            self.control("cancel");
            if let Some(abort) = &self.preload_abort {
                abort.abort();
            }
        }
    }
}
struct Body(Arc<Exchange>);
struct HeldBody {
    source: Box<dyn ExternalBody>,
    exchange: Arc<Exchange>,
}
impl ExternalBody for HeldBody {
    fn next(&mut self) -> futures::future::BoxFuture<'_, Piece> {
        async move {
            let piece = self.source.next().await;
            if matches!(&piece, Ok(None)) {
                self.exchange.ended.store(true, Ordering::Release);
                self.exchange.control("ended");
            }
            piece
        }
        .boxed()
    }
}
impl ExternalBody for Body {
    fn next(&mut self) -> futures::future::BoxFuture<'_, Piece> {
        async move {
            if self.0.ended.load(Ordering::Acquire) {
                return Ok(None);
            }
            let (tx, rx) = oneshot::channel();
            *self.0.pull.lock().unwrap() = Some(tx);
            self.0.control("pull");
            match tokio::time::timeout(EVENT_TIMEOUT, rx).await {
                Ok(Ok(piece)) => piece,
                _ => Err("Service-worker response body timed out".into()),
            }
        }
        .boxed()
    }
}

pub(crate) fn intercept(request: &Request) -> futures::future::BoxFuture<'_, Head> {
    intercept_inner(request).boxed()
}

async fn intercept_inner(request: &Request) -> Head {
    let started = crate::performance::now_ms();
    let Some(context) = &request.service_worker else {
        return Ok(None);
    };
    let destination = request.fetch_metadata.map_or("empty", |m| m.destination);
    if matches!(destination, "embed" | "object" | "serviceworker") {
        return Ok(None);
    }
    let client = context
        .reserved
        .as_ref()
        .map(sw::DocumentClient::client)
        .or_else(|| context.client.clone());
    let Some(client) = client else {
        return Ok(None);
    };
    let Some(worker) = client.controller() else {
        return Ok(None);
    };
    let Some(registration) = worker.registration.upgrade() else {
        return Ok(None);
    };
    struct UpdateAfterFetch(Option<(Arc<Profile>, Arc<sw::Registration>)>);
    impl Drop for UpdateAfterFetch {
        fn drop(&mut self) {
            if let Some((profile, registration)) = self.0.take() {
                profile.soft_update(&registration);
            }
        }
    }
    let should_update = context.reserved.is_some()
        || registration
            .last_update_check
            .lock()
            .unwrap()
            .is_some_and(|last| last.elapsed() > Duration::from_secs(86400));
    let _update =
        UpdateAfterFetch(should_update.then(|| (context.profile.clone(), registration.clone())));
    if !worker
        .event_types
        .get()
        .is_some_and(|types| types.iter().any(|t| t == "fetch"))
    {
        return Ok(None);
    }
    let lease = registration
        .begin_event(&worker)
        .ok_or("Service worker is no longer active")?;
    let permit = context
        .profile
        .functional
        .clone()
        .try_acquire_owned()
        .map_err(|_| "Too many pending service-worker requests")?;
    let preload_state = registration.navigation_preload.lock().unwrap().clone();
    let preload = if request.fetch_metadata.is_some_and(|m| m.mode == "navigate")
        && request.method == "GET"
        && preload_state.enabled
    {
        let mut request = request.clone();
        request.service_worker = None;
        request.headers.push((
            "service-worker-navigation-preload".into(),
            preload_state.header_value,
        ));
        Some(Preload(context.profile.handle.spawn(async move {
            crate::http::fetch_script_streaming(&request).await
        })))
    } else {
        None
    };
    let preload_abort = preload.as_ref().map(|p| p.0.abort_handle());
    if !worker.await_activated().await {
        return Err("Service worker could not activate".into());
    }
    let runtime = context
        .profile
        .start(&worker)
        .await
        .map_err(|e| e.message)?;
    let id = runtime.next_event.fetch_add(1, Ordering::Relaxed);
    let (tx, rx) = oneshot::channel();
    let (done, completed) = oneshot::channel();
    let body_lease = registration
        .begin_event(&worker)
        .ok_or("Service worker is no longer active")?;
    let exchange = Arc::new(Exchange {
        runtime: runtime.clone(),
        id,
        request: request.clone(),
        started,
        head: Mutex::new(Some(tx)),
        pull: Mutex::default(),
        ended: AtomicBool::new(false),
        _body_lease: body_lease,
        preload: Mutex::new(preload),
        preload_abort,
    });
    {
        let mut events = runtime.events.lock().unwrap();
        if !runtime.alive.load(Ordering::Acquire) {
            return Err("Service worker stopped before dispatch".into());
        }
        events.insert(id, done);
        runtime
            .fetches
            .lock()
            .unwrap()
            .insert(id, Arc::downgrade(&exchange));
    }
    let runtime_for_event = runtime.clone();
    context.profile.handle.spawn(async move {
        let (_lease, _permit) = (lease, permit);
        let pending = PendingEvent {
            runtime: &runtime_for_event,
            id,
        };
        let _ = tokio::time::timeout(EVENT_TIMEOUT, completed).await;
        drop(pending);
    });
    let mut headers = request.headers.clone();
    if let Some((kind, _)) = &request.body
        && !kind.is_empty()
        && !headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("content-type"))
    {
        headers.push(("content-type".into(), kind.clone()));
    }
    let mode = request.fetch_metadata.map_or_else(
        || {
            request
                .fetch_policy
                .as_ref()
                .map_or("no-cors", |p| p.mode.as_str())
        },
        |m| m.mode,
    );
    let credentials = request
        .fetch_policy
        .as_ref()
        .map_or("include", |p| match p.credentials {
            crate::http::CredentialsMode::Omit => "omit",
            crate::http::CredentialsMode::SameOrigin => "same-origin",
            crate::http::CredentialsMode::Include => "include",
        });
    let init = serde_json::json!({"url":request.url, "method":request.method,"headers":headers,"preload":exchange.preload_abort.is_some(),
        "cache":request.web.cache,"integrity":request.web.integrity,"keepalive":request.web.keepalive,"priority":request.web.priority,
        "reloadNavigation":request.web.reload_navigation,"historyNavigation":request.web.history_navigation,"useCORSPreflight":request.use_cors_preflight,
        "referrerPolicy":request.referrer_policy.unwrap_or_default().as_str(),
        "body":request.body.as_ref().map(|(_,b)| b), "mode":mode,"credentials":credentials,"destination":destination,
        "redirect":if mode=="navigate"{"manual"}else{match request.redirect {crate::http::RedirectMode::Follow=>"follow",crate::http::RedirectMode::Manual=>"manual",crate::http::RedirectMode::Error=>"error"}},
        "referrer":crate::http::header_list_get(&request.headers,"referer").unwrap_or_default(),
        "clientId":context.client.as_ref().map_or("",|c|c.id.as_str()),
        "replacesClientId":if mode=="navigate"{request.web.replaces_client_id.as_str()}else{""},
        "resultingClientId":context.reserved.as_ref().map(|c|c.client().id.clone()).unwrap_or_default()});
    if runtime
        .handle
        .ctl
        .sender
        .send(TrustJsWorkerCtl::ServiceEvent {
            id,
            kind: "fetch",
            init,
        })
        .is_err()
    {
        return Err("Service worker stopped before dispatch".into());
    }
    *runtime.last_event.lock().unwrap() = Instant::now();
    let result = tokio::time::timeout(EVENT_TIMEOUT, rx)
        .await
        .map_err(|_| "Service worker response timed out")?
        .map_err(|_| "Service worker response was lost")?;
    drop(exchange);
    result
}

pub(super) fn preload(ctx: &mut Ctx, args: &[Value]) -> Result<Value, Value> {
    let event_id = args.get(1).and_then(Value::as_num_opt).unwrap_or(0.) as u64;
    let (promise, resolve, _) = ctx.new_promise_with_resolvers();
    let state = ctx.host_mut::<HostState>().expect("service worker host");
    let exchange = state
        .service_scope
        .as_ref()
        .and_then(|s| s.runtime.upgrade())
        .and_then(|r| {
            r.fetches
                .lock()
                .unwrap()
                .get(&event_id)
                .and_then(Weak::upgrade)
        });
    let source = exchange.and_then(|e| e.preload.lock().unwrap().take());
    let Some(mut source) = source else {
        let _ = ctx.invoke(resolve, Value::Undefined, &[Value::Null]);
        return Ok(promise);
    };
    let network = state.network.as_mut().expect("service worker network");
    let id = network.next_fetch_id;
    network.next_fetch_id += 1;
    network.pending_fetches.insert(
        id,
        TrustJsPendingFetch {
            context: 0,
            resolve,
            native_lease: None,
            keepalive: false,
            task: None,
        },
    );
    let events = state.task_events.clone().expect("service worker queue");
    let work = async move {
        match (&mut source.0).await {
            Ok(Ok((response, Some(body)))) => {
                stream_fetch_body(id, trust_js_fetch_result(response), None, body, events).await;
            }
            result => {
                let result = result
                    .ok()
                    .and_then(Result::ok)
                    .and_then(|(r, _)| trust_js_fetch_result(r));
                let _ = events.send(TrustJsHostTask::FetchDone {
                    id,
                    result,
                    timing: None,
                });
            }
        }
    };
    let task = network
        .cache
        .task_scope()
        .spawn_abortable(&network.handle, work, false);
    network.pending_fetches.get_mut(&id).unwrap().task = task;
    Ok(promise)
}

pub(super) fn chunk(ctx: &mut Ctx, args: &[Value]) -> Result<Value, Value> {
    let id = args.get(1).and_then(Value::as_num_opt).unwrap_or(0.) as u64;
    let bytes = args.get(2).and_then(|v| ctx.buffer_source_bytes(v, false));
    let end = matches!(args.get(2), Some(Value::Null));
    let exchange = ctx
        .host::<HostState>()
        .and_then(|s| s.service_scope.as_ref())
        .and_then(|s| s.runtime.upgrade())
        .and_then(|r| r.fetches.lock().unwrap().get(&id).and_then(Weak::upgrade));
    if let Some(exchange) = exchange {
        if end || bytes.is_none() {
            exchange.ended.store(true, Ordering::Release);
        }
        if let Some(tx) = exchange.pull.lock().unwrap().take() {
            let _ = tx.send(if end {
                Ok(None)
            } else {
                bytes
                    .map(Some)
                    .ok_or_else(|| "Service-worker body stream failed".into())
            });
        }
    }
    Ok(Value::Undefined)
}

pub(super) fn response(
    ctx: &mut Ctx,
    op: &str,
    input: serde_json::Value,
) -> Result<serde_json::Value, Error> {
    let id = input["id"].as_u64().unwrap_or(0);
    let exchange = ctx
        .host::<HostState>()
        .and_then(|s| s.service_scope.as_ref())
        .and_then(|s| s.runtime.upgrade())
        .and_then(|r| r.fetches.lock().unwrap().get(&id).and_then(Weak::upgrade));
    let Some(exchange) = exchange else {
        return Ok(false.into());
    };
    if op == "fetchFallback" {
        exchange.ended.store(true, Ordering::Release);
        exchange.settle(if input["canceled"] == true {
            Err("Service worker canceled the fetch event".into())
        } else {
            Ok(None)
        });
        return Ok(true.into());
    }
    let result = response_head(ctx, &exchange, &input);
    let accepted = result.is_ok();
    if !accepted {
        exchange.ended.store(true, Ordering::Release);
    }
    exchange.settle(result.map(Some));
    Ok(accepted.into())
}

fn response_head(
    ctx: &mut Ctx,
    exchange: &Arc<Exchange>,
    input: &serde_json::Value,
) -> Result<(Response, Option<BodyStream>), String> {
    let head = &input["head"];
    let mut kind = head["type"].as_str().unwrap_or("error");
    let private = head["privateToken"]
        .as_str()
        .and_then(|token| private_response_host::for_service_worker(ctx, token));
    let mut status = head["status"].as_u64().unwrap_or(0) as u16;
    let mut status_text = head["statusMessage"].as_str().unwrap_or("").to_owned();
    let mut headers: Vec<(String, String)> =
        serde_json::from_value(head["headerList"].clone()).unwrap_or_default();
    let mut urls: Vec<url::Url> =
        serde_json::from_value(head["urlList"].clone()).unwrap_or_default();
    if let Some(payload) = &private {
        kind = payload.internal.kind;
        status = payload.internal.status;
        status_text.clone_from(&payload.internal.status_text);
        if matches!(kind, "opaque" | "opaqueredirect") {
            headers.clone_from(&payload.internal.headers);
        }
        urls.clone_from(&payload.internal.url_list);
    }
    let request = &exchange.request;
    let mode = request.fetch_metadata.map_or("no-cors", |m| m.mode);
    let redirect = if mode == "navigate" {
        crate::http::RedirectMode::Manual
    } else {
        request.redirect
    };
    let range_requested = private.as_ref().is_some_and(|p| p.internal.range_requested);
    if kind == "error"
        || status == 0
        || mode == "same-origin" && kind == "cors"
        || mode != "no-cors" && kind == "opaque"
        || redirect != crate::http::RedirectMode::Manual && kind == "opaqueredirect"
        || redirect != crate::http::RedirectMode::Follow && urls.len() > 1
        || kind == "opaque"
            && matches!(status, 206 | 416)
            && range_requested
            && !request
                .headers
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case("range"))
        || matches!(kind, "opaque" | "opaqueredirect") && private.is_none()
    {
        return Err(format!(
            "Service-worker response is not valid for this request (type={kind}, status={status}, mode={mode}, URLs={})",
            urls.len()
        ));
    }
    let null_body = matches!(request.method.as_str(), "HEAD" | "CONNECT")
        || matches!(status, 101 | 103 | 204 | 205 | 304);
    let body = if null_body {
        exchange.control("discard");
        None
    } else if let Some(payload) = &private
        && matches!(kind, "opaque" | "opaqueredirect")
    {
        payload.body.as_ref().map(|body| {
            BodyStream::external(Box::new(HeldBody {
                source: Box::new(body.cursor()),
                exchange: exchange.clone(),
            }))
        })
    } else if head["hasBody"] == true {
        Some(BodyStream::external(Box::new(Body(exchange.clone()))))
    } else {
        None
    };
    if body.is_none() {
        exchange.ended.store(true, Ordering::Release);
    }
    let internal = match kind {
        "cors" | "opaque" | "opaqueredirect" => Some(Box::new(crate::http::InternalResponse {
            kind: match kind {
                "cors" => "cors",
                "opaque" => "opaque",
                _ => "opaqueredirect",
            },
            status,
            status_text: status_text.clone(),
            content_type: private.as_ref().map_or_else(
                || {
                    crate::http::header_list_get(&headers, "content-type")
                        .unwrap_or_default()
                        .into_owned()
                },
                |p| p.internal.content_type.clone(),
            ),
            headers: private
                .as_ref()
                .map_or_else(|| headers.clone(), |p| p.internal.headers.clone()),
            body: None,
            url_list: urls.clone(),
            range_requested,
        })),
        _ => None,
    };
    // Synthetic responses with an empty URL list inherit the request URL.
    let url = urls.last().cloned().unwrap_or_else(|| request.url.clone());
    let opaque = matches!(kind, "opaque" | "opaqueredirect");
    if opaque {
        status = 0;
        status_text.clear();
        headers.clear();
    }
    let now = crate::performance::now_ms();
    let timing = crate::http::FetchTiming {
        start_time: exchange.started,
        fetch_start: exchange.started,
        worker_start: exchange.started,
        final_response_start: now,
        response_end: if body.is_none() { now } else { 0.0 },
        ..Default::default()
    };
    Ok((
        Response {
            internal,
            service_client: None,
            url,
            status,
            status_text,
            content_type: crate::http::header_list_get(&headers, "content-type")
                .unwrap_or_default()
                .into_owned(),
            headers,
            body: Vec::new(),
            rendered: None,
            js: None,
            blobs: None,
            live: None,
            images: None,
            declarative_refresh: None,
            challenge: None,
            from_post: false,
            timing: Some(Box::new(timing)),
            referrer: String::new(),
            referrer_policy: None,
        },
        body,
    ))
}
