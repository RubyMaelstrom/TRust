//! Service-worker jobs and agents share a browser storage shed, but keep all
//! registrations, script versions and runtime state in RAM. W3C Service
//! Workers 92aba3be: Register, Update, Install, Run/Terminate Service Worker.
use super::*;
use crate::service_worker::{
    self as sw, Action, Error, Notice, RegistrationOptions, Worker, WorkerState, WorkerType, jobs,
    scripts,
};
use futures::FutureExt;
use std::sync::{
    Mutex, Weak,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

type StorageMap = Mutex<HashMap<String, HashMap<String, String>>>;
const EXECUTION_TIMEOUT: Duration = Duration::from_secs(30);
const EVENT_TIMEOUT: Duration = Duration::from_secs(300);
pub(super) const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_AGENTS: usize = 32;
const MAX_ORIGIN_REGISTRATIONS: usize = 64;
const MAX_PROFILE_REGISTRATIONS: usize = 256;

pub(super) struct Profile {
    pub registry: Arc<sw::Registry>,
    queues: Arc<jobs::Queues>,
    budget: Arc<scripts::Budget>,
    handle: tokio::runtime::Handle,
    storage: Weak<StorageMap>,
    blobs: crate::js::BlobMap,
    ports: Arc<Mutex<message_port_host::Registry>>,
    permissions: PermissionStore,
    runtimes: Mutex<HashMap<u64, Arc<Runtime>>>,
    functional: Arc<tokio::sync::Semaphore>,
}

static PROFILES: Mutex<Vec<(Weak<StorageMap>, Arc<Profile>)>> = Mutex::new(Vec::new());

fn profile(state: &HostState) -> Option<Arc<Profile>> {
    let handle = state.network.as_ref()?.handle.clone();
    Some(profile_for(
        &state.storage,
        &state.blobs,
        handle,
        Some((
            state.message_ports.registry.clone(),
            state.permission_store.clone(),
        )),
    ))
}

/// Install shared capabilities before creating author-visible ports, including
/// in insecure Windows. A later auxiliary Window must not replace a registry
/// that already contains the first Window's MessageChannels.
pub(super) fn bind_profile(state: &mut HostState) {
    if let Some(profile) = profile(state) {
        state.message_ports.registry = profile.ports.clone();
        state.permission_store = profile.permissions.clone();
    }
}

fn profile_for(
    storage: &crate::js::WebStorage,
    page_blobs: &crate::js::BlobMap,
    handle: tokio::runtime::Handle,
    agent: Option<(Arc<Mutex<message_port_host::Registry>>, PermissionStore)>,
) -> Arc<Profile> {
    let key = Arc::downgrade(storage);
    let mut profiles = PROFILES.lock().unwrap();
    profiles.retain(|(key, _)| key.strong_count() != 0);
    if let Some((_, profile)) = profiles.iter().find(|(owner, _)| owner.ptr_eq(&key)) {
        profile.blobs.join(page_blobs);
        return profile.clone();
    }
    // The coordinator owns no document's entries. Its empty shard can resolve
    // live clients' URLs without keeping a closed page's blob data alive.
    let blobs = Arc::new(crate::blob_url_store::BlobUrlStore::default());
    blobs.join(page_blobs);
    let (ports, permissions) = agent.unwrap_or_default();
    let release_ports = ports.clone();
    let profile = Arc::new_cyclic(|weak: &Weak<Profile>| {
        let owner = weak.clone();
        let registry = sw::Registry::new(
            Arc::new(move |actions| {
                if let Some(profile) = owner.upgrade() {
                    profile.actions(actions);
                }
            }),
            Some(Arc::new(move |packet| {
                release_packet(&release_ports, packet)
            })),
        );
        Profile {
            registry,
            queues: Arc::default(),
            budget: Arc::default(),
            handle,
            storage: key.clone(),
            blobs,
            ports,
            permissions,
            runtimes: Mutex::default(),
            functional: Arc::new(tokio::sync::Semaphore::new(256)),
        }
    });
    profiles.push((key, profile.clone()));
    profile
}

/// HTML StructuredDeserializeWithTransfer may stop before claiming every
/// holder. Keep native ownership through delivery, cancellation and teardown;
/// cleanup only removes keys that were never claimed by the receiving realm.
struct MessagePacket {
    ports: Arc<Mutex<message_port_host::Registry>>,
    text: String,
}

impl MessagePacket {
    fn new(ports: Arc<Mutex<message_port_host::Registry>>, text: String) -> Self {
        Self { ports, text }
    }
    fn into_text(mut self) -> String {
        std::mem::take(&mut self.text)
    }
}

impl Drop for MessagePacket {
    fn drop(&mut self) {
        if !self.text.is_empty() {
            release_packet(&self.ports, &self.text);
        }
    }
}

/// Release a discarded packet's native transfers: MessagePort endpoints
/// (including transferred streams' entangled ports) and OffscreenCanvas
/// holders, recursively for packets queued on discarded ports. Bounded
/// work; unparseable packets hold no recognizable transfers.
fn release_packet(ports: &Arc<Mutex<message_port_host::Registry>>, packet: &str) {
    fn stream_keys(holder: &serde_json::Value, keys: &mut Vec<String>) {
        if holder["type"] == "TransformStream" {
            stream_keys(&holder["readable"], keys);
            stream_keys(&holder["writable"], keys);
        } else if let Some(port) = holder["port"].as_str() {
            keys.push(port.to_string());
        }
    }
    let mut pending = vec![packet.to_string()];
    // Removing an endpoint returns its queued packets only once. The native
    // registry bounds endpoints and queued bytes; do not abandon a long chain
    // partway through and leave its remaining holders permanently in flight.
    while let Some(text) = pending.pop() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        if let Some(canvases) = value["canvases"].as_array() {
            for key in canvases.iter().filter_map(serde_json::Value::as_str) {
                let _ = crate::canvas_offscreen::receive(key);
            }
        }
        let mut keys = Vec::new();
        if let Some(list) = value["ports"].as_array() {
            keys.extend(
                list.iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(str::to_string),
            );
        }
        if let Some(list) = value["streams"].as_array() {
            for holder in list {
                stream_keys(holder, &mut keys);
            }
        }
        if !keys.is_empty() {
            pending.extend(ports.lock().unwrap().discard_transfers(&keys));
        }
    }
}

fn client_for_context(state: &HostState, context: u64) -> Option<Arc<sw::Client>> {
    state
        .service_workers
        .endpoints
        .get(&context)
        .map(|endpoint| endpoint.client.clone())
        .or_else(|| state.service_workers.documents.get(&context).cloned())
}

/// Create an environment before author scripts, including for a static page.
/// The storage argument is the frontend's actual profile, never a new shed.
pub(crate) fn document(
    storage: &crate::js::WebStorage,
    blobs: &crate::js::BlobMap,
    handle: tokio::runtime::Handle,
    url: &url::Url,
    traversable: crate::js::TraversableState,
) -> Option<sw::DocumentClient> {
    let profile = profile_for(storage, blobs, handle, None);
    let origin = url.origin();
    let key = origin.ascii_serialization();
    let client = profile
        .registry
        .reserve_client_with_origin(
            key,
            url.clone(),
            origin,
            matches!(url.scheme(), "http" | "https"),
        )
        .ok()?;
    client.set_execution_ready();
    let document = sw::DocumentClient::new(client);
    document.presentation(traversable.hidden, traversable.focused);
    Some(document)
}

/// Native Window setup prepares an owned environment even if the child is
/// insecure and has no ServiceWorkerContainer. Its parent Document retains
/// it when the idle shared actor retires (HTML #creating-a-new-browsing-context).
pub(super) fn child_document(state: &mut HostState, context: u64, parent: u64, url: url::Url) {
    let Some(owner) = state.service_workers.document_owners.get(&parent).cloned() else {
        return;
    };
    let Some(profile) = profile(state) else {
        return;
    };
    let origin = realm_origin(state, context);
    let partition = state
        .window_cookie_contexts
        .get(&context)
        .map(|(_, _, cookies)| cookies.storage_partition())
        .unwrap_or("");
    let key = if !origin.is_tuple() || partition == "null" {
        "null".into()
    } else if partition.is_empty() {
        origin.ascii_serialization()
    } else {
        format!("{}^{partition}", origin.ascii_serialization())
    };
    // HTML e5071a20 #secure-context: Window settings use their top-level
    // creation URL. Local documents retain their settings origin separately;
    // absent a reserved navigation environment, their controller is null.
    let secure = owner.client().secure_context();
    let navigation = secure && matches!(url.scheme(), "http" | "https");
    let Ok(client) = profile
        .registry
        .reserve_client_with_origin(key, url, origin, navigation)
    else {
        return;
    };
    client.set_secure_context(secure);
    let mut window = owner.client().window_state();
    window.frame_type = "nested".into();
    window.focused = false;
    window.focus_within = false;
    window.ancestor_origins.clear(); // The Window bootstrap supplies its privacy-filtered list.
    client.configure(sw::ClientKind::Window, window);
    let child = sw::DocumentClient::new(client);
    owner.adopt_child(context, &child);
    state
        .service_workers
        .document(context, &child, state.blobs.clone(), state.blob_agent);
}

