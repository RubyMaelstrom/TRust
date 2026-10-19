use super::*;

/// HTML #navigation-as-a-download precedes the 204/205 gate, which itself
/// precedes #loading-a-document's unsupported-MIME download fallback. Read
/// these as real HTTP responses so the null-body status handling participates.
#[tokio::test]
async fn no_content_attachment_disposition_precedes_status_and_mime_classification() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let cases: Vec<_> = [200u16, 204, 205]
        .into_iter()
        .flat_map(|status| {
            [
                ("", false),
                ("inline; filename=sample.html", false),
                ("ATTACHMENT; filename=sample.html", true),
                ("extension-disposition", true),
            ]
            .into_iter()
            .flat_map(move |(disposition, attachment)| {
                ["text/html", "application/pdf"]
                    .into_iter()
                    .map(move |mime| (status, disposition, mime, attachment))
            })
        })
        .collect();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let responses = cases.clone();
    let server = tokio::spawn(async move {
        for (status, disposition, mime, _) in responses {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut head = Vec::new();
            while !head.ends_with(b"\r\n\r\n") {
                head.push(stream.read_u8().await.unwrap());
                assert!(head.len() < 16384);
            }
            let body = if status != 200 {
                ""
            } else if mime == "text/html" {
                "<!doctype html><body>document"
            } else {
                "%PDF-1.7"
            };
            let disposition = if disposition.is_empty() {
                String::new()
            } else {
                format!("Content-Disposition: {disposition}\r\n")
            };
            stream.write_all(format!("HTTP/1.1 {status} Response\r\nContent-Type: {mime}\r\n{disposition}Content-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
        }
    });
    let (_environment, receiver) = tokio::sync::watch::channel(WindowEnvironment {
        viewport: CssSize::new(800., 600.),
        device_pixel_ratio: 1.,
        screen_position: (0, 0),
        fullscreen_supported: false,
        traversable: crate::js::TraversableState::SHOWN,
    });
    for (index, (status, disposition, mime, attachment)) in cases.into_iter().enumerate() {
        let target = url::Url::parse(&format!("http://{address}/{index}")).unwrap();
        let fetched = fetch_protocol_interactive(
            &Link::Http(target),
            false,
            None,
            receiver.clone(),
            Default::default(),
            None,
            NavigationIntent::New,
            None,
            "",
        )
        .await
        .unwrap();
        let expected_download = attachment || (status == 200 && mime == "application/pdf");
        match fetched {
            InteractiveFetch::Download(response) => {
                assert!(expected_download, "{status} {disposition:?} {mime}");
                assert_eq!(response.status, status);
                if status != 200 {
                    assert!(response.body.is_empty());
                }
            }
            InteractiveFetch::Document(FetchedDocument::Http(response)) => {
                assert!(!expected_download, "{status} {disposition:?} {mime}");
                assert_eq!(response.status, status);
                if status != 200 {
                    assert!(response.body.is_empty());
                    assert!(
                        response.rendered.is_none()
                            && response.js.is_none()
                            && response.live.is_none(),
                        "a no-content response entered Document creation"
                    );
                }
            }
            _ => panic!("unexpected protocol response for {status} {disposition:?} {mime}"),
        }
    }
    server.await.unwrap();
}

fn report(path: &str) -> serde_json::Value {
    let query = format!("value={}", path.split_once('?').unwrap().1);
    let value = url::form_urlencoded::parse(query.as_bytes())
        .next()
        .unwrap()
        .1;
    serde_json::from_str(&value).unwrap()
}

/// HTML #navigate / #populating-a-session-history-entry /
/// #finalize-a-cross-document-navigation; SW #client-navigate. A response
/// with no new Document preserves the actual Realm, timers and child tree,
/// and its reserved client must stop delaying clients.get().
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_content_worker_navigations_keep_active_documents_until_a_real_commit() {
    let server = crate::http::referer_server::Server::start(|path| {
        let (mime, body): (&str, String) = match path {
            "/sw.js" => ("text/javascript", r#"
                const reservations=new Map();
                addEventListener('activate',event=>event.waitUntil(clients.claim()));
                addEventListener('fetch',event=>{
                    const url=new URL(event.request.url);
                    if(!url.pathname.startsWith('/abort-'))return;
                    const round=Number(url.searchParams.get('round'));
                    reservations.set(round,{resulting:event.resultingClientId,replaces:event.replacesClientId,client:event.clientId});
                    if(url.pathname==='/abort-download')event.respondWith(new Response('%PDF-1.7',{headers:{'Content-Type':'application/pdf','Content-Disposition':'attachment; filename=sample.pdf'}}));
                    else event.respondWith(new Response(null,{status:Number(url.pathname.substring(7)),headers:{'Content-Type':'text/html'}}));
                });
                addEventListener('message',event=>event.waitUntil((async()=>{
                    try{
                        const round=event.data;
                        const path=round<=2?'/scope/child':'/scope/top';
                        const windows=await clients.matchAll({type:'window',includeUncontrolled:true});
                        const before=windows.find(client=>new URL(client.url).pathname===path);
                        if(!before)throw Error('missing active target '+path);
                        const target=round===7?'/scope/replacement':round===6?'/scope/canceled':round===5?'/abort-download':round%2===1?'/abort-204':'/abort-205';
                        const next=await before.navigate(target+'?round='+round);
                        const reservation=reservations.get(round);
                        if(round===7){
                            for(let i=0;i<500&&await clients.get(before.id)!==undefined;i++)await new Promise(resolve=>setTimeout(resolve,10));
                        }
                        const result={round,old:before.id,next:next&&next.id,path:next&&new URL(next.url).pathname,
                            oldGone:await clients.get(before.id)===undefined,reservation};
                        if(reservation)result.reservedGone=await clients.get(reservation.resulting)===undefined;
                        await fetch('/result?'+encodeURIComponent(JSON.stringify(result)));
                        if(round!==7){
                            const parent=(await clients.matchAll({type:'window'})).find(client=>new URL(client.url).pathname==='/scope/top');
                            parent.postMessage({round});
                        }
                    }catch(error){await fetch('/failure?'+encodeURIComponent(error.name+':'+error.message));}
                })()));
            "#.into()),
            "/scope/top" => ("text/html", r#"<!doctype html><body><p id=ticks>0</p><iframe id=child src='/scope/child'></iframe><script>
                const originalDocument=document;
                let ticks=0,round=0,originalChild,unloadChecks=0;
                setInterval(()=>document.getElementById('ticks').textContent=String(++ticks),20);
                addEventListener('unload',()=>fetch('/unloaded',{keepalive:true}));
                addEventListener('beforeunload',event=>{++unloadChecks;if(round===6){event.preventDefault();event.returnValue='stay';}});
                // SW #client-postmessage-options dispatches at the
                // ServiceWorkerContainer, independently of Window messages.
                navigator.serviceWorker.addEventListener('message',event=>{
                    if(!event.data||!event.data.round)return;
                    const containerMessage=event.target===navigator.serviceWorker&&event.currentTarget===navigator.serviceWorker;
                    setTimeout(()=>fetch('/alive?'+encodeURIComponent(JSON.stringify({round:event.data.round,ticks,unloadChecks,
                        containerMessage,
                        documentKept:document===originalDocument,childKept:document.getElementById('child').contentDocument===originalChild,
                        childSentinel:document.getElementById('child').contentWindow.noContentSentinel===123,
                        path:location.pathname}))).catch(()=>{}),30);
                });
                (async()=>{
                    await navigator.serviceWorker.register('/sw.js',{scope:'/'});
                    await navigator.serviceWorker.ready;
                    if(!navigator.serviceWorker.controller)await new Promise(resolve=>navigator.serviceWorker.oncontrollerchange=resolve);
                    while(!document.getElementById('child').contentWindow.noContentSentinel)await new Promise(resolve=>setTimeout(resolve,20));
                    originalChild=document.getElementById('child').contentDocument;
                    addEventListener('keydown',()=>navigator.serviceWorker.controller.postMessage(++round));
                    await fetch('/ready');
                })().catch(error=>fetch('/failure?'+encodeURIComponent(error.message)));
            </script>"#.into()),
            "/scope/child" => ("text/html", "<!doctype html><body>unchanged child<script>window.noContentSentinel=123;</script>".into()),
            path if path.starts_with("/scope/replacement?") => ("text/html", "<!doctype html><body>replacement<script>fetch('/replacement-ready');</script>".into()),
            _ => ("text/plain", "ok".into()),
        };
        (mime.into(), "Cache-Control: no-store\r\n".into(), body.into_bytes())
    }).await;
    let mut browser = BrowserController::new(Handle::current(), || {}, CssSize::new(800., 600.));
    browser.storage = Default::default();
    browser.set_unload_prompts_available(true);
    browser.handle_action(UserAction::Navigate(format!("{}/scope/top", server.base)));
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(150);
    let mut sent = 0;
    let mut root = None;
    let mut child = None;
    let mut active_generation = 0;
    let mut previous_ticks = 0;
    let mut alive_checked = 0;
    let mut canceled_prompt = false;
    loop {
        browser.process_async_events();
        while let Some(prompt) = browser.take_unload_prompt() {
            assert_eq!(sent, 6);
            assert!(!canceled_prompt);
            canceled_prompt = true;
            prompt.answer(false);
        }
        let paths: Vec<_> = server
            .log
            .lock()
            .unwrap()
            .iter()
            .map(|(path, _)| path.clone())
            .collect();
        assert!(
            !paths.iter().any(|path| path.starts_with("/failure?")),
            "{paths:?}"
        );
        if root.is_none() && paths.iter().any(|path| path == "/ready") && browser.current.is_some()
        {
            let FetchedDocument::Http(response) = &browser.current.as_ref().unwrap().document
            else {
                panic!()
            };
            let client = response.service_client.as_ref().unwrap().client();
            let registry = client.registry().unwrap();
            child = registry
                .matching_clients(&client.storage_key(), 0, true, None)
                .into_iter()
                .find(|client| client.url().path() == "/scope/child");
            assert!(child.as_ref().unwrap().controller().is_some());
            active_generation = browser.document_generation;
            root = Some(client);
        }
        let results: Vec<_> = paths
            .iter()
            .filter(|path| path.starts_with("/result?"))
            .map(|path| report(path))
            .collect();
        let alive: Vec<_> = paths
            .iter()
            .filter(|path| path.starts_with("/alive?"))
            .map(|path| report(path))
            .collect();
        while alive_checked < alive.len() {
            let value = &alive[alive_checked];
            assert_eq!(value["documentKept"], true, "{value}");
            assert_eq!(value["containerMessage"], true, "{value}");
            assert_eq!(value["childKept"], true, "{value}");
            assert_eq!(value["childSentinel"], true, "{value}");
            assert_eq!(value["path"], "/scope/top");
            assert_eq!(
                value["unloadChecks"].as_u64().unwrap(),
                value["round"].as_u64().unwrap().saturating_sub(2)
            );
            let ticks = value["ticks"].as_u64().unwrap();
            assert!(
                ticks > previous_ticks,
                "the old actor's timer stopped: {value}"
            );
            previous_ticks = ticks;
            alive_checked += 1;
        }
        for (index, value) in results.iter().enumerate().take(6) {
            let original = if index < 2 {
                child.as_ref().unwrap()
            } else {
                root.as_ref().unwrap()
            };
            assert_eq!(value["old"], original.id);
            assert_eq!(value["next"], original.id, "{value}");
            assert_eq!(value["path"], original.url().path());
            assert_eq!(value["oldGone"], false, "{value}");
            if index == 5 {
                assert!(
                    value["reservation"].is_null(),
                    "a canceled navigation fetched: {value}"
                );
            } else {
                assert_eq!(value["reservedGone"], true, "{value}");
                assert_eq!(value["reservation"]["replaces"], original.id);
                // SW #client-navigate supplies the target's current
                // Document as HTML #navigate's source Document; the fetch
                // event identifies that ESO, not its destination reservation.
                assert_eq!(value["reservation"]["client"], original.id);
                assert_ne!(value["reservation"]["resulting"], original.id);
            }
        }
        if root.is_some()
            && sent < 7
            && (sent == 0 || (results.len() == sent && alive.len() == sent))
        {
            let FetchedDocument::Http(response) = &browser.current.as_ref().unwrap().document
            else {
                panic!()
            };
            assert_eq!(
                response.service_client.as_ref().unwrap().client().id,
                root.as_ref().unwrap().id
            );
            assert_eq!(response.url.path(), "/scope/top");
            assert_eq!(browser.document_generation, active_generation);
            assert!(browser.back.is_empty() && browser.forward.is_empty());
            assert!(
                browser.live_page.is_some(),
                "the active actor was unloaded before commit"
            );
            assert!(!paths.iter().any(|path| path == "/unloaded"), "{paths:?}");
            sent += 1;
            browser.handle_action(UserAction::Key(KeyInput {
                key: Key::Character("n".into()),
                code: "KeyN".into(),
                location: 0,
                state: KeyState::Pressed,
                modifiers: Modifiers::default(),
                repeat: false,
                composing: false,
            }));
        }
        if results.len() == 7 && paths.iter().any(|path| path == "/replacement-ready") {
            let value = results.last().unwrap();
            assert_eq!(value["old"], root.as_ref().unwrap().id);
            assert_ne!(value["next"], value["old"]);
            assert_eq!(value["path"], "/scope/replacement");
            assert_eq!(value["oldGone"], true);
            assert!(root.as_ref().unwrap().info().unwrap().is_none());
            assert!(child.as_ref().unwrap().info().unwrap().is_none());
            assert_eq!(
                browser.current.as_ref().unwrap().address(),
                format!("{}/scope/replacement?round=7", server.base)
            );
            assert_eq!(browser.back.len(), 1);
            assert_ne!(browser.document_generation, active_generation);
            assert!(canceled_prompt);
            assert!(!paths.iter().any(|path| path.starts_with("/scope/canceled")));
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no-content lifecycle did not complete: {paths:?}; {}",
            browser.status
        );
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}
