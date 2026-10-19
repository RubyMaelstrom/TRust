//! Session-only service-worker registrations and request coordination.
//!
//! W3C Service Workers 92aba3be (local 2026-09-06 snapshot): Start Register,
//! Register, Update, Match Service Worker Registration, Install and Activate.
//! The user's explicit storage policy keeps registrations and script resources
//! in RAM; they are cleared with the browser session, including bookmarked
//! sites. Nothing in this module writes through site_storage or opens files.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use url::Url;

pub(crate) mod jobs;
pub(crate) mod scripts;

/// Release native transfer ownership held by one serialized packet that
/// will never be deserialized (SW ServiceWorker/postMessage and
/// Client/postMessage discard paths). The host supplies the port/canvas
/// registries; this model only owns the packet text. Callers must not hold
/// a client/registration lock while invoking it.
pub(crate) type PacketRelease = Arc<dyn Fn(&str) + Send + Sync>;

/// Bounds for one client's undelivered container messages. A disabled
/// client message queue must not retain unbounded packets or transfers.
pub(crate) const MAX_QUEUED_MESSAGES: usize = 256;
pub(crate) const MAX_QUEUED_MESSAGE_BYTES: usize = 32 * 1024 * 1024;

#[derive(Clone, Debug, serde::Serialize)]
pub(crate) struct Error {
    pub name: &'static str,
    pub message: String,
}