/// The coordinator retains the inbox while an agent runs. Its scope uses a
/// weak reference back so teardown also works on panic or failed bootstrap.
pub(super) struct Runtime {
    handle: TrustJsWorkerHandle,
    startup: Mutex<Option<Result<(), Error>>>,
    started: tokio::sync::Notify,
    alive: AtomicBool,
    finished: AtomicBool,
    ended: tokio::sync::Notify,
    next_event: AtomicU64,
    events: Mutex<HashMap<u64, tokio::sync::oneshot::Sender<bool>>>,
    last_event: Mutex<Instant>,
}

struct PendingEvent<'a> {
    runtime: &'a Runtime,
    id: u64,
}
impl Drop for PendingEvent<'_> {
    fn drop(&mut self) {
        // Cancellation can happen while waiting for activation/startup or
        // from an outer functional-event deadline. Never leave an orphaned
        // pending event preventing this agent from returning to idle.
        let unfinished = self
            .runtime
            .events
            .lock()
            .unwrap()
            .remove(&self.id)
            .is_some();
        if unfinished {
            self.runtime.terminate();
        }
    }
}

impl Runtime {
    fn startup(&self, result: Result<(), Error>) {
        let mut startup = self.startup.lock().unwrap();
        if startup.is_none() {
            *startup = Some(result);
            self.started.notify_waiters();
        }
    }
    async fn started(&self) -> Result<(), Error> {
        loop {
            let changed = self.started.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if let Some(result) = self.startup.lock().unwrap().clone() {
                return result;
            }
            changed.await;
        }
    }
    fn terminate(&self) {
        self.alive.store(false, Ordering::Release);
        self.handle.interrupt.cancel();
        self.handle.tasks.cancel();
        let _ = self.handle.ctl.sender.send(TrustJsWorkerCtl::Terminate);
    }
    fn exited(&self) {
        self.alive.store(false, Ordering::Release);
        self.startup(Err(Error::type_error(
            "Service-worker agent failed to start",
        )));
        self.events.lock().unwrap().clear();
        self.finished.store(true, Ordering::Release);
        self.ended.notify_waiters();
    }
    async fn wait_for_exit(&self) {
        loop {
            let ended = self.ended.notified();
            tokio::pin!(ended);
            ended.as_mut().enable();
            if self.finished.load(Ordering::Acquire) {
                return;
            }
            ended.await;
        }
    }
    pub(super) fn complete(&self, id: u64, success: bool) {
        if let Some(sender) = self.events.lock().unwrap().remove(&id) {
            let _ = sender.send(success);
        }
        *self.last_event.lock().unwrap() = Instant::now();
    }
    pub(super) fn idle_remaining(&self) -> Option<Duration> {
        let events = self.events.lock().unwrap();
        if !events.is_empty() {
            return None;
        }
        let remaining = IDLE_TIMEOUT.saturating_sub(self.last_event.lock().unwrap().elapsed());
        if remaining.is_zero() {
            self.alive.store(false, Ordering::Release);
        }
        Some(remaining)
    }
    async fn event(&self, kind: &'static str, init: serde_json::Value) -> Result<bool, ()> {
        let id = self.next_event.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = tokio::sync::oneshot::channel();
        {
            let mut events = self.events.lock().unwrap();
            // Competes with idle shutdown under one lock. A caller losing
            // that race restarts the saved version before dispatching.
            if !self.alive.load(Ordering::Acquire) {
                return Err(());
            }
            events.insert(id, sender);
        }
        let _pending = PendingEvent { runtime: self, id };
        *self.last_event.lock().unwrap() = Instant::now();
        let result = self
            .handle
            .ctl
            .sender
            .send(TrustJsWorkerCtl::ServiceEvent { id, kind, init });
        let outcome = if result.is_ok() {
            tokio::time::timeout(EVENT_TIMEOUT, receiver)
                .await
                .ok()
                .and_then(Result::ok)
        } else {
            None
        };
        self.events.lock().unwrap().remove(&id);
        if outcome.is_none() {
            self.terminate();
        }
        Ok(outcome.unwrap_or(false))
    }
}

pub(super) struct Scope {
    pub profile: Weak<Profile>,
    pub worker: Arc<Worker>,
    pub version: Arc<scripts::Version>,
    pub runtime: Weak<Runtime>,
}

impl Scope {
    pub(super) fn started(&self, result: Result<(), Error>) {
        if let Some(runtime) = self.runtime.upgrade() {
            runtime.startup(result);
        }
    }
    pub(super) fn prepare_task(&self) {
        if let Some(runtime) = self.runtime.upgrade() {
            runtime
                .handle
                .interrupt
                .set_deadline(Some(Instant::now() + EXECUTION_TIMEOUT));
        }
    }
    pub(super) fn idle_remaining(&self) -> Option<Duration> {
        self.runtime
            .upgrade()
            .and_then(|runtime| runtime.idle_remaining())
    }
}

impl Profile {
    async fn event(
        self: &Arc<Self>,
        worker: &Arc<Worker>,
        kind: &'static str,
        init: serde_json::Value,
    ) -> bool {
        loop {
            let Ok(runtime) = self.start(worker).await else {
                return false;
            };
            if let Ok(result) = runtime.event(kind, init.clone()).await {
                return result;
            }
        }
    }
    /// ServiceWorker/postMessage's Run Service Worker + dispatch. Until the
    /// packet is handed to the worker agent its transfers stay native; a
    /// failed startup releases them instead of leaking the endpoints.
    async fn message_event(
        self: &Arc<Self>,
        worker: &Arc<Worker>,
        init: serde_json::Value,
        packet: MessagePacket,
    ) {
        loop {
            let runtime = match self.start(worker).await {
                Ok(runtime) => runtime,
                Err(_) => return,
            };
            match runtime.event("message", init.clone()).await {
                Ok(_) => {
                    drop(packet);
                    return;
                }
                Err(()) => continue,
            }
        }
    }
    fn functional_event(
        self: &Arc<Self>,
        registration: &Arc<sw::Registration>,
        kind: &'static str,
        init: serde_json::Value,
        permit: tokio::sync::OwnedSemaphorePermit,
    ) {
        if !registration.registered() {
            return;
        }
        let Some(worker) = registration.active() else {
            return;
        };
        if !worker
            .event_types
            .get()
            .is_some_and(|types| types.iter().any(|t| t == kind))
        {
            return;
        }
        let Some(lease) = registration.begin_event(&worker) else {
            return;
        };
        let profile = self.clone();
        self.handle.spawn(async move {
            let _permit = permit;
            let _lease = lease;
            let work = async {
                if worker.await_activated().await {
                    profile.event(&worker, kind, init).await;
                }
            };
            let _ = tokio::time::timeout(EVENT_TIMEOUT, work).await;
        });
    }

