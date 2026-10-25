//! Filtered response capabilities and cache ownership. Raw cross-origin
//! headers and opaque bytes stay in native RAM, never in JSON/JS records.
use super::*;
use crate::http::private_response::{Memory, Replay};
use std::sync::{Mutex, Weak};
#[path = "private_response_host/cache.rs"]
mod cache;

type StorageMap = Mutex<HashMap<String, HashMap<String, String>>>;
#[derive(Default)]
struct ProfileState {
    tokens: HashMap<String, Weak<Payload>>,
    cached: HashMap<(String, String), Vec<Arc<Payload>>>,
    cache_users: HashMap<(String, String), usize>,
}
#[derive(Default)]
pub(super) struct Profile {
    state: Mutex<ProfileState>,
    memory: Arc<Memory>,
}
static PROFILES: Mutex<Vec<(Weak<StorageMap>, Arc<Profile>)>> = Mutex::new(Vec::new());
fn profile(storage: &crate::js::WebStorage) -> Arc<Profile> {
    let mut profiles = PROFILES.lock().unwrap();
    profiles.retain(|(storage, _)| storage.strong_count() != 0);
    if let Some((_, profile)) = profiles
        .iter()
        .find(|(key, _)| key.ptr_eq(&Arc::downgrade(storage)))
    {
        return profile.clone();
    }
    let profile = Arc::new(Profile::default());
    profiles.push((Arc::downgrade(storage), profile.clone()));
    profile
}

#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct PublicHead {
    #[serde(rename = "type")]
    kind: &'static str,
    status: u16,
    status_message: String,
    header_list: Vec<(String, String)>,
    url_list: Vec<String>,
    token: String,
}
pub(super) struct Payload {
    owner: String,
    public: PublicHead,
    pub(super) internal: crate::http::InternalResponse,
    pub(super) body: Option<Arc<Replay>>,
    memory: Arc<Memory>,
    metadata_bytes: usize,
    padding: usize,
}
impl Payload {
    fn cache_size(&self) -> usize {
        // UA privacy measure discussed in WHATWG Storage issue #31:
        // https://github.com/whatwg/storage/issues/31 . Random padding is
        // stable across clones and cache matches, then rounded into buckets;
        // quota probing and estimate() must not expose an opaque body's size.
        self.body.as_ref().map_or(0, |body| {
            body.size()
                .saturating_add(self.padding)
                .div_ceil(64 * 1024)
                .max(1)
                * (64 * 1024)
        })
    }
}
impl Drop for Payload {
    fn drop(&mut self) {
        self.memory.release(self.metadata_bytes);
    }
}

#[derive(Default)]
pub(super) struct Agent {
    leases: HashMap<u64, Arc<Payload>>,
    cache_leases: HashMap<u64, cache::Lease>,
    transfers: Vec<(u64, bool, Weak<Replay>)>,
    next: u64,
}
impl Agent {
    pub(super) fn transferring(&self) -> bool {
        self.transfers
            .iter()
            .filter_map(|(_, _, body)| body.upgrade())
            .any(|body| body.pending())
    }
    pub(super) fn abort(&mut self, context: u64) -> bool {
        let mut aborted = false;
        self.transfers.retain(|(owner, keepalive, body)| {
            let Some(body) = body.upgrade() else {
                return false;
            };
            if *owner == context && !keepalive {
                aborted |= body.cancel();
            }
            true
        });
        aborted
    }
    fn retain(&mut self, payload: Arc<Payload>) -> String {
        self.next += 1;
        let mut head = serde_json::to_value(&payload.public).unwrap();
        head["lease"] = serde_json::Value::from(self.next);
        head["pending"] =
            serde_json::Value::Bool(payload.body.as_ref().is_some_and(|body| body.pending()));
        self.leases.insert(self.next, payload);
        head.to_string()
    }
    pub(super) fn scan(&self, visitor: &mut dyn HostRetainedMemoryVisitor) {
        for payload in self.leases.values() {
            visitor.allocation(RetainedManagedAllocation::new(
                "trust.private-response",
                Arc::as_ptr(payload) as usize,
                payload.metadata_bytes,
            ));
            if let Some(body) = &payload.body {
                visitor.allocation(RetainedManagedAllocation::new(
                    "trust.private-response-body",
                    Arc::as_ptr(body) as usize,
                    body.size(),
                ));
            }
        }
    }
}

fn owner<'s>(ctx: &mut Ctx<'s>) -> String {
    let context = ctx.host_job_context();
    let cookies = cookie_context_for_id(ctx, context);
    let (_, origin, _) = blob_environment(ctx);
    let partition = cookies.storage_partition();
    if origin == "null" || partition == "null" {
        "null".into()
    } else if partition.is_empty() {
        origin
    } else {
        format!("{origin}^{partition}")
    }
}

