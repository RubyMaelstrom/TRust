// HTML e5071a20 #initialise-the-document-object, SW92aba3be
// #control-and-use-window-client, reduced WPT about-blank-replacement.https.html.

#[test]
fn service_worker_initial_window_keeps_client_id_and_commits_navigation_controller() {
    let runtime = runtime();
    let (listener, base) = listen(&runtime);
    let received = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let records = received.clone();
    let server = runtime.spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let head = read_head(&mut socket).await;
            let (mime, body, extra) = if head.starts_with("get /sw.js ") {
                ("text/javascript", r#"
                    addEventListener('activate', event=>event.waitUntil(clients.claim()));
                    addEventListener('fetch', event=>event.respondWith(new Response('<body>controlled</body>',{headers:{'Content-Type':'text/html'}})));
                "#, "")
            } else if head.starts_with("get /outside/asset ") {
                ("text/plain", "new-document-context", "")
            } else {
                ("text/html", "<body>outside scope</body>", "Referrer-Policy: no-referrer\r\n")
            };
            records.lock().unwrap().push(head);
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nContent-Length: {}\r\n{extra}Connection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
        }
    });
    let (mut engine, mut tasks) = page(&runtime, &format!("{base}/scope/parent"));
    // Give this API-level page the same Document ownership the frontend
    // supplies, so initial child clients exist before their JS containers.
    {
        let state = engine.ctx().host_mut::<HostState>().unwrap();
        let client = service_worker_host::client_for_context(state, 0).unwrap();
        let owner = crate::service_worker::DocumentClient::new(client);
        state
            .service_workers
            .document(0, &owner, state.blobs.clone(), state.blob_agent);
    }
    eval(&mut engine,r#"
        globalThis.setupDone=false;
        (async()=>{
            globalThis.initialRegistration=await navigator.serviceWorker.register('/sw.js',{scope:'/scope/'});
            await navigator.serviceWorker.ready;
            if(!navigator.serviceWorker.controller)await new Promise(resolve=>navigator.serviceWorker.oncontrollerchange=resolve);
            const html=document.createElement('html'),body=document.createElement('body');document.appendChild(html);html.appendChild(body);
            globalThis.initialFrame=document.createElement('iframe');body.append(initialFrame);
            globalThis.initialWindow=initialFrame.contentWindow;initialWindow.retainedMarker='kept';
            globalThis.initialController=initialWindow.navigator.serviceWorker.controller;
            if(!initialController)throw Error('initial blank did not inherit its creator controller');
        })().catch(error=>log.push(error.name+':'+error.message)).finally(()=>setupDone=true);
    "#,"initial service-worker client").unwrap();
    run_until(&runtime, &mut engine, &mut tasks, "setupDone");
    assert_eq!(log(&mut engine), "");
    let (context, client) = {
        let state = engine.ctx().host::<HostState>().unwrap();
        let context = *state.frame_contexts.values().next().unwrap();
        (
            context,
            service_worker_host::client_for_context(state, context).unwrap(),
        )
    };
    let initial_id = client.id.clone();
    let original_controller = client.controller().unwrap().id;
    eval(
        &mut engine,
        r#"
        globalThis.outsideDone=false;
        initialFrame.onload=()=>outsideDone=true;
        initialFrame.src='/outside/first';__trust.hydrateFrames();
    "#,
        "initial replacement outside controller scope",
    )
    .unwrap();
    run_until(&runtime, &mut engine, &mut tasks, "outsideDone");
    assert_eq!(
        string_value(
            &mut engine,
            "[initialFrame.contentWindow===initialWindow,initialWindow.retainedMarker,initialWindow.navigator.serviceWorker.controller===null].join('|')"
        ),
        "true|kept|true"
    );
    assert_eq!(client.id, initial_id);
    assert_eq!(client.url().as_str(), format!("{base}/outside/first"));
    assert!(client.controller().is_none());
    {
        let state = engine.ctx().host::<HostState>().unwrap();
        assert_eq!(
            state.window_request_urls[&context].as_str(),
            format!("{base}/outside/first")
        );
        assert_eq!(
            state.window_cookie_contexts[&context].1.as_str(),
            format!("{base}/outside/first")
        );
        assert_eq!(
            state.referrer_policies[&context],
            crate::referrer_policy::ReferrerPolicy::NoReferrer
        );
        assert!(
            state.service_workers.navigation_documents.is_empty(),
            "the superseded reservation was retained"
        );
        assert_eq!(
            state.early_imports.borrow()[&context].page.as_str(),
            format!("{base}/outside/first")
        );
    }
    eval(
        &mut engine,
        r#"
        globalThis.assetDone=false;globalThis.assetText=null;globalThis.assetFailure=null;
        initialWindow.fetch('./asset',{mode:'same-origin'}).then(response=>response.text())
            .then(text=>{assetText=text;log.push(text)},error=>{assetFailure=error.name;log.push(error.name)})
            .catch(error=>assetFailure=error.name+':'+error.message).finally(()=>assetDone=true);
    "#,
        "reused Window request context",
    )
    .unwrap();
    run_until(&runtime, &mut engine, &mut tasks, "assetDone");
    assert_eq!(log(&mut engine), "new-document-context", "{}", string_value(&mut engine,"JSON.stringify({text:assetText,failure:assetFailure,errors:__trust.errors})"));
    assert!(
        received
            .lock()
            .unwrap()
            .iter()
            .any(|head| head.starts_with("get /outside/asset ") && !head.contains("referer:"))
    );
    eval(&mut engine,r#"
        globalThis.insideDone=false;
        globalThis.secondFrame=document.createElement('iframe');document.body.append(secondFrame);
        globalThis.secondWindow=secondFrame.contentWindow;secondWindow.retainedMarker='second';
        secondFrame.onload=()=>insideDone=true;secondFrame.src='/scope/controlled';__trust.hydrateFrames();
    "#,"initial replacement within controller scope").unwrap();
    let second_client = {
        let state = engine.ctx().host::<HostState>().unwrap();
        let second = *state
            .frame_contexts
            .values()
            .find(|id| **id != context)
            .unwrap();
        service_worker_host::client_for_context(state, second).unwrap()
    };
    let second_id = second_client.id.clone();
    run_until(&runtime, &mut engine, &mut tasks, "insideDone");
    assert_eq!(second_client.id, second_id);
    assert_eq!(second_client.controller().unwrap().id, original_controller);
    assert_eq!(
        second_client.url().as_str(),
        format!("{base}/scope/controlled")
    );
    assert_eq!(
        string_value(
            &mut engine,
            "[secondFrame.contentWindow===secondWindow,secondWindow.retainedMarker,secondWindow.navigator.serviceWorker.controller.scriptURL===initialController.scriptURL].join('|')"
        ),
        "true|second|true"
    );
    eval(&mut engine,"initialFrame.remove();secondFrame.remove();globalThis.unregistered=false;initialRegistration.unregister().then(()=>unregistered=true)","initial Window cleanup").unwrap();
    run_until(&runtime, &mut engine, &mut tasks, "unregistered");
    server.abort();
}