    fn cookie_subscriptions(
        self: &Arc<Self>,
        registration: &Arc<sw::Registration>,
        cookies: &crate::http::CookieContext,
        base: &url::Url,
        op: &str,
        input: &serde_json::Value,
    ) -> Result<serde_json::Value, Error> {
        use crate::http::cookie_store::{Filter, subscribe_filtered};
        let mut state = registration.cookies.lock().unwrap();
        if op == "cookieSubscriptions" {
            return Ok(serde_json::Value::Array(
                state
                    .filters
                    .iter()
                    .map(|filter| {
                        // IDL name is optional and nonnullable. Official WPT's
                        // getSubscriptions_multiple checks omission for all names;
                        // this preserves subscribe(getSubscriptions()) round trips.
                        let mut item = serde_json::json!({"url":filter.url.as_str()});
                        if let Some(name) = &filter.name {
                            item["name"] = name.clone().into();
                        }
                        item
                    })
                    .collect(),
            ));
        }
        let mut result = Ok(serde_json::Value::Null);
        for entry in input["subscriptions"].as_array().into_iter().flatten() {
            let url = entry["url"]
                .as_str()
                .map_or_else(|| Ok(registration.scope.clone()), |url| base.join(url));
            let Ok(url) = url else {
                result = Err(Error::type_error("Invalid cookie subscription URL"));
                break;
            };
            if !url.as_str().starts_with(registration.scope.as_str()) {
                result = Err(Error::type_error(
                    "Cookie subscription URL is outside the registration scope",
                ));
                break;
            }
            let filter = Filter {
                name: entry["name"]
                    .as_str()
                    .map(|name| name.trim_matches([' ', '\t']).to_string()),
                url,
            };
            if op == "cookieSubscribe" {
                if !state.filters.contains(&filter) {
                    if state.filters.len() >= 256 {
                        result = Err(Error::new(
                            "QuotaExceededError",
                            "Too many cookie subscriptions",
                        ));
                        break;
                    }
                    state.filters.push(filter);
                }
            } else {
                state.filters.retain(|existing| existing != &filter);
            }
        }
        // Entries apply sequentially; a later invalid URL does not roll back
        // earlier valid changes (Cookie Store subscribe/unsubscribe).
        state.observer = None;
        if !state.filters.is_empty() && registration.registered() {
            let owner = Arc::downgrade(self);
            let weak = Arc::downgrade(registration);
            state.observer = Some(subscribe_filtered(
                registration.scope.clone(),
                cookies.clone(),
                Some(state.filters.clone()),
                Arc::new(move |changes| {
                    // Called under the jar lock: only schedule work here. Acquire
                    // a permit before spawning so repeated writes are bounded.
                    if let (Some(profile), Some(registration)) = (owner.upgrade(), weak.upgrade())
                        && let Ok(permit) = profile.functional.clone().try_acquire_owned()
                    {
                        let queued = profile.clone();
                        profile.handle.spawn(async move {
                            queued.functional_event(
                                &registration,
                                "cookiechange",
                                serde_json::to_value(changes).unwrap(),
                                permit,
                            );
                        });
                    }
                }),
            ));
        }
        result
    }

    fn actions(self: &Arc<Self>, actions: Vec<Action>) {
        for action in actions {
            match action {
                Action::Terminate(worker) => {
                    let runtime = self.runtimes.lock().unwrap().get(&worker.id).cloned();
                    if let Some(runtime) = runtime {
                        runtime.terminate();
                    }
                }
                Action::Activate(registration, worker) => {
                    let profile = self.clone();
                    self.handle.spawn(async move {
                        if let Some(lease) = registration.begin_event(&worker) {
                            if worker
                                .event_types
                                .get()
                                .is_some_and(|types| types.iter().any(|t| t == "activate"))
                            {
                                profile
                                    .event(&worker, "activate", serde_json::Value::Null)
                                    .await;
                            }
                            // Activation completes even if its event rejects or
                            // the worker is forcibly terminated (Activate §11).
                            profile.registry.activated(&registration, &worker);
                            drop(lease);
                        }
                    });
                }
            }
        }
    }

    async fn start(self: &Arc<Self>, worker: &Arc<Worker>) -> Result<Arc<Runtime>, Error> {
        let _start = worker.start_lock.lock().await;
        if worker.state() == WorkerState::Redundant {
            return Err(Error::type_error("A redundant service worker cannot run"));
        }
        let previous = self.runtimes.lock().unwrap().get(&worker.id).cloned();
        if let Some(runtime) = previous {
            if runtime.alive.load(Ordering::Acquire) {
                return Ok(runtime);
            }
            tokio::time::timeout(EXECUTION_TIMEOUT, runtime.wait_for_exit())
                .await
                .map_err(|_| Error::type_error("Previous service-worker agent has not stopped"))?;
        }
        let version = worker
            .version
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| Error::type_error("Missing service-worker script"))?;
        let storage = self
            .storage
            .upgrade()
            .ok_or_else(|| Error::type_error("The browser session has ended"))?;
        let (ctl, receiver) = TrustJsWorkerInbox::new();
        let tasks = Arc::new(crate::http::PageTaskScope::default());
        let interrupt = Arc::new(trust_js::RuntimeInterrupt::default());
        let runtime = Arc::new(Runtime {
            handle: TrustJsWorkerHandle {
                ctl: ctl.clone(),
                interrupt: interrupt.clone(),
                tasks: tasks.clone(),
                context: 0,
            },
            startup: Mutex::new(None),
            started: tokio::sync::Notify::new(),
            alive: AtomicBool::new(true),
            finished: AtomicBool::new(false),
            ended: tokio::sync::Notify::new(),
            next_event: AtomicU64::new(1),
            events: Mutex::default(),
            last_event: Mutex::new(Instant::now()),
        });
        let scope = Arc::new(Scope {
            profile: Arc::downgrade(self),
            worker: worker.clone(),
            version: version.clone(),
            runtime: Arc::downgrade(&runtime),
        });
        let events = TrustJsTaskSender::Worker(Weak::new());
        let launch = TrustJsWorkerLaunch {
            id: worker.id as usize,
            owner_page: worker.script.clone(),
            cookie_context: version.environment.cookies.clone(),
            script_url: worker.script.clone(),
            kind: if worker.kind == WorkerType::Classic {
                TrustJsWorkerKind::Classic
            } else {
                TrustJsWorkerKind::Module
            },
            name: String::new(),
            script_blob: None,
            blobs: self.blobs.clone(),
            referrer_source: Some(version.environment.client.clone()),
            referrer_policy: version.root.referrer_policy().unwrap_or_default(),
            secure_context: true,
            shared: false,
            service: Some(scope),
            // Service workers have a distinct agent cluster. SAB must not be
            // serialized across this boundary (HTML structured serialization).
            agent_cluster: next_agent_cluster_id(),
            port_registry: self.ports.clone(),
            port_wake: message_port_host::Wake::Worker(Arc::downgrade(&ctl)),
            task_sender: TrustJsTaskSender::Worker(Arc::downgrade(&ctl)),
            storage,
            permission_store: self.permissions.clone(),
            #[cfg(test)]
            cross_origin_isolated: false,
        };
        self.admit_runtime(worker.id, runtime.clone())?;
        let weak = Arc::downgrade(&runtime);
        let handle = self.handle.clone();
        if let Err(error) = spawn_trust_js_worker_thread(worker.id as usize, move || {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run_trust_js_worker(launch, handle, tasks, events, receiver, interrupt);
            }));
            if let Some(runtime) = weak.upgrade() {
                runtime.exited();
            }
        }) {
            runtime.exited();
            return Err(Error::type_error(format!(
                "Cannot start service-worker thread: {error}"
            )));
        }
        match tokio::time::timeout(EXECUTION_TIMEOUT, runtime.started()).await {
            Ok(Ok(())) => Ok(runtime),
            result => {
                runtime.terminate();
                Err(result
                    .err()
                    .map(|_| Error::type_error("Service-worker startup timed out"))
                    .or_else(|| {
                        runtime
                            .startup
                            .lock()
                            .unwrap()
                            .clone()
                            .and_then(Result::err)
                    })
                    .unwrap_or_else(|| Error::type_error("Service-worker startup failed")))
            }
        }
    }

    fn admit_runtime(&self, worker: u64, runtime: Arc<Runtime>) -> Result<(), Error> {
        let mut runtimes = self.runtimes.lock().unwrap();
        // An idle/terminated agent is not finished until its thread exits.
        // Keep that handshake visible to another start of the same worker,
        // and count closing agents against the resource limit as well.
        runtimes.retain(|_, runtime| !runtime.finished.load(Ordering::Acquire));
        if runtimes.len() >= MAX_AGENTS {
            return Err(Error::new(
                "QuotaExceededError",
                "Too many running service workers",
            ));
        }
        runtimes.insert(worker, runtime);
        Ok(())
    }

    async fn update(
        self: &Arc<Self>,
        key: String,
        options: RegistrationOptions,
        environment: scripts::Environment,
        kind: jobs::Kind,
        completion: Arc<jobs::Completion>,
    ) -> Result<(), Error> {
        let registration = if kind == jobs::Kind::Update {
            self.registry
                .get(&key, &options.scope)
                .ok_or_else(|| Error::type_error("The service-worker registration was removed"))?
        } else {
            self.registry.set_registration_bounded(
                &key,
                &options,
                MAX_ORIGIN_REGISTRATIONS,
                MAX_PROFILE_REGISTRATIONS,
            )?
        };
        let newest = registration.newest();
        if let Some(newest) = &newest {
            if kind == jobs::Kind::Update && newest.script != options.script {
                return Err(Error::type_error(
                    "The service-worker script URL has changed",
                ));
            }
            if kind == jobs::Kind::Register
                && newest.script == options.script
                && newest.kind == options.kind
                && registration.info().update_via_cache == options.update_via_cache
            {
                completion.resolve(Ok(jobs::Output::registrations(
                    vec![registration.clone()],
                    false,
                )));
                return Ok(());
            }
        }
        let prepared = self
            .prepare_version(&options, &environment, newest.as_ref())
            .await;
        let (version, changed) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                self.registry.failed_update(&registration, None);
                // Update rejects a null fetched script with TypeError. The
                // fetch hook's explicit MIME/scope SecurityError takes priority.
                return Err(if error.name == "NetworkError" {
                    Error::type_error(error.message)
                } else {
                    error
                });
            }
        };
        if !changed {
            registration.set_update_via_cache(options.update_via_cache);
            completion.resolve(Ok(jobs::Output::registrations(
                vec![registration.clone()],
                false,
            )));
            return Ok(());
        }
        let worker = self.registry.new_worker(&registration, &options);
        *worker.version.lock().unwrap() = Some(version.clone());
        match self.start(&worker).await {
            Ok(_) => (),
            Err(error) => {
                self.registry.failed_update(&registration, Some(&worker));
                return Err(error);
            }
        };
        self.registry
            .installing(&registration, &worker, options.update_via_cache);
        completion.resolve(Ok(jobs::Output::registrations(
            vec![registration.clone()],
            false,
        )));
        self.registry.update_found(&registration);
        let lease = registration.begin_event(&worker);
        let succeeded = !worker
            .event_types
            .get()
            .is_some_and(|types| types.iter().any(|t| t == "install"))
            || self
                .event(&worker, "install", serde_json::Value::Null)
                .await;
        if succeeded {
            version.resources.finish_install();
        }
        worker
            .installation_notifications_pending
            .store(succeeded, Ordering::Release);
        self.registry.installed(&registration, &worker, succeeded);
        // Release only after the slot transition: dropping an event lease
        // retries activation and must not see a half-finished installation.
        drop(lease);
        if succeeded {
            let profile = self.clone();
            // Finish Job precedes the acknowledgement wait; another job in
            // this scope must be free to run while a client is suspended.
            self.handle.spawn(async move {
                profile.registry.state_updates_observed(&key).await;
                worker
                    .installation_notifications_pending
                    .store(false, Ordering::Release);
                profile.registry.try_activate(&registration);
            });
        }
        Ok(())
    }

    async fn prepare_version(
        &self,
        options: &RegistrationOptions,
        environment: &scripts::Environment,
        newest: Option<&Arc<Worker>>,
    ) -> Result<(Arc<scripts::Version>, bool), Error> {
        let root = scripts::fetch(
            options.script.clone(),
            environment,
            Some(options),
            &self.budget,
        )
        .await?;
        let resources = scripts::Resources::new(options.script.clone(), root.clone());
        let old = newest.and_then(|worker| worker.version.lock().unwrap().clone());
        let mut changed = newest
            .is_none_or(|worker| worker.script != options.script || worker.kind != options.kind)
            || old
                .as_ref()
                .is_none_or(|version| version.root.bytes != root.bytes);
        if !changed
            && let Some(old) = old
            && old.resources.classic_imported()
        {
            for (url, saved) in old.resources.snapshot() {
                if url == options.script {
                    continue;
                }
                let fetched = scripts::fetch(url.clone(), environment, None, &self.budget).await;
                if let (Ok(saved), Ok(fetched)) = (&saved, &fetched) {
                    changed |= saved.bytes != fetched.bytes;
                }
                resources.seed(url, fetched)?;
            }
        }
        Ok((
            Arc::new(scripts::Version {
                root,
                resources,
                environment: environment.clone(),
            }),
            changed,
        ))
    }
}