/// Response exposes its final URL and whether it redirected, never the
/// intermediate Location values. Keep the complete list in native metadata.
pub(super) fn public_urls(urls: &[url::Url], kind: &str) -> Vec<String> {
    if kind == "opaque" {
        return Vec::new();
    }
    urls.last()
        .map_or_else(Vec::new, |url| vec![url.to_string(); urls.len().min(2)])
}

/// Fetch's service-worker response path can forward the internal response,
/// but only from a capability owned by this worker's storage partition.
pub(super) fn for_service_worker<'s>(ctx: &mut Ctx<'s>, token: &str) -> Option<Arc<Payload>> {
    let key = owner(ctx);
    let state = ctx.host::<HostState>()?;
    profile(&state.storage)
        .state
        .lock()
        .unwrap()
        .tokens
        .get(token)
        .and_then(Weak::upgrade)
        .filter(|payload| payload.owner == key)
}

pub(super) fn install<'s>(
    ctx: &mut Ctx<'s>,
    mut internal: crate::http::InternalResponse,
    public_headers: Vec<(String, String)>,
    stream: Option<crate::http::BodyStream>,
    timing: Option<TrustJsResourceTiming>,
    keepalive: bool,
) -> Option<String> {
    let owner = owner(ctx);
    let context = ctx.host_job_context();
    let state = ctx.host_mut::<HostState>()?;
    let profile = profile(&state.storage);
    let mut random = [0; 16];
    getrandom::fill(&mut random).ok()?;
    let token = random
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let opaque = matches!(internal.kind, "opaque" | "opaqueredirect");
    let public = PublicHead {
        kind: internal.kind,
        status: if opaque { 0 } else { internal.status },
        status_message: if opaque {
            String::new()
        } else {
            internal.status_text.clone()
        },
        header_list: public_headers,
        url_list: public_urls(&internal.url_list, internal.kind),
        token: token.clone(),
    };
    let body = if opaque {
        let replay = Replay::new(
            internal.body.take().unwrap_or_default(),
            profile.memory.clone(),
        )
        .ok()?;
        if let Some(stream) = stream {
            let network = state.network.as_ref()?;
            let events = state.task_events.clone();
            let finished_token = token.clone();
            replay.stream(
                &network.handle,
                stream,
                keepalive,
                move |_, encoded, decoded| {
                    if let Some(events) = events {
                        let timing = timing.map(|mut timing| {
                            timing.timing.encoded_body_size = encoded;
                            timing.timing.decoded_body_size = decoded;
                            timing.timing.response_end = crate::performance::now_ms();
                            timing
                        });
                        let _ = events.send(TrustJsHostTask::PrivateResponseEnd {
                            context,
                            token: finished_token,
                            timing,
                        });
                    }
                },
            );
            state
                .private_responses
                .transfers
                .push((context, keepalive, Arc::downgrade(&replay)));
        } else {
            replay.finished();
        }
        Some(replay)
    } else {
        None
    };
    let metadata_bytes = std::mem::size_of::<Payload>()
        + owner.len()
        + public.token.len()
        + internal
            .headers
            .iter()
            .chain(public.header_list.iter())
            .map(|(k, v)| k.len() + v.len() + 64)
            .sum::<usize>()
        + internal
            .url_list
            .iter()
            .map(|url| url.as_str().len())
            .sum::<usize>()
        + public.url_list.iter().map(String::len).sum::<usize>();
    profile.memory.reserve(metadata_bytes).ok()?;
    let payload = Arc::new(Payload {
        owner,
        public,
        internal,
        body,
        memory: profile.memory.clone(),
        metadata_bytes,
        padding: u32::from_ne_bytes(random[12..].try_into().unwrap()) as usize % (128 * 1024),
    });
    {
        let mut state = profile.state.lock().unwrap();
        state.tokens.retain(|_, weak| weak.strong_count() != 0);
        state.tokens.insert(token, Arc::downgrade(&payload));
    }
    Some(state.private_responses.retain(payload))
}

