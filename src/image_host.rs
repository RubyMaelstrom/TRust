//! HTML image requests are independent of layout/paint visibility. In particular,
//! `new Image().src = ...` loads even while disconnected. Only the private prelude
//! binding receives pixels; opaque image responses never become script Fetch bodies.
//!
//! The document owns every image request (HTML e5071a20 #updating-the-image-data):
//! element requests and the stylesheet/presentation images its rendering needs
//! (CSS Images 4 #fetching-images) go through the page's shared request store
//! (`page_images`), whose responses the frontends present instead of fetching
//! again. Each request reports Resource Timing to its node document's global
//! (Fetch #fetch-finale, Resource Timing #marking-resource-timing); only
//! HTTP(S) requests produce entries (Fetch #fetch-finale's report timing steps).
use super::{Ctx, HostState, LumenHostTask, LumenPendingFetch, Value, host_arg_string};
use crate::http::{CredentialsMode, RequestMode};
use crate::page_images::{ImageFailure, ImageJoin, ImageRequestKey, SharedImageFetch};
use crate::performance::ResourceTiming;
use std::sync::Arc;

/// A completed element image request. The compressed bytes stay native: the
/// prelude's request record materializes pixels only for a canvas, WebGL or
/// ImageBitmap consumer, so presenting a gallery never copies every decoded
/// bitmap into the JavaScript heap.
pub(super) struct LoadedImage {
    node_id: usize,
    source: String,
    density: f32,
    width: u32,
    height: u32,
    clean: bool,
    bytes: Arc<[u8]>,
}

/// Bytes behind one element request record (see `LoadedImage`).
pub(super) struct ImagePayload {
    bytes: Arc<[u8]>,
}

impl ImagePayload {
    pub(super) fn len(&self) -> usize {
        self.bytes.len()
    }
}

/// Probe a fetched resource the way HTML's image request does: available once
/// its natural dimensions are known (#img-load), broken otherwise (#img-error).
pub(super) fn probe(source: &str, bytes: &[u8]) -> Option<(u32, u32)> {
    let size = crate::img::natural_size(bytes).ok()?;
    // SVG 2 §8.12: a ratio-only SVG publishes its intrinsic ratio for layout.
    crate::img::record_svg_intrinsic_metadata(source, bytes);
    Some(size)
}

pub(super) fn result_value(ctx: &mut Ctx, result: Option<LoadedImage>) -> Value {
    let Some(loaded) = result else {
        return Value::Null;
    };
    let Some(state) = ctx.host_mut::<HostState>() else {
        return Value::Null;
    };
    let previous = state
        .images
        .borrow_mut()
        .insert(loaded.source, (loaded.width, loaded.height));
    if previous != Some((loaded.width, loaded.height)) {
        state.geom_cache.borrow_mut().epoch = u64::MAX;
        state.dom.borrow_mut().image_changed(loaded.node_id);
    } else {
        // CSS Images 3 #default-sizing: another element completing the
        // same decoded source does not change the natural dimensions
        // already used by layout. Its bitmap still needs presentation.
        state
            .dom
            .borrow_mut()
            .replaced_pixels_changed(loaded.node_id);
    }
    let token = state.next_image_payload;
    state.next_image_payload += 1;
    state.image_payloads.insert(
        token,
        ImagePayload {
            bytes: loaded.bytes,
        },
    );
    // Promise resolution must not consult an author-supplied Array/Object
    // prototype `then`: that would hand opaque image pixels to page script.
    // Index 2 (pixels) is materialized on first use by the prelude.
    let record = ctx.new_object_with_proto(&Value::Null);
    for (index, value) in [
        (0, Value::Num(loaded.width as f64)),
        (1, Value::Num(loaded.height as f64)),
        (3, Value::Bool(loaded.clean)),
        (4, Value::Num(loaded.density as f64)),
        (6, Value::Num(token as f64)),
    ] {
        if ctx.member_set(&record, &index.to_string(), value).is_err() {
            return Value::Null;
        }
    }
    record
}

/// The settings context whose Document owns `node`: the request client of its
/// image fetches and the global that receives their timing entries.
fn node_context(ctx: &mut Ctx, node: usize) -> u64 {
    let calling = ctx.host_job_context();
    let state = ctx.host_mut::<HostState>().expect("image host");
    let dom = state.dom.clone();
    let dom = dom.borrow();
    if !dom.is_valid(node) {
        return calling;
    }
    super::node_window_context(state, &dom, node).unwrap_or(calling)
}

