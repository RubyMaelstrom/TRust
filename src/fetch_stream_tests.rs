//! Response bodies read as they arrive, against local servers that write
//! their bodies in delayed pieces: Fetch #concept-http-network-fetch for
//! fetch(), XMLHttpRequest's processBodyChunk, and HTML server-sent events
//! (#server-sent-events) with reconnection; and the network side of
//! responses (status message, header list, CORS preflight), of stopping a
//! document (HTML #abort-a-document) and of module graph fetching.

use super::tests::{configured_engine, run_microtask_checkpoint, string_value};
use super::*;
use std::time::Duration;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};

include!("service_worker_initial_tests.rs");

#[test]
fn service_worker_response_filtering_redirects_head_and_range_provenance() {
    let runtime = runtime();
    let (listener, base) = listen(&runtime);
    let (remote, remote_base) = listen(&runtime);
    let remote_url = remote_base.clone();
    let remote_server=runtime.spawn(async move {
        loop {
            let(mut socket,_)=remote.accept().await.unwrap();let head=read_head(&mut socket).await;
            let status=if head.starts_with("get /range-source "){
                assert!(head.contains("range: bytes=0-2\r\n"),"native range was lost: {head}");206
            }else{200};
            socket.write_all(format!("HTTP/1.1 {status} OK\r\nContent-Type: text/plain\r\nContent-Length: 3\r\nAccess-Control-Allow-Origin: *\r\nConnection: close\r\n\r\nabc").as_bytes()).await.unwrap();
        }
    });
    let server=runtime.spawn(async move {
        loop {
            let(mut socket,_)=listener.accept().await.unwrap();let head=read_head(&mut socket).await;
            assert!(head.starts_with("get /sw.js "),"unexpected fallback {head}");
            let source=r#"
                let ranged;
                addEventListener('activate',e=>e.waitUntil(clients.claim()));
                addEventListener('fetch',e=>{
                    const path=new URL(e.request.url).pathname;
                    if(path==='/cors')e.respondWith(fetch('REMOTE/cors'));
                    else if(path==='/opaque')e.respondWith(fetch('REMOTE/opaque',{mode:'no-cors'}));
                    else if(path==='/range-source')e.respondWith(fetch(e.request).then(r=>{ranged=r.clone();return r;}));
                    else if(path==='/range')e.respondWith(ranged.clone());
                    else if(path==='/redirect')e.respondWith(Response.redirect('/target'));
                    else if(path==='/options'){
                        let immutable=false;try{e.request.headers.set('x-worker','bad')}catch(error){immutable=error.name==='TypeError';}
                        e.respondWith(new Response([e.request.cache,e.request.keepalive,e.request.destination,immutable].join('|')));
                    } else e.respondWith(new Response('result'));
                });
            "#.replace("REMOTE",&remote_base);
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/javascript\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{source}",source.len()).as_bytes()).await.unwrap();
        }
    });
    let (mut engine, mut tasks) = page(&runtime, &format!("{base}/page"));
    eval(&mut engine,r#"
        globalThis.done=false;
        (async()=>{
            globalThis.reg=await navigator.serviceWorker.register('/sw.js');await navigator.serviceWorker.ready;
            if(!navigator.serviceWorker.controller)await new Promise(r=>navigator.serviceWorker.oncontrollerchange=r);
        })().finally(()=>done=true);
    "#,"worker range setup").unwrap();
    run_until(&runtime, &mut engine, &mut tasks, "done");
    // Range is a privileged no-CORS header. Author Headers mutations remove
    // it; start with an actual native media request, then replay its response.
    let context =
        service_worker_host::fetch::for_context(engine.ctx().host::<HostState>().unwrap(), 0);
    for target in [
        format!("{remote_url}/range-source"),
        format!("{base}/range"),
    ] {
        let mut request = crate::http::Request::subresource(
            url::Url::parse(&target).unwrap(),
            &url::Url::parse(&base).unwrap(),
            "video",
            None,
        );
        request.headers.push(("range".into(), "bytes=0-2".into()));
        request.service_worker = context.clone();
        let mut response = runtime.block_on(crate::http::fetch(&request)).unwrap();
        assert!(
            response
                .internal
                .as_ref()
                .is_some_and(|r| r.range_requested)
        );
        response.unfilter();
        assert_eq!(response.body, b"abc");
    }
    eval(&mut engine,r#"
        globalThis.done=false;
        (async()=>{
            log.push(await(await fetch('/cors')).text());
            for(const [url,options] of [['/cors',{mode:'same-origin'}],['/opaque',{}],['/range',{mode:'no-cors'}],['/redirect',{redirect:'error'}]]){
                try{await fetch(url,options);log.push('BAD')}catch(error){log.push(error.name)}
            }
            const opaque=await fetch('/opaque',{mode:'no-cors'});log.push(opaque.type,opaque.body===null);
            log.push((await fetch('/redirect',{redirect:'manual'})).type);
            log.push(await(await fetch('/redirect')).text());
            const head=await fetch('/head',{method:'HEAD'});log.push(head.body===null,await head.text()==='');
            log.push(await(await fetch('/options',{cache:'reload',keepalive:true})).text());
            await reg.unregister();
        })().catch(e=>log.push('ERROR:'+e.name+':'+e.message)).finally(()=>done=true);
    "#,"worker response filters").unwrap();
    run_until(&runtime, &mut engine, &mut tasks, "done");
    assert_eq!(
        log(&mut engine),
        "abc|TypeError|TypeError|TypeError|TypeError|opaque|true|opaqueredirect|result|true|true|reload|true||true"
    );
    server.abort();
    remote_server.abort();
}

#[test]
fn service_worker_controls_worker_scripts_and_their_subresources() {
    let runtime = runtime();
    let (listener, base) = listen(&runtime);
    let server=runtime.spawn(async move {
        loop {
            let(mut socket,_)=listener.accept().await.unwrap();let head=read_head(&mut socket).await;
            let source=if head.starts_with("get /actual/child.js?") {
                r#"fetch('inner').then(r=>r.text()).then(v=>postMessage(v+'|'+new URL(location.href).searchParams.get('id')+'|'+location.pathname));"#
            }else {
            assert!(head.starts_with("get /sw.js "),"intercepted resource reached network: {head}");
            r#"
                addEventListener('activate',e=>e.waitUntil(clients.claim()));
                addEventListener('fetch',e=>{
                    if(new URL(e.request.url).pathname==='/child.js'){
                        e.respondWith(fetch('/actual/child.js?id='+e.resultingClientId));
                    }else e.respondWith(clients.get(e.clientId).then(c=>new Response(c.type+'|'+c.id+'|'+e.resultingClientId+'|'+new URL(e.request.url).pathname)));
                });
            "#};
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/javascript\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{source}",source.len()).as_bytes()).await.unwrap();
        }
    });
    let (mut engine, mut tasks) = page(&runtime, &format!("{base}/page"));
    eval(&mut engine,r#"
        globalThis.done=false;
        (async()=>{
            const reg=await navigator.serviceWorker.register('/sw.js');await navigator.serviceWorker.ready;
            if(!navigator.serviceWorker.controller)await new Promise(r=>navigator.serviceWorker.oncontrollerchange=r);
            const worker=new Worker('/child.js');
            const result=await new Promise((resolve,reject)=>{worker.onmessage=e=>resolve(e.data);worker.onerror=e=>reject(Error(e.message));});
            const fields=result.split('|');log.push(fields[0],fields[1]===fields[4],fields[2],fields[3],fields[5]);
            worker.terminate();await reg.unregister();
        })().catch(e=>log.push('ERROR:'+e.name+':'+e.message)).finally(()=>done=true);
    "#,"controlled worker").unwrap();
    run_until(&runtime, &mut engine, &mut tasks, "done");
    assert_eq!(
        log(&mut engine),
        "worker|true||/actual/inner|/actual/child.js"
    );
    server.abort();
}

#[test]
fn service_worker_response_cancel_aborts_request_and_releases_the_source_reader() {
    let runtime = runtime();
    let (listener, base) = listen(&runtime);
    let server=runtime.spawn(async move {
        let(mut socket,_)=listener.accept().await.unwrap();let _=read_head(&mut socket).await;
        let source=r#"
            let bytes,source,aborted=false,canceled=false;
            addEventListener('activate',e=>e.waitUntil(clients.claim()));
            addEventListener('fetch',e=>{
                if(new URL(e.request.url).pathname==='/state'){
                    e.respondWith(new Response(aborted+':'+canceled+':'+bytes.buffer.byteLength+':'+source.locked));return;
                }
                let finish;
                e.waitUntil(new Promise(resolve=>finish=resolve));
                function check(){if(aborted&&canceled)clients.get(e.clientId).then(c=>{c.postMessage('canceled');finish();});}
                e.request.signal.addEventListener('abort',()=>{aborted=true;check();});
                bytes=new Uint8Array([1,2,3]);
                source=new ReadableStream({start(c){c.enqueue(bytes);},cancel(){canceled=true;check();}});
                e.respondWith(new Response(source));
            });
            // The response metadata bridge must not consult author-installed
            // collection hooks when serializing its private header records.
            Array.prototype.toJSON=()=>{throw new Error('unexpected collection hook');};
        "#;
        socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/javascript\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{source}",source.len()).as_bytes()).await.unwrap();
    });
    let (mut engine, mut tasks) = page(&runtime, &format!("{base}/page"));
    eval(&mut engine,r#"
        globalThis.done=false;
        (async()=>{
            const reg=await navigator.serviceWorker.register('/sw.js');await navigator.serviceWorker.ready;
            if(!navigator.serviceWorker.controller)await new Promise(r=>navigator.serviceWorker.oncontrollerchange=r);
            const canceled=new Promise(r=>navigator.serviceWorker.onmessage=e=>r(e.data));
            const controller=new AbortController();
            const response=await fetch('/stream',{signal:controller.signal});const reader=response.body.getReader();
            log.push(Array.from((await reader.read()).value).join(','));
            controller.abort();
            try{await reader.read();log.push('BAD');}catch(e){log.push(e.name);}
            log.push(await canceled);log.push(await(await fetch('/state')).text());await reg.unregister();
        })().catch(e=>log.push('ERROR:'+e.name+':'+e.message)).finally(()=>done=true);
    "#,"service worker cancellation").unwrap();
    run_until(&runtime, &mut engine, &mut tasks, "done");
    assert_eq!(
        log(&mut engine),
        "1,2,3|AbortError|canceled|true:true:0:false"
    );
    runtime.block_on(server).unwrap();
}

#[test]
fn service_worker_module_graph_rejects_await_before_execution_and_runs_saved_imports() {
    let runtime = runtime();
    let (listener, base) = listen(&runtime);
    let server=runtime.spawn(async move {
        loop {
            let(mut socket,_)=listener.accept().await.unwrap();let head=read_head(&mut socket).await;
            let source=if head.starts_with("get /dep.js "){"export const word='module'; async function nested(){await 1;}"}
            else if head.starts_with("get /async.js "){"class C { [await 1]() {} } export const value=1;"}
            else if head.starts_with("get /bad.js "){r#"import './async.js'; cookieStore.set({name:'module-ran',value:'bad'});"#}
            else {r#"import {word} from './dep.js'; import extra from 'data:application/json,%7B%22suffix%22%3A%22-local%22%7D' with {type:'json'}; addEventListener('activate',e=>e.waitUntil(clients.claim())); addEventListener('fetch',e=>e.respondWith(new Response(word+extra.suffix)));"#};
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/javascript\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{source}",source.len()).as_bytes()).await.unwrap();
        }
    });
    let (mut engine, mut tasks) = page(&runtime, &format!("{base}/page"));
    eval(&mut engine,r#"
        globalThis.done=false;
        (async()=>{
            try{await navigator.serviceWorker.register('/bad.js',{type:'module'});log.push('BAD');}catch(e){log.push(e.name);}
            log.push(await cookieStore.get('module-ran')===null);
            const reg=await navigator.serviceWorker.register('/sw.js',{type:'module'});
            await navigator.serviceWorker.ready;
            if(!navigator.serviceWorker.controller)await new Promise(r=>navigator.serviceWorker.oncontrollerchange=r);
            log.push(await(await fetch('/result')).text());
            await reg.unregister();
        })().catch(e=>log.push('ERROR:'+e.name+':'+e.message)).finally(()=>done=true);
    "#,"module service worker").unwrap();
    run_until(&runtime, &mut engine, &mut tasks, "done");
    assert_eq!(log(&mut engine), "TypeError|true|module-local");
    server.abort();
}

#[test]
fn service_worker_navigation_preload_keeps_reserved_client_and_streams_response() {
    // SW92aba3be #handle-fetch clones navigation metadata but exposes the
    // preload Response before HTML #navigation-as-a-download. The worker can
    // read/replace an octet-stream or attachment just as it can HTML.
    let runtime = runtime();
    let (listener, base) = listen(&runtime);
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = requests.clone();
    let server = runtime.spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let head = read_head(&mut socket).await;
            seen.lock().unwrap().push(head.clone());
            let (mime, source) = if head.starts_with("get /sw.js ") {
                ("text/javascript", r#"
                addEventListener('activate',e=>e.waitUntil((async()=>{
                    await registration.navigationPreload.enable();
                    await registration.navigationPreload.setHeaderValue('  preload-test\t');
                })()));
                addEventListener('fetch',e=>e.respondWith((async()=>{
                    let r=await e.preloadResponse;
                    const preload=await r.text();
                    if(new URL(e.request.url).pathname==='/forwarded'){
                        r=await fetch(e.request);
                    }
                    const text=new URL(e.request.url).pathname==='/forwarded'?await r.text():preload;
                    return new Response(e.request.mode+'|'+e.request.destination+'|'+e.resultingClientId+'|'+r.headers.get('content-type')+'|'+r.headers.get('content-disposition')+'|'+text,{headers:{'content-type':'text/html'}});
                })()));
                "#)
            } else if head.starts_with("get /next ") {
                ("text/html", "preloaded\n")
            } else {
                ("application/octet-stream", "preloaded\n")
            };
            let attachment = head.starts_with("get /attachment ")
                || head.starts_with("get /forwarded ");
            let disposition = if attachment {
                "Content-Disposition: attachment; filename=test.bin\r\n"
            } else {
                ""
            };
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\n{disposition}Content-Length: {}\r\nConnection: close\r\n\r\n{source}",source.len()).as_bytes()).await.unwrap();
        }
    });
    let (mut engine, mut tasks) = page(&runtime, &format!("{base}/page"));
    eval(&mut engine,r#"
        globalThis.done=false;
        (async()=>{
            const reg=await navigator.serviceWorker.register('/sw.js');await navigator.serviceWorker.ready;
            if(reg.active.state!=='activated')await new Promise(r=>reg.active.addEventListener('statechange',()=>{if(reg.active.state==='activated')r();}));
            const state=await reg.navigationPreload.getState();log.push(state.enabled,state.headerValue,reg.navigationPreload===reg.navigationPreload);
        })().catch(e=>log.push('ERROR:'+e.name+':'+e.message)).finally(()=>done=true);
    "#,"preload manager").unwrap();
    run_until(&runtime, &mut engine, &mut tasks, "done");
    assert_eq!(log(&mut engine), "true|preload-test|true");
    let storage = engine.ctx().host::<HostState>().unwrap().storage.clone();
    for (path, mime, disposition) in [
        ("next", "text/html", "null"),
        ("binary", "application/octet-stream", "null"),
        (
            "attachment",
            "application/octet-stream",
            "attachment; filename=test.bin",
        ),
        (
            "forwarded",
            "application/octet-stream",
            "attachment; filename=test.bin",
        ),
    ] {
        let response = runtime
            .block_on(crate::http::fetch_navigation(
                &url::Url::parse(&format!("{base}/{path}")).unwrap(),
                None,
                None,
                &storage,
                false,
            ))
            .unwrap();
        let id = response
            .service_client
            .as_ref()
            .unwrap()
            .client()
            .id
            .clone();
        assert_eq!(
            String::from_utf8(response.body).unwrap(),
            format!("navigate|document|{id}|{mime}|{disposition}|preloaded\n"),
            "worker's {path} navigation response",
        );
        let requests = requests.lock().unwrap();
        assert!(requests.iter().any(|h| {
            h.starts_with(&format!("get /{path} "))
                && h.contains("service-worker-navigation-preload: preload-test\r\n")
                && h.contains("sec-fetch-mode: navigate\r\n")
                && h.contains("sec-fetch-dest: document\r\n")
        }));
    }
    runtime.block_on(async {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while requests
            .lock()
            .unwrap()
            .iter()
            .filter(|h| h.starts_with("get /sw.js "))
            .count()
            < 2
        {
            assert!(
                tokio::time::Instant::now() < deadline,
                "navigation did not schedule Soft Update"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    });
    server.abort();
}

#[test]
fn service_worker_fetch_streams_falls_back_and_rejects_invalid_responses() {
    // SW 92aba3be Handle Fetch/respondWith, Fetch 394d20d HTTP fetch.
    let runtime = runtime();
    let (listener, base) = listen(&runtime);
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = requests.clone();
    let server = runtime.spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let head = read_head(&mut socket).await;
            seen.lock().unwrap().push(head.clone());
            let body = if head.starts_with("get /sw.js ") { r#"
                addEventListener('activate',e=>e.waitUntil(clients.claim()));
                addEventListener('fetch',e=>{
                    const path=new URL(e.request.url).pathname;
                    if(path==='/fallback')return;
                    if(path==='/cancel'){e.preventDefault();return;}
                    if(path==='/bad'){e.respondWith({});return;}
                    if(path==='/error'){e.respondWith(Response.error());return;}
                    if(path==='/used'){
                        const r=new Response('used');r.text();e.respondWith(r);return;
                    }
                    if(path==='/echo'){
                        e.respondWith(e.request.text().then(body=>new Response(body+':'+e.request.method)));return;
                    }
                    if(path==='/forward'){e.respondWith(fetch('/fallback'));return;}
                    if(path==='/stream'){
                        let n=0;
                        e.respondWith(new Response(new ReadableStream({pull(c){
                            if(n++===0)c.enqueue(new TextEncoder().encode('first'));
                            else {c.enqueue(new TextEncoder().encode('second'));c.close();}
                        }}),{headers:{'x-worker':'yes'}}));return;
                    }
                    e.respondWith(new Response([e.request.mode,e.request.destination,e.clientId.length>0,
                        e.resultingClientId,e.isTrusted,e.cancelable].join('|')));
                });
            "# } else { "network" };
            let mime=if head.starts_with("get /sw.js "){"text/javascript"}else{"text/plain"};
            let response=format!("HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len());
            socket.write_all(response.as_bytes()).await.unwrap();
        }
    });
    let (mut engine, mut tasks) = page(&runtime, &format!("{base}/page"));
    eval(&mut engine,r#"
        globalThis.done=false;
        (async()=>{
            const reg=await navigator.serviceWorker.register('/sw.js');
            await navigator.serviceWorker.ready;
            if(!navigator.serviceWorker.controller)await new Promise(r=>navigator.serviceWorker.oncontrollerchange=r);
            log.push(await(await fetch('/inspect')).text());
            const stream=await fetch('/stream');log.push(stream.headers.get('x-worker'));
            log.push(await stream.text());
            log.push(await(await fetch('/echo',{method:'POST',body:'hello'})).text());
            log.push(await(await fetch('/fallback')).text());
            log.push(await(await fetch('/forward')).text());
            for(const path of ['/cancel','/bad','/error','/used']){
                try{await fetch(path);log.push('BAD:'+path);}catch(e){log.push(e.name);}
            }
            await reg.unregister();
        })().catch(e=>log.push('ERROR:'+e.name+':'+e.message)).finally(()=>done=true);
    "#,"service worker fetch").unwrap();
    run_until(&runtime, &mut engine, &mut tasks, "done");
    assert_eq!(
        log(&mut engine),
        "cors||true||true|true|yes|firstsecond|hello:POST|network|network|TypeError|TypeError|TypeError|TypeError"
    );
    let heads = requests.lock().unwrap();
    assert_eq!(
        heads.len(),
        3,
        "Only script, fallback, and worker's own fetch reach the network: {heads:?}"
    );
    server.abort();
}

#[test]
fn service_worker_registration_runs_install_and_activate_with_ram_cache() {
    let runtime = runtime();
    let (listener, base) = listen(&runtime);
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = requests.clone();
    let server = runtime.spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let head = read_head(&mut socket).await;
            seen.lock().unwrap().push(head.clone());
            let source = if head.starts_with("get /import.js ") {
                "self.importValue = 'imported';"
            } else if head.starts_with("get /broken.js ") {
                "throw new Error('startup rejected');"
            } else {
                r#"
                importScripts('/import.js');
                self.addEventListener('install', event => {
                    event.waitUntil(caches.open('sw-lifecycle').then(cache => cache.put('/version',
                        new Response(importValue + ':' + (self instanceof ServiceWorkerGlobalScope)))));
                    event.waitUntil(self.skipWaiting());
                });
                self.addEventListener('activate', event => {
                    event.waitUntil(cookieStore.set({name:'sw-activated',value:'yes',path:'/'}));
                });
                "#
            };
            let response = format!("HTTP/1.1 200 OK\r\nContent-Type: text/javascript\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{source}", source.len());
            socket.write_all(response.as_bytes()).await.unwrap();
        }
    });
    let (mut engine, mut tasks) = page(&runtime, &format!("{base}/page"));
    eval(&mut engine, r#"
        globalThis.swDone = false;
        (async () => {
            const registration = await navigator.serviceWorker.register('/sw.js');
            log.push('registered:' + registration.installing.state);
            registration.onupdatefound = () => log.push('updatefound');
            const worker = registration.installing;
            worker.onstatechange = () => log.push(worker.state);
            const ready = await navigator.serviceWorker.ready;
            log.push('same:' + (ready === registration));
            if (worker.state !== 'activated') await new Promise(resolve => {
                worker.addEventListener('statechange', () => {if (worker.state === 'activated') resolve();});
            });
            log.push('controller:' + navigator.serviceWorker.controller);
            log.push('cache:' + await (await caches.match('/version')).text());
            log.push('cookie:' + (await cookieStore.get('sw-activated')).value);
            log.push('lookup:' + (await navigator.serviceWorker.getRegistration() === registration));
            log.push('same-register:' + (await navigator.serviceWorker.register('/sw.js') === registration));
            log.push('count:' + (await navigator.serviceWorker.getRegistrations()).length);
            log.push('removed:' + await registration.unregister());
            log.push('count-after:' + (await navigator.serviceWorker.getRegistrations()).length);
            try {await navigator.serviceWorker.register('/broken.js');log.push('bad-startup-accepted');}
            catch(error) {log.push('startup:' + error.name);}
        })().catch(error => log.push('ERROR:' + error.name + ':' + error.message)).finally(() => swDone = true);
    "#, "service worker lifecycle").unwrap();
    run_until(&runtime, &mut engine, &mut tasks, "swDone");
    let actual = log(&mut engine);
    assert_eq!(
        actual,
        "registered:installing|updatefound|installed|activating|same:true|activated|controller:null|cache:imported:true|cookie:yes|lookup:true|same-register:true|count:1|removed:true|redundant|count-after:0|startup:TypeError"
    );
    let heads = requests.lock().unwrap();
    assert_eq!(
        heads
            .iter()
            .filter(|head| head.starts_with("get /sw.js "))
            .count(),
        1,
        "same registration does not refetch"
    );
    assert!(
        heads[0]
            .to_ascii_lowercase()
            .contains("service-worker: script")
    );
    server.abort();
}

#[test]
fn service_worker_clients_query_claim_and_keep_document_snapshots() {
    let runtime = runtime();
    let (listener, base) = listen(&runtime);
    let (mut engine, mut tasks) = page(&runtime, &format!("{base}/register"));
    let storage = engine.ctx().host::<HostState>().unwrap().storage.clone();
    let blobs = crate::js::BlobMap::default();
    let static_client = prepare_service_worker_document(
        &storage,
        &blobs,
        runtime.handle().clone(),
        &url::Url::parse(&format!("{base}/static")).unwrap(),
        crate::js::TraversableState::SHOWN,
    )
    .unwrap();
    let insecure = prepare_service_worker_document(
        &storage,
        &blobs,
        runtime.handle().clone(),
        &url::Url::parse(&format!("{base}/insecure-ancestor")).unwrap(),
        crate::js::TraversableState::SHOWN,
    )
    .unwrap();
    insecure.client().set_secure_context(false);
    let isolated_storage = crate::js::WebStorage::default();
    let isolated_blobs = crate::js::BlobMap::default();
    let isolated = prepare_service_worker_document(
        &isolated_storage,
        &isolated_blobs,
        runtime.handle().clone(),
        &url::Url::parse(&format!("{base}/other-profile")).unwrap(),
        crate::js::TraversableState::SHOWN,
    )
    .unwrap();
    let source = format!(
        r#"
        self.addEventListener('activate', event=>event.waitUntil((async()=>{{
            const all=await clients.matchAll({{includeUncontrolled:true,type:'all'}});
            const log=[clients===self.clients,all.length,Object.isFrozen(all),all.every(c=>c instanceof WindowClient&&c instanceof Client&&c.type==='window'&&c.frameType==='top-level')];
            const again=await clients.get(all[0].id);
            log.push(again!==all[0],again.id===all[0].id,again.ancestorOrigins===again.ancestorOrigins,Object.isFrozen(again.ancestorOrigins));
            log.push((await clients.matchAll()).length,(await clients.matchAll({{includeUncontrolled:true,type:'worker'}})).length,await clients.get('absent')===undefined);
            try{{await clients.get({insecure});log.push('insecure allowed');}}catch(error){{log.push(error.name);}}
            await clients.claim();
            log.push((await clients.matchAll()).length);
            const cache=await caches.open('client-queries');await cache.put('/result',new Response(log.join('|')));
        }})()));
    "#,
        insecure = serde_json::to_string(&insecure.client().id).unwrap()
    );
    let server = runtime.spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let _ = read_head(&mut socket).await;
        let response=format!("HTTP/1.1 200 OK\r\nContent-Type: text/javascript\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{source}",source.len());
        socket.write_all(response.as_bytes()).await.unwrap();
    });
    eval(&mut engine,r#"
        globalThis.clientsDone=false;
        (async()=>{
            const registration=await navigator.serviceWorker.register('/clients.js');
            await navigator.serviceWorker.ready;
            const worker=registration.active;
            if(worker.state!=='activated')await new Promise(resolve=>worker.addEventListener('statechange',()=>{if(worker.state==='activated')resolve();}));
            log.push(await(await caches.match('/result')).text());
            log.push(navigator.serviceWorker.controller===worker);
            await registration.unregister();
        })().catch(error=>log.push('ERROR:'+error.name+':'+error.message)).finally(()=>clientsDone=true);
    "#,"service-worker Clients").unwrap();
    run_until(&runtime, &mut engine, &mut tasks, "clientsDone");
    assert_eq!(
        log(&mut engine),
        "true|2|true|true|true|true|true|true|0|0|true|SecurityError|2|true"
    );
    assert!(static_client.client().controller().is_some());
    assert!(insecure.client().controller().is_none());
    assert!(isolated.client().controller().is_none());
    runtime.block_on(server).unwrap();
}

#[test]
fn service_worker_messages_round_trip_through_clients_and_ports() {
    // SW 92aba3be ServiceWorker/postMessage, Client/postMessage and the
    // initially-disabled client message queue; HTML structured
    // serialize/deserialize with transfer in both directions.
    let runtime = runtime();
    let (listener, base) = listen(&runtime);
    let server = runtime.spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let head = read_head(&mut socket).await;
            let source = if head.starts_with("get /nomsg/sw.js ") {
                r#"
                self.addEventListener('install', event => event.waitUntil(self.skipWaiting()));
                self.addEventListener('activate', event => event.waitUntil(clients.claim()));
                "#
            } else {
                r#"
                self.addEventListener('install', event => event.waitUntil(self.skipWaiting()));
                self.addEventListener('activate', event => event.waitUntil(clients.claim()));
                self.addEventListener('message', event => {
                    event.waitUntil((async () => {
                        const cache = await caches.open('sw-messages');
                        await cache.put('/seen', new Response([
                            event.data.value, event.origin === self.location.origin,
                            event.source && event.source.type, event.source && event.source.frameType,
                            event instanceof ExtendableMessageEvent].join(':')));
                        if (event.ports[0]) event.ports[0].postMessage('port:' + event.data.value);
                        event.source.postMessage({reply: event.data.value + 1});
                        await cache.put('/replied', new Response('yes'));
                    })());
                });
                "#
            };
            let response = format!("HTTP/1.1 200 OK\r\nContent-Type: text/javascript\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{source}", source.len());
            socket.write_all(response.as_bytes()).await.unwrap();
        }
    });
    let (mut engine, mut tasks) = page(&runtime, &format!("{base}/page"));
    eval(&mut engine, r#"
        globalThis.msgDone = false;
        (async () => {
            const registration = await navigator.serviceWorker.register('/msg.js');
            const installing = registration.installing;
            installing.postMessage({value:1});
            await navigator.serviceWorker.ready;
            const worker = registration.active;
            if (worker.state !== 'activated') await new Promise(resolve =>
                worker.addEventListener('statechange', () => {if (worker.state === 'activated') resolve();}));
            log.push('script:' + worker.scriptURL.endsWith('/msg.js'));
            log.push('installing-same:' + (installing === worker));
            let seen = '';
            for (let i = 0; i < 200 && !seen.startsWith('1:'); i++) {
                const hit = await caches.match('/seen'); if (hit) seen = await hit.text();
            }
            log.push('seen:' + seen);
            const received = [];
            let receivedWaiters = [];
            navigator.serviceWorker.addEventListener('message', event => {
                received.push([event.data.reply, event.source === worker,
                    event.source instanceof ServiceWorker,
                    event.origin === location.origin].join(':'));
                const waiters = receivedWaiters; receivedWaiters = [];
                waiters.forEach(resolve => resolve());
            });
            const untilReceived = async count => {
                while (received.length < count)
                    await new Promise(resolve => receivedWaiters.push(resolve));
            };
            let replied = '';
            for (let i = 0; i < 200 && !replied; i++) {
                const hit = await caches.match('/replied'); if (hit) replied = await hit.text();
            }
            log.push('held:' + received.length);
            navigator.serviceWorker.startMessages();
            await untilReceived(1);
            log.push('received:' + received.join(','));
            const channel = new MessageChannel();
            const portReply = new Promise(resolve => channel.port1.onmessage = event => resolve(event.data));
            worker.postMessage({value:10}, [channel.port2]);
            log.push(await portReply);
            await untilReceived(2);
            log.push('received2:' + received.join(','));
            let cloneError = '';
            try { worker.postMessage(() => {}); } catch (error) { cloneError = error.name; }
            log.push('clone:' + cloneError);
            const quiet = await navigator.serviceWorker.register('/nomsg/sw.js');
            const quietWorker = quiet.installing || quiet.waiting || quiet.active;
            if (quietWorker.state !== 'activated') await new Promise(resolve =>
                quietWorker.addEventListener('statechange', () => {if (quietWorker.state === 'activated') resolve();}));
            const skipped = new MessageChannel();
            quietWorker.postMessage({value:1}, [skipped.port1]);
            let detached = false;
            try { structuredClone(skipped.port1, {transfer:[skipped.port1]}); }
            catch (error) { detached = error.name === 'DataCloneError'; }
            log.push('skip-detached:' + detached);
            await quiet.unregister();
            await registration.unregister();
        })().catch(error => log.push('ERROR:' + error.name + ':' + error.message)).finally(() => msgDone = true);
    "#, "service worker messages").unwrap();
    run_until(&runtime, &mut engine, &mut tasks, "msgDone");
    assert_eq!(
        log(&mut engine),
        "script:true|installing-same:true|seen:1:true:window:top-level:true|held:0|received:2:true:true:true|port:10|received2:2:true:true:true,11:true:true:true|clone:DataCloneError|skip-detached:true"
    );
    server.abort();
}

#[test]
fn service_worker_messages_keep_receiver_and_incumbent_realms() {
    // SW 92aba3be: postMessage targets the represented worker but uses the
    // incumbent sender; startMessages/onmessage enable the receiver's queue.
    let runtime = runtime();
    let (listener, base) = listen(&runtime);
    let server = runtime.spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let _ = read_head(&mut socket).await;
            let source = r#"
                self.addEventListener('message', event => {
                    event.source.postMessage({frameType:event.source.frameType});
                    event.ports[0].postMessage('queued');
                });
            "#;
            let response = format!("HTTP/1.1 200 OK\r\nContent-Type: text/javascript\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{source}", source.len());
            socket.write_all(response.as_bytes()).await.unwrap();
        }
    });
    for method in ["startMessages", "onmessage"] {
        let (mut engine, mut tasks) = page(&runtime, &format!("{base}/page"));
        eval(
            &mut engine,
            &format!("globalThis.enableMethod={method:?};"),
            "queue method",
        )
        .unwrap();
        eval(&mut engine, r#"
            globalThis.realmMessagesDone=false;
            (async()=>{
                const check=(ok,what)=>{if(!ok)throw Error(what);};
                const html=document.createElement('html');document.append(html);
                html.append(document.createElement('head'),document.createElement('body'));
                const frame=document.createElement('iframe');document.body.append(frame);
                const childURL=location.origin+'/child/frame';
                __dom_load_frame(frame.__id,'<body></body>',childURL);frame.__contentDoc=undefined;
                const child=__dom_create_window_realm(__dom_allocate_job_context(),frame.__id,childURL,
                    __trust_cfg,window,window,frame,0,'');
                await child.eval(`(async()=>{
                    globalThis.registration=await navigator.serviceWorker.register(${JSON.stringify(location.origin+'/child/sw.js')});
                    globalThis.worker=registration.installing||registration.waiting||registration.active;
                    if(worker.state!=='activated')await new Promise(resolve=>worker.addEventListener('statechange',()=>{
                        if(worker.state==='activated')resolve();
                    }));
                })()`);
                const received=[];
                let ready;
                const reply=new Promise(resolve=>{ready=resolve;});
                navigator.serviceWorker.addEventListener('message',event=>{received.push(event);ready();});
                // The parent has never obtained this registration. Borrowing
                // its method must still address the child's worker object.
                const channel=new MessageChannel();
                const queued=new Promise(resolve=>channel.port1.onmessage=event=>resolve(event.data));
                ServiceWorker.prototype.postMessage.call(child.worker,{value:1},[channel.port2]);
                // The port acknowledgement follows the native client enqueue.
                // It advances independently of the disabled container queue.
                check(await queued==='queued','borrowed postMessage reached its receiver');
                check(received.length===0,'parent message queue remains disabled');
                let handlerThis=false;
                if(enableMethod==='startMessages')child.ServiceWorkerContainer.prototype.startMessages.call(navigator.serviceWorker);
                else Object.getOwnPropertyDescriptor(child.ServiceWorkerContainer.prototype,'onmessage').set.call(navigator.serviceWorker,function(){handlerThis=this===navigator.serviceWorker;});
                await reply;
                const event=received[0];
                check(event.isTrusted&&event.data.frameType==='top-level','incumbent parent is the message source');
                check(event.source instanceof ServiceWorker&&event.source.scriptURL===child.worker.scriptURL,'reply source belongs to the receiving Window');
                check(event.origin===location.origin,'reply origin');
                check(enableMethod==='startMessages'||handlerThis,'borrowed onmessage setter receiver');
                log.push(enableMethod+':ok');
                channel.port1.close();
                await child.registration.unregister();
            })().catch(error=>log.push('ERROR:'+error.message)).finally(()=>realmMessagesDone=true);
        "#, "borrowed service-worker messaging").unwrap();
        run_until(&runtime, &mut engine, &mut tasks, "realmMessagesDone");
        assert_eq!(log(&mut engine), format!("{method}:ok"));
    }
    server.abort();
}

#[test]
fn service_worker_documents_keep_controllers_after_their_actors_retire() {
    let runtime = runtime();
    let (listener, base) = listen(&runtime);
    let server = runtime.spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let head = read_head(&mut socket).await;
            let (mime, source) = if head.starts_with("get /sw.js ") {
                ("text/javascript", "// A saved classic worker with no event handlers.")
            } else if head.starts_with("get /scripted ") {
                ("text/html", r#"<p id=result>waiting</p><script>
                    URL.createObjectURL(new Blob(['owned by the document']));
                    const frame=document.createElement('iframe');document.body.appendChild(frame);
                    frame.contentWindow.eval("URL.createObjectURL(new Blob(['owned by the child document']))");
                    document.getElementById('result').textContent = navigator.serviceWorker.controller ? 'controlled document' : 'missing controller';
                </script>"#)
            } else {
                ("text/html", "<p>Static document</p>")
            };
            let response = format!("HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{source}", source.len());
            socket.write_all(response.as_bytes()).await.unwrap();
        }
    });
    let (mut engine, mut tasks) = page(&runtime, &format!("{base}/register"));
    let storage = engine.ctx().host::<HostState>().unwrap().storage.clone();
    eval(
        &mut engine,
        r#"
        globalThis.readyDone = false;
        (async()=>{
            globalThis.registration = await navigator.serviceWorker.register('/sw.js');
            await navigator.serviceWorker.ready;
            if(registration.active.state !== 'activated') await new Promise(resolve=>{
                registration.active.addEventListener('statechange',()=>{
                    if(registration.active.state === 'activated') resolve();
                });
            });
            readyDone = true;
        })();
    "#,
        "service-worker document ownership",
    )
    .unwrap();
    run_until(&runtime, &mut engine, &mut tasks, "readyDone");
    let mut documents = Vec::new();
    for path in ["/plain", "/scripted"] {
        let url = url::Url::parse(&format!("{base}{path}")).unwrap();
        let mut response = runtime.block_on(async {
            let response = crate::http::fetch(&crate::http::Request::get(url))
                .await
                .unwrap();
            crate::http::execute_js(response, (80, 24), (8, 16), storage.clone()).await
        });
        let mut live = response
            .live
            .take()
            .expect("controlled Documents retain their native Window actor");
        // SW 92aba3be #client-focus / #client-navigate require later tasks
        // on this Window's event loop even without author event handlers.
        // Verify that endpoint before explicitly retiring the actor. Its
        // native Document owner remains held by `response`, so actor teardown
        // must not discard the client or revoke its document-owned blob URLs
        // (#service-worker-client-concept / #navigator-service-worker-unregister).
        runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(30), async {
                live.handle
                    .cmds
                    .send(crate::js::PageCmd::Resync)
                    .await
                    .unwrap();
                live.handle
                    .cmds
                    .send(crate::js::PageCmd::CheckUnload { request: 1 })
                    .await
                    .unwrap();
                let mut resynced = false;
                loop {
                    match live.events.recv().await {
                        Some(crate::js::PageEvt::Updated { html, outcome }) => {
                            assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
                            response.body = html.into_bytes();
                            resynced = true;
                        }
                        Some(crate::js::PageEvt::UnloadChecked {
                            request: 1,
                            proceed,
                        }) => {
                            assert!(proceed);
                            assert!(resynced, "Window task overtook its preceding resync");
                            break;
                        }
                        Some(crate::js::PageEvt::Static { .. }) => {
                            panic!("controlled Window retired before its native task")
                        }
                        Some(crate::js::PageEvt::Trouble(errors)) => panic!("{errors:?}"),
                        Some(_) => {}
                        None => panic!("controlled Window closed before its native task"),
                    }
                }
                live.handle.retire();
                while live.events.recv().await.is_some() {}
            })
            .await
            .expect("controlled Window task or explicit actor teardown did not finish");
        });
        if path == "/scripted" {
            assert!(
                response
                    .js
                    .as_ref()
                    .is_some_and(|outcome| outcome.errors.is_empty())
            );
            assert!(String::from_utf8_lossy(&response.body).contains(">controlled document<"));
        }
        assert!(
            response
                .service_client
                .as_ref()
                .unwrap()
                .client()
                .controller()
                .is_some()
        );
        documents.push(response);
    }
    let client = documents[0].service_client.as_ref().unwrap().client();
    let worker = client.controller().unwrap();
    let retained_blob_store = documents[1].blobs.as_ref().unwrap().clone();
    let entries = retained_blob_store.snapshot();
    assert_eq!(entries.len(), 2);
    for (blob_url, _) in &entries {
        assert!(retained_blob_store.resolve(blob_url).is_some());
    }
    eval(
        &mut engine,
        "globalThis.removed=false;registration.unregister().then(()=>removed=true);",
        "unregister",
    )
    .unwrap();
    run_until(&runtime, &mut engine, &mut tasks, "removed");
    assert_eq!(
        worker.state(),
        crate::service_worker::WorkerState::Activated
    );
    drop(documents.pop());
    for (blob_url, started_request) in &entries {
        assert!(
            retained_blob_store.resolve(blob_url).is_none(),
            "a retained history blob store kept an unloaded Document's URL published"
        );
        let bytes = started_request.read().unwrap();
        assert!(bytes == b"owned by the document" || bytes == b"owned by the child document");
    }
    assert_eq!(
        worker.state(),
        crate::service_worker::WorkerState::Activated
    );
    drop(documents);
    assert!(client.controller().is_none());
    assert_eq!(
        worker.state(),
        crate::service_worker::WorkerState::Redundant
    );
    server.abort();
}

