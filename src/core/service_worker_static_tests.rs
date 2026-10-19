use super::*;

fn report_value(path: &str) -> serde_json::Value {
    let query = format!("value={}", path.split_once('?').unwrap().1);
    let value = url::form_urlencoded::parse(query.as_bytes())
        .next()
        .unwrap()
        .1;
    serde_json::from_str(&value).unwrap()
}

/// HTML #create-navigation-params-by-fetching; Service Workers
/// #on-fetch-request-algorithm / #clients-matchall / #client-navigate.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn static_frames_worker_reservations_and_uncontrolled_window_actions_keep_identity() {
    let server = crate::http::referer_server::Server::start(|path| {
        let (mime, body): (&str, String) = match path {
            "/sw.js" => ("text/javascript", r#"
                addEventListener('activate',event=>event.waitUntil(clients.claim()));
                addEventListener('fetch',event=>{
                    if(new URL(event.request.url).pathname!=='/scope/controlled')return;
                    const ids={resulting:event.resultingClientId,replaces:event.replacesClientId,client:event.clientId};
                    event.respondWith(new Response('<body>controlled '+ids.resulting+'</body>',{headers:{'Content-Type':'text/html'}}));
                    event.waitUntil(fetch('/reservation?'+encodeURIComponent(JSON.stringify(ids))));
                });
                addEventListener('message',event=>event.waitUntil((async()=>{
                    try {
                        const windows=await clients.matchAll({includeUncontrolled:true,type:'window'});
                        const target=windows.find(client=>new URL(client.url).pathname==='/scope/target');
                        const sibling=windows.find(client=>new URL(client.url).pathname==='/outside/sibling');
                        if(!target||!sibling)throw Error('missing inert Window clients');
                        let uncontrolled='BAD';try{await sibling.navigate('/must-not-navigate');}catch(error){uncontrolled=error.name;}
                        const focused=await sibling.focus();
                        const next=await target.navigate('/outside/replacement');
                        const after=await clients.matchAll({includeUncontrolled:true,type:'window'});
                        const result={uncontrolled,old:target.id,next:next.id,path:new URL(next.url).pathname,focused:focused.focused,
                            oldGone:await clients.get(target.id)===undefined,
                            siblingKept:after.some(client=>client.id===sibling.id&&new URL(client.url).pathname==='/outside/sibling')};
                        await fetch('/native-result?'+encodeURIComponent(JSON.stringify(result)));
                    }catch(error){await fetch('/native-failure?'+encodeURIComponent(error.name+':'+error.message));}
                })()));
            "#.into()),
            "/scope/start" => ("text/html", r#"<body>controller<script>
                (async()=>{
                    await navigator.serviceWorker.register('/sw.js',{scope:'/scope/'});
                    await navigator.serviceWorker.ready;
                    if(!navigator.serviceWorker.controller)await new Promise(resolve=>navigator.serviceWorker.oncontrollerchange=resolve);
                    addEventListener('keydown',()=>navigator.serviceWorker.controller.postMessage('native'),{once:true});
                    await fetch('/controller-ready');
                })().catch(error=>fetch('/native-failure?'+encodeURIComponent(error.message)));
            </script>"#.into()),
            "/controlled-parent" => ("text/html", "<body><iframe src='/scope/controlled'></iframe><iframe src='/scope/controlled'></iframe>".into()),
            "/outside/parent" => ("text/html", "<body>inert parent<iframe src='/scope/target'></iframe><iframe src='/outside/sibling'></iframe>".into()),
            "/scope/target" => ("text/html", "<body><link rel=stylesheet href='/cached.css'><p>original target</p>".into()),
            "/outside/sibling" => ("text/html", "<body>untouched sibling".into()),
            "/outside/replacement" => ("text/html", "<body>replacement target".into()),
            "/cached.css" => ("text/css", "p{color:rgb(1,2,3)}".into()),
            _ => ("text/plain", "ok".into()),
        };
        (mime.into(), "Cache-Control: no-store\r\n".into(), body.into_bytes())
    }).await;
    let mut controller = BrowserController::new(Handle::current(), || {}, CssSize::new(800., 600.));
    // An isolated RAM profile makes unrelated tests' clients irrelevant.
    controller.storage = Default::default();
    let mut controlled = BrowserController::new(Handle::current(), || {}, CssSize::new(800., 600.));
    controlled.storage = controller.storage.clone();
    let mut inert = BrowserController::new(Handle::current(), || {}, CssSize::new(800., 600.));
    inert.storage = controller.storage.clone();
    inert.handle_action(UserAction::Navigate(format!(
        "{}/outside/parent",
        server.base
    )));
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(120);
    let mut controlled_started = false;
    let mut controller_started = false;
    let mut triggered = false;
    let mut original_root = String::new();
    let mut original_sibling = String::new();
    let mut original_target = String::new();
    let result = loop {
        controller.process_async_events();
        controlled.process_async_events();
        inert.process_async_events();
        if inert.take_focus_request() {
            inert.handle_action(UserAction::Focus(true));
        }
        let paths: Vec<_> = server
            .log
            .lock()
            .unwrap()
            .iter()
            .map(|(path, _)| path.clone())
            .collect();
        assert!(
            !paths
                .iter()
                .any(|path| path.starts_with("/native-failure?")),
            "{paths:?}"
        );
        if !controller_started && let Some(page) = &inert.current {
            if let FetchedDocument::Http(response) = &page.document
                && response.url.path() == "/outside/parent"
                && response.rendered.is_some()
            {
                assert!(
                    inert.live_page.is_none() && response.live.is_none(),
                    "the inert load started an actor eagerly"
                );
                let owner = response.service_client.as_ref().unwrap();
                original_root = owner.client().id.clone();
                let clients = owner.client().registry().unwrap().matching_clients(
                    &owner.client().storage_key(),
                    0,
                    true,
                    None,
                );
                original_sibling = clients
                    .iter()
                    .find(|client| client.url().path() == "/outside/sibling")
                    .unwrap()
                    .id
                    .clone();
                original_target = clients
                    .iter()
                    .find(|client| client.url().path() == "/scope/target")
                    .unwrap()
                    .id
                    .clone();
                assert!(clients.iter().all(|client| client.controller().is_none()));
                controller_started = true;
                controller
                    .handle_action(UserAction::Navigate(format!("{}/scope/start", server.base)));
            }
        }
        if !controlled_started && paths.iter().any(|path| path == "/controller-ready") {
            controlled_started = true;
            controlled.handle_action(UserAction::Navigate(format!(
                "{}/controlled-parent",
                server.base
            )));
        }
        let reservations: Vec<_> = paths
            .iter()
            .filter(|path| path.starts_with("/reservation?"))
            .map(|path| report_value(path))
            .collect();
        if !triggered && reservations.len() == 2 {
            let ready = controlled
                .current
                .as_ref()
                .and_then(|page| match &page.document {
                    FetchedDocument::Http(response) => response.service_client.as_ref(),
                    _ => None,
                })
                .is_some_and(|owner| {
                    let registry = owner.client().registry().unwrap();
                    let clients =
                        registry.matching_clients(&owner.client().storage_key(), 0, true, None);
                    let children: Vec<_> = clients
                        .iter()
                        .filter(|client| client.url().path() == "/scope/controlled")
                        .collect();
                    children.len() == 2 && children.iter().all(|child| owner.contains(&child.id))
                });
            if ready {
                assert!(
                    controlled.live_page.is_some(),
                    "controlled scriptless children lost their task endpoints"
                );
                assert_ne!(reservations[0]["resulting"], reservations[1]["resulting"]);
                assert_ne!(reservations[0]["replaces"], reservations[1]["replaces"]);
                let FetchedDocument::Http(parent) = &controlled.current.as_ref().unwrap().document
                else {
                    panic!()
                };
                let source_id = &parent.service_client.as_ref().unwrap().client().id;
                // SW #create-fetch-event-and-dispatch-algorithm uses the
                // initiating source Document, including for iframe loads.
                assert!(
                    reservations
                        .iter()
                        .all(|value| value["client"] == *source_id
                            && value["resulting"] != ""
                            && value["replaces"] != "")
                );
                assert!(
                    inert.live_page.is_none(),
                    "claim alone should not eagerly materialize an inert page"
                );
                triggered = true;
                controller.handle_action(UserAction::Key(KeyInput {
                    key: Key::Character("n".into()),
                    code: "KeyN".into(),
                    location: 0,
                    state: KeyState::Pressed,
                    modifiers: Modifiers::default(),
                    repeat: false,
                    composing: false,
                }));
            }
        }
        if let Some(path) = paths
            .iter()
            .find(|path| path.starts_with("/native-result?"))
        {
            break report_value(path);
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "static native action did not finish: {paths:?}; {}, {}, {}",
            controller.status,
            controlled.status,
            inert.status
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    };
    assert_eq!(result["uncontrolled"], "TypeError");
    assert_eq!(result["old"], original_target);
    assert_eq!(result["focused"], true);
    assert_eq!(result["oldGone"], true);
    assert_eq!(result["siblingKept"], true);
    assert_eq!(result["path"], "/outside/replacement");
    assert_ne!(result["old"], result["next"]);
    let FetchedDocument::Http(response) = &inert.current.as_ref().unwrap().document else {
        panic!()
    };
    let owner = response.service_client.as_ref().unwrap();
    assert_eq!(owner.client().id, original_root);
    assert!(owner.contains(&original_sibling));
    assert!(inert.live_page.is_some());
    let paths = server.log.lock().unwrap();
    assert!(!paths.iter().any(|(path, _)| path == "/must-not-navigate"));
    for path in [
        "/outside/parent",
        "/scope/target",
        "/outside/sibling",
        "/cached.css",
        "/outside/replacement",
    ] {
        assert_eq!(
            paths
                .iter()
                .filter(|(requested, _)| requested == path)
                .count(),
            1,
            "unexpected refetch of {path}: {paths:?}"
        );
    }
}