impl Error {
    pub(crate) fn new(name: &'static str, message: impl Into<String>) -> Self {
        Self {
            name,
            message: message.into(),
        }
    }
    pub(crate) fn type_error(message: impl Into<String>) -> Self {
        Self::new("TypeError", message)
    }
    pub(crate) fn security(message: impl Into<String>) -> Self {
        Self::new("SecurityError", message)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum WorkerType {
    Classic,
    Module,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum UpdateViaCache {
    Imports,
    All,
    None,
}

#[derive(Clone, Debug)]
pub(crate) struct RegistrationOptions {
    pub script: Url,
    pub scope: Url,
    pub kind: WorkerType,
    pub update_via_cache: UpdateViaCache,
}

impl RegistrationOptions {
    /// Start Register strips fragments and rejects encoded separators only
    /// in the path (a query may contain them). Register then applies origin
    /// and trustworthy-origin checks; source MIME and max scope follow fetch.
    pub(crate) fn parse(
        base: &Url,
        origin: &url::Origin,
        script: &str,
        scope: Option<&str>,
        kind: WorkerType,
        update_via_cache: UpdateViaCache,
    ) -> Result<Self, Error> {
        let mut script = base
            .join(script)
            .map_err(|_| Error::type_error("Invalid service-worker script URL"))?;
        script.set_fragment(None);
        if !valid_registration_url(&script) {
            return Err(Error::type_error("Unsupported service-worker script URL"));
        }
        let mut scope = match scope {
            Some(scope) => base.join(scope),
            None => script.join("./"),
        }
        .map_err(|_| Error::type_error("Invalid service-worker scope URL"))?;
        scope.set_fragment(None);
        if !valid_registration_url(&scope) {
            return Err(Error::type_error("Unsupported service-worker scope URL"));
        }
        if !crate::http::potentially_trustworthy(&script)
            || script.origin() != *origin
            || scope.origin() != *origin
        {
            return Err(Error::security(
                "Service-worker script and scope must have the client's trustworthy origin",
            ));
        }
        Ok(Self {
            script,
            scope,
            kind,
            update_via_cache,
        })
    }

    /// Update's fetch hook: Service-Worker-Allowed can widen the maximum
    /// path prefix, but cannot widen the script's origin. The query and
    /// fragment of that header never participate in the path-prefix check.
    pub(crate) fn check_max_scope(&self, allowed: Option<&str>) -> Result<(), Error> {
        let max = self
            .script
            .join(allowed.unwrap_or("./"))
            .map_err(|_| Error::security("Invalid Service-Worker-Allowed URL"))?;
        if max.origin() != self.script.origin() || !self.scope.path().starts_with(max.path()) {
            return Err(Error::security(
                "Service-worker scope exceeds the permitted path",
            ));
        }
        Ok(())
    }
}

fn valid_registration_url(url: &Url) -> bool {
    let path = url.path().to_ascii_lowercase();
    matches!(url.scheme(), "http" | "https") && !path.contains("%2f") && !path.contains("%5c")
}

/// Match Service Worker Registration is a longest serialized-URL prefix,
/// not a path-segment comparison. Storage partition matching is performed
/// before this comparison, by the registration map's key.
fn scope_matches(scope: &Url, client: &Url) -> bool {
    client.as_str().starts_with(scope.as_str())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum WorkerState {
    Parsed,
    Installing,
    Installed,
    Activating,
    Activated,
    Redundant,
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WorkerInfo {
    #[serde(skip)]
    pub handle: Arc<Worker>,
    pub id: u64,
    #[serde(rename = "scriptURL")]
    pub script_url: String,
    pub state: WorkerState,
    pub revision: u64,
}

pub(crate) struct Worker {
    pub id: u64,
    pub script: Url,
    pub kind: WorkerType,
    pub registration: Weak<Registration>,
    state: Mutex<(WorkerState, u64)>,
    pub skip_waiting: AtomicBool,
    pub pending_events: AtomicUsize,
    pub version: Mutex<Option<Arc<scripts::Version>>>,
    pub start_lock: tokio::sync::Mutex<()>,
    pub installation_notifications_pending: AtomicBool,
    pub event_types: std::sync::OnceLock<Vec<String>>,
    changed: tokio::sync::Notify,
}

impl Worker {
    pub(crate) fn info(self: &Arc<Self>) -> WorkerInfo {
        let (state, revision) = *self.state.lock().unwrap();
        WorkerInfo {
            handle: self.clone(),
            id: self.id,
            script_url: self.script.to_string(),
            state,
            revision,
        }
    }
    pub(crate) fn state(&self) -> WorkerState {
        self.state.lock().unwrap().0
    }
    pub(crate) async fn await_activated(&self) -> bool {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            match self.state() {
                WorkerState::Activated => return true,
                WorkerState::Activating => changed.await,
                _ => return false,
            }
        }
    }
}
impl std::fmt::Debug for Worker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Worker")
            .field("id", &self.id)
            .field("script", &self.script)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RegistrationInfo {
    #[serde(skip)]
    pub handle: Arc<Registration>,
    pub id: u64,
    pub scope: String,
    pub update_via_cache: UpdateViaCache,
    pub installing: Option<WorkerInfo>,
    pub waiting: Option<WorkerInfo>,
    pub active: Option<WorkerInfo>,
    pub revision: u64,
    pub registered: bool,
}

struct RegistrationState {
    update_via_cache: UpdateViaCache,
    installing: Option<Arc<Worker>>,
    waiting: Option<Arc<Worker>>,
    active: Option<Arc<Worker>>,
    revision: u64,
}

pub(crate) struct Registration {
    pub id: u64,
    pub key: String,
    pub scope: Url,
    pub registry: Weak<Registry>,
    registered: AtomicBool,
    state: Mutex<RegistrationState>,
    pub cookies: Mutex<CookieSubscriptions>,
}

#[derive(Default)]
pub(crate) struct CookieSubscriptions {
    pub filters: Vec<crate::http::cookie_store::Filter>,
    pub observer: Option<crate::http::cookie_store::Subscription>,
}

impl Registration {
    pub(crate) fn registered(&self) -> bool {
        self.registered.load(Ordering::Acquire)
    }
    pub(crate) fn info(self: &Arc<Self>) -> RegistrationInfo {
        let state = self.state.lock().unwrap();
        RegistrationInfo {
            handle: self.clone(),
            id: self.id,
            scope: self.scope.to_string(),
            update_via_cache: state.update_via_cache,
            installing: state.installing.as_ref().map(|w| w.info()),
            waiting: state.waiting.as_ref().map(|w| w.info()),
            active: state.active.as_ref().map(|w| w.info()),
            revision: state.revision,
            registered: self.registered(),
        }
    }
    pub(crate) fn active(&self) -> Option<Arc<Worker>> {
        self.state.lock().unwrap().active.clone()
    }
    pub(crate) fn newest(&self) -> Option<Arc<Worker>> {
        let state = self.state.lock().unwrap();
        state
            .installing
            .as_ref()
            .or(state.waiting.as_ref())
            .or(state.active.as_ref())
            .cloned()
    }
    pub(crate) fn set_update_via_cache(&self, value: UpdateViaCache) {
        self.state.lock().unwrap().update_via_cache = value;
    }
    /// Queue an extendable event while holding the same registration lock
    /// that Try Activate checks. An event and replacement activation cannot
    /// both win a race to an idle active worker.
    pub(crate) fn begin_event(&self, worker: &Arc<Worker>) -> Option<EventLease> {
        let state = self.state.lock().unwrap();
        // Messages can run installing and waiting workers as well as the
        // active worker (ServiceWorker/postMessage runs its worker).
        if ![&state.installing, &state.waiting, &state.active]
            .into_iter()
            .any(|slot| {
                slot.as_ref()
                    .is_some_and(|candidate| candidate.id == worker.id)
            })
        {
            return None;
        }
        worker.pending_events.fetch_add(1, Ordering::AcqRel);
        Some(EventLease(worker.clone()))
    }
}
impl std::fmt::Debug for Registration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Registration")
            .field("id", &self.id)
            .field("scope", &self.scope)
            .finish_non_exhaustive()
    }
}

/// Release on successful event completion, rejection, cancellation and
/// runtime failure alike (SW #try-activate / #try-clear-registration).
pub(crate) struct EventLease(Arc<Worker>);
impl Drop for EventLease {
    fn drop(&mut self) {
        let previous = self.0.pending_events.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous != 0);
        if previous == 1
            && let Some(registration) = self.0.registration.upgrade()
            && let Some(registry) = registration.registry.upgrade()
        {
            registry.client_left(&registration);
        }
    }
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub(crate) enum Notice {
    /// The Window's user-interaction task reads current presentation state.
    Inspect {
        #[serde(skip)]
        inspection: Arc<ClientInspection>,
    },
    /// Native task acknowledgement, never exposed as a DOM event. Install
    /// waits for its queued state updates before trying activation.
    Checkpoint {
        #[serde(skip)]
        completion: Arc<Checkpoint>,
    },
    Registration {
        registration: RegistrationInfo,
        update_found: bool,
    },
    Worker {
        worker: WorkerInfo,
    },
    Controller {
        worker: Option<WorkerInfo>,
    },
    Ready {
        registration: RegistrationInfo,
    },
    /// Wake hint only: packets stay in the client's native queue until its
    /// container takes them, so a lost wake cannot lose a message.
    Messages,
}

/// One undelivered Client/postMessage packet. Plain data on purpose: no
/// destructor may take a registry mutex, so release is always explicit
/// and outside every model lock.
#[derive(Clone, Debug)]
pub(crate) struct PendingMessage {
    pub packet: String,
    pub origin: String,
    pub source: WorkerInfo,
}

#[derive(Debug)]
pub(crate) struct Checkpoint(Mutex<Option<tokio::sync::oneshot::Sender<()>>>);
impl Checkpoint {
    pub(crate) fn complete(&self) {
        if let Some(sender) = self.0.lock().unwrap().take() {
            let _ = sender.send(());
        }
    }
}
impl Drop for Checkpoint {
    fn drop(&mut self) {
        self.complete();
    }
}

pub(crate) type Notify = Arc<dyn Fn(Notice) + Send + Sync>;

#[derive(Debug)]
pub(crate) struct ClientInspection(
    Mutex<Option<tokio::sync::oneshot::Sender<Option<WindowState>>>>,
);
impl ClientInspection {
    pub(crate) fn complete(&self, state: Option<WindowState>) {
        if let Some(sender) = self.0.lock().unwrap().take() {
            let _ = sender.send(state);
        }
    }
}

#[derive(Clone)]
struct Controller {
    // An unregistered registration continues serving its controlled clients
    // until Try Clear Registration permits disposal.
    registration: Arc<Registration>,
    worker: Arc<Worker>,
}

struct ClientState {
    controller: Option<Controller>,
    notify: Option<Notify>,
    attachment: u64,
    discarded: bool,
    execution_ready: bool,
    secure: bool,
    kind: ClientKind,
    window: WindowState,
    focus_order: Option<u64>,
    // The container's client message queue (SW #dfn-client-message-queue),
    // initially disabled. Packets are held natively until taken.
    messages: VecDeque<PendingMessage>,
    message_bytes: usize,
    messages_enabled: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ClientKind {
    #[default]
    Window,
    Worker,
    Sharedworker,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WindowState {
    pub frame_type: String,
    pub visibility_state: String,
    pub focused: bool,
    /// The navigable remains in its parent's focus chain when the native
    /// window loses system focus. Retain that distinction after actor retirement.
    #[serde(default)]
    pub focus_within: bool,
    pub ancestor_origins: Vec<String>,
}
impl Default for WindowState {
    fn default() -> Self {
        Self {
            frame_type: "top-level".into(),
            visibility_state: "visible".into(),
            focused: false,
            focus_within: false,
            ancestor_origins: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ClientInfo {
    pub id: String,
    pub url: String,
    #[serde(rename = "type")]
    pub kind: ClientKind,
    #[serde(flatten)]
    pub window: WindowState,
    #[serde(skip)]
    creation_order: u64,
    #[serde(skip)]
    focus_order: Option<u64>,
}
impl ClientInfo {
    /// SW #clients-matchall: focused Window clients first by last focus,
    /// other Windows next by creation, then workers by creation.
    pub(crate) fn sort(clients: &mut [Self]) {
        clients.sort_by_key(|client| {
            (
                if client.kind != ClientKind::Window {
                    2
                } else if client.focus_order.is_some() {
                    0
                } else {
                    1
                },
                std::cmp::Reverse(client.focus_order.unwrap_or(0)),
                client.creation_order,
            )
        });
    }
}

pub(crate) struct Client {
    pub id: String,
    pub key: String,
    pub url: Url,
    pub origin: url::Origin,
    creation_order: u64,
    registry: Weak<Registry>,
    state: Mutex<ClientState>,
    ready: tokio::sync::Notify,
}

/// The lifetime of a Document's environment, independently of its JavaScript
/// actor. Cloned presentations share ownership; request/inspection references
/// to the Client do not extend that lifetime. SW #service-worker-client-concept
/// and #on-client-unload-algorithm.
#[derive(Clone)]
pub struct DocumentClient(Arc<DocumentOwner>);

#[derive(Clone)]
pub(crate) struct WeakDocumentClient(Weak<DocumentOwner>);
impl WeakDocumentClient {
    pub(crate) fn upgrade(&self) -> Option<DocumentClient> {
        self.0.upgrade().map(DocumentClient)
    }
}

struct DocumentOwner {
    client: Arc<Client>,
    blobs: Mutex<Vec<Arc<DocumentBlobs>>>,
    children: Mutex<HashMap<u64, DocumentClient>>,
    parent: Mutex<Option<(WeakDocumentClient, u64)>>,
}

/// File API #lifeTime follows the Document, not a retained presentation
/// snapshot. The actor also holds this guard: if unloading script creates a
/// last URL after frontend teardown, its eventual exit removes that URL too.
pub(crate) struct DocumentBlobs {
    store: crate::js::BlobMap,
    agent: u64,
    context: u64,
}
impl DocumentBlobs {
    pub(crate) fn remove(&self) {
        self.store
            .remove_environment(self.agent, Some(self.context));
    }
}
impl Drop for DocumentBlobs {
    fn drop(&mut self) {
        self.remove();
    }
}

impl Drop for DocumentOwner {
    fn drop(&mut self) {
        self.client.discard();
        for blobs in self.blobs.get_mut().unwrap().iter() {
            blobs.remove();
        }
        DocumentClient::discard_all(
            std::mem::take(self.children.get_mut().unwrap())
                .into_values()
                .collect(),
        );
    }
}

impl DocumentClient {
    pub(crate) fn new(client: Arc<Client>) -> Self {
        Self(Arc::new(DocumentOwner {
            client,
            blobs: Mutex::default(),
            children: Mutex::default(),
            parent: Mutex::default(),
        }))
    }
    pub(crate) fn client(&self) -> Arc<Client> {
        self.0.client.clone()
    }
    pub(crate) fn downgrade(&self) -> WeakDocumentClient {
        WeakDocumentClient(Arc::downgrade(&self.0))
    }
    pub(crate) fn adopt_child(&self, context: u64, child: &Self) {
        *child.0.parent.lock().unwrap() = Some((self.downgrade(), context));
        self.0
            .children
            .lock()
            .unwrap()
            .insert(context, child.clone());
    }
    pub(crate) fn static_child(
        &self,
        node: usize,
        url: Url,
        origin: url::Origin,
        key: String,
        window: WindowState,
    ) -> Result<Self, Error> {
        // Static DOM node identities occupy a separate namespace from the
        // JS settings contexts (which are exact nonnegative JS integers).
        let context = (1u64 << 63) | node as u64;
        if let Some(child) = self.0.children.lock().unwrap().get(&context) {
            return Ok(child.clone());
        }
        let parent = self.client();
        let registry = parent
            .registry
            .upgrade()
            .ok_or_else(|| Error::type_error("The document's profile has closed"))?;
        let secure = parent.secure_context();
        let navigation = secure && matches!(url.scheme(), "http" | "https");
        let client = registry.reserve_client_with_origin(key, url, origin, navigation)?;
        client.set_secure_context(secure);
        client.configure(ClientKind::Window, window);
        client.set_execution_ready();
        let child = Self::new(client);
        self.adopt_child(context, &child);
        Ok(child)
    }
    pub(crate) fn discard(&self) {
        Self::discard_all(vec![self.clone()]);
    }
    fn discard_all(mut pending: Vec<Self>) {
        // Destruction must not recurse on the Rust stack with iframe depth.
        // Drain child ownership before dropping any handle, even if another
        // presentation or the unloading actor still holds that child.
        while let Some(document) = pending.pop() {
            document.0.client.discard();
            for blobs in document.0.blobs.lock().unwrap().iter() {
                blobs.remove();
            }
            let children = std::mem::take(&mut *document.0.children.lock().unwrap());
            pending.extend(children.into_values());
            let parent = document.0.parent.lock().unwrap().take();
            if let Some((parent, context)) = parent
                && let Some(parent) = parent.upgrade()
            {
                parent.0.children.lock().unwrap().remove(&context);
            }
        }
    }
    pub(crate) fn bind_blobs(
        &self,
        store: crate::js::BlobMap,
        agent: u64,
        context: u64,
    ) -> Arc<DocumentBlobs> {
        let guard = Arc::new(DocumentBlobs {
            store,
            agent,
            context,
        });
        self.0.blobs.lock().unwrap().push(guard.clone());
        guard
    }
    pub(crate) fn presentation(&self, hidden: bool, focused: bool) {
        let mut pending = vec![(self.clone(), focused, true)];
        while let Some((document, parent_focus, root)) = pending.pop() {
            let client = &document.0.client;
            let mut state = client.state.lock().unwrap();
            let focused = parent_focus && (root || state.window.focus_within);
            client.update_focus(&mut state, focused);
            state.window.visibility_state = if hidden { "hidden" } else { "visible" }.into();
            drop(state);
            pending.extend(
                document
                    .0
                    .children
                    .lock()
                    .unwrap()
                    .values()
                    .cloned()
                    .map(|child| (child, focused, false)),
            );
        }
    }
}

impl std::fmt::Debug for DocumentClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("DocumentClient")
            .field(&self.0.client.id)
            .finish()
    }
}
impl PartialEq for DocumentClient {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
impl Eq for DocumentClient {}

impl Client {
    pub(crate) fn secure_context(&self) -> bool {
        self.state.lock().unwrap().secure
    }
    pub(crate) fn window_state(&self) -> WindowState {
        self.state.lock().unwrap().window.clone()
    }
    pub(crate) async fn inspect(&self) -> Result<Option<ClientInfo>, Error> {
        let Some(info) = self.info()? else {
            return Ok(None);
        };
        if info.kind == ClientKind::Window && self.state.lock().unwrap().notify.is_some() {
            let (sender, receiver) = tokio::sync::oneshot::channel();
            self.send(Notice::Inspect {
                inspection: Arc::new(ClientInspection(Mutex::new(Some(sender)))),
            });
            if let Ok(Some(window)) = receiver.await {
                self.configure(ClientKind::Window, window);
            }
        }
        // The frontend may have discarded this environment while its task
        // was queued. A retired actor alone leaves the Document's snapshot.
        self.info()
    }
    fn update_focus(&self, state: &mut ClientState, focused: bool) {
        if focused && !state.window.focused {
            state.focus_order = self.registry.upgrade().map(|r| r.revision());
        }
        state.window.focused = focused;
    }
    pub(crate) fn configure(&self, kind: ClientKind, window: WindowState) {
        let mut state = self.state.lock().unwrap();
        self.update_focus(&mut state, window.focused);
        state.kind = kind;
        state.window = window;
    }
    pub(crate) fn info(&self) -> Result<Option<ClientInfo>, Error> {
        let state = self.state.lock().unwrap();
        if state.discarded || !state.execution_ready {
            return Ok(None);
        }
        if !state.secure {
            return Err(Error::security("The client is not a secure context"));
        }
        Ok(Some(ClientInfo {
            id: self.id.clone(),
            url: self.url.to_string(),
            kind: state.kind,
            window: state.window.clone(),
            creation_order: self.creation_order,
            focus_order: state.focus_order,
        }))
    }
    pub(crate) fn controller(&self) -> Option<Arc<Worker>> {
        self.state
            .lock()
            .unwrap()
            .controller
            .as_ref()
            .map(|c| c.worker.clone())
    }
    pub(crate) fn registration(&self) -> Option<Arc<Registration>> {
        self.state
            .lock()
            .unwrap()
            .controller
            .as_ref()
            .map(|c| c.registration.clone())
    }
    pub(crate) fn attach(&self, notify: Notify) {
        self.attach_with_readiness(notify, true);
    }
    pub(crate) fn attach_with_readiness(&self, notify: Notify, execution_ready: bool) -> u64 {
        let mut state = self.state.lock().unwrap();
        if state.discarded {
            return 0;
        }
        state.execution_ready = execution_ready;
        state.notify = Some(notify);
        state.attachment += 1;
        self.ready.notify_waiters();
        state.attachment
    }
    pub(crate) fn detach(&self, attachment: u64) {
        let mut state = self.state.lock().unwrap();
        if state.attachment == attachment {
            state.notify = None;
        }
    }
    pub(crate) fn set_execution_ready(&self) {
        self.state.lock().unwrap().execution_ready = true;
        self.ready.notify_waiters();
    }
    /// The secure-context check includes ancestors, so URL trustworthiness
    /// alone cannot authorize claim() or Clients.matchAll(). Set this from
    /// the environment's secure-context result before exposing its global.
    pub(crate) fn set_secure_context(&self, secure: bool) {
        self.state.lock().unwrap().secure = secure;
    }
    pub(crate) async fn await_ready(&self) -> bool {
        loop {
            let ready = self.ready.notified();
            tokio::pin!(ready);
            ready.as_mut().enable();
            {
                let state = self.state.lock().unwrap();
                if state.discarded {
                    return false;
                }
                if state.execution_ready {
                    return true;
                }
            }
            ready.await;
        }
    }
    fn send(&self, notice: Notice) {
        let callback = {
            let state = self.state.lock().unwrap();
            (!state.discarded).then(|| state.notify.clone()).flatten()
        };
        if let Some(callback) = callback {
            callback(notice);
        }
    }
    fn release_packet(&self, packet: &str) {
        if let Some(registry) = self.registry.upgrade() {
            registry.release_packet(packet);
        }
    }
    pub(crate) fn messages_enabled(&self) -> bool {
        self.state.lock().unwrap().messages_enabled
    }
    /// A message source snapshot, without the secure/execution-ready gates
    /// Clients queries apply: ServiceWorker/postMessage creates its source
    /// from the incumbent settings object as it was when posted.
    pub(crate) fn source_info(&self) -> Option<ClientInfo> {
        let state = self.state.lock().unwrap();
        if state.discarded {
            return None;
        }
        Some(ClientInfo {
            id: self.id.clone(),
            url: self.url.to_string(),
            kind: state.kind,
            window: state.window.clone(),
            creation_order: self.creation_order,
            focus_order: state.focus_order,
        })
    }
    /// Client/postMessage's parallel enqueue. Ownership of the packet's
    /// transfers stays native until the container takes the message or
    /// the queue discards it here.
    pub(crate) fn enqueue_message(&self, pending: PendingMessage) {
        let (wake, discard) = {
            let mut state = self.state.lock().unwrap();
            if state.discarded
                || state.messages.len() >= MAX_QUEUED_MESSAGES
                || pending.packet.len()
                    > MAX_QUEUED_MESSAGE_BYTES.saturating_sub(state.message_bytes)
            {
                (false, Some(pending))
            } else {
                state.message_bytes += pending.packet.len();
                state.messages.push_back(pending);
                (state.messages_enabled && state.notify.is_some(), None)
            }
        };
        if let Some(discard) = discard {
            self.release_packet(&discard.packet);
        } else if wake {
            self.send(Notice::Messages);
        }
    }
    pub(crate) fn take_message(&self) -> Option<PendingMessage> {
        let mut state = self.state.lock().unwrap();
        if state.discarded || !state.messages_enabled {
            return None;
        }
        let pending = state.messages.pop_front()?;
        state.message_bytes -= pending.packet.len();
        Some(pending)
    }
    pub(crate) fn enable_messages(&self) {
        self.state.lock().unwrap().messages_enabled = true;
    }
    /// Re-wake a (re)attached container that already enabled its queue.
    pub(crate) fn wake_messages(&self) {
        let wake = {
            let state = self.state.lock().unwrap();
            state.messages_enabled && !state.messages.is_empty() && state.notify.is_some()
        };
        if wake {
            self.send(Notice::Messages);
        }
    }
    pub(crate) fn discard(&self) {
        let (controller, queued) = {
            let mut state = self.state.lock().unwrap();
            state.discarded = true;
            state.notify = None;
            state.message_bytes = 0;
            (state.controller.take(), std::mem::take(&mut state.messages))
        };
        self.ready.notify_waiters();
        for pending in queued {
            self.release_packet(&pending.packet);
        }
        if let (Some(registry), Some(controller)) = (self.registry.upgrade(), controller) {
            registry.client_left(&controller.registration);
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.discard();
    }
}

/// Work returned by the lifecycle model for the runtime coordinator. The
/// caller terminates obsolete agents before dispatching Activate, and calls
/// activated after that event's lifetime promises settle (including rejection).
pub(crate) enum Action {
    Terminate(Arc<Worker>),
    Activate(Arc<Registration>, Arc<Worker>),
}

type ActionSink = Arc<dyn Fn(Vec<Action>) + Send + Sync>;

#[derive(Default)]
struct RegistryState {
    registrations: BTreeMap<(String, String), Arc<Registration>>,
    clients: HashMap<String, Weak<Client>>,
}

pub(crate) struct Registry {
    state: Mutex<RegistryState>,
    next_id: AtomicU64,
    revision: AtomicU64,
    actions: ActionSink,
    release: Option<PacketRelease>,
}

impl Registry {
    pub(crate) fn new(actions: ActionSink, release: Option<PacketRelease>) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::default(),
            next_id: AtomicU64::new(1),
            revision: AtomicU64::new(1),
            actions,
            release,
        })
    }
    pub(crate) fn release_packet(&self, packet: &str) {
        if let Some(release) = &self.release {
            release(packet);
        }
    }
    fn revision(&self) -> u64 {
        self.revision.fetch_add(1, Ordering::Relaxed)
    }
    pub(crate) fn get(&self, key: &str, scope: &Url) -> Option<Arc<Registration>> {
        self.state
            .lock()
            .unwrap()
            .registrations
            .get(&(key.to_string(), scope.to_string()))
            .cloned()
    }
    pub(crate) fn matching(&self, key: &str, url: &Url) -> Option<Arc<Registration>> {
        self.state
            .lock()
            .unwrap()
            .registrations
            .values()
            .filter(|r| r.key == key && scope_matches(&r.scope, url))
            .max_by_key(|r| r.scope.as_str().len())
            .cloned()
    }
    pub(crate) fn registrations(&self, key: &str) -> Vec<Arc<Registration>> {
        let mut values: Vec<_> = self
            .state
            .lock()
            .unwrap()
            .registrations
            .values()
            .filter(|r| r.key == key)
            .cloned()
            .collect();
        values.sort_by_key(|r| r.id);
        values
    }
    #[cfg(test)]
    pub(crate) fn set_registration(
        self: &Arc<Self>,
        key: &str,
        options: &RegistrationOptions,
    ) -> Arc<Registration> {
        self.set_registration_bounded(key, options, usize::MAX, usize::MAX)
            .unwrap()
    }
    pub(crate) fn set_registration_bounded(
        self: &Arc<Self>,
        key: &str,
        options: &RegistrationOptions,
        origin_limit: usize,
        profile_limit: usize,
    ) -> Result<Arc<Registration>, Error> {
        let mut state = self.state.lock().unwrap();
        if !state
            .registrations
            .contains_key(&(key.into(), options.scope.to_string()))
            && (state.registrations.len() >= profile_limit
                || state
                    .registrations
                    .keys()
                    .filter(|(owner, _)| owner == key)
                    .count()
                    >= origin_limit)
        {
            return Err(Error::new(
                "QuotaExceededError",
                "Too many service-worker registrations",
            ));
        }
        Ok(state
            .registrations
            .entry((key.into(), options.scope.to_string()))
            .or_insert_with(|| {
                Arc::new(Registration {
                    id: self.next_id.fetch_add(1, Ordering::Relaxed),
                    key: key.into(),
                    scope: options.scope.clone(),
                    registry: Arc::downgrade(self),
                    registered: AtomicBool::new(true),
                    state: Mutex::new(RegistrationState {
                        update_via_cache: options.update_via_cache,
                        installing: None,
                        waiting: None,
                        active: None,
                        revision: self.revision(),
                    }),
                    cookies: Mutex::default(),
                })
            })
            .clone())
    }
    pub(crate) fn new_worker(
        &self,
        registration: &Arc<Registration>,
        options: &RegistrationOptions,
    ) -> Arc<Worker> {
        Arc::new(Worker {
            id: self.next_id.fetch_add(1, Ordering::Relaxed),
            script: options.script.clone(),
            kind: options.kind,
            registration: Arc::downgrade(registration),
            state: Mutex::new((WorkerState::Parsed, self.revision())),
            skip_waiting: AtomicBool::new(false),
            pending_events: AtomicUsize::new(0),
            version: Mutex::new(None),
            start_lock: tokio::sync::Mutex::new(()),
            installation_notifications_pending: AtomicBool::new(false),
            event_types: std::sync::OnceLock::new(),
            changed: tokio::sync::Notify::new(),
        })
    }
    pub(crate) fn reserve_client(
        self: &Arc<Self>,
        key: String,
        url: Url,
        match_navigation: bool,
    ) -> Result<Arc<Client>, Error> {
        let origin = url.origin();
        self.reserve_client_with_origin(key, url, origin, match_navigation)
    }
    pub(crate) fn reserve_client_with_origin(
        self: &Arc<Self>,
        key: String,
        url: Url,
        origin: url::Origin,
        match_navigation: bool,
    ) -> Result<Arc<Client>, Error> {
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes).map_err(|_| {
            Error::type_error("Unable to create a service-worker client identifier")
        })?;
        let id: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
        let registration = match_navigation
            .then(|| self.matching(&key, &url))
            .flatten();
        // Handle Fetch's controller selection and Try Activate must agree
        // on whether this environment uses the active registration. Publish
        // the client before releasing the same lock Try Activate consults.
        let registration_state = registration.as_ref().map(|r| r.state.lock().unwrap());
        let controller = registration.as_ref().and_then(|registration| {
            registration_state
                .as_ref()
                .and_then(|state| state.active.clone())
                .map(|worker| Controller {
                    registration: registration.clone(),
                    worker,
                })
        });
        let secure = crate::http::potentially_trustworthy(&url);
        let client = Arc::new(Client {
            id,
            key,
            url,
            origin,
            creation_order: self.revision(),
            registry: Arc::downgrade(self),
            state: Mutex::new(ClientState {
                controller,
                notify: None,
                attachment: 0,
                discarded: false,
                execution_ready: false,
                secure,
                kind: ClientKind::Window,
                window: WindowState::default(),
                focus_order: None,
                messages: VecDeque::new(),
                message_bytes: 0,
                messages_enabled: false,
            }),
            ready: tokio::sync::Notify::new(),
        });
        let mut state = self.state.lock().unwrap();
        state.clients.retain(|_, client| client.strong_count() != 0);
        state
            .clients
            .insert(client.id.clone(), Arc::downgrade(&client));
        drop(state);
        drop(registration_state);
        Ok(client)
    }
    fn clients(&self, key: &str) -> Vec<Arc<Client>> {
        let clients: Vec<_> = self
            .state
            .lock()
            .unwrap()
            .clients
            .values()
            .cloned()
            .collect();
        // Dropping the last Client calls client_left; never do that while
        // holding the registration/client map lock.
        clients
            .into_iter()
            .filter_map(|client| client.upgrade())
            .filter(|client| client.key == key && !client.state.lock().unwrap().discarded)
            .collect()
    }
    pub(crate) fn get_client(&self, key: &str, id: &str) -> Option<Arc<Client>> {
        self.clients(key).into_iter().find(|client| client.id == id)
    }
    pub(crate) fn matching_clients(
        &self,
        key: &str,
        worker: u64,
        uncontrolled: bool,
        kind: Option<ClientKind>,
    ) -> Vec<Arc<Client>> {
        self.clients(key)
            .into_iter()
            .filter(|client| {
                let state = client.state.lock().unwrap();
                state.execution_ready
                    && !state.discarded
                    && state.secure
                    && kind.is_none_or(|kind| kind == state.kind)
                    && (uncontrolled
                        || state
                            .controller
                            .as_ref()
                            .is_some_and(|c| c.worker.id == worker))
            })
            .collect()
    }
    fn broadcast(&self, key: &str, notice: Notice) {
        for client in self.clients(key) {
            client.send(notice.clone());
        }
    }
    pub(crate) async fn state_updates_observed(&self, key: &str) {
        let mut completions = Vec::new();
        for client in self.clients(key) {
            let (sender, receiver) = tokio::sync::oneshot::channel();
            client.send(Notice::Checkpoint {
                completion: Arc::new(Checkpoint(Mutex::new(Some(sender)))),
            });
            completions.push(receiver);
        }
        for receiver in completions {
            let _ = receiver.await;
        }
    }
    fn set_worker_state(&self, key: &str, worker: &Arc<Worker>, state: WorkerState) {
        *worker.state.lock().unwrap() = (state, self.revision());
        worker.changed.notify_waiters();
        if state == WorkerState::Redundant {
            // Interface objects retain identity/state, but redundant workers
            // can never run again. A terminating scope holds its own version
            // until execution stops; the interface need not retain script RAM.
            worker.version.lock().unwrap().take();
        }
        self.broadcast(
            key,
            Notice::Worker {
                worker: worker.info(),
            },
        );
    }
    fn registration_changed(&self, registration: &Arc<Registration>, update_found: bool) {
        registration.state.lock().unwrap().revision = self.revision();
        self.broadcast(
            &registration.key,
            Notice::Registration {
                registration: registration.info(),
                update_found,
            },
        );
    }
    pub(crate) fn installing(
        &self,
        registration: &Arc<Registration>,
        worker: &Arc<Worker>,
        update_via_cache: UpdateViaCache,
    ) {
        {
            let mut state = registration.state.lock().unwrap();
            state.update_via_cache = update_via_cache;
            state.installing = Some(worker.clone());
        }
        self.set_worker_state(&registration.key, worker, WorkerState::Installing);
        self.registration_changed(registration, false);
    }
    /// Resolve the register/update job promise before queueing updatefound.
    pub(crate) fn update_found(&self, registration: &Arc<Registration>) {
        self.registration_changed(registration, true);
    }
    pub(crate) fn installed(
        &self,
        registration: &Arc<Registration>,
        worker: &Arc<Worker>,
        succeeded: bool,
    ) {
        let mut actions = Vec::new();
        let mut state = registration.state.lock().unwrap();
        if state
            .installing
            .as_ref()
            .is_none_or(|installing| installing.id != worker.id)
        {
            return;
        }
        state.installing = None;
        if !succeeded {
            let empty = state.waiting.is_none() && state.active.is_none();
            drop(state);
            self.set_worker_state(&registration.key, worker, WorkerState::Redundant);
            actions.push(Action::Terminate(worker.clone()));
            if empty {
                self.remove_registration(registration);
            }
            self.registration_changed(registration, false);
        } else {
            let old = state.waiting.replace(worker.clone());
            drop(state);
            if let Some(old) = old {
                self.set_worker_state(&registration.key, &old, WorkerState::Redundant);
                actions.push(Action::Terminate(old));
            }
            self.registration_changed(registration, false);
            self.set_worker_state(&registration.key, worker, WorkerState::Installed);
        }
        (self.actions)(actions);
    }
    fn using(&self, registration: &Registration) -> Vec<Arc<Client>> {
        self.clients(&registration.key)
            .into_iter()
            .filter(|client| {
                client
                    .registration()
                    .is_some_and(|r| r.id == registration.id)
            })
            .collect()
    }
    pub(crate) fn try_activate(&self, registration: &Arc<Registration>) {
        // Declare client owners first so an early return drops the lock
        // before the last owner can invoke Handle Client Unload from Drop.
        let using;
        let mut state = registration.state.lock().unwrap();
        using = self.using(registration);
        let Some(waiting) = state.waiting.clone() else {
            return;
        };
        if waiting
            .installation_notifications_pending
            .load(Ordering::Acquire)
        {
            return;
        }
        if state.active.as_ref().is_some_and(|worker| {
            worker.state() == WorkerState::Activating
                || worker.pending_events.load(Ordering::Acquire) != 0
                || !using.is_empty() && !waiting.skip_waiting.load(Ordering::Acquire)
        }) {
            return;
        }
        let previous = state.active.replace(waiting.clone());
        state.waiting = None;
        drop(state);
        let mut actions = Vec::new();
        if let Some(previous) = previous {
            self.set_worker_state(&registration.key, &previous, WorkerState::Redundant);
            actions.push(Action::Terminate(previous));
        }
        self.set_worker_state(&registration.key, &waiting, WorkerState::Activating);
        self.registration_changed(registration, false);
        for client in self.clients(&registration.key) {
            if self
                .matching(&client.key, &client.url)
                .is_some_and(|r| r.id == registration.id)
            {
                client.send(Notice::Ready {
                    registration: registration.info(),
                });
            }
        }
        for client in using {
            let mut state = client.state.lock().unwrap();
            if state.discarded
                || state
                    .controller
                    .as_ref()
                    .is_none_or(|c| c.registration.id != registration.id)
            {
                continue;
            }
            state.controller = Some(Controller {
                registration: registration.clone(),
                worker: waiting.clone(),
            });
            drop(state);
            client.send(Notice::Controller {
                worker: Some(waiting.info()),
            });
        }
        actions.push(Action::Activate(registration.clone(), waiting));
        (self.actions)(actions);
    }
    pub(crate) fn activated(&self, registration: &Arc<Registration>, worker: &Arc<Worker>) {
        if worker.state() == WorkerState::Activating {
            self.set_worker_state(&registration.key, worker, WorkerState::Activated);
            self.registration_changed(registration, false);
            self.client_left(registration);
        }
    }
    pub(crate) fn claim(
        &self,
        registration: &Arc<Registration>,
        worker: &Arc<Worker>,
    ) -> Result<(), Error> {
        let clients = self.clients(&registration.key);
        let registration_state = registration.state.lock().unwrap();
        if registration_state
            .active
            .as_ref()
            .is_none_or(|active| active.id != worker.id)
        {
            return Err(Error::new(
                "InvalidStateError",
                "Only an active service worker can claim clients",
            ));
        }
        let mut departed = Vec::new();
        for client in &clients {
            if !self
                .matching(&client.key, &client.url)
                .is_some_and(|r| r.id == registration.id)
            {
                continue;
            }
            let mut state = client.state.lock().unwrap();
            if state.discarded
                || !state.execution_ready
                || !state.secure
                || state
                    .controller
                    .as_ref()
                    .is_some_and(|c| c.worker.id == worker.id)
            {
                continue;
            }
            let previous = state.controller.replace(Controller {
                registration: registration.clone(),
                worker: worker.clone(),
            });
            drop(state);
            client.send(Notice::Controller {
                worker: Some(worker.info()),
            });
            if let Some(previous) = previous {
                departed.push(previous.registration);
            }
        }
        // Cleanup can try activation on this same registration. Run it only
        // after publishing the new controller and releasing its state lock.
        drop(registration_state);
        for previous in departed {
            self.client_left(&previous);
        }
        Ok(())
    }
    fn remove_registration(&self, registration: &Registration) -> bool {
        let mut state = self.state.lock().unwrap();
        let key = (registration.key.clone(), registration.scope.to_string());
        if state
            .registrations
            .get(&key)
            .is_none_or(|r| r.id != registration.id)
        {
            return false;
        }
        state.registrations.remove(&key);
        registration.registered.store(false, Ordering::Release);
        registration.cookies.lock().unwrap().observer = None;
        true
    }
    /// Update failed before installing: only a newly empty registration is removed.
    pub(crate) fn failed_update(
        &self,
        registration: &Arc<Registration>,
        worker: Option<&Arc<Worker>>,
    ) {
        if let Some(worker) = worker {
            self.set_worker_state(&registration.key, worker, WorkerState::Redundant);
        }
        if registration.newest().is_none() {
            self.remove_registration(registration);
        }
    }
    #[cfg(test)]
    pub(crate) fn unregister(&self, registration: &Arc<Registration>) -> bool {
        self.unregister_resolving(registration, |_| {})
    }
    /// Unregister queues its successful job result before Try Clear
    /// Registration queues redundant/state changes (SW #unregister-algorithm).
    pub(crate) fn unregister_resolving(
        &self,
        registration: &Arc<Registration>,
        resolve: impl FnOnce(bool),
    ) -> bool {
        let removed = self.remove_registration(registration);
        resolve(removed);
        if removed {
            self.client_left(registration);
        }
        removed
    }
    fn client_left(&self, registration: &Arc<Registration>) {
        if registration.registered.load(Ordering::Acquire) {
            self.try_activate(registration);
            return;
        }
        let using;
        let mut state = registration.state.lock().unwrap();
        using = self.using(registration);
        if !using.is_empty() {
            return;
        }
        if [&state.installing, &state.waiting, &state.active]
            .into_iter()
            .any(|worker| {
                worker
                    .as_ref()
                    .is_some_and(|worker| worker.pending_events.load(Ordering::Acquire) != 0)
            })
        {
            return;
        }
        let workers: Vec<_> = [
            state.installing.take(),
            state.waiting.take(),
            state.active.take(),
        ]
        .into_iter()
        .flatten()
        .collect();
        drop(state);
        let mut actions = Vec::new();
        for worker in workers {
            self.set_worker_state(&registration.key, &worker, WorkerState::Redundant);
            actions.push(Action::Terminate(worker));
        }
        self.registration_changed(registration, false);
        (self.actions)(actions);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(script: &str, scope: Option<&str>) -> Result<RegistrationOptions, Error> {
        let base = Url::parse("https://service-worker.test/page/index").unwrap();
        RegistrationOptions::parse(
            &base,
            &base.origin(),
            script,
            scope,
            WorkerType::Classic,
            UpdateViaCache::Imports,
        )
    }

    fn registry() -> (Arc<Registry>, Arc<Mutex<Vec<Action>>>) {
        let actions = Arc::new(Mutex::new(Vec::new()));
        let output = actions.clone();
        (
            Registry::new(
                Arc::new(move |next| output.lock().unwrap().extend(next)),
                None,
            ),
            actions,
        )
    }

    fn install(
        registry: &Arc<Registry>,
        registration: &Arc<Registration>,
        script: &str,
    ) -> Arc<Worker> {
        let options = parse(script, Some("/")).unwrap();
        let worker = registry.new_worker(registration, &options);
        registry.installing(registration, &worker, options.update_via_cache);
        registry.installed(registration, &worker, true);
        worker
    }

    #[test]
    fn install_activate_claim_and_updates_preserve_client_control() {
        let (registry, actions) = registry();
        let key = "https://service-worker.test";
        let url = Url::parse("https://service-worker.test/page").unwrap();
        let original = registry
            .reserve_client(key.into(), url.clone(), true)
            .unwrap();
        original.attach(Arc::new(|_| {}));
        let registration = registry.set_registration(key, &parse("/v1.js", Some("/")).unwrap());
        let first = install(&registry, &registration, "/v1.js");
        assert_eq!(first.state(), WorkerState::Installed);
        registry.try_activate(&registration);
        assert_eq!(first.state(), WorkerState::Activating);
        assert!(
            original.controller().is_none(),
            "installation does not claim existing clients"
        );
        registry.activated(&registration, &first);
        let later = registry.reserve_client(key.into(), url, true).unwrap();
        assert_eq!(later.controller().unwrap().id, first.id);
        registry.claim(&registration, &first).unwrap();
        assert_eq!(original.controller().unwrap().id, first.id);
        let second = install(&registry, &registration, "/v2.js");
        registry.try_activate(&registration);
        assert_eq!(
            second.state(),
            WorkerState::Installed,
            "controlled clients keep the old version active"
        );
        let event = registration.begin_event(&first).unwrap();
        second.skip_waiting.store(true, Ordering::Release);
        registry.try_activate(&registration);
        assert_eq!(
            second.state(),
            WorkerState::Installed,
            "skipWaiting still waits for pending events"
        );
        drop(event);
        assert_eq!(first.state(), WorkerState::Redundant);
        assert_eq!(second.state(), WorkerState::Activating);
        assert_eq!(original.controller().unwrap().id, second.id);
        assert_eq!(later.controller().unwrap().id, second.id);
        assert!(
            actions
                .lock()
                .unwrap()
                .iter()
                .any(|action| matches!(action, Action::Terminate(worker) if worker.id==first.id))
        );
    }

    #[test]
    fn message_events_admit_waiting_workers_and_client_queues_hold_until_enabled() {
        let (registry, _) = registry();
        let key = "https://service-worker.test";
        let url = Url::parse("https://service-worker.test/page").unwrap();
        let registration = registry.set_registration(key, &parse("/v1.js", Some("/")).unwrap());
        let first = install(&registry, &registration, "/v1.js");
        registry.try_activate(&registration);
        registry.activated(&registration, &first);
        let client = registry.reserve_client(key.into(), url, true).unwrap();
        let wakes = Arc::new(AtomicUsize::new(0));
        let counted = wakes.clone();
        client.attach(Arc::new(move |notice| {
            if matches!(notice, Notice::Messages) {
                counted.fetch_add(1, Ordering::AcqRel);
            }
        }));
        registry.claim(&registration, &first).unwrap();
        let second = install(&registry, &registration, "/v2.js");
        registry.try_activate(&registration);
        assert_eq!(second.state(), WorkerState::Installed);
        assert!(
            registration.begin_event(&second).is_some(),
            "a waiting worker can run for a message"
        );
        assert!(registration.begin_event(&first).is_some());

        let pending = |value: usize| PendingMessage {
            packet: format!("packet{value}"),
            origin: key.into(),
            source: first.info(),
        };
        client.enqueue_message(pending(1));
        assert!(client.take_message().is_none(), "queue starts disabled");
        assert_eq!(wakes.load(Ordering::Acquire), 0);
        client.enable_messages();
        assert!(client.messages_enabled());
        assert_eq!(client.take_message().unwrap().packet, "packet1");
        client.enqueue_message(pending(2));
        assert_eq!(wakes.load(Ordering::Acquire), 1, "enabled enqueue wakes");
        assert_eq!(client.take_message().unwrap().packet, "packet2");

        for value in 0..MAX_QUEUED_MESSAGES + 5 {
            client.enqueue_message(pending(value));
        }
        let mut taken = 0;
        while client.take_message().is_some() {
            taken += 1;
        }
        assert_eq!(taken, MAX_QUEUED_MESSAGES, "queue count is bounded");

        client.enqueue_message(pending(99));
        client.discard();
        assert!(client.take_message().is_none(), "discard clears the queue");
    }

    #[test]
    fn document_ownership_survives_actor_detach_but_not_document_discard() {
        let (registry, _) = registry();
        let key = "https://service-worker.test";
        let registration = registry.set_registration(key, &parse("/v1.js", Some("/")).unwrap());
        let first = install(&registry, &registration, "/v1.js");
        registry.try_activate(&registration);
        registry.activated(&registration, &first);
        let client = registry
            .reserve_client(key.into(), registration.scope.clone(), true)
            .unwrap();
        client.set_execution_ready();
        let document = DocumentClient::new(client.clone());
        let presentation = document.clone();
        let attachment = client.attach_with_readiness(Arc::new(|_| {}), true);
        client.detach(attachment);
        assert_eq!(registry.clients(key).len(), 1);
        assert!(client.state.lock().unwrap().execution_ready);
        let second = install(&registry, &registration, "/v2.js");
        registry.try_activate(&registration);
        assert_eq!(second.state(), WorkerState::Installed);
        drop(document);
        assert_eq!(second.state(), WorkerState::Installed);
        drop(presentation);
        // An outstanding request's Arc<Client> cannot keep the closed
        // Document using the old registration or block its replacement.
        assert!(client.controller().is_none());
        assert!(registry.clients(key).is_empty());
        assert_eq!(second.state(), WorkerState::Activating);
    }

    #[test]
    fn stale_actor_detach_does_not_remove_replacement_attachment() {
        let (registry, _) = registry();
        let url = Url::parse("https://service-worker.test/page").unwrap();
        let client = registry
            .reserve_client(url.origin().ascii_serialization(), url, false)
            .unwrap();
        let old = client.attach_with_readiness(Arc::new(|_| {}), true);
        let notices = Arc::new(Mutex::new(0));
        let seen = notices.clone();
        let current =
            client.attach_with_readiness(Arc::new(move |_| *seen.lock().unwrap() += 1), true);
        client.detach(old);
        client.send(Notice::Controller { worker: None });
        assert_eq!(*notices.lock().unwrap(), 1);
        client.detach(current);
        client.send(Notice::Controller { worker: None });
        assert_eq!(*notices.lock().unwrap(), 1);
        client.discard();
        assert!(!futures::executor::block_on(client.await_ready()));
    }

    #[test]
    fn navigation_and_activation_atomically_select_a_live_controller() {
        for _ in 0..64 {
            let (registry, _) = registry();
            let key = "https://service-worker.test";
            let registration = registry.set_registration(key, &parse("/v1.js", Some("/")).unwrap());
            let first = install(&registry, &registration, "/v1.js");
            registry.try_activate(&registration);
            registry.activated(&registration, &first);
            let second = install(&registry, &registration, "/v2.js");
            let barrier = Arc::new(std::sync::Barrier::new(2));
            let nav_registry = registry.clone();
            let nav_barrier = barrier.clone();
            let url = registration.scope.clone();
            let navigation = std::thread::spawn(move || {
                nav_barrier.wait();
                nav_registry.reserve_client(key.into(), url, true).unwrap()
            });
            barrier.wait();
            registry.try_activate(&registration);
            let client = navigation.join().unwrap();
            let controller = client.controller().unwrap();
            assert_ne!(controller.state(), WorkerState::Redundant);
            if controller.id == first.id {
                assert_eq!(second.state(), WorkerState::Installed);
            } else {
                assert_eq!(controller.id, second.id);
                assert_eq!(second.state(), WorkerState::Activating);
            }
        }
    }

    #[test]
    fn client_queries_filter_readiness_security_control_and_sort_focus_history() {
        let (registry, _) = registry();
        let key = "https://service-worker.test";
        let registration = registry.set_registration(key, &parse("/v1.js", Some("/")).unwrap());
        let worker = install(&registry, &registration, "/v1.js");
        registry.try_activate(&registration);
        registry.activated(&registration, &worker);
        let make = |path: &str, ready: bool| {
            let client = registry
                .reserve_client(key.into(), registration.scope.join(path).unwrap(), false)
                .unwrap();
            if ready {
                client.set_execution_ready();
            }
            client
        };
        let first = make("first", true);
        let second = make("second", true);
        let unfocused = make("never-focused", true);
        let dedicated = make("worker.js", true);
        dedicated.configure(
            ClientKind::Worker,
            WindowState {
                frame_type: "none".into(),
                ..Default::default()
            },
        );
        let pending = make("pending", false);
        let insecure = make("sandboxed-ancestor", true);
        insecure.set_secure_context(false);
        assert!(
            registry
                .matching_clients(key, worker.id, false, None)
                .is_empty()
        );
        assert_eq!(
            registry
                .matching_clients(key, worker.id, true, Some(ClientKind::Window))
                .len(),
            3
        );
        registry.claim(&registration, &worker).unwrap();
        assert!(pending.controller().is_none());
        assert!(insecure.controller().is_none());
        assert_eq!(insecure.info().unwrap_err().name, "SecurityError");
        assert!(registry.get_client("other-partition", &first.id).is_none());
        first.configure(
            ClientKind::Window,
            WindowState {
                focused: true,
                ..Default::default()
            },
        );
        first.configure(ClientKind::Window, WindowState::default());
        second.configure(
            ClientKind::Window,
            WindowState {
                focused: true,
                ..Default::default()
            },
        );
        let mut clients: Vec<_> = registry
            .matching_clients(key, worker.id, false, None)
            .into_iter()
            .map(|client| client.info().unwrap().unwrap())
            .collect();
        ClientInfo::sort(&mut clients);
        assert_eq!(
            clients.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            [
                second.id.as_str(),
                first.id.as_str(),
                unfocused.id.as_str(),
                dedicated.id.as_str()
            ]
        );
        first.discard();
        assert!(registry.get_client(key, &first.id).is_none());
    }

    #[test]
    fn closing_a_document_discards_nested_environments_despite_retained_child_handles() {
        let (registry, _) = registry();
        let base = Url::parse("https://service-worker.test/").unwrap();
        let make = |index: usize| {
            let client = registry
                .reserve_client(
                    base.origin().ascii_serialization(),
                    base.join(&format!("{index}")).unwrap(),
                    false,
                )
                .unwrap();
            client.set_execution_ready();
            DocumentClient::new(client)
        };
        let root = make(0);
        let mut deepest = root.clone();
        for index in 1..512 {
            let child = make(index);
            deepest.adopt_child(index as u64, &child);
            deepest = child;
        }
        let client = deepest.client();
        assert!(client.info().unwrap().is_some());
        drop(root);
        assert!(client.info().unwrap().is_none());
        assert!(!futures::executor::block_on(client.await_ready()));
        assert!(
            registry
                .clients(&base.origin().ascii_serialization())
                .is_empty()
        );
    }

    #[test]
    fn unregistration_keeps_existing_clients_until_they_leave() {
        let (registry, _) = registry();
        let key = "https://service-worker.test";
        let options = parse("/worker.js", Some("/")).unwrap();
        let registration = registry.set_registration(key, &options);
        let first = install(&registry, &registration, "/worker.js");
        registry.try_activate(&registration);
        registry.activated(&registration, &first);
        let client = registry
            .reserve_client(key.into(), options.scope.clone(), true)
            .unwrap();
        assert!(registry.unregister(&registration));
        assert!(!registry.unregister(&registration));
        assert!(registry.matching(key, &options.scope).is_none());
        assert_eq!(client.controller().unwrap().id, first.id);
        assert_eq!(first.state(), WorkerState::Activated);
        let replacement = registry.set_registration(key, &options);
        assert_ne!(registration.id, replacement.id);
        client.discard();
        assert_eq!(first.state(), WorkerState::Redundant);
        assert!(registration.active().is_none());
        assert_eq!(
            registry.get(key, &options.scope).unwrap().id,
            replacement.id
        );
    }

    #[tokio::test]
    async fn claim_waits_for_execution_readiness_and_respects_ancestor_security() {
        let (registry, _) = registry();
        let key = "https://service-worker.test";
        let options = parse("/worker.js", Some("/")).unwrap();
        let pending = registry
            .reserve_client(key.into(), options.scope.clone(), false)
            .unwrap();
        let insecure = registry
            .reserve_client(key.into(), options.scope.clone(), false)
            .unwrap();
        insecure.set_secure_context(false);
        insecure.attach(Arc::new(|_| {}));
        let service_global = registry
            .reserve_client(key.into(), options.script.clone(), false)
            .unwrap();
        let notices = Arc::new(AtomicUsize::new(0));
        let observed = notices.clone();
        service_global.attach_with_readiness(
            Arc::new(move |_| {
                observed.fetch_add(1, Ordering::Relaxed);
            }),
            false,
        );
        let registration = registry.set_registration(key, &options);
        let worker = install(&registry, &registration, "/worker.js");
        registry.try_activate(&registration);
        registry.activated(&registration, &worker);
        registry.claim(&registration, &worker).unwrap();
        assert!(pending.controller().is_none());
        assert!(insecure.controller().is_none());
        let waiting = pending.clone();
        let ready = tokio::spawn(async move { waiting.await_ready().await });
        pending.attach(Arc::new(|_| {}));
        assert!(ready.await.unwrap());
        registry.claim(&registration, &worker).unwrap();
        assert_eq!(pending.controller().unwrap().id, worker.id);
        assert!(insecure.controller().is_none());
        assert!(service_global.controller().is_none());
        assert!(
            notices.load(Ordering::Relaxed) > 0,
            "service globals still observe registration state"
        );
        let discarded = registry
            .reserve_client(key.into(), options.scope.clone(), false)
            .unwrap();
        let waiting = discarded.clone();
        let ready = tokio::spawn(async move { waiting.await_ready().await });
        discarded.discard();
        assert!(!ready.await.unwrap());
    }

    #[test]
    fn failed_install_does_not_replace_an_active_worker() {
        let (registry, _) = registry();
        let key = "https://service-worker.test";
        let options = parse("/worker.js", Some("/")).unwrap();
        let registration = registry.set_registration(key, &options);
        let first = install(&registry, &registration, "/worker.js");
        registry.try_activate(&registration);
        registry.activated(&registration, &first);
        let failed = registry.new_worker(&registration, &options);
        registry.installing(&registration, &failed, UpdateViaCache::None);
        registry.installed(&registration, &failed, false);
        assert_eq!(failed.state(), WorkerState::Redundant);
        assert_eq!(registration.active().unwrap().id, first.id);
        assert_eq!(
            registry.get(key, &options.scope).unwrap().id,
            registration.id
        );
        let separate = parse("/failed/worker.js", None).unwrap();
        let registration = registry.set_registration(key, &separate);
        let failed = registry.new_worker(&registration, &separate);
        registry.installing(&registration, &failed, UpdateViaCache::Imports);
        registry.installed(&registration, &failed, false);
        assert!(registry.get(key, &separate.scope).is_none());
    }

    #[test]
    fn longest_scope_and_storage_partition_select_one_registration() {
        let (registry, _) = registry();
        let broad = parse("/worker.js", Some("/")).unwrap();
        let narrow = parse("/worker.js", Some("/prefix")).unwrap();
        let first = registry.set_registration("origin", &broad);
        let second = registry.set_registration("origin", &narrow);
        let partitioned = registry.set_registration("origin^other-site", &narrow);
        let url = Url::parse("https://service-worker.test/prefix-of/page").unwrap();
        assert_eq!(registry.matching("origin", &url).unwrap().id, second.id);
        assert_eq!(
            registry.matching("origin^other-site", &url).unwrap().id,
            partitioned.id
        );
        assert!(registry.matching("unrelated", &url).is_none());
        assert_eq!(
            registry
                .registrations("origin")
                .iter()
                .map(|r| r.id)
                .collect::<Vec<_>>(),
            [first.id, second.id]
        );
    }

    #[test]
    fn registration_urls_follow_scope_origin_and_encoded_separator_rules() {
        let options = parse("/scripts/worker.js?q=%2f#fragment", None).unwrap();
        assert_eq!(
            options.script.as_str(),
            "https://service-worker.test/scripts/worker.js?q=%2f"
        );
        assert_eq!(
            options.scope.as_str(),
            "https://service-worker.test/scripts/"
        );
        assert!(options.check_max_scope(None).is_ok());
        for path in [
            "/scripts%2Fworker.js",
            "/scripts%5cworker.js",
            "data:text/javascript,1",
        ] {
            assert_eq!(parse(path, None).unwrap_err().name, "TypeError");
        }
        assert_eq!(
            parse("/worker.js", Some("/scope%2F/")).unwrap_err().name,
            "TypeError"
        );
        assert_eq!(
            parse("https://other.test/worker.js", None)
                .unwrap_err()
                .name,
            "SecurityError"
        );
        assert_eq!(
            parse("/worker.js", Some("https://other.test/"))
                .unwrap_err()
                .name,
            "SecurityError"
        );
        let widened = parse("/scripts/worker.js", Some("/#discarded")).unwrap();
        assert_eq!(widened.scope.as_str(), "https://service-worker.test/");
        assert!(widened.check_max_scope(None).is_err());
        assert!(widened.check_max_scope(Some("/")).is_ok());
        assert!(
            widened
                .check_max_scope(Some("https://other.test/"))
                .is_err()
        );
    }

    #[test]
    fn scope_matching_is_prefix_based_and_registration_needs_a_trustworthy_origin() {
        let scope = Url::parse("https://service-worker.test/prefix").unwrap();
        assert!(scope_matches(
            &scope,
            &scope.join("/prefix-of/resource").unwrap()
        ));
        assert!(!scope_matches(
            &scope,
            &Url::parse("https://service-worker.test.evil/prefix").unwrap()
        ));
        for base in [
            "http://127.0.0.1/",
            "http://localhost/",
            "https://service-worker.test/",
        ] {
            let base = Url::parse(base).unwrap();
            assert!(
                RegistrationOptions::parse(
                    &base,
                    &base.origin(),
                    "/worker.js",
                    None,
                    WorkerType::Classic,
                    UpdateViaCache::Imports
                )
                .is_ok()
            );
        }
        let base = Url::parse("http://service-worker.test/").unwrap();
        assert_eq!(
            RegistrationOptions::parse(
                &base,
                &base.origin(),
                "/worker.js",
                None,
                WorkerType::Classic,
                UpdateViaCache::Imports
            )
            .unwrap_err()
            .name,
            "SecurityError"
        );
    }
}