#[test]
fn service_worker_cookie_subscriptions_wake_saved_scripts_and_extend_event_lifetime() {
    let runtime = runtime();
    let (listener, base) = listen(&runtime);
    let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count = requests.clone();
    let server=runtime.spawn(async move {
        loop {
            let (mut socket,_)=listener.accept().await.unwrap();
            let head=read_head(&mut socket).await;
            count.fetch_add(1,std::sync::atomic::Ordering::SeqCst);
            let source=if head.starts_with("get /import.js ") {"self.savedVersion='original';"} else {r#"
                importScripts('/import.js');
                self.oninstall = event => event.waitUntil(import('/forbidden-dynamic.js').then(
                    () => {throw new Error('dynamic import was allowed');},
                    error => {if(error.name!=='TypeError')throw error;}));
                self.oncookiechange = event => {
                    event.waitUntil((async () => {
                        const cache=await caches.open('cookie-worker-events');
                        const old=await cache.match('/count');
                        const count=old?Number(await old.text()):0;
                        const names=event.changed.map(x=>x.name+'='+x.value).join(',')+';'+event.deleted.map(x=>x.name+':'+('value' in x)).join(',');
                        await cache.put('/last',new Response(savedVersion+':'+names+':'+event.isTrusted+':'+
                            (event instanceof ExtendableCookieChangeEvent)+':'+Object.isFrozen(event.changed)+':'+(event.changed===event.changed)));
                        await cache.put('/count',new Response(String(count+1)));
                    })());
                };
            "#};
            let response=format!("HTTP/1.1 200 OK\r\nContent-Type: text/javascript\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{source}",source.len());
            socket.write_all(response.as_bytes()).await.unwrap();
        }
    });
    let (mut engine, mut tasks) = page(&runtime, &format!("{base}/page"));
    eval(&mut engine,r#"
        globalThis.cookieWorkerPhase='starting';globalThis.cookieWorkerDone=false;
        (async()=>{
            const reg=await navigator.serviceWorker.register('/cookie-worker.js');
            await navigator.serviceWorker.ready;
            const worker=reg.active;
            if(worker.state!=='activated')await new Promise(resolve=>worker.addEventListener('statechange',()=>{if(worker.state==='activated')resolve();}));
            globalThis.cookieRegistration=reg;
            const manager=reg.cookies;
            log.push('manager:'+(manager===reg.cookies)+':'+(manager instanceof CookieStoreManager));
            await manager.subscribe([{}]);
            const all=await manager.getSubscriptions();
            log.push('all:'+all.length+':'+('name' in all[0]));
            await manager.unsubscribe(all);
            try {await manager.subscribe([{name:' partial '},{url:'https://other.invalid/'}]);}
            catch(error){log.push('partial:'+error.name);}
            log.push('kept:'+(await manager.getSubscriptions())[0].name);
            await manager.unsubscribe([{name:'partial'}]);
            await manager.subscribe([{name:'sw-watched'},{name:'sw-watched',url:reg.scope+'nested'}]);
            await manager.subscribe([{name:'sw-watched'}]);
            log.push('subscriptions:'+(await manager.getSubscriptions()).length);
            cookieWorkerPhase='activated';
        })().catch(error=>{log.push('ERROR:'+error.name+':'+error.message);cookieWorkerPhase='failed';cookieWorkerDone=true;});
    "#,"service cookie subscriptions").unwrap();
    run_until(
        &runtime,
        &mut engine,
        &mut tasks,
        "cookieWorkerPhase!=='starting'",
    );
    assert_eq!(
        string_value(&mut engine, "cookieWorkerPhase"),
        "activated",
        "{}",
        log(&mut engine)
    );
    assert_eq!(
        service_worker_host::stop_idle_agents_for_test(engine.ctx()),
        1
    );
    eval(&mut engine,r#"
        (async()=>{
            await cookieStore.set({name:'sw-unrelated',value:'ignored',path:'/'});
            await cookieStore.set({name:'sw-watched',value:'one',path:'/'});
            const cache=await caches.open('cookie-worker-events');
            async function waitCount(expected){for(;;){const response=await cache.match('/count');if(response&&Number(await response.text())>=expected)return;await new Promise(resolve=>setTimeout(resolve,5));}}
            await waitCount(1);
            log.push('changed:'+await(await cache.match('/last')).text());
            await cookieStore.delete('sw-watched');
            await waitCount(2);
            log.push('deleted:'+await(await cache.match('/last')).text());
            log.push('events:'+await(await cache.match('/count')).text());
            await cookieRegistration.cookies.unsubscribe(await cookieRegistration.cookies.getSubscriptions());
            log.push('empty:'+(await cookieRegistration.cookies.getSubscriptions()).length);
            await cookieRegistration.unregister();
        })().catch(error=>log.push('ERROR:'+error.name+':'+error.message)).finally(()=>cookieWorkerDone=true);
    "#,"wake cookie worker").unwrap();
    run_until(&runtime, &mut engine, &mut tasks, "cookieWorkerDone");
    assert_eq!(
        log(&mut engine),
        "manager:true:true|all:1:false|partial:TypeError|kept:partial|subscriptions:2|changed:original:sw-watched=one;:true:true:true:true|deleted:original:;sw-watched:false:true:true:true:true|events:2|empty:0"
    );
    assert_eq!(
        requests.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "restarting must reuse the root and imported script bytes"
    );
    server.abort();
}

#[test]
fn service_worker_registration_uses_settings_origins_for_inherited_and_opaque_windows() {
    let runtime = runtime();
    let (listener, base) = listen(&runtime);
    let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count = requests.clone();
    let server=runtime.spawn(async move {
        loop {
            let (mut socket,_)=listener.accept().await.unwrap();
            let _=read_head(&mut socket).await;
            count.fetch_add(1,std::sync::atomic::Ordering::SeqCst);
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/javascript\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
        }
    });
    let (mut engine, mut tasks) = page(&runtime, &format!("{base}/page"));
    eval(&mut engine,&format!(r#"
        globalThis.originWorkerDone=false;
        const root=document.appendChild(document.createElement('html'));
        // The normal browser bootstrap supplies this flag explicitly; the
        // small test page uses the prelude's loopback-URL fallback.
        const frameConfig={{...__trust_cfg,secureContext:isSecureContext}};
        function realm(url,sandbox){{
            const frame=document.createElement('iframe');
            if(sandbox!==undefined)frame.setAttribute('sandbox',sandbox);
            root.appendChild(frame);frame.__contentDoc=undefined;
            __dom_load_frame(frame.__id,'<body></body>',url);
            const child=__dom_create_window_realm(__dom_allocate_job_context(),frame.__id,url,frameConfig,window,window,frame,0,'');
            frame.__frameUrl=url;frame.__contentRealmWindow=child;frame.__contentDoc=child.document;
            return child;
        }}
        // Construct both realms before starting the asynchronous check: a
        // fresh debug-build Window bootstrap is substantial synchronous work
        // and must not consume the network task pump's deadline midway through.
        const inherited=realm('about:blank');
        const opaque=realm({opaque},'allow-scripts');
        (async()=>{{
            const registration=await inherited.navigator.serviceWorker.register({script},{{scope:{scope}}});
            log.push('inherited:'+registration.scope);
            opaque.eval(`globalThis.originResults=[];
                globalThis.originChecks=(async()=>{{
                    for(const op of [()=>navigator.serviceWorker.register({script}),()=>navigator.serviceWorker.getRegistrations(),()=>navigator.serviceWorker.getRegistration()]){{
                        try{{await op();originResults.push('allowed');}}catch(error){{originResults.push(error.name);}}
                    }}
                }})();`);
            await opaque.originChecks;
            log.push('opaque:'+opaque.originResults.join(','));
            log.push('removed:'+await registration.unregister());
        }})().catch(error=>log.push('ERROR:'+error.name+':'+error.message)).finally(()=>originWorkerDone=true);
    "#,script=serde_json::to_string(&format!("{base}/frame-sw.js")).unwrap(),scope=serde_json::to_string(&format!("{base}/")).unwrap(),opaque=serde_json::to_string(&format!("{base}/opaque")).unwrap()),"service-worker settings origins").unwrap();
    run_until(&runtime, &mut engine, &mut tasks, "originWorkerDone");
    assert_eq!(
        log(&mut engine),
        format!("inherited:{base}/|opaque:SecurityError,SecurityError,SecurityError|removed:true")
    );
    assert_eq!(
        requests.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "opaque registration must not fetch a script"
    );
    server.abort();
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
}

fn listen(runtime: &tokio::runtime::Runtime) -> (TcpListener, String) {
    let listener = runtime.block_on(TcpListener::bind("127.0.0.1:0")).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    (listener, base)
}

/// A Window realm at `page` whose network tasks arrive on the returned queue.
fn page(
    runtime: &tokio::runtime::Runtime,
    page: &str,
) -> (
    trust_js::Engine,
    tokio::sync::mpsc::UnboundedReceiver<TrustJsHostTask>,
) {
    let (task_tx, task_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut state = HostState::new(
        Rc::new(RefCell::new(Dom::new())),
        Rc::new(RealmClock::new()),
    );
    state
        .dom
        .borrow_mut()
        .set_doc_url(Some(url::Url::parse(page).unwrap()));
    state.enable_network(
        url::Url::parse(page).unwrap(),
        runtime.handle().clone(),
        Arc::new(crate::http::PageCache::default()),
        task_tx,
    );
    let mut engine = configured_engine(state, page);
    eval(&mut engine, "globalThis.log = [];", "log").unwrap();
    (engine, task_rx)
}

fn log(engine: &mut trust_js::Engine) -> String {
    string_value(engine, "log.join('|')")
}

/// Run the event loop until `condition` holds: host networking tasks,
/// platform tasks and microtasks, with the timer clock advancing in steps.
fn run_until(
    runtime: &tokio::runtime::Runtime,
    engine: &mut trust_js::Engine,
    tasks: &mut tokio::sync::mpsc::UnboundedReceiver<TrustJsHostTask>,
    condition: &str,
) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        run_microtask_checkpoint(engine);
        while string_value(engine, "__trust.hasPlatformTask()") == "true" {
            eval(engine, "__trust.runPlatformTask()", "platform task").unwrap();
            run_microtask_checkpoint(engine);
        }
        // A frame's DOM-manipulation tasks belong to its own realm. Tests
        // with inherited/sandboxed settings must pump those queues as the
        // resident page actor does, including promise-only native failures.
        let contexts: Vec<_> = engine
            .ctx()
            .host::<HostState>()
            .unwrap()
            .window_realms
            .keys()
            .copied()
            .collect();
        for context in contexts {
            run_in_context_realm(engine, context, "frame platform tasks", |engine| {
                for _ in 0..32 {
                    if !matches!(
                        host_call_trust(engine.ctx(), "hasPlatformTask", &[]),
                        Ok(Value::Bool(true))
                    ) {
                        break;
                    }
                    host_call_trust(engine.ctx(), "runPlatformTask", &[])
                        .map_err(|_| String::from("frame platform task failed"))?;
                    run_microtask_checkpoint(engine);
                }
                Ok(())
            })
            .unwrap();
        }
        if string_value(engine, condition) == "true" {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {condition}; log: {}",
            log(engine)
        );
        eval(engine, "__trust.tickTo(__trust.now() + 25)", "timers").unwrap();
        run_microtask_checkpoint(engine);
        let task = runtime.block_on(async {
            tokio::time::timeout(Duration::from_millis(20), tasks.recv()).await
        });
        if let Ok(Some(task)) = task {
            dispatch_host_task(engine, task).unwrap();
        }
    }
}

/// Dispatch host tasks for `duration` without waiting on script.
fn run_for(
    runtime: &tokio::runtime::Runtime,
    engine: &mut trust_js::Engine,
    tasks: &mut tokio::sync::mpsc::UnboundedReceiver<TrustJsHostTask>,
    duration: Duration,
) {
    let deadline = Instant::now() + duration;
    while Instant::now() < deadline {
        let task = runtime.block_on(async {
            tokio::time::timeout(Duration::from_millis(20), tasks.recv()).await
        });
        if let Ok(Some(task)) = task {
            dispatch_host_task(engine, task).unwrap();
        }
        run_microtask_checkpoint(engine);
    }
}

/// The request head, lowercased.
async fn read_head(socket: &mut TcpStream) -> String {
    let mut request = Vec::new();
    while !request.windows(4).any(|window| window == b"\r\n\r\n") {
        let mut chunk = [0u8; 2048];
        let read = socket.read(&mut chunk).await.unwrap();
        if read == 0 {
            break;
        }
        request.extend_from_slice(&chunk[..read]);
    }
    String::from_utf8_lossy(&request).to_ascii_lowercase()
}

fn chunk(data: &[u8]) -> Vec<u8> {
    let mut out = format!("{:x}\r\n", data.len()).into_bytes();
    out.extend_from_slice(data);
    out.extend_from_slice(b"\r\n");
    out
}

/// Write a chunk every few milliseconds until the client goes away; true
/// once a write fails because the client closed the connection.
async fn stream_until_closed(socket: &mut TcpStream) -> bool {
    for _ in 0..1000 {
        if socket.write_all(&chunk(b"tick\n")).await.is_err() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    false
}

fn streams_in_flight(engine: &mut trust_js::Engine) -> usize {
    engine
        .ctx()
        .host_mut::<HostState>()
        .and_then(|state| state.network.as_ref())
        .map_or(0, |network| network.body_streams.len())
}

#[test]
fn fetch_body_chunks_reach_script_before_the_response_completes() {
    // Fetch #concept-http-network-fetch returns the response once its
    // header section arrives and enqueues body bytes as they are received.
    let runtime = runtime();
    let (listener, base) = listen(&runtime);
    let (release, released) = tokio::sync::oneshot::channel::<()>();
    let server = runtime.spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let request = read_head(&mut socket).await;
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nTransfer-Encoding: chunked\r\n\r\n",
            )
            .await
            .unwrap();
        socket.write_all(&chunk("first 👋\n".as_bytes())).await.unwrap();
        released.await.unwrap();
        socket.write_all(&chunk(b"second\n")).await.unwrap();
        socket.write_all(b"0\r\n\r\n").await.unwrap();
        request
    });
    let (mut engine, mut tasks) = page(&runtime, &format!("{base}/page"));
    eval(
        &mut engine,
        r#"
        fetch('/stream').then(async response => {
            log.push('status:' + response.status);
            const reader = response.body.getReader(), decoder = new TextDecoder();
            for (;;) {
                const {value, done} = await reader.read();
                if (done) { log.push('done'); return; }
                log.push('chunk:' + decoder.decode(value, {stream: true}).trim());
            }
        }, error => log.push('rejected:' + error));
        "#,
        "streaming fetch",
    )
    .unwrap();
    run_until(&runtime, &mut engine, &mut tasks, "log.length >= 2");
    assert_eq!(log(&mut engine), "status:200|chunk:first 👋");
    assert!(!server.is_finished(), "the body is still being sent");
    release.send(()).unwrap();
    run_until(&runtime, &mut engine, &mut tasks, "log.includes('done')");
    assert_eq!(
        log(&mut engine),
        "status:200|chunk:first 👋|chunk:second|done"
    );
    assert_eq!(streams_in_flight(&mut engine), 0);
    let request = runtime.block_on(server).unwrap();
    assert!(request.starts_with("get /stream http/1.1"));
}

#[test]
fn response_text_and_clone_read_a_streamed_body_to_its_end() {
    // #concept-body-consume-body and #concept-body-clone over a body whose
    // bytes arrive in delayed pieces, decoded from gzip as they arrive.
    let runtime = runtime();
    let (listener, base) = listen(&runtime);
    let server = runtime.spawn(async move {
        use std::io::Write as _;
        let (mut socket, _) = listener.accept().await.unwrap();
        read_head(&mut socket).await;
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Encoding: gzip\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
        let mut encoder =
            flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        for piece in [&b"{\"items\": [1,"[..], b" 2,", b" 3]}"] {
            encoder.write_all(piece).unwrap();
            encoder.flush().unwrap();
            let bytes = std::mem::take(encoder.get_mut());
            socket.write_all(&chunk(&bytes)).await.unwrap();
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        let tail = encoder.finish().unwrap();
        socket.write_all(&chunk(&tail)).await.unwrap();
        socket.write_all(b"0\r\n\r\n").await.unwrap();
    });
    let (mut engine, mut tasks) = page(&runtime, &format!("{base}/page"));
    eval(
        &mut engine,
        r#"
        fetch('/json').then(async response => {
            const copy = response.clone();
            const [json, text] = await Promise.all([response.json(), copy.text()]);
            log.push(json.items.join(','), text);
        }).catch(error => log.push('failed:' + error));
        "#,
        "consume a streamed body",
    )
    .unwrap();
    run_until(&runtime, &mut engine, &mut tasks, "log.length >= 2");
    assert_eq!(log(&mut engine), "1,2,3|{\"items\": [1, 2, 3]}");
    runtime.block_on(server).unwrap();
}

#[test]
fn aborting_a_fetch_or_canceling_its_body_stops_the_transfer() {
    // Fetch #fetch-controller-abort errors the body with the abort reason;
    // a canceled stream aborts the fetch (its cancelAlgorithm). Either way
    // the HTTP/1.1 connection is closed.
    let runtime = runtime();
    let (listener, base) = listen(&runtime);
    let server = runtime.spawn(async move {
        let mut closed = Vec::new();
        for _ in 0..2 {
            let (mut socket, _) = listener.accept().await.unwrap();
            read_head(&mut socket).await;
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nTransfer-Encoding: chunked\r\n\r\n",
                )
                .await
                .unwrap();
            closed.push(stream_until_closed(&mut socket).await);
        }
        closed
    });
    let (mut engine, mut tasks) = page(&runtime, &format!("{base}/page"));
    eval(
        &mut engine,
        r#"
        (async () => {
            const controller = new AbortController();
            const response = await fetch('/endless', {signal: controller.signal});
            const reader = response.body.getReader();
            await reader.read();
            log.push('first');
            controller.abort();
            try { await reader.read(); log.push('read after abort'); }
            catch (error) { log.push('abort:' + error.name); }
            const second = await fetch('/endless');
            const secondReader = second.body.getReader();
            await secondReader.read();
            await secondReader.cancel();
            log.push('canceled');
        })().catch(error => log.push('failed:' + error));
        "#,
        "abort and cancel",
    )
    .unwrap();
    run_until(
        &runtime,
        &mut engine,
        &mut tasks,
        "log.includes('canceled')",
    );
    assert_eq!(log(&mut engine), "first|abort:AbortError|canceled");
    let closed = runtime.block_on(server).unwrap();
    assert_eq!(closed, vec![true, true], "both transfers were stopped");
    run_for(
        &runtime,
        &mut engine,
        &mut tasks,
        Duration::from_millis(100),
    );
    assert_eq!(streams_in_flight(&mut engine), 0);
}

#[test]
fn an_unread_body_suspends_its_transfer_within_the_buffer_limit() {
    // Fetch #concept-http-network-fetch: the user agent suspends the fetch
    // while its buffer exceeds an upper limit, and a pull resumes it.
    const LENGTH: usize = 6 * 1024 * 1024;
    let runtime = runtime();
    let (listener, base) = listen(&runtime);
    let server = runtime.spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_head(&mut socket).await;
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {LENGTH}\r\n\r\n"
        );
        socket.write_all(head.as_bytes()).await.unwrap();
        let piece = vec![0x5a; 64 * 1024];
        for _ in 0..LENGTH / piece.len() {
            socket.write_all(&piece).await.unwrap();
        }
    });
    let (mut engine, mut tasks) = page(&runtime, &format!("{base}/page"));
    eval(
        &mut engine,
        "fetch('/large').then(response => { globalThis.held = response; log.push('head'); });",
        "unread body",
    )
    .unwrap();
    run_until(&runtime, &mut engine, &mut tasks, "log.includes('head')");
    run_for(
        &runtime,
        &mut engine,
        &mut tasks,
        Duration::from_millis(600),
    );
    let (buffered, suspended) = engine
        .ctx()
        .host_mut::<HostState>()
        .and_then(|state| state.network.as_ref())
        .and_then(|network| network.body_streams.values().next())
        .map(|stream| (stream.buffer.len(), stream.flow.suspended()))
        .unwrap();
    assert!(suspended, "the transfer waits for the body to be read");
    assert!(
        buffered <= crate::http::BODY_FLOW_UPPER + 64 * 1024,
        "{buffered} bytes buffered"
    );
    eval(
        &mut engine,
        "held.arrayBuffer().then(buffer => log.push('length:' + buffer.byteLength));",
        "read the body",
    )
    .unwrap();
    run_until(
        &runtime,
        &mut engine,
        &mut tasks,
        "log.some(entry => entry.startsWith('length:'))",
    );
    assert_eq!(log(&mut engine), format!("head|length:{LENGTH}"));
    runtime.block_on(server).unwrap();
}

