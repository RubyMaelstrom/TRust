//! Original localhost fixtures, never third-party challenge code or values.
use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn serve(body: String, extra: &'static str) -> (Url, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
    let server = tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let body = body.clone();
            tokio::spawn(async move {
                let mut request = Vec::new();
                while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                    let mut chunk = [0; 2048];
                    let n = socket.read(&mut chunk).await.unwrap();
                    if n == 0 {
                        return;
                    }
                    request.extend_from_slice(&chunk[..n]);
                }
                let path = std::str::from_utf8(&request)
                    .unwrap()
                    .split_whitespace()
                    .nth(1)
                    .unwrap();
                let (mime, body) = match path {
                    "/data" | "/xhr" => ("text/plain", "original bytes"),
                    "/child" => (
                        "text/html",
                        "<!doctype html><head><link rel='stylesheet' href='/main.css'><script src='/parser.js'></script></head><body>original child",
                    ),
                    "/parser.js" => (
                        "text/javascript",
                        "globalThis.parserTimingVisible=performance.getEntriesByName(new URL('/parser.js',location.href).href).length===1;",
                    ),
                    "/dynamic.js" => (
                        "text/javascript",
                        "globalThis.dynamicTimingVisible=performance.getEntriesByName(new URL('/dynamic.js',location.href).href).length===1;",
                    ),
                    "/module.js" => ("text/javascript", "globalThis.originalModuleLoaded=true;"),
                    "/main.css" | "/dynamic.css" | "/print.css" => {
                        ("text/css", "body { color: black }")
                    }
                    "/image.svg" => (
                        "image/svg+xml",
                        "<svg xmlns='http://www.w3.org/2000/svg' width='2' height='2'><path fill='red' d='M0 0h2v2H0z'/></svg>",
                    ),
                    "/bad-image" => ("image/png", "not an image"),
                    _ => ("text/html", body.as_str()),
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n{extra}\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            });
        }
    });
    (url, server)
}

#[tokio::test]
async fn resource_timing_element_fetches_keep_native_measurements_and_completion_order() {
    let html = r#"<!doctype html><head>
    <link rel="stylesheet" href="/main.css"><link rel="stylesheet" media="print" href="/print.css">
    <link rel="modulepreload" href="/module.js"><script src="/parser.js"></script>
    <script type="module" src="/module.js"></script></head><body><script>
    addEventListener('load',async()=>{
      try {
        const check=(v,m)=>{if(!v)throw Error(m)};
        const entry=(path,type)=>{const rows=performance.getEntriesByName(new URL(path,location.href).href,'resource');
          check(rows.length===1,path+' entry count '+rows.length); const e=rows[0];
          check(e.initiatorType===type,path+' initiator');
          check(e.startTime>=0 && e.fetchStart>=e.startTime && e.requestStart>=e.fetchStart &&
            e.responseStart>=e.requestStart && e.responseEnd>=e.responseStart && e.responseEnd<=performance.now(),path+' timestamps');
          check(e.responseStatus===200 && e.decodedBodySize>0,path+' real response');return e;};
        check(parserTimingVisible,'parser script entry before execution');
        entry('/parser.js','script');
        check(entry('/main.css','css').renderBlockingStatus==='blocking','head sheet blocks');
        check(entry('/print.css','css').renderBlockingStatus==='non-blocking','unmatched media does not block');
        entry('/module.js','script'); check(originalModuleLoaded,'module preload consumed');
        const observed=[];const observer=new PerformanceObserver(list=>observed.push(...list.getEntries()));
        observer.observe({type:'resource'});
        await new Promise((resolve,reject)=>{const s=document.createElement('script');s.src='/dynamic.js';
          s.onload=()=>{try{entry('/dynamic.js','script');check(dynamicTimingVisible,'dynamic entry before execution');resolve()}catch(e){reject(e)}};
          s.onerror=reject;document.head.appendChild(s)});
        await new Promise((resolve,reject)=>{const l=document.createElement('link');l.rel='stylesheet';l.href='/dynamic.css';
          l.onload=()=>{try{check(entry('/dynamic.css','css').renderBlockingStatus==='non-blocking','late sheet');resolve()}catch(e){reject(e)}};
          l.onerror=reject;document.head.appendChild(l)});
        for(const [path,works] of [['/image.svg',true],['/bad-image',false]]) {
          await new Promise((resolve,reject)=>{const i=new Image();
            const done=ok=>{try{check(ok===works,'image decode outcome');entry(path,'img');resolve()}catch(e){reject(e)}};
            i.onload=()=>done(true);i.onerror=()=>done(false);i.src=path});
        }
        await new Promise(resolve=>setTimeout(resolve,100));
        check(observed.length===4,'one observer record per dynamic fetch');observer.disconnect();
        document.body.setAttribute('data-resource-elements','ok');
      } catch(e) {document.body.setAttribute('data-resource-elements',String(e && e.message || e));}
    });</script>"#;
    let (url, server) = serve(String::from(html), "").await;
    let response = fetch(&Request::get(url)).await.unwrap();
    let response = execute_js(response, (80, 24), (8, 16), Default::default()).await;
    let rendered =
        super::tests::navigation_timing_snapshot_after(response, "data-resource-elements=").await;
    server.abort();
    assert!(
        rendered.contains("data-resource-elements=\"ok\""),
        "{rendered}"
    );
}