struct Endpoint {
    profile: Arc<Profile>,
    client: Arc<sw::Client>,
    cookies: crate::http::CookieContext,
    completions: HashMap<u64, Arc<jobs::Completion>>,
    registrations: HashMap<u64, Arc<sw::Registration>>,
    workers: HashMap<u64, Arc<Worker>>,
    owns_client: bool,
    attachment: u64,
    message: Option<MessagePacket>,
}
impl Drop for Endpoint {
    fn drop(&mut self) {
        if self.owns_client {
            self.client.discard();
        } else {
            self.client.detach(self.attachment);
        }
    }
}
impl Endpoint {
    fn remember_worker(&mut self, info: &sw::WorkerInfo) {
        self.workers.insert(info.id, info.handle.clone());
    }
    fn remember(&mut self, info: &sw::RegistrationInfo) {
        self.registrations.insert(info.id, info.handle.clone());
        for worker in [&info.installing, &info.waiting, &info.active]
            .into_iter()
            .flatten()
        {
            self.remember_worker(worker);
        }
    }
}

#[derive(Default)]
pub(super) struct Agent {
    endpoints: HashMap<u64, Endpoint>,
    documents: HashMap<u64, Arc<sw::Client>>,
    // A running Document may still execute unloading cleanup after its
    // frontend has moved on. Idle actor drop releases these handles; the
    // frontend then owns static Documents and their descendants alone.
    document_owners: HashMap<u64, sw::DocumentClient>,
    document_blobs: HashMap<u64, Arc<sw::DocumentBlobs>>,
    next_request: u64,
}

impl Agent {
    pub(super) fn remove(&mut self, context: u64) {
        // Explicit Window destruction differs from idle Agent drop. The
        // frontend may still hold the old presentation until its replacement
        // paints, but the old environment is already discarded.
        if let Some(owner) = self.document_owners.remove(&context) {
            owner.discard();
        }
        if let Some(endpoint) = self.endpoints.remove(&context) {
            endpoint.client.discard();
        }
        if let Some(client) = self.documents.remove(&context) {
            client.discard();
        }
        if let Some(blobs) = self.document_blobs.remove(&context) {
            blobs.remove();
        }
    }
    pub(super) fn document(
        &mut self,
        context: u64,
        document: &sw::DocumentClient,
        blobs: crate::js::BlobMap,
        agent: u64,
    ) {
        self.documents.insert(context, document.client());
        self.document_owners.insert(context, document.clone());
        self.document_blobs
            .insert(context, document.bind_blobs(blobs, agent, context));
    }
    pub(super) fn execution_ready(&self, context: u64) {
        if let Some(client) = self.documents.get(&context) {
            client.set_execution_ready();
        } else if let Some(endpoint) = self.endpoints.get(&context) {
            endpoint.client.set_execution_ready();
        }
    }
}

fn packet(result: Result<serde_json::Value, Error>) -> serde_json::Value {
    match result {
        Ok(value) => serde_json::json!({"value": value}),
        Err(error) => serde_json::json!({"error": error}),
    }
}

pub(super) fn operation(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let op = host_arg_string(ctx, args, 0);
    let context = args.get(1).and_then(Value::as_num_opt).unwrap_or(0.) as u64;
    let input = host_arg_string(ctx, args, 2);
    let input: serde_json::Value = serde_json::from_str(&input).unwrap_or_default();
    let result = operation_inner(ctx, &op, context, input);
    Ok(Value::from_string(packet(result).to_string()))
}