#[test]
fn xhr_reports_loading_and_progress_as_the_body_arrives() {
    // XHR #the-send()-method processBodyChunk: state LOADING, readystatechange
    // and progress while the response body is still arriving.
    let runtime = runtime();
    let (listener, base) = listen(&runtime);
    let (release, released) = tokio::sync::oneshot::channel::<()>();
    let server = runtime.spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_head(&mut socket).await;
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/plain;charset=utf-8\r\nTransfer-Encoding: chunked\r\n\r\n",
            )
            .await
            .unwrap();
        socket.write_all(&chunk(b"alpha")).await.unwrap();
        released.await.unwrap();
        socket.write_all(&chunk(b"beta")).await.unwrap();
        socket.write_all(b"0\r\n\r\n").await.unwrap();
    });
    let (mut engine, mut tasks) = page(&runtime, &format!("{base}/page"));
    eval(
        &mut engine,
        r#"
        const xhr = new XMLHttpRequest();
        xhr.open('GET', '/progress');
        xhr.onreadystatechange = () => log.push('state:' + xhr.readyState + ':' + xhr.responseText);
        xhr.onprogress = event => log.push('progress:' + event.loaded);
        xhr.onload = () => log.push('load:' + xhr.responseText);
        xhr.send();
        "#,
        "incremental XHR",
    )
    .unwrap();
    run_until(
        &runtime,
        &mut engine,
        &mut tasks,
        "log.includes('state:3:alpha')",
    );
    assert!(
        !server.is_finished(),
        "LOADING precedes the end of the body"
    );
    assert_eq!(log(&mut engine), "state:2:|state:3:alpha|progress:5");
    release.send(()).unwrap();
    run_until(
        &runtime,
        &mut engine,
        &mut tasks,
        "log.some(entry => entry.startsWith('load:'))",
    );
    let log = log(&mut engine);
    assert!(
        log.ends_with("progress:9|state:4:alphabeta|load:alphabeta"),
        "{log}"
    );
    runtime.block_on(server).unwrap();
}

