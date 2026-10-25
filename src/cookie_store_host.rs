//! Cookie Store's native boundary. All storage uses http::cookie_store, whose
//! writes pass through the existing bookmark policy. The JS object supplies
//! its owning settings context and creation URL, independently of the caller.
use super::*;

pub(super) fn operation<'s>(
    ctx: &mut Ctx<'s>,
    _this: Value<'s>,
    args: &[Value<'s>],
) -> Result<Value<'s>, Value<'s>> {
    let op = host_arg_string(ctx, args, 0);
    let context = args.get(1).and_then(Value::as_num_opt).unwrap_or(0.) as u64;
    let cookie_args = [
        args.get(2).cloned().unwrap_or(Value::Undefined),
        Value::Num(context as f64),
        args.get(3).cloned().unwrap_or(Value::Undefined),
    ];
    let Some(url) = document_cookie_url(ctx, &cookie_args) else {
        return Ok(ctx.string(
            serde_json::json!({"error":"SecurityError",
            "message":"Cookie storage is unavailable for this environment"})
            .to_string(),
        ));
    };
    let cookie_context = cookie_context_for_id(ctx, context);
    let input = host_arg_string(ctx, args, 4);
    let input: serde_json::Value = serde_json::from_str(&input).unwrap_or_default();
    let result = match op.as_str() {
        "watch" => {
            let state = ctx.host_mut::<HostState>().expect("cookie host");
            if !state.cookie_subscriptions.contains_key(&context)
                && let Some(events) = state.task_events.clone()
            {
                let watch = crate::http::cookie_store::subscribe(
                    url,
                    cookie_context,
                    Arc::new(move |changes| {
                        let _ = events.send(TrustJsHostTask::CookieChanges { context, changes });
                    }),
                );
                state.cookie_subscriptions.insert(context, watch);
            }
            serde_json::Value::Null
        }
        "get" => serde_json::json!(crate::http::cookie_store::query(
            &url,
            &cookie_context,
            input["name"].as_str()
        )),
        "set" => match serde_json::from_value(input) {
            Ok(init) => match crate::http::cookie_store::set(&url, &cookie_context, init) {
                Ok(()) => serde_json::Value::Null,
                Err(message) => serde_json::json!({"error":"TypeError", "message":message}),
            },
            Err(_) => serde_json::json!({"error":"TypeError", "message":"Invalid cookie options"}),
        },
        _ => serde_json::Value::Null,
    };
    Ok(ctx.string(result.to_string()))
}

pub(super) fn dispatch(
    engine: &mut trust_js::Engine,
    context: u64,
    changes: crate::http::cookie_store::Changes,
) -> Result<(), String> {
    if !engine
        .host::<HostState>()
        .is_some_and(|state| state.cookie_subscriptions.contains_key(&context))
    {
        return Ok(());
    }
    run_in_context_realm(engine, context, "cookie change", |cx| {
        let changes = cx.string(serde_json::to_string(&changes).unwrap());
        match host_call_trust_interruptible(cx, "cookieStoreChange", &[changes]) {
            Ok(_) => Ok(()),
            Err(error) => host_task_failure(cx, error, "cookie change"),
        }
    })
}