fn operation_inner(
    ctx: &mut Ctx,
    op: &str,
    context: u64,
    input: serde_json::Value,
) -> Result<serde_json::Value, Error> {
    use serde_json::{Value as Json, json};
    // Serialization already transferred these holders. Even a stale receiver
    // or an early native error must release them when no queue takes ownership.
    let mut message = if matches!(op, "postWorker" | "postClient") {
        ctx.host::<HostState>().map(|state| {
            MessagePacket::new(
                state.message_ports.registry.clone(),
                input["packet"].as_str().unwrap_or("").to_string(),
            )
        })
    } else {
        None
    };
    if op == "complete" {
        if let Some(scope) = ctx
            .host::<HostState>()
            .and_then(|s| s.service_scope.as_ref())
            && let Some(runtime) = scope.runtime.upgrade()
        {
            runtime.complete(
                input["id"].as_u64().unwrap_or(0),
                input["ok"].as_bool().unwrap_or(false),
            );
        }
        return Ok(Json::Null);
    }
    if op == "import" {
        return import(ctx, input["url"].as_str().unwrap_or(""));
    }
    if op == "init" {
        let cookies = cookie_context_for_id(ctx, context);
        let state = ctx
            .host_mut::<HostState>()
            .ok_or_else(|| Error::type_error("Missing service-worker host"))?;
        if !state.service_workers.endpoints.contains_key(&context) {
            let profile = state
                .service_scope
                .as_ref()
                .and_then(|scope| scope.profile.upgrade())
                .or_else(|| profile(state));
            let Some(profile) = profile else {
                return Ok(json!({"controller":null,"ready":null,"messagesEnabled":false}));
            };
            let url = url::Url::parse(input["url"].as_str().unwrap_or(""))
                .map_err(|_| Error::security("Invalid service-worker client URL"))?;
            let key = input["key"].as_str().unwrap_or("").to_string();
            let origin = if let Some(worker) = &state.worker_self {
                worker.settings_origin.clone()
            } else {
                realm_origin(state, context)
            };
            let document = state.service_workers.documents.remove(&context);
            let owns_client = document.is_none();
            let client = match document {
                Some(client) => client,
                None => profile
                    .registry
                    .reserve_client_with_origin(key, url, origin, false)?,
            };
            client.set_secure_context(input["secure"].as_bool().unwrap_or(false));
            let kind = serde_json::from_value(input["kind"].clone()).unwrap_or_default();
            let window = serde_json::from_value(input["window"].clone()).unwrap_or_else(|_| {
                sw::WindowState {
                    frame_type: "none".into(),
                    ..Default::default()
                }
            });
            client.configure(kind, window);
            let mut attachment = 0;
            if let Some(events) = state.task_events.clone() {
                // HTML's execution-ready flag is initially unset. The
                // Window/dedicated/shared setup algorithms set it; SW
                // Setup/Run ServiceWorkerGlobalScope do not. A SW observes
                // registrations but is not a claimable/enumerable client.
                attachment = client.attach_with_readiness(
                    Arc::new(move |notice| {
                        let _ = events.send(TrustJsHostTask::ServiceNotice { context, notice });
                    }),
                    state.service_scope.is_none(),
                );
            }
            // Transferred ports must use the same registry across tabs and
            // service-worker restarts. This runs before author-created ports.
            state.message_ports.registry = profile.ports.clone();
            state.service_workers.endpoints.insert(
                context,
                Endpoint {
                    profile,
                    client,
                    cookies,
                    completions: HashMap::new(),
                    registrations: HashMap::new(),
                    workers: HashMap::new(),
                    owns_client,
                    attachment,
                    message: None,
                },
            );
        }
        let endpoint = state.service_workers.endpoints.get_mut(&context).unwrap();
        let ready = endpoint
            .profile
            .registry
            .matching(&endpoint.client.key, &endpoint.client.url)
            .filter(|r| r.active().is_some())
            .map(|r| r.info());
        let controller = endpoint.client.controller().map(|w| w.info());
        if let Some(ready) = &ready {
            endpoint.remember(ready);
        }
        if let Some(controller) = &controller {
            endpoint.remember_worker(controller);
        }
        let messages_enabled = endpoint.client.messages_enabled();
        endpoint.client.wake_messages();
        return Ok(
            json!({"controller":controller, "ready":ready, "messagesEnabled":messages_enabled}),
        );
    }
    if op == "settled" {
        if let Some(endpoint) = ctx
            .host_mut::<HostState>()
            .and_then(|s| s.service_workers.endpoints.get_mut(&context))
            && let Some(completion) = endpoint
                .completions
                .remove(&input["id"].as_u64().unwrap_or(0))
        {
            completion.settled();
        }
        return Ok(Json::Null);
    }
    let state = ctx
        .host_mut::<HostState>()
        .ok_or_else(|| Error::type_error("Missing service-worker host"))?;
    if op == "clientState" {
        // Presentation updates are best-effort. Offline DOM-only hosts have
        // no network coordinator, so init installed no endpoint; visibility
        // and focus tasks must not fail for want of one.
        if let Some(endpoint) = state.service_workers.endpoints.get(&context)
            && let Ok(window) = serde_json::from_value(input)
        {
            endpoint.client.configure(sw::ClientKind::Window, window);
        }
        return Ok(Json::Null);
    }
    if op == "enableMessages" {
        if let Some(endpoint) = state.service_workers.endpoints.get(&context) {
            endpoint.client.enable_messages();
        }
        return Ok(Json::Null);
    }
    if op == "finishMessage" {
        if let Some(endpoint) = state.service_workers.endpoints.get_mut(&context) {
            endpoint.message.take();
        }
        return Ok(Json::Null);
    }
    if op == "takeMessage" {
        let Some(endpoint) = state.service_workers.endpoints.get_mut(&context) else {
            return Ok(Json::Null);
        };
        endpoint.message.take();
        let Some(pending) = endpoint.client.take_message() else {
            return Ok(Json::Null);
        };
        endpoint.remember_worker(&pending.source);
        let packet = MessagePacket::new(endpoint.profile.ports.clone(), pending.packet);
        let result =
            json!({"packet":packet.text, "origin":pending.origin, "source":pending.source});
        endpoint.message = Some(packet);
        return Ok(result);
    }
    let endpoint = state
        .service_workers
        .endpoints
        .get(&context)
        .ok_or_else(|| Error::new("InvalidStateError", "Service-worker host is unavailable"))?;
    let profile = endpoint.profile.clone();
    let client = endpoint.client.clone();
    if !client.origin.is_tuple() {
        return Err(Error::security(
            "Service-worker storage requires a non-opaque origin",
        ));
    }
    let cookies = endpoint.cookies.clone();
    let events = state
        .task_events
        .clone()
        .ok_or_else(|| Error::type_error("Service-worker task queue is unavailable"))?;
    state.service_workers.next_request += 1;
    let id = state.service_workers.next_request;
    let reply: jobs::Reply = Box::new(move |result| {
        let _ = events.send(TrustJsHostTask::ServiceResult {
            context,
            id,
            result,
        });
    });
    let base = url::Url::parse(input["base"].as_str().unwrap_or(client.url.as_str()))
        .map_err(|_| Error::type_error("Invalid service-worker API base URL"))?;
    match op {
        "getClient" | "matchClients" => {
            let permit = profile
                .functional
                .clone()
                .try_acquire_owned()
                .map_err(|_| Error::new("QuotaExceededError", "Too many pending client queries"))?;
            let scope = state
                .service_scope
                .as_ref()
                .ok_or_else(|| Error::type_error("Not a service worker"))?;
            let worker = scope.worker.clone();
            let registration = worker
                .registration
                .upgrade()
                .ok_or_else(|| Error::new("InvalidStateError", "Missing registration"))?;
            let single = op == "getClient";
            let candidates = if single {
                profile
                    .registry
                    .get_client(&registration.key, input["id"].as_str().unwrap_or(""))
                    .into_iter()
                    .collect()
            } else {
                let kind = if input["type"] == "all" {
                    None
                } else {
                    Some(
                        serde_json::from_value(input["type"].clone())
                            .map_err(|_| Error::type_error("Invalid client type"))?,
                    )
                };
                profile.registry.matching_clients(
                    &registration.key,
                    worker.id,
                    input["includeUncontrolled"].as_bool().unwrap_or(false),
                    kind,
                )
            };
            profile.handle.spawn(async move {
                let _permit = permit;
                let mut found = Vec::new();
                for client in candidates {
                    if single && !client.await_ready().await {
                        continue;
                    }
                    match client.inspect().await {
                        Ok(Some(info)) => found.push(info),
                        Ok(None) => {}
                        Err(error) if single => {
                            reply(Err(error));
                            return;
                        }
                        Err(_) => {}
                    }
                }
                sw::ClientInfo::sort(&mut found);
                let value = if single {
                    json!(found.into_iter().next())
                } else {
                    json!(found)
                };
                reply(Ok(value.into()));
            });
        }
        "getRegistrations" => reply(Ok(jobs::Output::registrations(
            profile.registry.registrations(&client.key),
            true,
        ))),
        "getRegistration" => {
            let mut url = base
                .join(input["url"].as_str().unwrap_or(""))
                .map_err(|_| Error::type_error("Invalid client URL"))?;
            url.set_fragment(None);
            if url.origin() != client.origin {
                return Err(Error::security("Client URL is not same origin"));
            }
            reply(Ok(jobs::Output::registrations(
                profile
                    .registry
                    .matching(&client.key, &url)
                    .into_iter()
                    .collect(),
                false,
            )));
        }
        "cookieSubscribe" | "cookieUnsubscribe" | "cookieSubscriptions" => {
            let registration = state
                .service_workers
                .endpoints
                .get(&context)
                .and_then(|endpoint| {
                    endpoint
                        .registrations
                        .get(&input["registration"].as_u64().unwrap_or(0))
                })
                .cloned()
                .ok_or_else(|| {
                    Error::new("InvalidStateError", "Unknown service-worker registration")
                })?;
            reply(
                profile
                    .cookie_subscriptions(&registration, &cookies, &base, op, &input)
                    .map(Into::into),
            );
        }
        "register" | "update" | "unregister" => {
            let kind = match op {
                "update" => jobs::Kind::Update,
                "unregister" => jobs::Kind::Unregister,
                _ => jobs::Kind::Register,
            };
            if kind == jobs::Kind::Update
                && state
                    .service_scope
                    .as_ref()
                    .is_some_and(|s| s.worker.state() == WorkerState::Installing)
            {
                return Err(Error::new(
                    "InvalidStateError",
                    "An installing service worker cannot update a registration",
                ));
            }
            let options: RegistrationOptions = if kind == jobs::Kind::Register {
                RegistrationOptions::parse(
                    &base,
                    &client.origin,
                    input["script"].as_str().unwrap_or(""),
                    input["scope"].as_str(),
                    serde_json::from_value(input["kind"].clone()).unwrap_or(WorkerType::Classic),
                    serde_json::from_value(input["cache"].clone())
                        .unwrap_or(sw::UpdateViaCache::Imports),
                )?
            } else {
                let scope = url::Url::parse(input["scope"].as_str().unwrap_or(""))
                    .map_err(|_| Error::type_error("Invalid registration scope"))?;
                if scope.origin() != client.origin {
                    return Err(Error::security("Registration is not same origin"));
                }
                let registration = profile
                    .registry
                    .get(&client.key, &scope)
                    .filter(|r| Some(r.id) == input["registration"].as_u64());
                if kind == jobs::Kind::Unregister && registration.is_none() {
                    reply(Ok(json!(false).into()));
                    return Ok(json!(id));
                }
                let registration = registration.ok_or_else(|| {
                    Error::new("InvalidStateError", "No service worker is registered")
                })?;
                let worker = registration
                    .newest()
                    .ok_or_else(|| Error::new("InvalidStateError", "Registration has no worker"))?;
                RegistrationOptions {
                    script: worker.script.clone(),
                    scope,
                    kind: worker.kind,
                    update_via_cache: registration.info().update_via_cache,
                }
            };
            let environment = scripts::Environment {
                client: environment_referrer_source(state, context)
                    .unwrap_or_else(|| client.url.clone()),
                origin: url::Url::parse(&client.origin.ascii_serialization())
                    .map_err(|_| Error::security("Invalid client origin"))?,
                cookies,
                referrer_policy: environment_referrer_policy(state, context),
            };
            let spec = jobs::Spec {
                kind,
                key: client.key.clone(),
                scope: options.scope.to_string(),
                script: options.script.to_string(),
                worker_type: options.kind,
                update_via_cache: options.update_via_cache,
            };
            let owner = profile.clone();
            let key = client.key.clone();
            let (completion, primary) = profile.queues.schedule(
                &profile.handle,
                spec,
                reply,
                Box::new(move |completion| {
                    async move {
                        if kind == jobs::Kind::Unregister {
                            if let Some(registration) = owner.registry.get(&key, &options.scope) {
                                owner
                                    .registry
                                    .unregister_resolving(&registration, |removed| {
                                        completion.resolve(Ok(json!(removed).into()))
                                    });
                            } else {
                                completion.resolve(Ok(json!(false).into()));
                            }
                            Ok(())
                        } else {
                            owner
                                .update(key, options, environment, kind, completion)
                                .await
                        }
                    }
                    .boxed()
                }),
            );
            if primary {
                state
                    .service_workers
                    .endpoints
                    .get_mut(&context)
                    .unwrap()
                    .completions
                    .insert(id, completion);
            }
        }
        "postWorker" => {
            // SW ServiceWorker/postMessage: serialize first, use the incumbent
            // sender, then Should Skip Event and Run Service Worker in parallel.
            let packet = message.take().expect("message has a host");
            let destination = state
                .service_workers
                .endpoints
                .get(&context)
                .and_then(|endpoint| endpoint.workers.get(&input["worker"].as_u64().unwrap_or(0)))
                .cloned();
            if packet.text.len() <= sw::MAX_QUEUED_MESSAGE_BYTES
                && let Some(worker) = destination
            {
                let (source, origin) = if let Some(scope) = state.service_scope.clone() {
                    (
                        json!({"worker": scope.worker.info()}),
                        scope.worker.script.origin().ascii_serialization(),
                    )
                } else {
                    let sender = input["incumbent"]
                        .as_u64()
                        .and_then(|incumbent| client_for_context(state, incumbent))
                        .unwrap_or_else(|| client.clone());
                    let origin = sender.origin.ascii_serialization();
                    (json!({"client": sender.source_info()}), origin)
                };
                let skip = worker
                    .event_types
                    .get()
                    .is_some_and(|types| !types.iter().any(|t| t == "message"));
                if !skip
                    && let Some(registration) = worker.registration.upgrade()
                    && let Some(lease) = registration.begin_event(&worker)
                    && let Ok(permit) = profile.functional.clone().try_acquire_owned()
                {
                    let owner = profile.clone();
                    let init = json!({"packet": packet.text, "origin": origin, "source": source});
                    // Construct ownership before spawning: dropping an unpolled
                    // task also discards its serialized transfers.
                    profile.handle.spawn(async move {
                        let _permit = permit;
                        let _lease = lease;
                        owner.message_event(&worker, init, packet).await;
                    });
                }
            }
        }
        "postClient" => {
            // Client/postMessage uses the receiver's relevant settings. Its
            // native client queue takes ownership only when enqueue succeeds.
            let packet = message.take().expect("message has a host");
            if packet.text.len() <= sw::MAX_QUEUED_MESSAGE_BYTES
                && let Some(scope) = state.service_scope.clone()
                && let Some(registration) = scope.worker.registration.upgrade()
                && let Some(target) = profile
                    .registry
                    .get_client(&registration.key, input["client"].as_str().unwrap_or(""))
            {
                target.enqueue_message(sw::PendingMessage {
                    packet: packet.into_text(),
                    origin: scope.worker.script.origin().ascii_serialization(),
                    source: scope.worker.info(),
                });
            }
        }
        "skipWaiting" | "claim" => {
            let scope = state
                .service_scope
                .as_ref()
                .ok_or_else(|| Error::new("InvalidStateError", "Not a service worker"))?;
            let worker = scope.worker.clone();
            let registration = worker
                .registration
                .upgrade()
                .ok_or_else(|| Error::new("InvalidStateError", "Registration has been removed"))?;
            let skip = op == "skipWaiting";
            let owner = profile.clone();
            profile.handle.spawn(async move {
                let result = if skip {
                    worker.skip_waiting.store(true, Ordering::Release);
                    owner.registry.try_activate(&registration);
                    Ok(())
                } else {
                    owner.registry.claim(&registration, &worker)
                };
                reply(result.map(|()| Json::Null.into()));
            });
        }
        "scope" => {
            let scope = state
                .service_scope
                .as_ref()
                .ok_or_else(|| Error::type_error("Not a service worker"))?;
            let worker = scope.worker.info();
            let registration = scope.worker.registration.upgrade().map(|r| r.info());
            let endpoint = state.service_workers.endpoints.get_mut(&context).unwrap();
            endpoint.remember_worker(&worker);
            if let Some(registration) = &registration {
                endpoint.remember(registration);
            }
            return Ok(json!({"worker":worker, "registration":registration}));
        }
        _ => return Err(Error::type_error("Unknown service-worker operation")),
    }
    Ok(json!(id))
}