/// One event-stream connection's script: what the server sends, and the
/// request it received.
async fn serve_event_stream(socket: &mut TcpStream, pieces: &[&[u8]]) {
    socket
        .write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        )
        .await
        .unwrap();
    for piece in pieces {
        socket.write_all(piece).await.unwrap();
        socket.flush().await.unwrap();
        tokio::time::sleep(Duration::from_millis(15)).await;
    }
}

#[test]
fn event_source_dispatches_streamed_events_and_reconnects_with_last_event_id() {
    // HTML #event-stream-interpretation over pieces that split a CRLF, a
    // UTF-8 sequence and a field; #reestablish-the-connection with the
    // `retry` reconnection time and `Last-Event-ID`; a 204 fails the
    // connection (#fail-the-connection).
    let runtime = runtime();
    let (listener, base) = listen(&runtime);
    let (release, released) = tokio::sync::oneshot::channel::<()>();
    let server = runtime.spawn(async move {
        let mut requests = Vec::new();
        let (mut socket, _) = listener.accept().await.unwrap();
        requests.push(read_head(&mut socket).await);
        let wave = "👋".as_bytes();
        serve_event_stream(
            &mut socket,
            &[
                b"\xEF\xBB\xBF: a comment\nretry: 40\n",
                b"id: 1\ndata: fir",
                b"st\r",
                &[b"\ndata:second ", &wave[..2]].concat(),
                &[&wave[2..], b"\r\r"].concat(),
            ],
        )
        .await;
        // The first event is dispatched while the stream is still open.
        released.await.unwrap();
        serve_event_stream(
            &mut socket,
            &[b"event: ping\ndata: x\n\n", b"data: never dispatched"],
        )
        .await;
        drop(socket);
        let (mut socket, _) = listener.accept().await.unwrap();
        requests.push(read_head(&mut socket).await);
        serve_event_stream(&mut socket, &[b"data: again\n\n"]).await;
        drop(socket);
        let (mut socket, _) = listener.accept().await.unwrap();
        requests.push(read_head(&mut socket).await);
        socket
            .write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        requests
    });
    let page_url = format!("{base}/page");
    let (mut engine, mut tasks) = page(&runtime, &page_url);
    eval(
        &mut engine,
        r#"
        const source = new EventSource('/events');
        log.push('created:' + source.readyState + ':' + source.url.endsWith('/events') + ':' + source.withCredentials);
        source.onopen = () => log.push('open:' + source.readyState);
        source.onmessage = event => log.push('message:' + JSON.stringify(event.data) + ':' +
            event.lastEventId + ':' + (event.origin === location.origin) + ':' + (event instanceof MessageEvent));
        source.addEventListener('ping', event => log.push('ping:' + event.data + ':' + event.lastEventId));
        source.onerror = event => log.push('error:' + source.readyState + ':' + event.constructor.name);
        "#,
        "EventSource",
    )
    .unwrap();
    run_until(
        &runtime,
        &mut engine,
        &mut tasks,
        "log.some(entry => entry.startsWith('message:'))",
    );
    assert!(!server.is_finished(), "the event stream is still open");
    release.send(()).unwrap();
    run_until(
        &runtime,
        &mut engine,
        &mut tasks,
        "source.readyState === EventSource.CLOSED",
    );
    assert_eq!(
        log(&mut engine),
        [
            "created:0:true:false",
            "open:1",
            "message:\"first\\nsecond 👋\":1:true:true",
            "ping:x:1",
            "error:0:Event",
            "open:1",
            "message:\"again\":1:true:true",
            "error:0:Event",
            "error:2:Event",
        ]
        .join("|")
    );
    let requests = runtime.block_on(server).unwrap();
    assert!(requests[0].contains("accept: text/event-stream\r\n"));
    assert!(!requests[0].contains("last-event-id"));
    assert!(
        requests[1].contains("last-event-id: 1\r\n"),
        "{}",
        requests[1]
    );
    assert!(requests[2].contains("last-event-id: 1\r\n"));
    run_for(
        &runtime,
        &mut engine,
        &mut tasks,
        Duration::from_millis(100),
    );
    assert_eq!(streams_in_flight(&mut engine), 0);
}

#[test]
fn event_source_fails_on_a_wrong_type_or_status_and_close_aborts_the_stream() {
    let runtime = runtime();
    let (listener, base) = listen(&runtime);
    let server = runtime.spawn(async move {
        let mut aborted = false;
        for _ in 0..3 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_head(&mut socket).await;
            if request.starts_with("get /plain ") {
                socket
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 9\r\n\r\ndata: x\n\n")
                    .await
                    .unwrap();
            } else if request.starts_with("get /missing ") {
                socket
                    .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Type: text/event-stream\r\nContent-Length: 9\r\n\r\ndata: x\n\n")
                    .await
                    .unwrap();
            } else {
                socket
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream;charset=utf-8\r\nTransfer-Encoding: chunked\r\n\r\n")
                    .await
                    .unwrap();
                socket.write_all(&chunk(b"data: hello\n\n")).await.unwrap();
                aborted = stream_until_closed(&mut socket).await;
            }
        }
        aborted
    });
    let (mut engine, mut tasks) = page(&runtime, &format!("{base}/page"));
    eval(
        &mut engine,
        r#"
        for (const path of ['/plain', '/missing']) {
            const source = new EventSource(path);
            source.onmessage = () => log.push(path + ':message');
            source.onerror = event => log.push(path + ':error:' + source.readyState + ':' + ('data' in event));
        }
        const live = new EventSource('/live');
        live.onmessage = event => { log.push('live:' + event.data); live.close(); log.push('live:' + live.readyState); };
        live.onerror = () => log.push('live:error');
        "#,
        "failing EventSources",
    )
    .unwrap();
    run_until(&runtime, &mut engine, &mut tasks, "log.length >= 4");
    let aborted = runtime.block_on(server).unwrap();
    assert!(aborted, "close() aborted the fetch");
    run_for(
        &runtime,
        &mut engine,
        &mut tasks,
        Duration::from_millis(200),
    );
    let mut entries: Vec<String> = log(&mut engine).split('|').map(String::from).collect();
    entries.sort();
    assert_eq!(
        entries,
        [
            "/missing:error:2:false",
            "/plain:error:2:false",
            "live:2",
            "live:hello",
        ]
    );
}