/// How a document image request reports Resource Timing for one requester.
pub(super) fn request_timing(
    name: &str,
    initiator: Option<&'static str>,
    join: ImageJoin,
    owner: u64,
    outcome: &Result<Arc<crate::page_images::FetchedImage>, ImageFailure>,
    started: f64,
) -> Option<ResourceTiming> {
    let initiator = initiator?;
    // Fetch #fetch-finale: only HTTP(S) requests report timing.
    if !(name.starts_with("http:") || name.starts_with("https:")) {
        return None;
    }
    match join {
        ImageJoin::Started => match outcome {
            Ok(image) => {
                ResourceTiming::fetched(name.to_owned(), initiator, image.timing.clone(), started)
            }
            Err(ImageFailure::Network(timing)) => {
                ResourceTiming::fetched(name.to_owned(), initiator, timing.clone(), started)
            }
            Err(_) => None,
        },
        // Resource Timing #resources-included-in-the-performanceresourcetiming-interface:
        // a second element reusing its document's download is not another fetch.
        ImageJoin::Joined { owner: first } if first == owner => None,
        // Another document's download is this document's memory-cache load.
        ImageJoin::Joined { .. } => match outcome {
            Ok(image) => {
                ResourceTiming::cached(name.to_owned(), initiator, image.timing.as_deref(), started)
            }
            Err(_) => None,
        },
    }
}

/// Start (or join) a document image request for `source`, owned by settings
/// context `context`. `None` when the request failed a precondition; the
/// store then records the refusal for waiting presentation consumers.
pub(super) fn document_request(
    ctx: &mut Ctx,
    context: u64,
    source: &str,
    cors: Option<CredentialsMode>,
    referrer_policy: crate::referrer_policy::ReferrerPolicy,
) -> Option<(SharedImageFetch, ImageJoin, tokio::runtime::Handle)> {
    let client = super::request_context_url(ctx, context);
    let restricted = super::cookie_context_for_id(ctx, context).cross_site_ancestor;
    let state = ctx.host_mut::<HostState>().expect("image host");
    let store = state.network.as_ref()?.cache.images();
    let policy = cors.map(|credentials| (RequestMode::Cors, credentials));
    let Some((handle, cache, mut request)) = super::prepare_uncounted_client_request(
        state,
        &client,
        source,
        "GET".into(),
        None,
        vec![],
        policy,
    ) else {
        if let Ok(url) = url::Url::parse(source) {
            store.refuse(
                ImageRequestKey {
                    url: url.to_string(),
                    cors,
                    origin: client.origin(),
                    cookie_restricted: restricted,
                },
                context,
            );
        }
        return None;
    };
    request
        .headers
        .retain(|(key, _)| !key.eq_ignore_ascii_case("referer"));
    if let Some(url) = referrer_policy.determine(&client, &request.url) {
        request.headers.push(("Referer".into(), url.to_string()));
    }
    crate::http::set_image_accept(&mut request);
    crate::http::set_fetch_metadata(
        &mut request,
        &client,
        "image",
        if cors.is_some() { "cors" } else { "no-cors" },
    );
    // RFC 6265bis #document-requests: a frame's image retains its document's
    // ancestor restriction (`set_fetch_metadata` keeps the top-level site).
    if let Some(cookies) = request.cookie_context.as_mut() {
        cookies.cross_site_ancestor |= restricted;
    }
    let key = ImageRequestKey {
        url: request.url.to_string(),
        cors,
        origin: client.origin(),
        cookie_restricted: request
            .cookie_context
            .as_ref()
            .is_some_and(|cookies| cookies.cross_site_ancestor),
    };
    // HTML #consume-a-preloaded-resource: a no-CORS image may use the bytes of
    // a matching `<link rel=preload as=image>`, retaining taint through every
    // redirect hop. A raw page-cache entry cannot authorize a CORS image.
    if cors.is_none()
        && let Some(preloaded) = cache.peek_resource(&request.url, &client, "image", None)
    {
        let (fetch, join) = store.adopt(key, context, async move {
            preloaded
                .await
                .map(|response| {
                    Arc::new(crate::page_images::FetchedImage::new(
                        response.status,
                        Arc::from(response.body.as_slice()),
                        response.url_list.clone(),
                        response.timing.clone(),
                    ))
                })
                .map_err(|()| ImageFailure::Network(None))
        });
        // The preload reported its own fetch; each consumer's load is a
        // memory-cache load with its own lifetime.
        let join = match join {
            ImageJoin::Started => ImageJoin::Joined { owner: u64::MAX },
            joined => joined,
        };
        return Some((fetch, join, handle));
    }
    let (fetch, join) = store.fetch(&handle, key, context, request, referrer_policy);
    Some((fetch, join, handle))
}