fn import(ctx: &mut Ctx, target: &str) -> Result<serde_json::Value, Error> {
    let scope = ctx
        .host::<HostState>()
        .and_then(|state| state.service_scope.clone())
        .ok_or_else(|| Error::type_error("Not a service worker"))?;
    let url =
        url::Url::parse(target).map_err(|_| Error::new("SyntaxError", "Invalid import URL"))?;
    let script = if let Some(saved) = scope.version.resources.import(&url, scope.worker.state()) {
        saved?
    } else {
        let profile = scope
            .profile
            .upgrade()
            .ok_or_else(|| Error::type_error("Browser session ended"))?;
        let script = if url.scheme() == "data" || url.scheme() == "blob" {
            let (body, mime) = if url.scheme() == "data" {
                (
                    crate::img::decode_data_url(url.as_str())
                        .ok_or_else(|| Error::new("NetworkError", "Invalid data URL"))?,
                    data_url_content_type(url.as_str()),
                )
            } else {
                let (_, _, key) = blob_environment(ctx);
                let entry = profile
                    .blobs
                    .obtain(url.as_str(), Some(&key))
                    .ok_or_else(|| Error::new("NetworkError", "Blob URL is unavailable"))?;
                (
                    entry
                        .read()
                        .map_err(|_| Error::new("NetworkError", "Blob could not be read"))?,
                    entry.content_type.clone(),
                )
            };
            if !crate::http::is_javascript_mime_type(&mime) {
                return Err(Error::new(
                    "NetworkError",
                    "Import must have a JavaScript MIME type",
                ));
            }
            scripts::Script::new(
                url.clone(),
                body,
                vec![("Content-Type".into(), mime)],
                &profile.budget,
            )?
        } else {
            let environment = scripts::Environment {
                client: scope.worker.script.clone(),
                origin: scope.worker.script.clone(),
                cookies: scope.version.environment.cookies.clone(),
                referrer_policy: scope.version.root.referrer_policy().unwrap_or_default(),
            };
            let target = url.clone();
            let budget = profile.budget.clone();
            let (sender, receiver) = std::sync::mpsc::channel();
            let runtime = scope
                .runtime
                .upgrade()
                .ok_or_else(|| Error::new("NetworkError", "Service worker has stopped"))?;
            runtime.handle.tasks.spawn(&profile.handle, async move {
                let _ = sender.send(scripts::fetch(target, &environment, None, &budget).await);
            });
            receiver
                .recv_timeout(EXECUTION_TIMEOUT)
                .map_err(|_| Error::new("NetworkError", "Script import timed out"))??
        };
        scope.version.resources.imported(url, script.clone())?;
        script
    };
    Ok(
        serde_json::json!({"source":&*script.source,"muted":script.muted_errors,"url":script.url.as_str()}),
    )
}