#[test]
fn event_stream_fields_follow_the_interpretation_rules() {
    // #event-stream-interpretation, from data: URLs (fetched with basic
    // tainting by #scheme-fetch): leading-space removal, empty and repeated
    // data fields, a NULL in `id`, bogus `retry`, case-sensitive names,
    // an empty `event` type and an unterminated last event.
    let runtime = runtime();
    let (mut engine, mut tasks) = page(&runtime, "http://127.0.0.1:1/page");
    eval(
        &mut engine,
        r#"
        const stream = 'data:test\ndata: test\n\n' +
            'data\n\ndata\ndata\n\n' +
            'id: 7\ndata:a\n\nid: x\u0000y\nData: ignored\nretry: 1x\nevent:\ndata: b\n\n' +
            ':only a comment\n\n' +
            'event: custom\ndata: c\n\ndata: unterminated';
        const source = new EventSource('data:text/event-stream;charset=utf-8,' + encodeURIComponent(stream));
        source.onmessage = event => log.push(event.type + '=' + JSON.stringify(event.data) + '#' + event.lastEventId);
        source.addEventListener('custom', event => { log.push('custom=' + event.data); source.close(); });
        "#,
        "data: event stream",
    )
    .unwrap();
    run_until(&runtime, &mut engine, &mut tasks, "log.length >= 6");
    run_for(&runtime, &mut engine, &mut tasks, Duration::from_millis(50));
    assert_eq!(
        log(&mut engine),
        [
            "message=\"test\\ntest\"#",
            "message=\"\"#",
            "message=\"\\n\"#",
            "message=\"a\"#7",
            "message=\"b\"#7",
            "custom=c",
        ]
        .join("|")
    );
}

#[test]
fn event_source_interface_conversions() {
    // #the-eventsource-interface: constants, URL parsing failure, the
    // EventSourceInit dictionary and the event handler attributes.
    let runtime = runtime();
    let (mut engine, _tasks) = page(&runtime, "http://127.0.0.1:1/dir/page");
    assert_eq!(
        string_value(
            &mut engine,
            r#"(() => {
                const results = [EventSource.CONNECTING, EventSource.OPEN, EventSource.CLOSED,
                    EventSource.prototype.CLOSED, EventSource.length,
                    Object.prototype.toString.call(EventSource.prototype)];
                try { new EventSource('http://exa mple.com/'); results.push('no throw'); }
                catch (error) { results.push(error.name); }
                try { new EventSource('x', 5); results.push('no throw'); }
                catch (error) { results.push(error.constructor.name); }
                const source = new EventSource('stream?x', {withCredentials: true});
                results.push(source.url, source.withCredentials, source.readyState,
                    source.onopen, typeof Object.getOwnPropertyDescriptor(EventSource.prototype, 'onmessage').get);
                const handler = () => {};
                source.onerror = handler;
                results.push(source.onerror === handler);
                source.close();
                results.push(source.readyState, source instanceof EventTarget);
                return results.join(',');
            })()"#
        ),
        "0,1,2,2,1,[object EventSource],SyntaxError,TypeError,\
http://127.0.0.1:1/dir/stream?x,true,0,,function,true,2,true"
    );
}

#[test]
fn dedicated_workers_stream_fetch_bodies_and_event_streams() {
    // fetch() and EventSource are [Exposed=(Window,Worker)]: a worker's
    // body chunks and events arrive as tasks of its own event loop.
    let runtime = runtime();
    let (listener, base) = listen(&runtime);
    let (release, released) = tokio::sync::oneshot::channel::<()>();
    let script = r#"
        (async () => {
            const response = await fetch('/stream');
            const reader = response.body.getReader(), decoder = new TextDecoder();
            postMessage('chunk:' + decoder.decode((await reader.read()).value));
            let rest = '';
            for (;;) {
                const {value, done} = await reader.read();
                if (done) break;
                rest += decoder.decode(value);
            }
            postMessage('rest:' + rest);
            const source = new EventSource('/events');
            source.onopen = () => postMessage('open:' + source.readyState);
            source.onmessage = event => {
                postMessage('event:' + event.data + ':' + event.lastEventId + ':' + (event.origin === self.origin));
                source.close();
            };
        })().catch(error => postMessage('ERROR:' + error));
    "#;
    let server = runtime.spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        assert!(read_head(&mut socket).await.starts_with("get /worker.js "));
        let reply = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/javascript\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{script}",
            script.len()
        );
        socket.write_all(reply.as_bytes()).await.unwrap();
        drop(socket);
        let (mut socket, _) = listener.accept().await.unwrap();
        assert!(read_head(&mut socket).await.starts_with("get /stream "));
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        socket.write_all(&chunk(b"first")).await.unwrap();
        released.await.unwrap();
        socket.write_all(&chunk(b"second")).await.unwrap();
        socket.write_all(b"0\r\n\r\n").await.unwrap();
        drop(socket);
        let (mut socket, _) = listener.accept().await.unwrap();
        let request = read_head(&mut socket).await;
        assert!(request.contains("accept: text/event-stream\r\n"), "{request}");
        serve_event_stream(&mut socket, &[b"id: 9\ndata: from", b" worker\n\n"]).await;
        stream_until_closed(&mut socket).await
    });
    let (mut engine, mut tasks) = page(&runtime, &format!("{base}/page"));
    eval(
        &mut engine,
        "const worker = new Worker('/worker.js'); worker.onmessage = event => log.push(event.data);",
        "worker",
    )
    .unwrap();
    run_until(&runtime, &mut engine, &mut tasks, "log.length >= 1");
    assert_eq!(log(&mut engine), "chunk:first");
    assert!(
        !server.is_finished(),
        "the worker read a chunk of an open body"
    );
    release.send(()).unwrap();
    run_until(&runtime, &mut engine, &mut tasks, "log.length >= 4");
    assert_eq!(
        log(&mut engine),
        "chunk:first|rest:second|open:1|event:from worker:9:true"
    );
    assert!(
        runtime.block_on(server).unwrap(),
        "close() ended the stream"
    );
    eval(&mut engine, "worker.terminate()", "terminate").unwrap();
}

/// Serve `count` requests, one per connection, answering each request head
/// with `respond(head)`; returns the heads in arrival order.
fn serve_each(
    runtime: &tokio::runtime::Runtime,
    listener: TcpListener,
    count: usize,
    respond: fn(&str) -> Vec<u8>,
) -> tokio::task::JoinHandle<Vec<String>> {
    runtime.spawn(async move {
        let mut heads = Vec::new();
        for _ in 0..count {
            let (mut socket, _) = listener.accept().await.unwrap();
            let head = read_head(&mut socket).await;
            socket.write_all(&respond(&head)).await.unwrap();
            heads.push(head);
        }
        heads
    })
}

#[test]
fn responses_expose_the_reason_phrase_and_the_ordered_header_list() {
    // Fetch #concept-response-status-message is the HTTP/1.1 reason phrase
    // (RFC 9112 §4). The header list keeps the fields' order and duplicates
    // (#concept-header-list); Headers iterates it sorted and combined
    // (#concept-header-list-sort-and-combine), and getAllResponseHeaders()
    // sorts and combines it too (XHR #the-getallresponseheaders()-method).
    // Set-Cookie is a forbidden response-header name.
    let runtime = runtime();
    let (listener, base) = listen(&runtime);
    let server = serve_each(&runtime, listener, 2, |_| {
        b"HTTP/1.1 203 Partly Fine\r\nX-B: 1\r\nX-A: 2\r\nx-b: 3\r\nSet-Cookie: s=1\r\nContent-Type: text/plain\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok".to_vec()
    });
    let (mut engine, mut tasks) = page(&runtime, &format!("{base}/page"));
    eval(
        &mut engine,
        r#"
        fetch('/fetch').then(async response => {
            log.push(response.status + ' ' + response.statusText,
                [...response.headers].map(pair => pair.join('=')).join(','),
                response.headers.get('X-B'), await response.text());
            const xhr = new XMLHttpRequest();
            xhr.open('GET', '/xhr');
            xhr.onload = () => log.push(xhr.status + ' ' + xhr.statusText,
                JSON.stringify(xhr.getAllResponseHeaders()), xhr.getResponseHeader('x-b'));
            xhr.send();
        }).catch(error => log.push('failed:' + error));
        "#,
        "status message and header list",
    )
    .unwrap();
    run_until(&runtime, &mut engine, &mut tasks, "log.length >= 7");
    assert_eq!(
        log(&mut engine),
        "203 Partly Fine|connection=close,content-length=2,content-type=text/plain,x-a=2,x-b=1, 3|1, 3|ok|\
         203 Partly Fine|\"connection: close\\r\\ncontent-length: 2\\r\\ncontent-type: text/plain\\r\\nx-a: 2\\r\\nx-b: 1, 3\\r\\n\"|1, 3"
    );
    runtime.block_on(server).unwrap();
}

#[test]
fn xhr_upload_listeners_preflight_a_cross_origin_request() {
    // XHR #the-send()-method sets the request's use-CORS-preflight flag when
    // the upload object has event listeners; Fetch #main-fetch then makes a
    // CORS-preflight fetch (#cors-preflight-fetch-0) even for a safelisted
    // method and header list. Without listeners the request goes directly.
    let runtime = runtime();
    let (_page_listener, page_base) = listen(&runtime);
    let (listener, base) = listen(&runtime);
    let server = serve_each(&runtime, listener, 3, |head| {
        let origin = head
            .lines()
            .find_map(|line| line.strip_prefix("origin: "))
            .unwrap_or("null")
            .to_string();
        format!(
            "HTTP/1.1 200 OK\r\nAccess-Control-Allow-Origin: {origin}\r\nAccess-Control-Allow-Methods: GET\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok"
        )
        .into_bytes()
    });
    let (mut engine, mut tasks) = page(&runtime, &format!("{page_base}/page"));
    eval(
        &mut engine,
        &format!(
            r#"
            const watched = new XMLHttpRequest();
            watched.open('GET', '{base}/watched');
            watched.upload.onprogress = () => {{}};
            watched.onloadend = () => {{
                log.push('watched:' + watched.status + ':' + watched.responseText);
                const plain = new XMLHttpRequest();
                plain.open('GET', '{base}/plain');
                plain.onloadend = () => log.push('plain:' + plain.status + ':' + plain.responseText);
                plain.send();
            }};
            watched.send();
            "#
        ),
        "XHR upload listener preflight",
    )
    .unwrap();
    run_until(&runtime, &mut engine, &mut tasks, "log.length >= 2");
    assert_eq!(log(&mut engine), "watched:200:ok|plain:200:ok");
    let heads = runtime.block_on(server).unwrap();
    assert!(heads[0].starts_with("options /watched "), "{}", heads[0]);
    assert!(
        heads[0].contains("access-control-request-method: get\r\n"),
        "{}",
        heads[0]
    );
    assert!(heads[1].starts_with("get /watched "), "{}", heads[1]);
    assert!(heads[2].starts_with("get /plain "), "{}", heads[2]);
}

/// A server for `window_stop_*`: requests that wait for the client to give
/// up (reporting whether it closed the connection), an event stream that
/// never answers, a response whose body never ends and a keepalive request
/// answered only once `release` fires.
fn serve_stoppable(
    runtime: &tokio::runtime::Runtime,
    listener: TcpListener,
    count: usize,
    release: tokio::sync::oneshot::Receiver<()>,
) -> (
    tokio::task::JoinHandle<Vec<(String, bool)>>,
    std::sync::Arc<std::sync::atomic::AtomicUsize>,
) {
    let arrived = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = arrived.clone();
    let server = runtime.spawn(async move {
        let release = std::sync::Arc::new(tokio::sync::Mutex::new(Some(release)));
        let mut connections = Vec::new();
        for _ in 0..count {
            let (mut socket, _) = listener.accept().await.unwrap();
            let counter = counter.clone();
            let release = release.clone();
            connections.push(tokio::spawn(async move {
                let head = read_head(&mut socket).await;
                let path = head.split(' ').nth(1).unwrap_or("").to_string();
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if path == "/keep" {
                    if let Some(release) = release.lock().await.take() {
                        let _ = release.await;
                    }
                    socket
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nkept")
                        .await
                        .unwrap();
                    return (path, false);
                }
                if path == "/stream" {
                    socket
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nTransfer-Encoding: chunked\r\n\r\n")
                        .await
                        .unwrap();
                    socket.write_all(&chunk(b"first")).await.unwrap();
                }
                // Wait for the client to close the connection.
                let mut buffer = [0u8; 64];
                let closed = tokio::time::timeout(Duration::from_secs(10), async {
                    loop {
                        match socket.read(&mut buffer).await {
                            Ok(0) | Err(_) => return true,
                            Ok(_) => {}
                        }
                    }
                })
                .await
                .unwrap_or(false);
                (path, closed)
            }));
        }
        let mut results = Vec::new();
        for connection in connections {
            results.push(connection.await.unwrap());
        }
        results.sort();
        results
    });
    (server, arrived)
}

#[test]
fn window_stop_aborts_the_documents_fetches() {
    // HTML #dom-window-stop → #nav-stop → #abort-a-document step 2: the
    // document's fetches are aborted (Fetch #fetch-controller-abort): a
    // pending fetch() rejects with an "AbortError" DOMException, a body
    // still arriving errors with it, XMLHttpRequest runs its request error
    // steps for "abort" inside stop() and an EventSource whose response has
    // not arrived fails its connection. Their connections are closed. A
    // keepalive fetch outlives the document's loading and still completes.
    let runtime = runtime();
    let (listener, base) = listen(&runtime);
    let (release, released) = tokio::sync::oneshot::channel::<()>();
    let (server, arrived) = serve_stoppable(&runtime, listener, 5, released);
    let (mut engine, mut tasks) = page(&runtime, &format!("{base}/page"));
    eval(
        &mut engine,
        r#"
        fetch('/slow').then(() => log.push('fetch resolved'), error => log.push('fetch:' + error.name));
        fetch('/keep', {keepalive: true}).then(response => response.text())
            .then(text => log.push('keepalive:' + text), error => log.push('keepalive failed:' + error));
        const xhr = new XMLHttpRequest();
        xhr.open('GET', '/xhr');
        xhr.onreadystatechange = () => log.push('rs' + xhr.readyState);
        xhr.onabort = () => log.push('abort:' + xhr.status + ':' + xhr.readyState);
        xhr.onerror = () => log.push('xhr error');
        xhr.onloadend = () => log.push('loadend');
        xhr.send();
        const source = new EventSource('/events');
        source.onerror = () => log.push('eventsource error:' + source.readyState);
        fetch('/stream').then(async response => {
            globalThis.reader = response.body.getReader();
            await reader.read();
            log.push('chunk');
        });
        "#,
        "loading fetches",
    )
    .unwrap();
    run_until(&runtime, &mut engine, &mut tasks, "log.includes('chunk')");
    let deadline = Instant::now() + Duration::from_secs(10);
    while arrived.load(std::sync::atomic::Ordering::SeqCst) < 5 {
        assert!(Instant::now() < deadline, "requests reached the server");
        run_for(&runtime, &mut engine, &mut tasks, Duration::from_millis(20));
    }
    eval(
        &mut engine,
        r#"
        log.push('stop');
        window.stop();
        log.push('stopped');
        reader.read().then(() => log.push('read'), error => log.push('read:' + error.name));
        "#,
        "window.stop()",
    )
    .unwrap();
    // The XMLHttpRequest's events fire inside stop().
    assert_eq!(log(&mut engine), "chunk|stop|rs4|abort:0:4|loadend|stopped");
    run_until(
        &runtime,
        &mut engine,
        &mut tasks,
        "['fetch:AbortError', 'read:AbortError', 'eventsource error:2'].every(entry => log.includes(entry))",
    );
    release.send(()).unwrap();
    run_until(
        &runtime,
        &mut engine,
        &mut tasks,
        "log.includes('keepalive:kept')",
    );
    let results = runtime.block_on(server).unwrap();
    assert_eq!(
        results,
        [
            (String::from("/events"), true),
            (String::from("/keep"), false),
            (String::from("/slow"), true),
            (String::from("/stream"), true),
            (String::from("/xhr"), true),
        ],
        "stopping closed every connection but the keepalive one"
    );
    let log = log(&mut engine);
    assert!(
        !log.contains("resolved") && !log.contains("xhr error"),
        "{log}"
    );
    assert_eq!(streams_in_flight(&mut engine), 0);
}