#[tokio::test]
async fn resource_timing_container_navigation_respects_tao_and_child_ownership() {
    let (cross, cross_server) = serve(String::new(), "").await;
    let (allowed, allowed_server) = serve(String::new(), "Timing-Allow-Origin: *\r\n").await;
    let html = r#"<!doctype html><body><script>
    addEventListener('load',async()=>{
      try {
        const check=(v,m)=>{if(!v)throw Error(m)};
        performance.clearResourceTimings();
        for(const [url,detail] of [[new URL('/child',location.href).href,true],['CROSSchild',false],['ALLOWEDchild',true]]) {
          const f=document.createElement('iframe');
          await new Promise((resolve,reject)=>{f.onload=()=>{try {
            const entries=performance.getEntriesByName(url,'resource');
            check(entries.length===1,'iframe entry before load '+entries.length); const e=entries[0];
            check(e.initiatorType==='iframe' && e.startTime>=0 && e.responseEnd>=e.startTime && e.responseEnd<=performance.now(),'iframe lifetime');
            if(detail) check(e.requestStart>=e.startTime && e.responseStart>=e.requestStart,'allowed detail');
            else check(e.fetchStart===0 && e.requestStart===0 && e.responseStart===0 && e.transferSize===0 &&
              e.decodedBodySize===0 && e.responseStatus===0,'opaque iframe fallback');
            if(new URL(url).origin===location.origin) {
              check(f.contentWindow.parserTimingVisible,'child parser entry before execution');
              const child=f.contentWindow.performance.getEntriesByType('resource');
              check(child.filter(e=>e.initiatorType==='script').length===1 && child.filter(e=>e.initiatorType==='css').length===1,'child owns its resources');
            }
            resolve();
          }catch(e){reject(e)}}; f.src=url;document.body.appendChild(f)});
        }
        check(performance.getEntriesByType('resource').length===3,'only navigation entries in parent');
        document.body.setAttribute('data-resource-container','ok');
      }catch(e){document.body.setAttribute('data-resource-container',String(e && e.message || e))}
    });</script>"#.replace("CROSS",cross.as_str()).replace("ALLOWED",allowed.as_str());
    let (url, server) = serve(html, "").await;
    let response = fetch(&Request::get(url)).await.unwrap();
    let response = execute_js(response, (80, 24), (8, 16), Default::default()).await;
    let rendered =
        super::tests::navigation_timing_snapshot_after(response, "data-resource-container=").await;
    server.abort();
    cross_server.abort();
    allowed_server.abort();
    assert!(
        rendered.contains("data-resource-container=\"ok\""),
        "{rendered}"
    );
}

#[tokio::test]
async fn resource_timing_transport_failure_differs_from_policy_denial() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
    drop(listener);
    let request = Request::subresource(
        url.clone(),
        &url,
        "empty",
        Some(CredentialsMode::SameOrigin),
    );
    let failure = fetch_script_with_timing(&request).await.unwrap_err();
    let timing = failure
        .timing
        .expect("real failed connection has start/end");
    assert!(timing.response_end >= timing.start_time && timing.start_time > 0.0);
    let data = timing.resource_data(url.as_str(), "fetch");
    assert_eq!(data["requestStart"], 0.0);
    assert_eq!(data["transferSize"], 0);
    assert_eq!(data["responseStatus"], 0);
    let mut rejected = request;
    rejected.fetch_policy = Some(FetchPolicy {
        origin: Url::parse("https://original.test").unwrap(),
        mode: RequestMode::SameOrigin,
        credentials: CredentialsMode::SameOrigin,
    });
    assert!(
        fetch_script_with_timing(&rejected)
            .await
            .unwrap_err()
            .timing
            .is_none()
    );
}