pub(super) fn call(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let started = crate::performance::now_ms();
    let op = host_arg_string(ctx, args, 0);
    if op == "slots" {
        let candidate = args.get(1).cloned().unwrap_or(Value::Undefined);
        return Ok(ctx
            .host_mut::<HostState>()
            .expect("image host")
            .image_element_slots
            .get_or_insert(candidate)
            .clone());
    }
    match op.as_str() {
        "pixels" => return Ok(pixels(ctx, args)),
        "adopt" => {
            adopt(ctx, args);
            return Ok(Value::Undefined);
        }
        "created" => return Ok(created(ctx, args)),
        // HTML #updating-the-image-data runs synchronously when an img is
        // created, adopted or mutated; `created_images` defers some of those
        // to the next microtask checkpoint. "queued" asks whether one is
        // still pending; "retire" drops it once a later update has run.
        "queued" => {
            let id = args.get(1).and_then(Value::as_num_opt).unwrap_or(-1.) as usize;
            let dom = super::host_dom(ctx);
            let dom = dom.borrow();
            return Ok(Value::Bool(dom.is_valid(id) && dom.image_update_queued(id)));
        }
        "retire" => {
            let id = args.get(1).and_then(Value::as_num_opt).unwrap_or(-1.) as usize;
            let dom = super::host_dom(ctx);
            let mut dom = dom.borrow_mut();
            if dom.has_created_images() {
                dom.retire_queued_image_update(id);
            }
            return Ok(Value::Undefined);
        }
        "lazy" => {
            expect(ctx, args);
            return Ok(Value::Undefined);
        }
        "load" => {}
        _ => return Ok(Value::Null),
    }
    let id = args.get(1).and_then(Value::as_num_opt).unwrap_or(-1.) as usize;
    let context = node_context(ctx, id);
    let client = super::request_context_url(ctx, context);
    let calling = ctx.host_job_context();
    let state = ctx.host_mut::<HostState>().expect("image host");
    let (selected, cors, referrer_policy) = {
        let dom = state.dom.borrow();
        if dom.is_valid(id) {
            (
                crate::responsive_image::select(
                    &dom,
                    id,
                    &client,
                    state.viewport.get(),
                    state.device_pixel_ratio.get(),
                ),
                dom.attr(id, "crossorigin").map(str::to_string),
                dom.attr(id, "referrerpolicy")
                    .and_then(crate::referrer_policy::ReferrerPolicy::parse)
                    .unwrap_or_default(),
            )
        } else {
            (None, None, Default::default())
        }
    };
    // HTML e5071a20 #updating-the-image-data: while this algorithm runs the node document
    // strongly retains the element. Do not depend on the JS reaction retaining its wrapper:
    // allocation and Realm entry can collect before the completion reaction is installed.
    let native_lease = if selected.is_some() {
        state.dom_gc.start_resource(id);
        Some(state.dom_gc.resource_lease(id))
    } else {
        None
    };
    let (promise, resolve, _) = ctx.new_promise_with_resolvers();
    let Some(selected) = selected else {
        ctx.invoke(resolve, Value::Undefined, &[Value::Null])?;
        return Ok(promise);
    };
    let density = if selected.density.is_finite() && selected.density > 0. {
        selected.density
    } else {
        1.
    };
    let state = ctx.host_mut::<HostState>().expect("image host");
    let inline = if selected.source.starts_with("data:") {
        Some(crate::img::decode_data_url(&selected.source).map(|bytes| (Arc::from(bytes), true)))
    } else if selected.source.starts_with("blob:") {
        let same_origin = url::Url::parse(&selected.source)
            .ok()
            .is_some_and(|url| url.origin() == client.origin());
        Some(
            same_origin
                .then(|| {
                    state
                        .blobs
                        .lock()
                        .ok()?
                        .get(&selected.source)
                        .map(|entry| (Arc::from(entry.0.as_slice()), true))
                })
                .flatten(),
        )
    } else {
        None
    };
    let source = selected.source.clone();
    let loaded = move |bytes: Arc<[u8]>, clean: bool, size: Option<(u32, u32)>| {
        let (width, height) = size?;
        Some(LoadedImage {
            node_id: id,
            source,
            density,
            width,
            height,
            clean,
            bytes,
        })
    };
    // Inline data has no network latency. Promise reactions still defer the
    // element's completion, and the prelude queues load/error as an element task.
    if let Some(data) = inline {
        let result = data.and_then(|(bytes, clean)| {
            let size = probe(&selected.source, &bytes);
            loaded(bytes, clean, size)
        });
        let result = result_value(ctx, result);
        ctx.invoke(resolve, Value::Undefined, &[result])?;
        return Ok(promise);
    }
    let credentials = cors.as_ref().map(|value| {
        if value.eq_ignore_ascii_case("use-credentials") {
            CredentialsMode::Include
        } else {
            CredentialsMode::SameOrigin
        }
    });
    let events = state.task_events.clone();
    let requested = events
        .is_some()
        .then(|| document_request(ctx, context, &selected.source, credentials, referrer_policy));
    let Some((fetch, join, handle)) = requested.flatten() else {
        ctx.invoke(resolve, Value::Undefined, &[Value::Null])?;
        return Ok(promise);
    };
    let state = ctx.host_mut::<HostState>().expect("image host");
    let network = state.network.as_mut().expect("prepared network");
    let cache = network.cache.clone();
    let request_id = network.next_fetch_id;
    network.next_fetch_id += 1;
    network.pending_fetches.insert(
        request_id,
        LumenPendingFetch {
            context: calling,
            resolve,
            native_lease,
        },
    );
    let name = selected.source.clone();
    cache.spawn(&handle, async move {
        let outcome = fetch.await;
        let timing = request_timing(&name, Some("img"), join, context, &outcome, started);
        let result = match outcome {
            Ok(image) => {
                // HTML #cors-same-origin: a CORS request that passed is clean;
                // a no-CORS response is clean only when every hop was same-origin.
                let clean = credentials.is_some()
                    || (!image.url_list.is_empty()
                        && image
                            .url_list
                            .iter()
                            .all(|url| url.origin() == client.origin()));
                // Elements sharing a response probe its dimensions once.
                let size = match image.known_natural_size() {
                    Some(size) => size,
                    None => {
                        let (probed, source) = (image.clone(), name.clone());
                        tokio::task::spawn_blocking(move || {
                            probed.natural_size(|bytes| probe(&source, bytes))
                        })
                        .await
                        .ok()
                        .flatten()
                    }
                };
                loaded(image.body.clone(), clean, size)
            }
            Err(_) => None,
        };
        let _ = events
            .expect("checked events")
            .send(LumenHostTask::ImageDone {
                id: request_id,
                result,
                timing,
                timing_context: context,
            });
    });
    Ok(promise)
}