pub(super) fn dispatch_notice(
    engine: &mut trust_js::Engine,
    context: u64,
    notice: Notice,
) -> Result<(), String> {
    if let Notice::Inspect { inspection } = notice {
        let mut window = None;
        let result = run_in_context_realm(
            engine,
            context,
            "service-worker client inspection",
            |engine| {
                if let Ok(value) =
                    host_call_trust_interruptible(engine.ctx(), "serviceClientInfo", &[])
                    && let Some(text) = value.as_text()
                {
                    window = serde_json::from_str(&text).ok();
                }
                Ok(())
            },
        );
        inspection.complete(window);
        return result;
    }
    if let Notice::Checkpoint { completion } = notice {
        completion.complete();
        return Ok(());
    }
    let Some(endpoint) = engine
        .ctx()
        .host_mut::<HostState>()
        .and_then(|s| s.service_workers.endpoints.get_mut(&context))
    else {
        return Ok(());
    };
    match &notice {
        Notice::Registration { registration, .. }
            if endpoint.registrations.contains_key(&registration.id) =>
        {
            endpoint.remember(registration)
        }
        Notice::Ready { registration } => endpoint.remember(registration),
        Notice::Controller {
            worker: Some(worker),
        } => endpoint.remember_worker(worker),
        _ => {}
    }
    deliver(
        engine,
        context,
        "serviceNotice",
        &[Value::from_string(serde_json::to_string(&notice).unwrap())],
    )
}

pub(super) fn dispatch_result(
    engine: &mut trust_js::Engine,
    context: u64,
    id: u64,
    result: Result<jobs::Output, Error>,
) -> Result<(), String> {
    let Some(endpoint) = engine
        .ctx()
        .host_mut::<HostState>()
        .and_then(|s| s.service_workers.endpoints.get_mut(&context))
    else {
        return Ok(());
    };
    if let Ok(output) = &result {
        for registration in &output.registrations {
            endpoint.remember(registration);
        }
    }
    let result = packet(result.map(|output| output.value));
    deliver(
        engine,
        context,
        "serviceResult",
        &[
            Value::Num(id as f64),
            Value::from_string(result.to_string()),
        ],
    )
}

pub(super) fn deliver(
    engine: &mut trust_js::Engine,
    context: u64,
    method: &str,
    args: &[Value],
) -> Result<(), String> {
    run_in_context_realm(engine, context, "service worker", |engine| {
        match host_call_trust_interruptible(engine.ctx(), method, args) {
            Ok(_) => Ok(()),
            Err(error) => host_task_failure(engine.ctx(), error, "service worker"),
        }
    })
}

#[cfg(test)]
pub(super) fn stop_idle_agents_for_test(ctx: &mut Ctx) -> usize {
    let Some(state) = ctx.host::<HostState>() else {
        return 0;
    };
    let mut stopped = 0;
    for endpoint in state.service_workers.endpoints.values() {
        let runtimes = endpoint.profile.runtimes.lock().unwrap();
        for runtime in runtimes.values() {
            if runtime.alive.load(Ordering::Acquire) && runtime.events.lock().unwrap().is_empty() {
                runtime.terminate();
                stopped += 1;
            }
        }
    }
    stopped
}

#[cfg(test)]
mod tests {
    use super::*;
    fn port(
        engine: &mut trust_js::Engine,
        op: &str,
        id: Value,
        payload: &str,
    ) -> serde_json::Value {
        let value = message_port_host::call(
            engine.ctx(),
            Value::Undefined,
            &[
                Value::from_string(op.to_string()),
                id,
                Value::from_string(payload.to_string()),
            ],
        )
        .unwrap_or_else(|_| panic!("MessagePort {op} failed"));
        serde_json::from_str(&value.as_text().unwrap()).unwrap()
    }

    fn packet_engine() -> trust_js::Engine {
        let mut engine = trust_js::Engine::new();
        engine
            .ctx()
            .op_state()
            .put_retained_memory_with_external_memory(HostState::new(
                Rc::new(RefCell::new(Dom::new())),
                Rc::new(RealmClock::new()),
            ));
        engine
    }

    fn transferred_port(engine: &mut trust_js::Engine) -> (u64, String) {
        let pair = port(engine, "create", Value::Num(0.), "");
        let peer = pair[0].as_u64().unwrap();
        let token = port(
            engine,
            "transfer",
            Value::Num(pair[1].as_u64().unwrap() as f64),
            "",
        );
        (peer, token.as_str().unwrap().to_string())
    }

    #[test]
    fn service_worker_message_packets_release_on_error_and_unpolled_task_drop() {
        let mut engine = packet_engine();
        let ports = engine
            .ctx()
            .host::<HostState>()
            .unwrap()
            .message_ports
            .registry
            .clone();
        let (peer, key) = transferred_port(&mut engine);
        let text = serde_json::json!({"ports":[key]}).to_string();
        // A removed receiver fails before the postWorker match arm. Its
        // already-detached transfer must still be discarded.
        assert!(
            operation_inner(
                engine.ctx(),
                "postWorker",
                999,
                serde_json::json!({"worker":1,"packet":text})
            )
            .is_err()
        );
        assert_eq!(
            port(&mut engine, "peer", Value::Num(peer as f64), ""),
            serde_json::Value::Null
        );
        assert_eq!(
            port(&mut engine, "receive", Value::from_string(key), ""),
            false
        );

        let (_, key) = transferred_port(&mut engine);
        let packet = MessagePacket::new(
            ports.clone(),
            serde_json::json!({"ports":[key]}).to_string(),
        );
        let unpolled = async move {
            std::future::pending::<()>().await;
            drop(packet);
        };
        drop(unpolled);
        assert_eq!(
            port(&mut engine, "receive", Value::from_string(key), ""),
            false
        );

        let (peer, key) = transferred_port(&mut engine);
        let packet = MessagePacket::new(ports, serde_json::json!({"ports":[key]}).to_string());
        let received = port(&mut engine, "receive", Value::from_string(key), "");
        drop(packet);
        assert_eq!(
            port(
                &mut engine,
                "peer",
                Value::Num(received.as_u64().unwrap() as f64),
                ""
            ),
            peer
        );
    }

