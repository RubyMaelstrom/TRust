/// SW92aba3be #service-worker-client-concept / #client-id; HTML e5071a20
/// #current-document-readiness. Materializing an inert Document is neither
/// a navigation nor another load lifecycle, including for opaque origins.
#[tokio::test]
async fn static_frames_resume_native_binding_preserves_opaque_identity_readiness_and_timing() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url =
        url::Url::parse(&format!("http://{}/parent", listener.local_addr().unwrap())).unwrap();
    let requests = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let recorded = requests.clone();
    let server = tokio::spawn(async move {
        for _ in 0..2 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut head = Vec::new();
            while !head.ends_with(b"\r\n\r\n") {
                head.push(socket.read_u8().await.unwrap());
                assert!(head.len() < 16384);
            }
            let head = String::from_utf8(head).unwrap();
            let child = head.starts_with("GET /child ");
            recorded.lock().unwrap().push(head);
            let body = if child {
                "<!doctype html><body>already loaded"
            } else {
                "<!doctype html><body><iframe id=f sandbox src='/child'></iframe>"
            };
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",body.len()).as_bytes()).await.unwrap();
            // A distinct initial blank exists while the HTTP body is pending.
            // The final Document must date its creation after this response,
            // rather than inheriting that earlier blank's loading stamp.
            if child {
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
            socket.write_all(body.as_bytes()).await.unwrap();
        }
    });
    let storage = crate::js::WebStorage::default();
    let response = crate::http::fetch(&crate::http::Request::get(url.clone()))
        .await
        .unwrap();
    let response = crate::http::execute_js(response, (80, 24), (8, 16), storage.clone()).await;
    assert!(response.live.is_none());
    let owner = response.service_client.as_ref().unwrap();
    let original = owner
        .client()
        .registry()
        .unwrap()
        .matching_clients("null", 0, true, None)
        .into_iter()
        .find(|client| client.url().path() == "/child")
        .unwrap();
    let origin = original.origin();
    assert!(!origin.is_tuple());
    let completed = crate::performance::now_ms();
    let crate::http::frames::DeferredPage { html, env } = owner.take_deferred_page().unwrap();
    let root_creation = env.document_creation_time.unwrap();
    let dom = Rc::new(RefCell::new(Dom::parse_document(&html)));
    dom.borrow_mut().set_doc_url(Some(url.clone()));
    let mut state = HostState::new(dom.clone(), Rc::new(RealmClock::new()));
    state.storage = storage;
    state.blobs = env.blobs;
    state.service_workers.prefetched_frames =
        crate::http::frames::ActorFrames::new(env.prefetched_frames, &dom.borrow(), true);
    state.realm_origins.insert(0, owner.client().origin());
    state
        .service_workers
        .document(0, owner, state.blobs.clone(), state.blob_agent);
    let (events, _tasks) = tokio::sync::mpsc::unbounded_channel();
    state.enable_network(
        url.clone(),
        tokio::runtime::Handle::current(),
        env.cache,
        events,
    );
    let mut engine = configured_engine_before_prelude(state, url.as_str());
    eval(
        &mut engine,
        "__trust_cfg.resumedDocument=true;",
        "restore existing Document config",
    )
    .unwrap();
    eval_platform_prelude(&mut engine).unwrap();
    eval(&mut engine,r#"
        globalThis.replayed=0;
        document.getElementById('f').onload=()=>++replayed;
        for(const type of ['load','DOMContentLoaded','readystatechange','pageshow'])addEventListener(type,()=>++replayed);
        __trust.hydrateFrames();__trust.parserFinished();__trust.fireDOMContentLoaded();__trust.completeDocumentLoad();
    "#,"materialize completed frame tree").unwrap();
    let context = {
        let state = engine.ctx().host::<HostState>().unwrap();
        let context = *state.frame_contexts.values().next().unwrap();
        let restored = service_worker_host::client_for_context(state, context).unwrap();
        assert_eq!(restored.id, original.id);
        assert_eq!(restored.origin(), origin);
        assert!(state.service_workers.navigation_documents.is_empty());
        context
    };
    assert_eq!(
        string_value(&mut engine, "document.readyState+'|'+replayed"),
        "complete|0"
    );
    run_in_context_realm(&mut engine, context, "restored child metadata", |engine| {
        assert_eq!(string_value(engine, "document.readyState"), "complete");
        let timing: Vec<f64> = serde_json::from_str(&string_value(
            engine,
            "JSON.stringify([performance.timeOrigin,performance.timing.domLoading,performance.timing.responseStart,performance.timing.responseEnd])",
        ))
        .unwrap();
        assert!(
            timing
                .iter()
                .all(|value| *value > 0. && *value <= completed.floor()),
            "{timing:?}, hydration after {completed}"
        );
        assert!(root_creation.floor() <= timing[2], "root was dated after its child response: {root_creation}, {timing:?}");
        assert!(timing[1] >= timing[3] && timing[3] > timing[2], "final child was dated before its delayed response: {timing:?}");
        Ok(())
    })
    .unwrap();
    assert_eq!(
        requests
            .lock()
            .unwrap()
            .iter()
            .filter(|head| head.starts_with("GET /child "))
            .count(),
        1
    );
    server.await.unwrap();
}