#[tokio::test]
async fn resource_timing_cors_and_tao_are_independent() {
    let (url, server) = serve(
        String::from("original"),
        "Access-Control-Allow-Origin: *\r\n",
    )
    .await;
    let client = Url::parse("http://127.0.0.1:1/").unwrap();
    let request = Request::subresource(
        url.clone(),
        &client,
        "empty",
        Some(CredentialsMode::SameOrigin),
    );
    let response = fetch_script_with_timing(&request).await.unwrap();
    let timing = response.timing.unwrap();
    assert!(!timing.resource_timing_allowed && timing.resource_body_exposed);
    let data = timing.resource_data(url.as_str(), "fetch");
    assert_eq!(data["requestStart"], 0.0);
    assert_eq!(data["responseStatus"], 200);
    assert_eq!(data["decodedBodySize"], 8);
    server.abort();

    let (url, server) = serve(String::from("original"), "Timing-Allow-Origin: *\r\n").await;
    let mut request = Request::subresource(url.clone(), &client, "empty", None);
    request.fetch_policy = Some(FetchPolicy {
        origin: client.clone(),
        mode: RequestMode::NoCors,
        credentials: CredentialsMode::Omit,
    });
    let response = fetch_script_with_timing(&request).await.unwrap();
    assert_eq!(response.status, 0);
    let timing = response.timing.unwrap();
    assert!(timing.resource_timing_allowed && !timing.resource_body_exposed);
    let data = timing.resource_data(url.as_str(), "fetch");
    assert!(data["requestStart"].as_f64().unwrap() > 0.0);
    assert_eq!(data["responseStatus"], 0);
    assert_eq!(data["decodedBodySize"], 0);
    assert_eq!(data["transferSize"], 0);
    request.fetch_policy.as_mut().unwrap().mode = RequestMode::Cors;
    request.fetch_metadata.as_mut().unwrap().mode = "cors";
    assert!(
        fetch_script_with_timing(&request)
            .await
            .unwrap_err()
            .timing
            .is_none()
    );
    server.abort();
}

#[tokio::test]
async fn resource_timing_real_fetch_and_xhr_finish_before_author_completion() {
    let html = r#"<!doctype html><body><script>
    addEventListener('load',async()=>{
        const check=(v,m)=>{if(!v)throw Error(m)};
        const received=[];
        const observer=new PerformanceObserver(list=>received.push(...list.getEntries()));
        observer.observe({type:'resource'});
        performance.clearResourceTimings();
        await fetch('/data').then(r=>r.text());
        await fetch('/data').then(r=>r.text());
        await new Promise((resolve,reject)=>{const x=new XMLHttpRequest();x.open('GET','/xhr');x.onerror=reject;
            x.onload=()=>{try{check(performance.getEntriesByType('resource').filter(e=>e.initiatorType==='xmlhttprequest').length===1,'XHR entry before load');resolve()}catch(e){reject(e)}};x.send()});
        const rows=performance.getEntriesByType('resource');
        check(rows.length===3,'each real fetch has an entry');
        check(rows.filter(e=>e.initiatorType==='fetch').length===2,'repeat requests');
        check(rows.every(e=>e instanceof PerformanceResourceTiming && e.startTime>=0 && e.duration>=0 &&
            e.fetchStart>=e.startTime && e.requestStart>=e.fetchStart && e.responseStart>=e.requestStart &&
            e.responseEnd>=e.responseStart && e.responseEnd<=performance.now() &&
            e.encodedBodySize===14 && e.decodedBodySize===14 && e.transferSize===314 && e.responseStatus===200),'native measurements');
        setTimeout(()=>{check(received.length===3,'observer deliveries');observer.disconnect();document.body.setAttribute('data-resource-native','ok')},100);
    });</script>"#;
    let (url, server) = serve(String::from(html), "").await;
    let response = fetch(&Request::get(url)).await.unwrap();
    let response = execute_js(response, (80, 24), (8, 16), Default::default()).await;
    assert!(
        response
            .js
            .as_ref()
            .is_some_and(|outcome| outcome.errors.is_empty()),
        "original timing page errors: {:?}",
        response.js.as_ref().map(|outcome| &outcome.errors)
    );
    let rendered =
        super::tests::navigation_timing_snapshot_after(response, "data-resource-native=").await;
    server.abort();
    assert!(
        rendered.contains("data-resource-native=\"ok\""),
        "{rendered}"
    );
}

