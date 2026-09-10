//! HTML image requests are independent of layout/paint visibility. In particular,
//! `new Image().src = ...` loads even while disconnected. Only the private prelude
//! binding receives pixels; opaque image responses never become script Fetch bodies.
use super::{Ctx, HostState, LumenHostTask, LumenPendingFetch, Value, host_arg_string};
use crate::http::{CredentialsMode, RequestMode};
use crate::performance::ResourceTiming;

pub(super) struct LoadedImage {
    node_id: usize,
    source: String,
    density: f32,
    image: crate::render::ImageResource,
    clean: bool,
}

pub(super) fn result_value(ctx: &mut Ctx, result: Option<LoadedImage>) -> Value {
    let Some(loaded) = result else {
        return Value::Null;
    };
    let image = loaded.image;
    // The private WeakMap owns these bytes, not a page-lifetime NodeId -> bitmap
    // table. Dropping detached image wrappers therefore allows GC to reclaim them.
    let Ok(pixels) = ctx.make_uint8array(&image.rgba) else {
        return Value::Null;
    };
    if let Some(state) = ctx.host_mut::<HostState>() {
        state
            .images
            .borrow_mut()
            .insert(loaded.source, (image.width, image.height));
        state.geom_cache.borrow_mut().epoch = u64::MAX;
        state.dom.borrow_mut().image_changed(loaded.node_id);
    }
    // Promise resolution must not consult an author-supplied Array/Object
    // prototype `then`: that would hand opaque image pixels to page script.
    let record = ctx.new_object_with_proto(&Value::Null);
    for (index, value) in [
        Value::Num(image.width as f64),
        Value::Num(image.height as f64),
        pixels,
        Value::Bool(loaded.clean),
        Value::Num(loaded.density as f64),
    ]
    .into_iter()
    .enumerate()
    {
        if ctx.member_set(&record, &index.to_string(), value).is_err() {
            return Value::Null;
        }
    }
    record
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
    let client = super::request_client_url(ctx);
    let context = ctx.host_job_context();
    let state = ctx.host_mut::<HostState>().expect("image host");
    let id = args.get(1).and_then(Value::as_num_opt).unwrap_or(-1.) as usize;
    let (selected, cors, referrer_policy) = {
        let dom = state.dom.borrow();
        let selected = crate::responsive_image::select(
            &dom,
            id,
            &client,
            state.viewport.get(),
            state.device_pixel_ratio.get(),
        );
        (
            selected,
            dom.attr(id, "crossorigin").map(str::to_string),
            dom.attr(id, "referrerpolicy")
                .and_then(crate::referrer_policy::ReferrerPolicy::parse)
                .unwrap_or_default(),
        )
    };
    if op == "size" {
        let dimensions = selected
            .and_then(|selected| {
                state
                    .images
                    .borrow()
                    .get(&selected.source)
                    .copied()
                    .map(|(w, h)| {
                        (
                            w as f64 / selected.density as f64,
                            h as f64 / selected.density as f64,
                        )
                    })
            })
            .unwrap_or((0., 0.));
        return Ok(ctx.make_array(vec![Value::Num(dimensions.0), Value::Num(dimensions.1)]));
    }
    if op != "load" {
        return Ok(Value::Null);
    }
    let (promise, resolve, _) = ctx.new_promise_with_resolvers();
    let Some(selected) = selected else {
        ctx.invoke(resolve, Value::Undefined, &[Value::Null])?;
        return Ok(promise);
    };
    let state = ctx.host_mut::<HostState>().expect("image host");
    let inline = if selected.source.starts_with("data:") {
        Some(crate::img::decode_data_url(&selected.source).map(|bytes| (bytes, true)))
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
                        .map(|entry| (entry.0.clone(), true))
                })
                .flatten(),
        )
    } else {
        None
    };
    let source = selected.source.clone();
    let decode = move |data: Option<(Vec<u8>, bool)>| {
        let (bytes, clean) = data?;
        Some(LoadedImage {
            node_id: id,
            source: selected.source.clone(),
            density: if selected.density.is_finite() && selected.density > 0. {
                selected.density
            } else {
                1.
            },
            image: crate::img::decode_graphical(&bytes).ok()?,
            clean,
        })
    };
    // Inline data has no network latency. Promise reactions still defer the
    // element's completion, and the prelude queues load/error as an element task.
    if let Some(data) = inline {
        let result = result_value(ctx, decode(data));
        ctx.invoke(resolve, Value::Undefined, &[result])?;
        return Ok(promise);
    }
    let policy = cors.as_ref().map(|value| {
        (
            RequestMode::Cors,
            if value.eq_ignore_ascii_case("use-credentials") {
                CredentialsMode::Include
            } else {
                CredentialsMode::SameOrigin
            },
        )
    });
    let prepared =
        super::prepare_client_request(state, &client, &source, "GET".into(), None, vec![], policy);
    let events = state.task_events.clone();
    let Some((handle, cache, mut request)) = prepared.filter(|_| events.is_some()) else {
        ctx.invoke(resolve, Value::Undefined, &[Value::Null])?;
        return Ok(promise);
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
    // A raw page-cache entry cannot authorize a CORS image. Only the no-CORS
    // path may reuse those bytes, retaining taint through every redirect hop.
    let cached = cors
        .is_none()
        .then(|| cache.peek_resource(&request.url, &client, "image", None))
        .flatten();
    let network = state.network.as_mut().expect("prepared network");
    let id = network.next_fetch_id;
    network.next_fetch_id += 1;
    network
        .pending_fetches
        .insert(id, LumenPendingFetch { context, resolve });
    cache.spawn(&handle, async move {
        let (data, timing) = if let Some(cached) = cached {
            match cached.await {
                Ok(response) => {
                    let clean = !response.url_list.is_empty()
                        && response
                            .url_list
                            .iter()
                            .all(|url| url.origin() == client.origin());
                    let timing = ResourceTiming::cached(
                        source.clone(),
                        "img",
                        response.timing.as_deref(),
                        started,
                    );
                    (Some((response.body.clone(), clean)), timing)
                }
                Err(()) => (None, None),
            }
        } else {
            // GET with UA-owned safelisted headers needs no preflight. The
            // normal HTTP path applies credentials and response CORS checks.
            match crate::http::fetch_with_timing(&request, referrer_policy).await {
                Ok(details) => {
                    let clean = cors.is_some()
                        || (!details.url_list.is_empty()
                            && details
                                .url_list
                                .iter()
                                .all(|url| url.origin() == client.origin()));
                    let timing =
                        ResourceTiming::fetched(source, "img", details.response.timing, started);
                    (Some((details.response.body, clean)), timing)
                }
                Err(error) => (
                    None,
                    ResourceTiming::fetched(source, "img", error.timing, started),
                ),
            }
        };
        let result = tokio::task::spawn_blocking(move || decode(data))
            .await
            .ok()
            .flatten();
        let _ = events
            .expect("checked events")
            .send(LumenHostTask::ImageDone { id, result, timing });
    });
    Ok(promise)
}