    #[test]
    fn service_worker_message_cleanup_reaches_the_end_of_nested_queued_transfers() {
        let mut engine = packet_engine();
        let ports = engine
            .ctx()
            .host::<HostState>()
            .unwrap()
            .message_ports
            .registry
            .clone();
        let canvas = crate::canvas_offscreen::send(crate::canvas_offscreen::DataHolder {
            width: 1,
            height: 1,
            language: None,
            rtl: false,
            placeholder: None,
        })
        .unwrap();
        let mut text = serde_json::json!({"canvases":[canvas]}).to_string();
        let mut innermost = String::new();
        for index in 0..1100 {
            let pair = port(&mut engine, "create", Value::Num(0.), "");
            let destination = pair[1].as_u64().unwrap();
            port(&mut engine, "send", Value::Num(destination as f64), &text);
            let key = port(&mut engine, "transfer", Value::Num(destination as f64), "");
            if index == 0 {
                innermost = key.as_str().unwrap().to_string();
            }
            text = serde_json::json!({"ports":[key]}).to_string();
        }
        drop(MessagePacket::new(ports, text));
        assert!(crate::canvas_offscreen::receive(&canvas).is_none());
        assert_eq!(
            port(&mut engine, "receive", Value::from_string(innermost), ""),
            false
        );
    }

    #[tokio::test]
    async fn service_worker_taken_message_keeps_transfers_until_finish_or_detach() {
        let mut engine = packet_engine();
        let url = url::Url::parse(DEFAULT_URL).unwrap();
        let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel();
        engine
            .ctx()
            .host_mut::<HostState>()
            .unwrap()
            .enable_network(
                url.clone(),
                tokio::runtime::Handle::current(),
                Arc::default(),
                sender,
            );
        operation_inner(engine.ctx(), "init", 0, serde_json::json!({
            "url":url.as_str(),"key":url.origin().ascii_serialization(),"secure":true,"kind":"window"
        })).unwrap();
        let (profile, client) = {
            let endpoint = &engine
                .ctx()
                .host::<HostState>()
                .unwrap()
                .service_workers
                .endpoints[&0];
            (endpoint.profile.clone(), endpoint.client.clone())
        };
        let options = RegistrationOptions::parse(
            &url,
            &url.origin(),
            "/sw.js",
            None,
            WorkerType::Classic,
            sw::UpdateViaCache::Imports,
        )
        .unwrap();
        let registration = profile.registry.set_registration(&client.key, &options);
        let worker = profile.registry.new_worker(&registration, &options);
        client.enable_messages();
        for detach in [false, true] {
            let (_, unclaimed) = transferred_port(&mut engine);
            let (peer, claimed) = transferred_port(&mut engine);
            client.enqueue_message(sw::PendingMessage {
                packet: serde_json::json!({"ports":[claimed,unclaimed]}).to_string(),
                origin: url.origin().ascii_serialization(),
                source: worker.info(),
            });
            assert!(
                operation_inner(engine.ctx(), "takeMessage", 0, serde_json::Value::Null)
                    .unwrap()
                    .is_object()
            );
            // A decode can claim its first holder and then throw. Finishing
            // or destroying the recipient must release only the remainder.
            let received = port(&mut engine, "receive", Value::from_string(claimed), "");
            if detach {
                engine
                    .ctx()
                    .host_mut::<HostState>()
                    .unwrap()
                    .service_workers
                    .remove(0);
            } else {
                operation_inner(engine.ctx(), "finishMessage", 0, serde_json::Value::Null).unwrap();
            }
            assert_eq!(
                port(&mut engine, "receive", Value::from_string(unclaimed), ""),
                false
            );
            assert_eq!(
                port(
                    &mut engine,
                    "peer",
                    Value::Num(received.as_u64().unwrap() as f64),
                    ""
                ),
                peer
            );
        }
    }

    #[tokio::test]
    async fn closing_agents_keep_their_slots_and_restart_handshakes_until_exit() {
        fn runtime() -> Arc<Runtime> {
            let (ctl, _) = TrustJsWorkerInbox::new();
            Arc::new(Runtime {
                handle: TrustJsWorkerHandle {
                    ctl,
                    interrupt: Arc::default(),
                    tasks: Arc::default(),
                    context: 0,
                },
                startup: Mutex::new(Some(Ok(()))),
                started: tokio::sync::Notify::new(),
                alive: AtomicBool::new(true),
                finished: AtomicBool::new(false),
                ended: tokio::sync::Notify::new(),
                next_event: AtomicU64::new(1),
                events: Mutex::default(),
                last_event: Mutex::new(Instant::now()),
            })
        }
        let storage = crate::js::WebStorage::default();
        let profile = profile_for(
            &storage,
            &Default::default(),
            tokio::runtime::Handle::current(),
            None,
        );
        for id in 0..MAX_AGENTS {
            let old = runtime();
            profile.admit_runtime(id as u64, old.clone()).unwrap();
            old.terminate();
        }
        let contender = runtime();
        assert_eq!(
            profile
                .admit_runtime(999, contender.clone())
                .unwrap_err()
                .name,
            "QuotaExceededError"
        );
        let old = profile.runtimes.lock().unwrap().get(&0).unwrap().clone();
        assert!(old.wait_for_exit().now_or_never().is_none());
        old.exited();
        assert!(old.wait_for_exit().now_or_never().is_some());
        profile.admit_runtime(999, contender).unwrap();
        let running = profile.runtimes.lock().unwrap();
        assert_eq!(running.len(), MAX_AGENTS);
        assert!(!running.contains_key(&0));
        assert!(
            running.contains_key(&1),
            "another closing worker lost its exit handshake"
        );
    }

    #[tokio::test]
    async fn profile_binding_preserves_ports_created_before_a_service_worker_container() {
        let storage = crate::js::WebStorage::default();
        let blobs = crate::js::BlobMap::default();
        let url = url::Url::parse("http://insecure.test/page").unwrap();
        // The frontend creates the profile before the first Window bootstrap.
        let _document = document(
            &storage,
            &blobs,
            tokio::runtime::Handle::current(),
            &url,
            crate::js::TraversableState::SHOWN,
        )
        .unwrap();
        let make = |url: url::Url| {
            let mut state = HostState::new(
                Rc::new(RefCell::new(Dom::new())),
                Rc::new(RealmClock::new()),
            );
            state.storage = storage.clone();
            state.blobs = blobs.clone();
            let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel();
            state.enable_network(
                url,
                tokio::runtime::Handle::current(),
                Arc::default(),
                sender,
            );
            let mut engine = trust_js::Engine::new();
            engine
                .ctx()
                .op_state()
                .put_retained_memory_with_external_memory(state);
            engine
        };
        let mut first = make(url);
        let pair = port(&mut first, "create", Value::Num(0.), "");
        let receiver = pair[1].as_u64().unwrap();
        port(
            &mut first,
            "send",
            Value::Num(receiver as f64),
            "queued before container initialization",
        );
        let mut second = make(url::Url::parse("https://secure.test/page").unwrap());
        operation_inner(second.ctx(), "init", 0, serde_json::json!({
            "url":"https://secure.test/page", "key":"https://secure.test", "secure":true, "kind":"window"
        })).unwrap();
        let token = port(&mut first, "transfer", Value::Num(receiver as f64), "");
        let received = port(
            &mut second,
            "receive",
            Value::from_string(token.as_str().unwrap().to_string()),
            "",
        );
        assert_eq!(received.as_u64(), Some(receiver));
        port(&mut second, "start", Value::Num(receiver as f64), "");
        assert_eq!(
            port(&mut second, "take", Value::Num(0.), ""),
            serde_json::json!([receiver, "queued before container initialization"])
        );
    }
}