#[test]
fn the_user_agents_stop_cancels_the_navigation_and_fetches_but_not_scripts() {
    // HTML #nav-stop for the top-level navigable (`PageCmd::StopLoading`):
    // the navigation the running task requested is canceled, the document's
    // fetch is aborted, and the page keeps running.
    let runtime = runtime();
    let (listener, base) = listen(&runtime);
    let (_release, released) = tokio::sync::oneshot::channel::<()>();
    let (server, arrived) = serve_stoppable(&runtime, listener, 1, released);
    let (mut engine, mut tasks) = page(&runtime, &format!("{base}/page"));
    eval(
        &mut engine,
        "fetch('/slow').catch(error => log.push('fetch:' + error.name));",
        "loading fetch",
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while arrived.load(std::sync::atomic::Ordering::SeqCst) < 1 {
        assert!(Instant::now() < deadline, "the request reached the server");
        run_for(&runtime, &mut engine, &mut tasks, Duration::from_millis(20));
    }
    eval(&mut engine, "location.href = '/elsewhere';", "navigate").unwrap();
    assert!(stop_loading_navigable(engine.ctx(), 0));
    assert_eq!(
        string_value(&mut engine, "String(__trust.takeNavigationRequest())"),
        "null"
    );
    run_until(
        &runtime,
        &mut engine,
        &mut tasks,
        "log.includes('fetch:AbortError')",
    );
    eval(&mut engine, "log.push('still running')", "script").unwrap();
    assert_eq!(log(&mut engine), "fetch:AbortError|still running");
    // Nothing is loading any more.
    assert!(!stop_loading_navigable(engine.ctx(), 0));
    assert_eq!(
        runtime.block_on(server).unwrap(),
        [(String::from("/slow"), true)]
    );
}

#[test]
fn window_stop_while_parsing_aborts_the_parser() {
    // HTML #abort-a-document step 4 and #abort-a-parser: the content after
    // the parser's position is thrown away, readiness becomes "interactive"
    // then "complete", later parser scripts and deferred scripts never run,
    // "the end" (DOMContentLoaded, load) never happens, and
    // document.write() is ignored once the active parser was aborted.
    let html = r#"<!doctype html><html><head>
        <script>
            window.events = [];
            document.addEventListener('readystatechange', () => events.push('rs:' + document.readyState));
            document.addEventListener('DOMContentLoaded', () => document.documentElement.setAttribute('data-dcl', ''));
            window.addEventListener('load', () => document.documentElement.setAttribute('data-load', ''));
        </script>
        <script defer src="data:text/javascript,document.documentElement.setAttribute('data-deferred','')"></script>
        <script>
            events.push('before:' + document.readyState + ':' + !!document.getElementById('later'));
            window.stop();
            events.push('after:' + document.readyState + ':' + !!document.getElementById('later') + ':' + !!document.body);
            document.write('<p id=written>w</p>');
            document.documentElement.setAttribute('data-events', events.join(','));
        </script>
        <script>document.documentElement.setAttribute('data-next', '')</script>
        </head><body><p id=later>later</p></body></html>"#;
    let (rendered, outcome) = crate::js::transform(html, &crate::js::PageEnv::bare(DEFAULT_URL));
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert!(
        rendered.contains(
            "data-events=\"before:loading:true,rs:interactive,rs:complete,after:complete:false:false\""
        ),
        "{rendered}"
    );
    for absent in [
        "data-dcl",
        "data-load",
        "data-deferred",
        "data-next",
        "id=\"later\"",
        "id=\"written\"",
    ] {
        assert!(!rendered.contains(absent), "{absent}: {rendered}");
    }
}

#[test]
fn stopping_aborts_descendant_documents_by_window_realm() {
    // HTML #abort-a-document-and-its-descendants: every descendant navigable,
    // found through the Window Realm whose Document contains its container.
    let mut state = HostState::new(
        Rc::new(RefCell::new(Dom::new())),
        Rc::new(RealmClock::new()),
    );
    for (child, parent) in [(1, 0), (2, 1), (3, 0), (4, 9)] {
        state.window_realms.insert(child, Value::Num(child as f64));
        state.window_parents.insert(child, parent);
    }
    // A retired child's Window Realm is gone.
    state.window_parents.insert(5, 3);
    assert_eq!(descendant_contexts(&state, 0), [1, 2, 3]);
    assert_eq!(descendant_contexts(&state, 1), [2]);
    assert!(descendant_contexts(&state, 2).is_empty());
}

#[test]
fn a_module_scripts_static_graph_is_fetched_as_its_responses_arrive() {
    // HTML #fetch-a-module-script-tree: when a module script's fetch
    // completes, its descendants are fetched (and theirs in turn) without
    // waiting for the graph to be linked for evaluation.
    let runtime = runtime();
    let (listener, base) = listen(&runtime);
    let server = serve_each(&runtime, listener, 2, |head| {
        let body: &[u8] = if head.starts_with("get /b.js ") {
            b"import {c} from './c.js'; export const b = c;"
        } else {
            b"export const c = 1;"
        };
        let mut response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/javascript\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        response.extend_from_slice(body);
        response
    });
    let page = url::Url::parse(&format!("{base}/page")).unwrap();
    let cache = Arc::new(crate::http::PageCache::default());
    speculate_module_graph(
        page.clone(),
        runtime.handle().clone(),
        cache.clone(),
        Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        page.join("/a.js").unwrap(),
        b"import {b} from \"./b.js\";\nimport('./dynamic.js');",
        None,
    );
    let heads = runtime
        .block_on(async { tokio::time::timeout(Duration::from_secs(10), server).await })
        .unwrap()
        .unwrap();
    assert!(heads[0].starts_with("get /b.js "), "{}", heads[0]);
    assert!(heads[1].starts_with("get /c.js "), "{}", heads[1]);
    for module in ["/b.js", "/c.js"] {
        assert!(
            cache
                .peek_resource(
                    &page.join(module).unwrap(),
                    &page,
                    "script",
                    Some(crate::http::CredentialsMode::SameOrigin),
                )
                .is_some(),
            "{module} waits in the page cache for the graph loader"
        );
    }
}

#[test]
fn an_unreachable_streamed_body_still_cancels_its_transfer() {
    // The fetch group holds a body's abort steps weakly: a response nobody
    // can read is collected, which cancels its transfer and ends its fetch
    // (Fetch #garbage-collection), so stopping finds nothing to cancel.
    let runtime = runtime();
    let (listener, base) = listen(&runtime);
    let server = runtime.spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_head(&mut socket).await;
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nTransfer-Encoding: chunked\r\n\r\n",
            )
            .await
            .unwrap();
        stream_until_closed(&mut socket).await
    });
    let (mut engine, mut tasks) = page(&runtime, &format!("{base}/page"));
    eval(
        &mut engine,
        "fetch('/endless').then(response => { log.push('head:' + response.status); });",
        "unread body",
    )
    .unwrap();
    run_until(&runtime, &mut engine, &mut tasks, "log.length >= 1");
    let deadline = Instant::now() + Duration::from_secs(10);
    while streams_in_flight(&mut engine) != 0 {
        assert!(Instant::now() < deadline, "the unread body was collected");
        engine.ctx().collect_garbage_for_host();
        run_for(&runtime, &mut engine, &mut tasks, Duration::from_millis(20));
        while string_value(&mut engine, "__trust.hasPlatformTask()") == "true" {
            eval(&mut engine, "__trust.runPlatformTask()", "platform task").unwrap();
            run_microtask_checkpoint(&mut engine);
        }
    }
    assert!(
        runtime.block_on(server).unwrap(),
        "its transfer was canceled"
    );
    assert!(!stop_loading_navigable(engine.ctx(), 0));
}

#[test]
fn import_starts_its_fetch_before_the_page_thread_takes_its_task() {
    // HTML #hostloadimportedmodule fetches the imported module script as
    // import() runs; only processing the response waits for a task. The
    // request reaches the network while the page thread is still busy (here:
    // not dispatching host tasks at all), and the queued task then joins it.
    let runtime = runtime();
    let (listener, base) = listen(&runtime);
    let server = serve_each(&runtime, listener, 1, |_| {
        b"HTTP/1.1 200 OK\r\nContent-Type: text/javascript\r\nContent-Length: 22\r\nConnection: close\r\n\r\nexport const v = 'ok';"
            .to_vec()
    });
    let (mut engine, mut tasks) = page(&runtime, &format!("{base}/page"));
    eval(
        &mut engine,
        "import('/dynamic.js').then(module => log.push(module.v), error => log.push('failed:' + error));",
        "dynamic import",
    )
    .unwrap();
    let heads = runtime
        .block_on(async { tokio::time::timeout(Duration::from_secs(10), server).await })
        .expect("the import's request started without its task")
        .unwrap();
    assert!(heads[0].starts_with("get /dynamic.js "), "{}", heads[0]);
    run_until(&runtime, &mut engine, &mut tasks, "log.length >= 1");
    assert_eq!(log(&mut engine), "ok");
}

#[test]
fn failed_module_fetches_retry_without_repeating_successful_evaluation() {
    // HTML #fetch-a-single-module-script: network/HTTP/MIME failures remove
    // the map entry before rejecting; successful modules remain singletons.
    let runtime = runtime();
    let (listener, base) = listen(&runtime);
    let server = runtime.spawn(async move {
        let mut counts = std::collections::HashMap::<String, usize>::new();
        let mut heads = Vec::new();
        for _ in 0..4 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let head = read_head(&mut socket).await;
            let path = head.split_whitespace().nth(1).unwrap().to_owned();
            let count = counts.entry(path.clone()).or_default();
            *count += 1;
            let failed = *count == 1;
            let status = if failed && path == "/http.js" { "404 Not Found" } else { "200 OK" };
            let mime = if failed && path == "/mime.js" { "text/plain" } else { "text/javascript" };
            let body = "export const value = {}; globalThis.evaluations = (globalThis.evaluations || 0) + 1;";
            let response = format!("HTTP/1.1 {status}\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}", body.len());
            socket.write_all(response.as_bytes()).await.unwrap();
            heads.push(head);
        }
        heads
    });
    let (mut engine, mut tasks) = page(&runtime, &format!("{base}/page"));
    eval(
        &mut engine,
        r#"
        (async () => {
            for (const path of ['/http.js', '/mime.js']) {
                const failures = await Promise.allSettled([import(path), import(path)]);
                log.push(failures.map(result => result.status).join(','));
                const first = await import(path), again = await import(path);
                log.push(String(first === again && first.value === again.value));
            }
            log.push('evaluations:' + evaluations);
        })().catch(error => log.push('failed:' + error));
    "#,
        "module retry",
    )
    .unwrap();
    run_until(
        &runtime,
        &mut engine,
        &mut tasks,
        "log.length >= 5 || log.some(x => x.startsWith('failed:'))",
    );
    assert_eq!(
        log(&mut engine),
        "rejected,rejected|true|rejected,rejected|true|evaluations:2"
    );
    let heads = runtime.block_on(server).unwrap();
    assert_eq!(
        heads
            .iter()
            .filter(|head| head.starts_with("get /http.js "))
            .count(),
        2
    );
    assert_eq!(
        heads
            .iter()
            .filter(|head| head.starts_with("get /mime.js "))
            .count(),
        2
    );
}

#[test]
fn image_preloads_follow_source_set_changes_and_removals() {
    // HTML #link-type-preload / #update-the-source-set, including a link
    // without href and a media query becoming applicable after insertion.
    let runtime = runtime();
    let (listener, base) = listen(&runtime);
    let server = serve_each(&runtime, listener, 5, |_| {
        b"HTTP/1.1 200 OK\r\nContent-Type: image/svg+xml\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
    });
    let (mut engine, mut tasks) = page(&runtime, &format!("{base}/page"));
    eval(
        &mut engine,
        r#"
        const root = document.createElement('html'); document.appendChild(root);
        const link = document.createElement('link');
        link.rel = 'preload'; link.as = 'image'; link.href = '/fallback.svg';
        link.imageSrcset = '/small.svg 10w, /medium.svg 20w, /large.svg 30w';
        link.imageSizes = '10px';
        link.onload = () => {
            log.push('load');
            if (log.length === 1) link.imageSizes = '20px';
            else if (log.length === 2) link.removeAttribute('imagesizes');
            else if (log.length === 3) link.removeAttribute('imagesrcset');
            else if (log.length === 4) {
                const only = document.createElement('link');
                only.rel = 'preload'; only.as = 'image'; only.imageSrcset = '/only.svg 1x';
                only.media = 'not all'; only.onload = () => log.push('only');
                root.appendChild(only); only.media = 'all';
            }
        };
        root.appendChild(link);
    "#,
        "responsive preloads",
    )
    .unwrap();
    run_until(&runtime, &mut engine, &mut tasks, "log.length === 5");
    assert_eq!(log(&mut engine), "load|load|load|load|only");
    let heads = runtime.block_on(server).unwrap();
    let paths: Vec<_> = heads
        .iter()
        .map(|head| head.split_whitespace().nth(1).unwrap())
        .collect();
    assert_eq!(
        paths,
        [
            "/small.svg",
            "/medium.svg",
            "/large.svg",
            "/fallback.svg",
            "/only.svg"
        ]
    );
}