pub(super) fn operation<'s>(
    ctx: &mut Ctx<'s>,
    _this: Value<'s>,
    args: &[Value<'s>],
) -> Result<Value<'s>, Value<'s>> {
    let op = host_arg_string(ctx, args, 0);
    if op.starts_with("cache-") {
        return cache::operation(ctx, &op, args);
    }
    if op == "release" {
        let lease = args.get(1).and_then(Value::as_num_opt).unwrap_or(0.) as u64;
        if let Some(state) = ctx.host_mut::<HostState>() {
            state.private_responses.leases.remove(&lease);
        }
        return Ok(Value::Undefined);
    }
    let token = host_arg_string(ctx, args, 1);
    let key = owner(ctx);
    let payload = {
        let state = ctx.host_mut::<HostState>().expect("private response host");
        let profile = profile(&state.storage);
        profile
            .state
            .lock()
            .unwrap()
            .tokens
            .get(&token)
            .and_then(Weak::upgrade)
            .filter(|payload| payload.owner == key)
    };
    if op == "abort" {
        if let Some(payload) = payload
            && let Some(body) = &payload.body
        {
            body.cancel();
        }
        return Ok(Value::Undefined);
    }
    if op == "retain" {
        return Ok(match payload {
            Some(payload) => {
                let token = ctx
                    .host_mut::<HostState>()
                    .unwrap()
                    .private_responses
                    .retain(payload);
                ctx.string(token)
            }
            None => Value::Null,
        });
    }
    let (promise, resolve, _) = ctx.new_promise_with_resolvers();
    let Some(payload) = payload else {
        let _ = ctx.invoke(resolve, Value::Undefined, &[Value::Bool(false)]);
        return Ok(promise);
    };
    let Some(body) = payload.body.clone() else {
        let _ = ctx.invoke(resolve, Value::Undefined, &[Value::Bool(true)]);
        return Ok(promise);
    };
    let context = ctx.host_job_context();
    let resolve_handle = super::hold(ctx, &resolve);
    let work = ctx.host_mut::<HostState>().and_then(|state| {
        let events = state.task_events.clone()?;
        let network = state.network.as_mut()?;
        let id = network.next_fetch_id;
        network.next_fetch_id += 1;
        network.pending_fetches.insert(
            id,
            TrustJsPendingFetch {
                context,
                resolve: resolve_handle,
                native_lease: None,
                keepalive: false,
                task: None,
            },
        );
        Some((id, events, network.handle.clone(), network.cache.clone()))
    });
    if let Some((id, events, handle, cache)) = work {
        cache.spawn(&handle, async move {
            let ok = body.complete().await.is_ok();
            let _ = events.send(TrustJsHostTask::PrivateResponseReady { id, ok });
            drop(payload);
        });
    } else {
        let _ = ctx.invoke(resolve, Value::Undefined, &[Value::Bool(false)]);
    }
    Ok(promise)
}

/// Called under the storage shed's write lock, only once its CAS comparison
/// matched. Cache entries hold native capabilities independently of globals.
/// Validate partition and quota here even if author-modified JSON methods
/// corrupt a platform record; a token never changes a response's origin/type.
pub(super) fn cache_commit(
    storage: &crate::js::WebStorage,
    bucket: &str,
    key: &str,
    encoded: &str,
    serialized_bytes: usize,
) -> Result<(), &'static str> {
    let Some(owner) = bucket.strip_prefix("cache-storage-data:") else {
        return Ok(());
    };
    let profile = profile(storage);
    let records: serde_json::Value = serde_json::from_str(encoded).map_err(|_| "TypeError")?;
    let mut state = profile.state.lock().unwrap();
    let mut payloads = Vec::new();
    for record in records.as_array().ok_or("TypeError")? {
        if let Some(token) = record["response"]["privateToken"].as_str() {
            let payload = state
                .tokens
                .get(token)
                .and_then(Weak::upgrade)
                .filter(|payload| payload.owner == owner)
                .ok_or("TypeError")?;
            payloads.push(payload);
        }
    }
    let cache_key = (bucket.to_string(), key.to_string());
    let bytes = |items: &Vec<Arc<Payload>>| {
        items
            .iter()
            .map(|payload| payload.cache_size())
            .sum::<usize>()
    };
    let total = state
        .cached
        .iter()
        .filter(|((other, _), _)| other == bucket)
        .filter(|(k, _)| **k != cache_key)
        .map(|(_, items)| bytes(items))
        .sum::<usize>()
        .saturating_add(bytes(&payloads));
    if total.saturating_add(serialized_bytes) > 64 * 1024 * 1024 {
        return Err("QuotaExceededError");
    }
    state.cached.insert(cache_key, payloads);
    Ok(())
}

pub(super) fn cache_usage(storage: &crate::js::WebStorage, owner: &str) -> usize {
    let bucket = format!("cache-storage-data:{owner}");
    profile(storage)
        .state
        .lock()
        .unwrap()
        .cached
        .iter()
        .filter(|((key, _), _)| *key == bucket)
        .flat_map(|(_, payloads)| payloads)
        .map(|payload| payload.cache_size())
        .sum()
}

#[cfg(test)]
pub(super) fn cached_payloads(storage: &crate::js::WebStorage) -> Vec<Arc<Payload>> {
    profile(storage)
        .state
        .lock()
        .unwrap()
        .cached
        .values()
        .flatten()
        .cloned()
        .collect()
}