/// The decoded pixels behind one request record (HTML #prepare-an-image-for-
/// presentation), materialized for a canvas, WebGL or ImageBitmap consumer.
fn pixels(ctx: &mut Ctx, args: &[Value]) -> Value {
    let token = args.get(1).and_then(Value::as_num_opt).unwrap_or(-1.) as u64;
    let bytes = ctx
        .host_mut::<HostState>()
        .and_then(|state| state.image_payloads.get(&token))
        .map(|payload| payload.bytes.clone());
    let Some(image) = bytes.and_then(|bytes| crate::img::decode_graphical(&bytes).ok()) else {
        return Value::Null;
    };
    ctx.make_uint8array(&image.rgba).unwrap_or(Value::Null)
}

/// The element task that completed an image request either made it the
/// element's current request (retain its bytes, release the previous ones) or
/// found it superseded by a newer request (release it).
fn adopt(ctx: &mut Ctx, args: &[Value]) {
    let node = args
        .get(1)
        .and_then(Value::as_num_opt)
        .map(|id| id as usize);
    let token = args
        .get(2)
        .and_then(Value::as_num_opt)
        .map(|token| token as u64);
    let accepted = matches!(args.get(3), Some(Value::Bool(true)));
    let Some(state) = ctx.host_mut::<HostState>() else {
        return;
    };
    let (Some(node), true) = (node, accepted) else {
        if let Some(token) = token {
            state.image_payloads.remove(&token);
        }
        return;
    };
    let previous = match token {
        Some(token) => state.element_image_payloads.insert(node, token),
        None => state.element_image_payloads.remove(&node),
    };
    if let Some(previous) = previous.filter(|previous| Some(*previous) != token) {
        state.image_payloads.remove(&previous);
    }
}

/// HTML #lazy-loading-attributes: a lazy image's request waits for its
/// resumption steps. Presentation consumers wait for it instead of fetching.
fn expect(ctx: &mut Ctx, args: &[Value]) {
    let source = host_arg_string(ctx, args, 1);
    let Ok(url) = url::Url::parse(&source) else {
        return;
    };
    if !matches!(url.scheme(), "http" | "https") {
        return;
    }
    if let Some(network) = ctx
        .host_mut::<HostState>()
        .and_then(|state| state.network.as_ref())
    {
        network.cache.images().expect(url.as_str());
    }
}

/// Image elements created by the HTML parser or fragment parsing/cloning in
/// `document` that have not yet run "update the image data" (HTML #when-to-
/// obtain-images: whenever the element is created). Removed from the list.
fn created(ctx: &mut Ctx, args: &[Value]) -> Value {
    let document = args
        .get(1)
        .and_then(Value::as_num_opt)
        .map(|id| id as usize);
    let ids = {
        let dom = super::host_dom(ctx);
        let mut dom = dom.borrow_mut();
        match document {
            Some(document) if dom.is_valid(document) => dom.take_created_images_in(document),
            _ => Vec::new(),
        }
    };
    ctx.make_array(ids.into_iter().map(|id| Value::Num(id as f64)).collect())
}