#[tokio::test]
async fn resource_timing_iframe_fetch_belongs_to_the_initiating_global() {
    let child_html = r#"<!doctype html><body><script>
    addEventListener('load', async()=>{
        await fetch('/data').then(r=>r.text());
        const entries=performance.getEntriesByType('resource').filter(e=>e.initiatorType==='fetch');
        const ok=entries.length===1 && entries[0] instanceof PerformanceResourceTiming &&
            entries[0].name===new URL('/data',location.href).href && entries[0].startTime>=0 &&
            entries[0].responseEnd<=performance.now() && entries[0].responseStatus===200;
        parent.postMessage(ok?'original-child-ok':'original-child-failed','*');
    });</script>"#;
    let (child, child_server) = serve(String::from(child_html), "").await;
    let html = r#"<!doctype html><body><script>
    addEventListener('load', async()=>{
        const f=document.createElement('iframe');
        const result=new Promise(resolve=>addEventListener('message',e=>{
            if(e.source===f.contentWindow)resolve(e.data);
        }));
        f.src='CHILD_URL'; document.body.appendChild(f);
        const ok=await result;
        await fetch('/data').then(r=>r.text());
        const entries=performance.getEntriesByType('resource').filter(e=>e.initiatorType==='fetch');
        document.body.setAttribute('data-resource-frame',String(ok==='original-child-ok' &&
            entries.length===1 && entries[0].name===new URL('/data',location.href).href));
    });</script>"#
        .replace("CHILD_URL", child.as_str());
    let (url, server) = serve(html, "").await;
    let response = fetch(&Request::get(url)).await.unwrap();
    let response = execute_js(response, (80, 24), (8, 16), Default::default()).await;
    assert!(
        response
            .js
            .as_ref()
            .is_some_and(|outcome| outcome.errors.is_empty())
    );
    let rendered =
        super::tests::navigation_timing_snapshot_after(response, "data-resource-frame=").await;
    server.abort();
    child_server.abort();
    assert!(
        rendered.contains("data-resource-frame=\"true\""),
        "{rendered}"
    );
}

#[tokio::test]
async fn resource_timing_redirect_tao_failure_is_monotone() {
    let (final_url, final_server) =
        serve(String::from("original"), "Timing-Allow-Origin: *\r\n").await;
    let client = Url::parse("http://127.0.0.1:1/").unwrap();
    for explicit in [false, true] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let redirect_url =
            Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let target = final_url.clone();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.windows(4).any(|b| b == b"\r\n\r\n") {
                let mut bytes = [0; 1024];
                let n = socket.read(&mut bytes).await.unwrap();
                if n == 0 {
                    return;
                }
                request.extend_from_slice(&bytes[..n]);
            }
            let tao = if explicit {
                "Timing-Allow-Origin: *\r\n"
            } else {
                ""
            };
            socket.write_all(format!("HTTP/1.1 302 Found\r\nLocation: {target}\r\nContent-Length: 0\r\nConnection: close\r\n{tao}\r\n").as_bytes()).await.unwrap();
        });
        let request = Request::subresource(redirect_url.clone(), &client, "image", None);
        let response = fetch_with_timing(&request, Default::default())
            .await
            .unwrap()
            .response;
        server.await.unwrap();
        let timing = response.timing.unwrap();
        assert_eq!(timing.resource_timing_allowed, explicit);
        let data = timing.resource_data(redirect_url.as_str(), "img");
        if explicit {
            assert!(data["redirectStart"].as_f64().unwrap() > 0.0);
            assert!(data["requestStart"].as_f64().unwrap() > 0.0);
        } else {
            assert_eq!(data["redirectStart"], 0.0);
            assert_eq!(data["requestStart"], 0.0);
            assert_eq!(data["fetchStart"], data["startTime"]);
        }
        assert_eq!(data["decodedBodySize"], 0);
    }
    final_server.abort();
}