#[test]
fn font_face_sets_load_matching_css_faces_and_keep_live_connections() {
    // CSS Font Loading 3 #document-font-face-set, #font-face-css-connection,
    // #font-face-set-load and #font-face-set-check. The third family must
    // remain unrequested when only the first family's two ranges are loaded.
    let runtime = runtime();
    let (listener, base) = listen(&runtime);
    let server = serve_each(&runtime, listener, 2, |_| {
        let bytes = include_bytes!("../assets/fonts/dejavu/DejaVuSans.ttf");
        let mut response = format!("HTTP/1.1 200 OK\r\nContent-Type: font/ttf\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", bytes.len()).into_bytes();
        response.extend_from_slice(bytes);
        response
    });
    let (mut engine, mut tasks) = page(&runtime, &format!("{base}/page"));
    eval(&mut engine, r#"
        const root = document.createElement('html'); document.appendChild(root);
        const style = document.createElement('style');
        style.textContent = `
            @font-face {font-family: Review;src:url(/latin.ttf);unicode-range:U+0-7F}
            @font-face {font-family: Review;src:url(/cyrillic.ttf);unicode-range:U+400-4FF}
            @font-face {font-family: Other;src:url(/unused.ttf)}
        `;
        root.appendChild(style);
        const fonts = document.fonts, before = Array.from(fonts);
        log.push('size:' + fonts.size);
        log.push('iterator:' + (fonts.keys === fonts.values) + ':' + (fonts[Symbol.iterator] === fonts.values) + ':' + fonts.keys.name);
        log.push('check:' + fonts.check('12px Review','a') + ':' + fonts.check('12px Review','') + ':' + fonts.check('12px Missing'));
        let syntax = ''; try { fonts.check('inherit'); } catch(e) {syntax = e.name;}
        log.push('syntax:' + syntax);
        (async () => {
            const loaded = await fonts.load('12px Review','aЖ');
            log.push('loaded:' + loaded.length + ':' + (loaded[0] === before[0]) + ':' + (loaded[1] === before[1]));
            log.push('status:' + loaded.map(f => f.status).join(',') + ':' + fonts.check('12px Review','aЖ'));
            log.push('empty:' + (await fonts.load('12px Other','')).length);
            log.push('delete:' + fonts.delete(before[0])); fonts.clear(); log.push('clear:' + fonts.size);
            before[0].weight = '700';
            log.push('to-css:' + style.sheet.cssRules[0].style.fontWeight);
            style.sheet.cssRules[0].style.fontWeight = '500'; log.push('from-css:' + before[0].weight);
            style.sheet.cssRules[0].style.setProperty('src','url(/replacement.ttf)');
            log.push('replace:' + (Array.from(fonts)[0] !== before[0]));
            style.sheet.deleteRule(0); log.push('remove:' + fonts.size);
            // Removed objects retain their former descriptors and can be
            // manually added; their old CSS connection never comes back.
            fonts.add(before[0]); log.push('manual:' + fonts.has(before[0]) + ':' + fonts.delete(before[0]));
            log.push('done');
        })().catch(error => log.push('failed:' + error));
    "#, "font loading CSS connection").unwrap();
    run_until(
        &runtime,
        &mut engine,
        &mut tasks,
        "log.includes('done') || log.some(x => x.startsWith('failed:'))",
    );
    assert_eq!(
        log(&mut engine),
        "size:3|iterator:true:true:values|check:false:true:true|syntax:SyntaxError|loaded:2:true:true|status:loaded,loaded:true|empty:0|delete:false|clear:3|to-css:700|from-css:500|replace:true|remove:2|manual:true:true|done"
    );
    let heads = runtime.block_on(server).unwrap();
    assert!(heads.iter().any(|head| head.starts_with("get /latin.ttf ")));
    assert!(
        heads
            .iter()
            .any(|head| head.starts_with("get /cyrillic.ttf "))
    );
}

#[test]
fn font_descriptor_dictionaries_read_and_convert_each_member_once() {
    let runtime = runtime();
    let (mut engine, _tasks) = page(&runtime, "https://example.test/page");
    eval(&mut engine, r#"
        const reads=[];
        const face=new FontFace('Converted','local("DejaVu Sans")',{
            get width(){reads.push('width');return '125%'},
            get weight(){reads.push('weight');return {toString(){reads.push('convert');return '700'}}}
        });
        log.push(reads.join(','),face.weight,face.stretch,face.status);
    "#, "FontFace dictionary conversion").unwrap();
    assert_eq!(log(&mut engine), "weight,convert,width|700|125%|unloaded");
}

#[test]
fn font_sources_include_shadow_and_adopted_rules_and_disconnect_removed_faces() {
    let runtime = runtime();
    let (mut engine, _tasks) = page(&runtime, "https://example.test/page");
    eval(&mut engine, r#"
        const root=document.createElement('html'); document.appendChild(root);
        const host=document.createElement('div'); root.appendChild(host);
        const shadow=host.attachShadow({mode:'closed'}), style=document.createElement('style');
        style.textContent='@font-face{font-family:Shadow;src:local("DejaVu Sans")}'; shadow.appendChild(style);
        const sheet=new CSSStyleSheet(); sheet.replaceSync('@font-face{font-family:Adopted;src:local("DejaVu Sans")}');
        document.adoptedStyleSheets=[sheet];
        const fonts=document.fonts, old=Array.from(fonts).find(f=>f.family==='Shadow');
        log.push(Array.from(fonts,f=>f.family).sort().join(','));
        log.push(String(fonts.delete(old))); style.remove(); log.push(String(fonts.has(old)));
        shadow.appendChild(style);
        log.push(String(Array.from(fonts).find(f=>f.family==='Shadow')!==old));
        old.family='Detached'; log.push(style.sheet.cssRules[0].style.fontFamily);
        document.adoptedStyleSheets=[]; log.push('remaining:'+fonts.size);
    "#, "shadow font source").unwrap();
    assert_eq!(
        log(&mut engine),
        "Adopted,Shadow|false|false|true|Shadow|remaining:1"
    );
}

#[test]
fn font_loading_uses_source_fallbacks_and_rejects_failed_decoding() {
    let runtime = runtime();
    let (listener, base) = listen(&runtime);
    let server = serve_each(&runtime, listener, 2, |head| {
        let status = if head.starts_with("get /missing.ttf ") {
            "404 Not Found"
        } else {
            "200 OK"
        };
        format!("HTTP/1.1 {status}\r\nContent-Type: font/ttf\r\nContent-Length: 4\r\nConnection: close\r\n\r\noops").into_bytes()
    });
    let (mut engine, mut tasks) = page(&runtime, &format!("{base}/page"));
    eval(&mut engine, r#"
        (async()=>{
            const fallback=new FontFace('Fallback','url(/bad.ttf),url(/missing.ttf),local("DejaVu Sans")');
            await fallback.load(); log.push('fallback:'+fallback.status);
            const failed=new FontFace('Failed','url(/bad.ttf)');
            try {await failed.load();} catch(e) {log.push(e.name+':'+failed.status);}
            log.push('done');
        })().catch(e=>log.push('failed:'+e));
    "#, "font source fallbacks").unwrap();
    run_until(
        &runtime,
        &mut engine,
        &mut tasks,
        "log.includes('done') || log.some(x=>x.startsWith('failed:'))",
    );
    assert_eq!(log(&mut engine), "fallback:loaded|NetworkError:error|done");
    assert_eq!(runtime.block_on(server).unwrap().len(), 2);
}

#[test]
fn worker_font_source_loads_for_transferred_offscreen_canvas() {
    // CSS Font Loading #fontfacesource-workers and HTML
    // #canvastextdrawingstyles: the receiver's fonts, initially empty.
    let runtime = runtime();
    let (listener, base) = listen(&runtime);
    let server = serve_each(&runtime, listener, 2, |head| {
        let (mime, bytes): (&str, &[u8]) = if head.starts_with("get /workers/paint.js ") {
            ("text/javascript", br#"
                onmessage = async event => {
                    try {
                        const out=['empty:'+fonts.size+':'+(fonts===self.fonts)];
                        const ctx=event.data.getContext('2d'); ctx.font='24px WorkerFont, monospace';
                        const before=ctx.measureText('iiii').width;
                        const face=new FontFace('WorkerFont','url(font.ttf)',{display:'optional'});
                        fonts.add(face);
                        const events=[];
                        fonts.onloading=e=>events.push('loading:'+e.isTrusted);
                        fonts.onloadingdone=e=>events.push('done:'+e.isTrusted+':'+(e.fontfaces[0]===face));
                        ctx.fillText('iiii',0,30); // demand, without explicit load()
                        await fonts.ready;
                        out.push('loaded:'+face.status+':'+fonts.check('24px WorkerFont')+':'+(ctx.measureText('iiii').width<before));
                        ctx.clearRect(0,0,160,50); ctx.fillText('iiii',0,30);
                        const bad=new FontFace('Broken',new Uint8Array([1,2,3]));
                        try { await bad.loaded; } catch(e) {out.push('bad:'+e.name);}
                        await new Promise(r=>setTimeout(r,0));
                        out.push(events.join(','));
                        fonts.delete(face); out.push('removed:'+(ctx.measureText('iiii').width===before));
                        postMessage(out.join('|'));
                    } catch(e) {postMessage('failed:'+e);}
                };
            "#)
        } else {
            assert!(head.starts_with("get /workers/font.ttf "), "{head}");
            (
                "font/ttf",
                include_bytes!("../assets/fonts/dejavu/DejaVuSerif.ttf"),
            )
        };
        let mut response = format!("HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",bytes.len()).into_bytes();
        response.extend_from_slice(bytes);
        response
    });
    let (mut engine, mut tasks) = page(&runtime, &format!("{base}/page"));
    eval(&mut engine, r#"
        const canvas=document.createElement('canvas'); canvas.width=160;canvas.height=50;
        const worker=new Worker('/workers/paint.js');
        worker.onmessage=e=>log.push(e.data);
        worker.onerror=e=>log.push('worker-error:'+e.message);
        const offscreen=canvas.transferControlToOffscreen();worker.postMessage(offscreen,[offscreen]);
    "#, "worker font source").unwrap();
    run_until(&runtime, &mut engine, &mut tasks, "log.length>0");
    assert_eq!(
        log(&mut engine),
        "empty:0:true|loaded:loaded:true:true|bad:SyntaxError|loading:true,done:true:true|removed:true"
    );
    assert_eq!(string_value(&mut engine, "document.fonts.size"), "0");
    eval(&mut engine, "worker.terminate()", "terminate").unwrap();
    assert_eq!(runtime.block_on(server).unwrap().len(), 2);
}

#[test]
fn blob_worker_font_loading_uses_its_settings_origin_and_still_enforces_cors() {
    let runtime = runtime();
    let (listener, base) = listen(&runtime);
    let (cross_listener, cross) = listen(&runtime);
    let response: fn(&str) -> Vec<u8> = |_| {
        let bytes = include_bytes!("../assets/fonts/dejavu/DejaVuSerif.ttf");
        let mut response = format!("HTTP/1.1 200 OK\r\nContent-Type: font/ttf\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",bytes.len()).into_bytes();
        response.extend_from_slice(bytes);
        response
    };
    let server = serve_each(&runtime, listener, 1, response);
    let cross_server = serve_each(&runtime, cross_listener, 1, response);
    let (mut engine, mut tasks) = page(&runtime, &format!("{base}/page"));
    let script = format!(
        r#"(async()=>{{try {{
        const face=new FontFace('BlobWorker','url({base}/font.ttf)');fonts.add(face);
        const c=new OffscreenCanvas(100,50),x=c.getContext('2d');x.font='24px BlobWorker,monospace';
        const before=x.measureText('iiii').width;await fonts.ready;
        let blocked='';try{{await new FontFace('Cross','url({cross}/font.ttf)').load();}}catch(e){{blocked=e.name;}}
        postMessage([face.status,x.measureText('iiii').width<before,blocked].join('|'));
    }}catch(e){{postMessage('failed:'+e);}}}})();"#
    );
    eval(&mut engine,&format!(r#"const worker=new Worker(URL.createObjectURL(new Blob([{}],{{type:'text/javascript'}})));worker.onmessage=e=>log.push(e.data);worker.onerror=e=>log.push('error:'+e.message);"#,serde_json::to_string(&script).unwrap()),"blob worker fonts").unwrap();
    run_until(&runtime, &mut engine, &mut tasks, "log.length>0");
    assert_eq!(log(&mut engine), "loaded|true|NetworkError");
    eval(&mut engine, "worker.terminate()", "terminate").unwrap();
    assert_eq!(runtime.block_on(server).unwrap().len(), 1);
    assert_eq!(runtime.block_on(cross_server).unwrap().len(), 1);
}

#[test]
fn font_ready_waits_for_pending_stylesheets_and_their_fonts() {
    // CSS Font Loading #font-face-set-ready: completing document loading
    // does not settle ready while a stylesheet can still discover fonts.
    let runtime = runtime();
    let (listener, base) = listen(&runtime);
    let server = serve_each(&runtime, listener, 2, |head| {
        let (kind, bytes): (&str, &[u8]) = if head.starts_with("get /late.css ") {
            (
                "text/css",
                b"@font-face{font-family:Late;src:url(/late.ttf)}",
            )
        } else {
            (
                "font/ttf",
                include_bytes!("../assets/fonts/dejavu/DejaVuSans.ttf"),
            )
        };
        let mut response = format!("HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", bytes.len()).into_bytes();
        response.extend_from_slice(bytes);
        response
    });
    let (mut engine, mut tasks) = page(&runtime, &format!("{base}/page"));
    eval(&mut engine, r#"
        const root=document.createElement('html'); document.appendChild(root);
        const text=document.createElement('span'); text.style.fontFamily='Late'; text.textContent='Font readiness'; root.appendChild(text);
        const link=document.createElement('link'); link.rel='stylesheet'; link.href='/late.css';
        link.onload=()=>log.push('sheet'); root.appendChild(link);
        __trust.setDocumentReadiness('complete');
        document.fonts.ready.then(()=>log.push('ready:'+document.fonts.size+':'+Array.from(document.fonts)[0].status));
        log.push('initial');
    "#, "font ready environment").unwrap();
    run_until(
        &runtime,
        &mut engine,
        &mut tasks,
        "log.some(x=>x.startsWith('ready:'))",
    );
    assert_eq!(log(&mut engine), "initial|sheet|ready:1:loaded");
    assert_eq!(runtime.block_on(server).unwrap().len(), 2);
}

#[test]
fn font_demand_loads_used_faces_in_layout_and_canvas_only() {
    let runtime = runtime();
    let (listener, base) = listen(&runtime);
    let server = serve_each(&runtime, listener, 4, |_| {
        let bytes = include_bytes!("../assets/fonts/dejavu/DejaVuSans.ttf");
        let mut response = format!("HTTP/1.1 200 OK\r\nContent-Type: font/ttf\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", bytes.len()).into_bytes();
        response.extend_from_slice(bytes);
        response
    });
    let (mut engine, mut tasks) = page(&runtime, &format!("{base}/page"));
    eval(
        &mut engine,
        r#"
        (async()=>{
            const root=document.createElement('html'); document.appendChild(root);
            root.innerHTML=`<style>
                @font-face{font-family:Web;src:url(/latin.ttf);unicode-range:U+0-FF}
                @font-face{font-family:Web;src:url(/cyrillic.ttf);unicode-range:U+400-4FF}
                @font-face{font-family:Web;src:url(/bold.ttf);font-weight:700}
                @font-face{font-family:Hidden;src:url(/hidden.ttf)}
                @font-face{font-family:Unused;src:url(/unused.ttf)}
                </style><p id=text style='font-family:Web'>Latin</p>
                <p style='display:none;font-family:Hidden'>Hidden text</p>`;
            const manual=new FontFace('Canvas','url(/canvas.ttf)'); document.fonts.add(manual);
            __trust.setDocumentReadiness('complete');
            await document.fonts.ready;
            log.push(Array.from(document.fonts,f=>f.status).join(','));
            text.style.fontWeight='700'; await document.fonts.ready;
            text.style.fontWeight='400'; text.textContent='Ж'; await document.fonts.ready;
            const canvas=document.createElement('canvas'), ctx=canvas.getContext('2d');
            ctx.font='20px Canvas'; ctx.measureText('Canvas use'); await document.fonts.ready;
            log.push(Array.from(document.fonts,f=>f.status).join(','));
            log.push('done');
        })().catch(e=>log.push('failed:'+e));
    "#,
        "font demand",
    )
    .unwrap();
    run_until(
        &runtime,
        &mut engine,
        &mut tasks,
        "log.includes('done') || log.some(x=>x.startsWith('failed:'))",
    );
    assert_eq!(
        log(&mut engine),
        "loaded,unloaded,unloaded,unloaded,unloaded,unloaded|loaded,loaded,loaded,unloaded,unloaded,loaded|done"
    );
    let requests = runtime.block_on(server).unwrap();
    for (request, path) in
        requests
            .iter()
            .zip(["/latin.ttf", "/bold.ttf", "/cyrillic.ttf", "/canvas.ttf"])
    {
        assert!(request.starts_with(&format!("get {path} ")), "{request}");
    }
}

#[test]
fn font_face_loading_decodes_binary_data_and_updates_layout() {
    // CSS Font Loading #font-face-constructor / #font-face-set-css: bytes
    // must decode, and adding/removing a loaded face changes real layout.
    let runtime = runtime();
    let (mut engine, mut tasks) = page(&runtime, "https://example.test/page");
    let bytes = include_bytes!("../assets/fonts/dejavu/DejaVuSans.ttf");
    let buffer = engine
        .ctx()
        .make_array_buffer(bytes)
        .unwrap_or_else(|_| panic!("font buffer"));
    let global = engine.global_this();
    engine
        .ctx()
        .member_set(&global, "fontBytes", buffer)
        .unwrap_or_else(|_| panic!("font global"));
    eval(&mut engine, r#"
        const root = document.createElement('html'); document.appendChild(root);
        const body = document.createElement('body'); root.appendChild(body);
        const span = document.createElement('span'); span.style.cssText='font:30px LoadedFont,monospace';
        span.textContent='iiiiWWWW'; body.appendChild(span);
        const before = span.getBoundingClientRect().width;
        const canvas = document.createElement('canvas'); body.appendChild(canvas);
        const context = canvas.getContext('2d'); context.font = '30px LoadedFont, monospace';
        const canvasBefore = context.measureText('iiiiWWWW').width;
        const fonts = document.fonts;
        const face = new FontFace('LoadedFont',new Uint8Array(fontBytes));
        const promise = face.loaded; log.push('initial:' + face.status + ':' + (face.load() === promise));
        fonts.add(face);
        const events=[]; for(const name of ['loading','loadingdone','loadingerror']) fonts.addEventListener(name,e=>events.push(e.type));
        (async()=>{
            await face.loaded;
            log.push('decoded:' + face.status + ':' + (Math.abs(span.getBoundingClientRect().width-before)>1));
            log.push('canvas:' + (Math.abs(context.measureText('iiiiWWWW').width-canvasBefore)>1));
            fonts.delete(face); log.push('removed:' + (Math.abs(span.getBoundingClientRect().width-before)<0.01));
            const url=URL.createObjectURL(new Blob([fontBytes],{type:'font/ttf'}));
            const blobFace=new FontFace('BlobFont','url("'+url+'")');
            await blobFace.load(); log.push('blob:'+blobFace.status); URL.revokeObjectURL(url);
            const bad = new FontFace('Broken',new Uint8Array([0,1,0,0]));
            try {await bad.loaded;} catch(e) {log.push('bad:' + e.name + ':' + bad.status);}
            __trust.setDocumentReadiness('complete');
            await fonts.ready; log.push('ready:' + fonts.status); log.push('done');
        })().catch(e=>log.push('failed:'+e));
    "#, "binary FontFace").unwrap();
    run_until(
        &runtime,
        &mut engine,
        &mut tasks,
        "log.includes('done') || log.some(x=>x.startsWith('failed:'))",
    );
    assert_eq!(
        log(&mut engine),
        "initial:unloaded:true|decoded:loaded:true|canvas:true|removed:true|blob:loaded|bad:SyntaxError:error|ready:loaded|done"
    );
}

#[test]
fn script_sees_header_values_as_byte_strings() {
    // Fetch #header-value: byte sequences, which Headers, getResponseHeader
    // and getAllResponseHeaders expose isomorphic decoded (Web IDL
    // ByteString); a ByteString request header value goes out as its bytes.
    let runtime = runtime();
    let (listener, base) = listen(&runtime);
    let server = runtime.spawn(async move {
        let mut heads = Vec::new();
        for _ in 0..2 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut head = Vec::new();
            let mut buffer = [0u8; 2048];
            while !head.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = socket.read(&mut buffer).await.unwrap();
                if read == 0 {
                    break;
                }
                head.extend_from_slice(&buffer[..read]);
            }
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nX-Name: caf\xc3\xa9\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
            heads.push(head);
        }
        heads
    });
    let (mut engine, mut tasks) = page(&runtime, &format!("{base}/page"));
    eval(
        &mut engine,
        r#"
        const codes = text => [...text].map(c => c.charCodeAt(0).toString(16)).join(' ');
        fetch('/fetch', {headers: {'X-Sent': 'café'}}).then(response => {
            log.push(codes(response.headers.get('x-name')));
            const xhr = new XMLHttpRequest();
            xhr.open('GET', '/xhr');
            xhr.onload = () => log.push(codes(xhr.getResponseHeader('X-Name')),
                xhr.getAllResponseHeaders().includes('x-name: cafÃ©\r\n'));
            xhr.send();
        }).catch(error => log.push('failed:' + error));
        "#,
        "header bytes",
    )
    .unwrap();
    run_until(&runtime, &mut engine, &mut tasks, "log.length >= 3");
    assert_eq!(log(&mut engine), "63 61 66 c3 a9|63 61 66 c3 a9|true");
    let heads = runtime.block_on(server).unwrap();
    assert!(
        heads[0]
            .windows(b"X-Sent: caf\xe9".len())
            .any(|window| window == b"X-Sent: caf\xe9"),
        "{}",
        String::from_utf8_lossy(&heads[0])
    );
}

#[test]
fn a_child_windows_import_starts_its_fetch_as_import_runs() {
    // HTML #hostloadimportedmodule for a same-agent child Window: the fetch
    // starts while import() runs, before the page thread takes the task.
    let runtime = runtime();
    let (listener, base) = listen(&runtime);
    let server = serve_each(&runtime, listener, 1, |_| {
        b"HTTP/1.1 200 OK\r\nContent-Type: text/javascript\r\nContent-Length: 22\r\nConnection: close\r\n\r\nexport const v = 'ok';"
            .to_vec()
    });
    let (mut engine, mut tasks) = page(&runtime, &format!("{base}/page"));
    eval(
        &mut engine,
        &format!(
            r#"
            const html = document.createElement('html'), body = document.createElement('body');
            document.appendChild(html); html.appendChild(body);
            const frame = document.createElement('iframe'); body.appendChild(frame);
            const context = __dom_allocate_job_context();
            __dom_load_frame(frame.__id, '<body></body>', '{base}/frame');
            frame.__contentDoc = undefined;
            globalThis.child = __dom_create_window_realm(context, frame.__id, '{base}/frame',
                __trust_cfg, window, window, frame, 0, '');
            child.eval("globalThis.result = 'pending'; import('/child.js')" +
                ".then(m => globalThis.result = m.v, e => globalThis.result = 'failed:' + e)");
            "#
        ),
        "child Window import",
    )
    .unwrap();
    // No host task has been dispatched: the request left as import() ran.
    let heads = runtime
        .block_on(async { tokio::time::timeout(Duration::from_secs(10), server).await })
        .expect("the child Window's import fetch started without its task")
        .unwrap();
    assert!(heads[0].starts_with("get /child.js "), "{}", heads[0]);
    run_until(&runtime, &mut engine, &mut tasks, "child.result === 'ok'");
}

#[test]
fn stopping_the_document_leaves_its_workers_script_fetches() {
    // HTML #run-a-worker fetches the worker's script as a non-subresource
    // request (destination "worker") whose reserved client is the worker's
    // own environment; Fetch #concept-fetch records only subresource
    // requests in the client's fetch group, so stopping the Document (HTML
    // #abort-a-document) leaves it, and the worker still starts. A running
    // worker's fetches have the worker as client and are not touched either.
    let runtime = runtime();
    let (listener, base) = listen(&runtime);
    let (release, released) = tokio::sync::watch::channel(false);
    let arrived = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = arrived.clone();
    let server = runtime.spawn(async move {
        let mut connections = Vec::new();
        for _ in 0..3 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let counter = counter.clone();
            let mut released = released.clone();
            connections.push(tokio::spawn(async move {
                let head = read_head(&mut socket).await;
                let path = head.split(' ').nth(1).unwrap_or("").to_string();
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let body = match path.as_str() {
                    "/running.js" => {
                        "fetch('/in-worker').then(r => r.text()).then(t => postMessage(t));"
                    }
                    "/in-worker" => "done",
                    _ => "postMessage('loaded');",
                };
                if path != "/running.js" {
                    let _ = released.wait_for(|released| *released).await;
                }
                socket
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/javascript\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        )
                        .as_bytes(),
                    )
                    .await
                    .unwrap();
                path
            }));
        }
        let mut paths = Vec::new();
        for connection in connections {
            paths.push(connection.await.unwrap());
        }
        paths.sort();
        paths
    });
    let (mut engine, mut tasks) = page(&runtime, &format!("{base}/page"));
    eval(
        &mut engine,
        r#"
        globalThis.running = new Worker('/running.js');
        running.onmessage = event => log.push('running:' + event.data);
        running.onerror = () => log.push('running error');
        "#,
        "running worker",
    )
    .unwrap();
    let wait_for = |engine: &mut trust_js::Engine,
                    tasks: &mut tokio::sync::mpsc::UnboundedReceiver<TrustJsHostTask>,
                    count: usize| {
        let deadline = Instant::now() + Duration::from_secs(10);
        while arrived.load(std::sync::atomic::Ordering::SeqCst) < count {
            assert!(
                Instant::now() < deadline,
                "{count} requests reached the server"
            );
            run_for(&runtime, engine, tasks, Duration::from_millis(20));
        }
    };
    wait_for(&mut engine, &mut tasks, 2);
    eval(
        &mut engine,
        r#"
        globalThis.loading = new Worker('/loading.js');
        loading.onerror = () => log.push('loading error');
        loading.onmessage = event => log.push('loading:' + event.data);
        "#,
        "loading worker",
    )
    .unwrap();
    wait_for(&mut engine, &mut tasks, 3);
    eval(&mut engine, "window.stop()", "window.stop()").unwrap();
    release.send(true).unwrap();
    run_until(
        &runtime,
        &mut engine,
        &mut tasks,
        "log.includes('running:done') && log.includes('loading:loaded')",
    );
    assert!(!log(&mut engine).contains("error"), "{}", log(&mut engine));
    assert_eq!(
        runtime.block_on(server).unwrap(),
        ["/in-worker", "/loading.js", "/running.js"]
    );
}