#[tokio::test]
async fn document_clock_restored_srcdoc_data_and_initial_blank_precede_materialization() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let data = crate::img::base64_encode(b"<!doctype html><body>data child");
    let markup = format!(
        "<!doctype html><body><iframe id=blank></iframe><iframe id=srcdoc srcdoc='<p>inline child'></iframe><iframe id=data src='data:text/html;base64,{data}'></iframe><iframe id=no-content src='/no-document'></iframe>"
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url =
        url::Url::parse(&format!("http://{}/parent", listener.local_addr().unwrap())).unwrap();
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorded = requests.clone();
    let server = tokio::spawn(async move {
        for _ in 0..2 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut head = Vec::new();
            while !head.ends_with(b"\r\n\r\n") {
                head.push(socket.read_u8().await.unwrap());
                assert!(head.len() < 16384);
            }
            let head = String::from_utf8(head).unwrap();
            let no_document = head.starts_with("GET /no-document ");
            recorded.lock().unwrap().push(head);
            let (status, body) = if no_document {
                (204, "")
            } else {
                (200, markup.as_str())
            };
            socket.write_all(format!("HTTP/1.1 {status} OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
        }
    });
    let storage = crate::js::WebStorage::default();
    let mut response = crate::http::fetch(&crate::http::Request::get(url.clone()))
        .await
        .unwrap();
    // Optional FetchTiming is not a prerequisite for retaining a Document's
    // own clock; exercise the genuinely absent-metadata root path as well.
    response.timing = None;
    let response = crate::http::execute_js(response, (80, 24), (8, 16), storage.clone()).await;
    assert!(response.live.is_none());
    let owner = response.service_client.as_ref().unwrap();
    let crate::http::frames::DeferredPage { html, env } = owner.take_deferred_page().unwrap();
    let completed = crate::performance::now_ms();
    tokio::time::sleep(Duration::from_millis(20)).await;
    let dom = Rc::new(RefCell::new(Dom::parse_document(&html)));
    dom.borrow_mut().set_doc_url(Some(url.clone()));
    let mut state = HostState::new(dom.clone(), Rc::new(RealmClock::new()));
    state.storage = storage;
    state.blobs = env.blobs;
    state.service_workers.prefetched_frames =
        crate::http::frames::ActorFrames::new(env.prefetched_frames, &dom.borrow(), true);
    state.realm_origins.insert(0, owner.client().origin());
    state
        .service_workers
        .document(0, owner, state.blobs.clone(), state.blob_agent);
    let (events, _tasks) = tokio::sync::mpsc::unbounded_channel();
    state.enable_network(
        url.clone(),
        tokio::runtime::Handle::current(),
        env.cache,
        events,
    );
    let mut engine = configured_engine_before_prelude(state, url.as_str());
    eval(
        &mut engine,
        "__trust_cfg.resumedDocument=true;",
        "existing root",
    )
    .unwrap();
    eval_platform_prelude(&mut engine).unwrap();
    eval(&mut engine, r#"
        globalThis.replayed=0;
        for(const frame of document.querySelectorAll('iframe'))frame.onload=()=>++replayed;
        __trust.hydrateFrames();__trust.parserFinished();__trust.fireDOMContentLoaded();__trust.completeDocumentLoad();
    "#, "materialize local frame Documents").unwrap();
    for name in ["blank", "srcdoc", "data", "no-content"] {
        let context = {
            let state = engine.ctx().host::<HostState>().unwrap();
            let node = dom.borrow().get_by_id(name).unwrap();
            state.frame_contexts[&node]
        };
        run_in_context_realm(&mut engine, context, "original local Document clock", |engine| {
            assert_eq!(string_value(engine, "document.readyState"), "complete");
            let stamps: Vec<f64> = serde_json::from_str(&string_value(engine,
                "JSON.stringify([performance.timeOrigin,performance.timing.navigationStart,performance.timing.domLoading,performance.timing.fetchStart,performance.timing.domainLookupStart,performance.timing.connectStart,performance.timing.domInteractive,performance.timing.domComplete,performance.timing.loadEventEnd,performance.getEntriesByType('navigation').length,performance.now()])"
            )).unwrap();
            assert!(stamps[0] > 0. && stamps[0] <= completed, "{name}: {stamps:?}");
            assert_eq!(stamps[1], stamps[0].floor());
            if matches!(name, "blank" | "no-content") {
                assert_eq!(stamps[2], 0., "initial blank never entered loading");
            } else {
                assert!(stamps[1] <= stamps[2] && stamps[2] <= completed.floor(), "{name}: {stamps:?}");
            }
            assert_eq!(&stamps[3..10], &[0.,0.,0.,0.,0.,0.,0.], "missing phases: {name}");
            assert!(stamps[10] >= 20., "local clock restarted at materialization: {name}: {stamps:?}");
            Ok(())
        }).unwrap();
    }
    assert_eq!(string_value(&mut engine, "String(replayed)"), "0");
    assert_eq!(requests.lock().unwrap().len(), 2);
    server.await.unwrap();
}

#[test]
fn document_clock_live_local_replacement_updates_initial_blank_clock() {
    let data = crate::img::base64_encode(b"<!doctype html><body>local data");
    for source in ["srcdoc", "data"] {
        let mut engine = platform_engine_with_body();
        eval(
            &mut engine,
            r#"
            globalThis.frame=document.createElement('iframe');document.body.append(frame);
            globalThis.initialWindow=frame.contentWindow;
        "#,
            "initial blank clock",
        )
        .unwrap();
        let initial_context = *engine
            .ctx()
            .host::<HostState>()
            .unwrap()
            .frame_contexts
            .values()
            .next()
            .unwrap();
        let mut initial_origin = 0.;
        run_in_context_realm(&mut engine, initial_context, "initial clock", |engine| {
            assert_eq!(
                string_value(engine, "String(performance.timing.domLoading)"),
                "0"
            );
            initial_origin = string_value(engine, "String(performance.timeOrigin)")
                .parse::<f64>()
                .unwrap();
            Ok(())
        })
        .unwrap();
        std::thread::sleep(Duration::from_millis(20));
        let navigate = if source == "srcdoc" {
            "frame.srcdoc='<body>local inline';__trust.hydrateFrames();".into()
        } else {
            format!("frame.src='data:text/html;base64,{data}';__trust.hydrateFrames();")
        };
        eval(&mut engine, &navigate, "replace initial blank locally").unwrap();
        if source == "srcdoc" {
            assert_eq!(
                string_value(&mut engine, "String(frame.contentWindow===initialWindow)"),
                "true"
            );
        }
        let context = *engine
            .ctx()
            .host::<HostState>()
            .unwrap()
            .frame_contexts
            .values()
            .next()
            .unwrap();
        run_in_context_realm(&mut engine, context, "replacement clock", |engine| {
            let stamps: Vec<f64> = serde_json::from_str(&string_value(engine,
                "JSON.stringify([performance.timeOrigin,performance.timing.navigationStart,performance.timing.domLoading,performance.timing.fetchStart,performance.getEntriesByType('navigation').length])"
            )).unwrap();
            assert!(stamps[0] > initial_origin, "{source}: {stamps:?}");
            assert!(stamps[1] <= stamps[2] && stamps[2] > 0., "{source}: {stamps:?}");
            assert_eq!(&stamps[3..], &[0.,0.], "local navigation invented fetch metadata");
            Ok(())
        }).unwrap();
    }
}

#[test]
fn static_frames_resumed_styles_keep_resource_states_and_allow_later_insertions() {
    let mut dom = Dom::parse_document(
        "<link id=ok rel=stylesheet href=https://style-state.test/ok.css><link id=failed rel=stylesheet href=https://style-state.test/failed.css><body>",
    );
    dom.set_doc_url(url::Url::parse("https://style-state.test/page").ok());
    dom.attach_external_sheets(&[crate::http::PageSheet::link(
        "https://style-state.test/ok.css",
        "body{color:red}",
    )]);
    let document = dom.document();
    let mut engine = configured_engine_before_prelude(
        HostState::new(Rc::new(RefCell::new(dom)), Rc::new(RealmClock::new())),
        "https://style-state.test/page",
    );
    eval(
        &mut engine,
        r#"
        __trust_cfg.resumedDocument=true;
        globalThis.newStyleRequests=[];
        globalThis.__dom_load_injected_stylesheet=id=>newStyleRequests.push(id);
    "#,
        "resume stylesheet state config",
    )
    .unwrap();
    eval_platform_prelude(&mut engine).unwrap();
    let resources = host_parsed_style_resources(
        engine.ctx(),
        Value::Undefined,
        &[Value::Num(document as f64)],
    )
    .map_err(|error| describe_throw(&mut engine, error, "restored stylesheet resources"))
    .unwrap();
    let global = engine.ctx().global_this();
    engine
        .ctx()
        .member_set(&global, "nativeStyleResources", resources)
        .map_err(|error| describe_throw(&mut engine, error, "publish stylesheet resources"))
        .unwrap();
    assert_eq!(
        string_value(
            &mut engine,
            "nativeStyleResources.map(entry=>entry[1]).join('|')"
        ),
        "true|false"
    );
    assert_eq!(
        string_value(
            &mut engine,
            r#"
        const late=document.createElement('link');late.rel='stylesheet';late.href='/late.css';document.body.append(late);
        newStyleRequests.length+'|'+(newStyleRequests[0]===late.__id);
    "#
        ),
        "1|true"
    );
}

/// HTML #snapshotting-source-snapshot-params / #create-navigation-params-by-
/// fetching. A Location navigation carries its source settings through the
/// queued frame task, including the top-level settings token zero.
#[test]
fn navigation_source_context_survives_queued_location_navigation_with_zero_and_child_settings() {
    let mut engine = configured_engine_before_prelude(
        HostState::new(
            Rc::new(RefCell::new(Dom::parse_document(
                "<body><iframe id=f referrerpolicy=unsafe-url srcdoc='<body>child'></iframe>",
            ))),
            Rc::new(RealmClock::new()),
        ),
        "https://source-context.test/parent",
    );
    eval(
        &mut engine,
        r#"
        globalThis.navigationSources=[];
        globalThis.__http_navigate=(url,source,policy,frame,context)=>{
            navigationSources.push([context,policy,source]);return [204];
        };
        globalThis.__http_navigate_async=undefined;
    "#,
        "observe native navigation source handoff",
    )
    .unwrap();
    eval_platform_prelude(&mut engine).unwrap();
    eval(
        &mut engine,
        r#"
        __trust.hydrateFrames();
        globalThis.targetWindow=document.getElementById('f').contentWindow;
        targetWindow.location.href='/from-parent';
        __trust.takeNavigationRequest();__trust.hydrateFrames();
    "#,
        "parent-source child navigation",
    )
    .unwrap();
    let context = *engine
        .ctx()
        .host::<HostState>()
        .unwrap()
        .frame_contexts
        .values()
        .next()
        .unwrap();
    assert_ne!(context, 0);
    run_in_context_realm(&mut engine, context, "child-source navigation", |engine| {
        eval(
            engine,
            "location.href='/from-child';",
            "navigate from child settings",
        )
    })
    .unwrap();
    eval(
        &mut engine,
        "__trust.takeNavigationRequest();__trust.hydrateFrames();",
        "queue child-source frame navigation",
    )
    .unwrap();
    let sources: serde_json::Value = serde_json::from_str(&string_value(
        &mut engine,
        "JSON.stringify(navigationSources)",
    ))
    .unwrap();
    assert_eq!(sources.as_array().unwrap().len(), 2, "{sources}");
    assert_eq!(
        sources[0][0], 0,
        "context zero was replaced by the target's settings"
    );
    assert_eq!(
        sources[1][0], context,
        "the child source was replaced by its container's settings"
    );
    assert_eq!(sources[0][1], "");
    assert_eq!(
        sources[1][1], "",
        "Location navigation incorrectly used the iframe attribute policy"
    );
}