#[test]
fn a_page_navigation_aborts_the_old_documents_fetches_first() {
    // HTML #navigate queues "abort a document and its descendants" for the
    // navigable's active document as a cross-document navigation begins:
    // its XMLHttpRequest fires abort (here sending a beacon, a keepalive
    // fetch that outlives the document) before the navigation leaves the
    // page actor, and the XHR's connection closes.
    let runtime = runtime();
    let (listener, base) = listen(&runtime);
    let (arrivals, mut arrived) = tokio::sync::mpsc::unbounded_channel::<(String, bool)>();
    runtime.spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let arrivals = arrivals.clone();
            tokio::spawn(async move {
                let head = read_head(&mut socket).await;
                let path = head.split(' ').nth(1).unwrap_or("").to_string();
                if path == "/slow" {
                    let _ = arrivals.send((path.clone(), false));
                    let mut buffer = [0u8; 64];
                    let closed = tokio::time::timeout(Duration::from_secs(10), async {
                        loop {
                            match socket.read(&mut buffer).await {
                                Ok(0) | Err(_) => return true,
                                Ok(_) => {}
                            }
                        }
                    })
                    .await
                    .unwrap_or(false);
                    let _ = arrivals.send((String::from("/slow closed"), closed));
                } else {
                    let _ = socket
                        .write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n")
                        .await;
                    let _ = arrivals.send((path, false));
                }
            });
        }
    });
    let html = r#"<!doctype html><body><script>
        const xhr = new XMLHttpRequest();
        xhr.open('GET', '/slow');
        xhr.onabort = () => navigator.sendBeacon('/aborted', 'gone');
        xhr.send();
        setTimeout(() => { location.href = '/next'; }, 1000);
    </script>"#;
    let mut env = crate::js::PageEnv::bare(&format!("{base}/page"));
    env.net = Some(runtime.handle().clone());
    let (_handle, mut events) = crate::js::spawn_page(html.into(), env);
    let navigated = runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                match events.recv().await {
                    Some(crate::js::PageEvt::Navigate(url, ..)) => return url,
                    Some(crate::js::PageEvt::Trouble(errors)) => panic!("page: {errors:?}"),
                    Some(_) => {}
                    None => panic!("page closed before navigating"),
                }
            }
        })
        .await
        .expect("navigation")
    });
    assert_eq!(navigated, format!("{base}/next"));
    let mut seen = Vec::new();
    runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(10), async {
            while !(seen.iter().any(|(path, _)| path == "/aborted")
                && seen.iter().any(|(path, _)| path == "/slow closed"))
            {
                seen.push(arrived.recv().await.unwrap());
            }
        })
        .await
        .unwrap_or_else(|_| panic!("abort effects: {seen:?}"));
    });
    assert!(
        seen.contains(&(String::from("/slow closed"), true)),
        "{seen:?}"
    );
}

#[test]
fn fetch_redirect_modes_stop_before_the_next_request_and_cache_manual_responses() {
    // Fetch #http-fetch / #concept-response-location-url: redirect mode is
    // applied to the redirect status before Location is parsed/followed.
    let rt = runtime();
    let (listener, base) = listen(&rt);
    let targets = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count = targets.clone();
    let server = rt.spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let head = read_head(&mut socket).await;
            let response = if head.starts_with("get /target ") {
                count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                "HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\ntarget"
            } else if head.starts_with("get /missing ") {
                "HTTP/1.1 302 Found\r\nContent-Length: 4\r\nConnection: close\r\n\r\nbody"
            } else if head.starts_with("get /multi ") {
                "HTTP/1.1 302 Found\r\nLocation: /target\r\nLocation: /other\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            } else {
                "HTTP/1.1 302 Found\r\nLocation: /target\r\nContent-Length: 4\r\nConnection: close\r\n\r\nbody"
            };
            socket.write_all(response.as_bytes()).await.unwrap();
        }
    });
    let (mut engine, mut tasks) = page(&rt, &format!("{base}/page"));
    eval(&mut engine, r#"
        (async () => {
            for (const path of ['/redirect', '/missing', '/multi']) {
                try { await fetch(path, {redirect:'error'}); log.push('bad'); }
                catch(e) { log.push('error:' + e.name); }
                const r = await fetch(path, {redirect:'manual'});
                log.push([r.type,r.status,r.body === null,r.redirected,r.headers.get('location'),
                    r.url === new URL(path,location.href).href,await r.text()].join(':'));
                const c = await caches.open('redirects');
                await c.put(path, r.clone());
                const copy = (await c.match(path)).clone();
                log.push([copy.type,copy.status,copy.body===null,await copy.text()].join(':'));
            }
            log.push('follow:' + await (await fetch('/redirect')).text());
            const missing = await fetch('/missing');
            log.push('missing:' + missing.status + ':' + await missing.text());
            try { await fetch('/multi'); log.push('bad'); } catch(e) { log.push('multi:' + e.name); }
            log.push('done');
        })().catch(e => log.push('FAILED:' + e.name + ':' + e.message));
    "#, "redirect modes").unwrap();
    run_until(
        &rt,
        &mut engine,
        &mut tasks,
        "log.includes('done') || log.some(x=>x.startsWith('FAILED'))",
    );
    let mut expected = Vec::new();
    for _ in 0..3 {
        expected.extend([
            "error:TypeError",
            "opaqueredirect:0:true:false::true:",
            "opaqueredirect:0:true:",
        ]);
    }
    expected.extend([
        "follow:target",
        "missing:302:body",
        "multi:TypeError",
        "done",
    ]);
    assert_eq!(log(&mut engine), expected.join("|"));
    assert_eq!(targets.load(std::sync::atomic::Ordering::SeqCst), 1);
    server.abort();
}

#[test]
fn opaque_and_cors_cache_roundtrips_keep_private_bytes_without_exposing_them() {
    // Fetch #concept-filtered-response / #concept-response-clone and SW
    // #cache-put: an opaque cached image keeps real bytes behind a null Body.
    let rt = runtime();
    let (listener, base) = listen(&rt);
    let server = rt.spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            read_head(&mut socket).await;
            socket.write_all(b"HTTP/1.1 200 OK\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Expose-Headers: X-Public\r\nContent-Type: application/octet-stream\r\nX-Public: yes\r\nX-Secret: private\r\nContent-Length: 6\r\nConnection: close\r\n\r\n").await.unwrap();
            socket.write_all(&[0, 1, 127]).await.unwrap();
            tokio::time::sleep(Duration::from_millis(5)).await;
            socket.write_all(&[128, 254, 255]).await.unwrap();
        }
    });
    let (mut engine, mut tasks) = page(&rt, "http://127.0.0.1:1/page");
    eval(&mut engine, &format!(r#"
        const remote={base:?};
        (async () => {{
            const c = await caches.open('opaque');
            let jsonHooks = 0;
            Object.defineProperty(Object.prototype, 'toJSON', {{configurable:true, value() {{ jsonHooks++; return this; }} }});
            const r = await fetch(remote + '/image', {{mode:'no-cors'}});
            const clone = r.clone();
            log.push([r.type,r.status,r.body===null,r.url,r.headers.get('x-secret'),await r.text(),r.bodyUsed].join(':'));
            await c.put('/image', clone);
            await c.put('/image2', (await c.match('/image')).clone());
            const copy = (await c.match('/image2')).clone();
            log.push([copy.type,copy.status,copy.body===null,copy.url,await copy.text(),copy.bodyUsed].join(':'));
            delete Object.prototype.toJSON;
            log.push('json-hooks:' + jsonHooks);
            const cors = await fetch(remote + '/data');
            log.push(cors.type + ':' + cors.headers.get('x-public') + ':' + cors.headers.get('x-secret'));
            await c.put('/cors', cors.clone());
            const cached = (await c.match('/cors')).clone();
            log.push(cached.type + ':' + cached.headers.get('x-public') + ':' + cached.headers.get('x-secret') + ':' + [...await cached.bytes()].join(','));
            log.push('original:' + [...await cors.bytes()].join(','));
            log.push('done');
        }})().catch(e => log.push('FAILED:' + e.name + ':' + e.message));
    "#), "opaque cache").unwrap();
    run_until(
        &rt,
        &mut engine,
        &mut tasks,
        "log.includes('done') || log.some(x=>x.startsWith('FAILED'))",
    );
    assert_eq!(
        log(&mut engine),
        "opaque:0:true::::false|opaque:0:true:::false|json-hooks:0|cors:yes:null|cors:yes:null:0,1,127,128,254,255|original:0,1,127,128,254,255|done"
    );
    // Native-only inspection: no opaque bytes or hidden headers are in the
    // serialized JS cache records. The cache owns native capabilities.
    let storage = engine.ctx().host::<HostState>().unwrap().storage.clone();
    let payloads = private_response_host::cached_payloads(&storage);
    assert_eq!(payloads.len(), 3);
    for payload in payloads {
        assert_eq!(payload.internal.status, 200);
        assert!(
            payload
                .internal
                .headers
                .iter()
                .any(|(name, value)| name == "x-secret" && value == "private")
        );
        if let Some(body) = &payload.body {
            let mut cursor = body.cursor();
            let mut bytes = Vec::new();
            while let Some(piece) = rt
                .block_on(crate::http::ExternalBody::next(&mut cursor))
                .unwrap()
            {
                bytes.extend(piece);
            }
            assert_eq!(bytes, [0, 1, 127, 128, 254, 255]);
        }
    }
    let storage = storage.lock().unwrap();
    let encoded = storage
        .iter()
        .find(|(key, _)| key.starts_with("cache-storage-data:"))
        .unwrap()
        .1
        .values()
        .next()
        .unwrap();
    assert!(!encoded.contains("x-secret"));
    let records: serde_json::Value = serde_json::from_str(encoded).unwrap();
    for record in records.as_array().unwrap() {
        assert!(record["response"]["privateToken"].as_str().is_some());
        if record["response"]["type"] == "opaque" {
            assert_eq!(record["response"]["body64"], "");
        }
    }
    drop(storage);
    server.abort();
}

#[test]
fn opaque_streams_abort_on_the_signal_and_window_stop_after_the_head() {
    let rt = runtime();
    let (listener, base) = listen(&rt);
    let server = rt.spawn(async move {
        let mut clients = Vec::new();
        for _ in 0..2 {
            let (mut socket, _) = listener.accept().await.unwrap();
            clients.push(tokio::spawn(async move {
                read_head(&mut socket).await;
                socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nTransfer-Encoding: chunked\r\n\r\n1\r\nx\r\n").await.unwrap();
                let mut byte = [0];
                matches!(tokio::time::timeout(Duration::from_secs(5), socket.read(&mut byte)).await, Ok(Ok(0) | Err(_)))
            }));
        }
        for client in clients { assert!(client.await.unwrap(), "opaque body connection must be canceled"); }
    });
    let (mut engine, mut tasks) = page(&rt, "http://127.0.0.1:1/page");
    eval(&mut engine, &format!(r#"
        globalThis.opaqueReplies = [];
        (async () => {{
            const controller = new AbortController();
            const first = await fetch({base:?} + '/signal', {{mode:'no-cors', signal:controller.signal}});
            opaqueReplies.push(first);
            log.push(first.type + ':' + first.body);
            controller.abort();
            const second = await fetch({base:?} + '/stop', {{mode:'no-cors'}});
            opaqueReplies.push(second);
            log.push(second.type + ':' + second.body);
            window.stop();
        }})().catch(e => log.push('FAILED:' + e.name + ':' + e.message));
    "#), "opaque cancellation").unwrap();
    run_until(
        &rt,
        &mut engine,
        &mut tasks,
        "log.length >= 2 || log.some(x=>x.startsWith('FAILED'))",
    );
    assert_eq!(log(&mut engine), "opaque:null|opaque:null");
    rt.block_on(server).unwrap();
    run_for(&rt, &mut engine, &mut tasks, Duration::from_millis(50));
    assert!(
        !engine
            .ctx()
            .host::<HostState>()
            .unwrap()
            .private_responses
            .transferring()
    );
}
