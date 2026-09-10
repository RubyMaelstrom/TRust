//! Original local conformance cases for Fetch Metadata's native request context.
use super::*;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[test]
fn fetch_metadata_schemeful_sites_and_redirect_taint() {
    use FetchSite::{CrossSite, SameOrigin, SameSite};
    for (left, right, expected) in [
        (
            "https://a.example.com/a",
            "https://a.example.com/b",
            SameOrigin,
        ),
        ("https://a.example.com", "https://b.example.com", SameSite),
        ("http://a.example.com", "https://a.example.com", CrossSite),
        ("https://example.co.uk", "https://a.example.co.uk", SameSite),
        ("https://a.co.uk", "https://b.co.uk", CrossSite),
        ("https://a.github.io", "https://b.github.io", CrossSite),
        ("https://a.x.github.io", "https://b.x.github.io", SameSite),
        ("https://a.www.ck", "https://b.www.ck", SameSite),
        ("https://a.b.ck", "https://x.b.ck", CrossSite),
        ("https://a.example.com.", "https://b.example.com.", SameSite),
        ("https://example.com.", "https://example.com", CrossSite),
        ("http://127.0.0.1:1", "http://127.0.0.1:2", SameSite),
        ("http://[::1]:1", "http://[::1]:2", SameSite),
        ("http://127.0.0.1", "http://127.0.0.2", CrossSite),
        ("http://localhost", "http://sub.localhost", CrossSite),
        ("https://com", "https://com:8443", SameSite),
        ("data:text/plain,a", "https://example.com", CrossSite),
        (
            "https://a.食狮.中国",
            "https://b.xn--85x722f.xn--fiqs8s",
            SameSite,
        ),
    ] {
        let left = Url::parse(left).unwrap();
        let right = Url::parse(right).unwrap();
        assert_eq!(fetch_site(&left, &right), expected, "{left} → {right}");
    }
    let source = Url::parse("https://a.example.com/").unwrap();
    let mut request = Request::subresource(source.clone(), &source, "script", None);
    for (target, expected) in [
        ("https://b.example.com/", SameSite),
        ("https://a.example.com/", SameSite),
        ("https://unrelated.test/", CrossSite),
        ("https://a.example.com/", CrossSite),
    ] {
        let target = Url::parse(target).unwrap();
        update_navigation_metadata_for_redirect(&mut request, &target);
        request.url = target;
        assert_eq!(request.fetch_metadata.unwrap().site, expected);
    }
    set_navigation_metadata(&mut request, None);
    update_navigation_metadata_for_redirect(
        &mut request,
        &Url::parse("https://other.test").unwrap(),
    );
    assert_eq!(request.fetch_metadata.unwrap().site, FetchSite::None);
}

#[test]
fn fetch_metadata_trustworthy_url_gate() {
    for url in [
        "https://example.test",
        "http://localhost",
        "http://child.localhost.",
        "http://127.1.2.3",
        "http://[::1]",
    ] {
        assert!(potentially_trustworthy(&Url::parse(url).unwrap()), "{url}");
    }
    for url in [
        "http://example.test",
        "http://localhost.example",
        "http://192.168.1.1",
        "http://localhost..",
        "data:text/plain,test",
    ] {
        assert!(!potentially_trustworthy(&Url::parse(url).unwrap()), "{url}");
    }
}

#[test]
fn fetch_metadata_scanner_preserves_element_modes_and_order() {
    use crate::js::{ExternalResourceKind as K, external_resources};
    let jobs = external_resources(
        r#"<!doctype html>
        <script src="a.js" crossorigin></script><script src="old.js" nomodule></script>
        <script type="module" src="m.js"></script><link rel="modulepreload" href="m.js">
        <link rel="stylesheet" href="s.css" crossorigin="use-credentials">
        <link rel="stylesheet" href="disabled.css" disabled>
        <link rel="alternate stylesheet" href="alternate.css">
        <link rel="stylesheet" href="s.css"><link rel="stylesheet" href="n.css">
        <svg><use href="i.svg#one"/><use href="i.svg#two"/></svg>"#,
    );
    let summary: Vec<_> = jobs
        .iter()
        .map(|job| {
            (
                job.kind,
                job.source.as_str(),
                job.destination(),
                job.cors_credentials(),
            )
        })
        .collect();
    assert_eq!(
        summary,
        vec![
            (
                K::Script,
                "a.js",
                "script",
                Some(CredentialsMode::SameOrigin)
            ),
            (K::Sheet, "s.css", "style", Some(CredentialsMode::Include)),
            (K::Sheet, "n.css", "style", None),
            (
                K::Preload,
                "m.js",
                "script",
                Some(CredentialsMode::SameOrigin)
            ),
            (K::Sprite, "i.svg", "image", None),
        ]
    );
    assert_eq!(
        crate::js::element_cors_credentials(Some("invalid"), false),
        Some(CredentialsMode::SameOrigin)
    );
}

#[test]
fn fetch_metadata_preload_cache_keeps_authorization_context() {
    let cache = PageCache::default();
    let source = Url::parse("https://page.example/").unwrap();
    let target = Url::parse("https://cdn.example/script.js").unwrap();
    cache.seed_resource(
        target.to_string(),
        &source,
        "script",
        Some(CredentialsMode::SameOrigin),
        CachedResp {
            status: 200,
            content_type: "text/javascript".into(),
            headers: vec![],
            body: b"/*original authorized source*/".to_vec(),
            url_list: vec![target.clone()],
            timing: None,
        },
    );
    assert!(
        cache
            .peek_resource(
                &target,
                &source,
                "script",
                Some(CredentialsMode::SameOrigin)
            )
            .is_some()
    );
    assert!(
        cache
            .peek_resource(&target, &source, "script", Some(CredentialsMode::Include))
            .is_none()
    );
    assert!(
        cache
            .peek_resource(&target, &source, "script", None)
            .is_none()
    );
    assert!(
        cache
            .peek_resource(&target, &source, "style", Some(CredentialsMode::SameOrigin))
            .is_none()
    );
    assert!(
        cache
            .peek_resource(
                &target,
                &Url::parse("https://another.example").unwrap(),
                "script",
                Some(CredentialsMode::SameOrigin)
            )
            .is_none()
    );
}

async fn request_head(socket: &mut tokio::net::TcpStream) -> String {
    let mut bytes = Vec::new();
    while !bytes.windows(4).any(|window| window == b"\r\n\r\n") {
        let mut chunk = [0; 2048];
        let count = socket.read(&mut chunk).await.unwrap();
        assert!(count > 0);
        bytes.extend_from_slice(&chunk[..count]);
        assert!(bytes.len() < 65536);
    }
    String::from_utf8(bytes).unwrap()
}

fn header<'a>(head: &'a str, name: &str) -> Vec<&'a str> {
    head.lines()
        .filter_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case(name).then(|| value.trim())
        })
        .collect()
}

#[tokio::test]
async fn fetch_metadata_subresources_reject_forged_activation_and_navigation_headers() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = Url::parse(&format!(
        "http://{}/resource",
        listener.local_addr().unwrap()
    ))
    .unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let head = request_head(&mut socket).await;
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
            .await
            .unwrap();
        head
    });
    let mut request = Request::subresource(url.clone(), &url, "image", None);
    request.fetch_metadata.as_mut().unwrap().user_activation = true;
    request.headers = [
        ("Sec-Fetch-Dest", "document"),
        ("sec-fetch-mode", "navigate"),
        ("SEC-FETCH-SITE", "none"),
        ("Sec-Fetch-User", "?1"),
        ("Upgrade-Insecure-Requests", "1"),
    ]
    .map(|(key, value)| (key.into(), value.into()))
    .to_vec();
    assert_eq!(fetch(&request).await.unwrap().body, b"ok");
    let head = server.await.unwrap();
    assert_eq!(header(&head, "Sec-Fetch-Dest"), ["image"]);
    assert_eq!(header(&head, "Sec-Fetch-Mode"), ["no-cors"]);
    assert_eq!(header(&head, "Sec-Fetch-Site"), ["same-origin"]);
    assert!(header(&head, "Sec-Fetch-User").is_empty());
    assert!(header(&head, "Upgrade-Insecure-Requests").is_empty());
    assert!(
        header(&head, "Referer").is_empty(),
        "suppressed referrer does not change site"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_metadata_real_page_and_nested_requests() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let url = Url::parse(&format!("http://{address}/")).unwrap();
    let frame_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let frame_url = format!("http://{}/frame", frame_listener.local_addr().unwrap());
    let unapproved_script = format!(
        "http://{}/not-cors.js",
        frame_listener.local_addr().unwrap()
    );
    let observed = Arc::new(Mutex::new(Vec::new()));
    let records = observed.clone();
    let server = tokio::spawn(async move {
        loop {
            let (mut socket, _) = tokio::select! {
                accepted = listener.accept() => accepted.unwrap(),
                accepted = frame_listener.accept() => accepted.unwrap(),
            };
            let head = request_head(&mut socket).await;
            let path = head.split_whitespace().nth(1).unwrap().to_string();
            records.lock().unwrap().push((path.clone(), head));
            let (kind, body) = match path.as_str() {
                "/" => ("text/html", format!(r#"<!doctype html><link rel="stylesheet" href="/style.css">
                    <body><script>
                    let frameDone = new Promise(resolve => addEventListener('message', event => {{
                        if(event.data === 'original-frame-done') resolve();
                    }}));
                    addEventListener('load', async () => {{
                        await fetch('/fetch').then(r => r.text());
                        await new Promise(resolve => {{ const x = new XMLHttpRequest();
                            x.open('GET','/xhr'); x.onload = resolve; x.send(); }});
                        await new Promise(resolve => {{ const s = document.createElement('script');
                            s.src='/dynamic.js'; s.crossOrigin='anonymous'; s.onload=resolve; document.head.appendChild(s); }});
                        await new Promise(resolve => {{ const s=document.createElement('link'); s.rel='stylesheet';
                            s.href='/dynamic.css'; s.onload=resolve; document.head.appendChild(s); }});
                        const denied = await new Promise(resolve => {{ const s=document.createElement('script');
                            s.src='{unapproved_script}'; s.crossOrigin='anonymous'; s.onload=()=>resolve(false);
                            s.onerror=()=>resolve(true); document.head.appendChild(s); }});
                        document.body.setAttribute('data-cors-cache-protected', String(denied && globalThis.originalNoCorsRuns === 1));
                        await frameDone;
                        document.body.setAttribute('data-request-context-done','true');
                    }});
                    </script><script src="/classic.js"></script><script src="{unapproved_script}"></script><script type="module" src="/module.js"></script>
                    <iframe src="{frame_url}" referrerpolicy="no-referrer"></iframe>"#)),
                "/frame" => ("text/html", "<!doctype html><body><script src='/frame-classic.js' crossorigin></script>".into()),
                "/frame-classic.js" => ("text/javascript", "fetch('/frame-fetch').then(r=>r.text()).then(()=>parent.postMessage('original-frame-done','*'));".into()),
                "/classic.js" | "/module.js" | "/dynamic.js" => ("text/javascript", "globalThis.originalResourceLoaded=true;".into()),
                "/not-cors.js" => ("text/javascript", "globalThis.originalNoCorsRuns=(globalThis.originalNoCorsRuns||0)+1;".into()),
                "/style.css" | "/dynamic.css" => ("text/css", "body{color:black}".into()),
                _ => ("text/plain", "original".into()),
            };
            let reply = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(reply.as_bytes()).await.unwrap();
        }
    });
    let response = fetch_web_default(&url).await.unwrap();
    let response = execute_js(response, (80, 24), (8, 16), Default::default()).await;
    let html = super::tests::navigation_timing_snapshot_after(
        response,
        "data-request-context-done=\"true\"",
    )
    .await;
    assert!(html.contains("data-request-context-done=\"true\""));
    assert!(
        html.contains("data-cors-cache-protected=\"true\""),
        "a no-CORS preload must not authorize a CORS script: {html}"
    );
    server.abort();
    let records = observed.lock().unwrap();
    let modes: Vec<_> = records
        .iter()
        .filter(|(path, _)| path == "/not-cors.js")
        .flat_map(|(_, head)| header(head, "Sec-Fetch-Mode"))
        .collect();
    assert_eq!(modes, ["no-cors", "cors"]);
    for (path, dest, mode, site) in [
        ("/", "document", "navigate", "none"),
        ("/classic.js", "script", "no-cors", "same-origin"),
        ("/module.js", "script", "cors", "same-origin"),
        ("/style.css", "style", "no-cors", "same-origin"),
        ("/frame", "iframe", "navigate", "same-site"),
        ("/frame-classic.js", "script", "cors", "same-origin"),
        ("/frame-fetch", "empty", "cors", "same-origin"),
        ("/fetch", "empty", "cors", "same-origin"),
        ("/xhr", "empty", "cors", "same-origin"),
        ("/dynamic.js", "script", "cors", "same-origin"),
        ("/dynamic.css", "style", "no-cors", "same-origin"),
    ] {
        let heads: Vec<_> = records.iter().filter(|(route, _)| route == path).collect();
        assert!(!heads.is_empty(), "missing {path}");
        for (_, head) in heads {
            assert_eq!(header(head, "Sec-Fetch-Dest"), [dest], "{path}");
            assert_eq!(header(head, "Sec-Fetch-Mode"), [mode], "{path}");
            assert_eq!(header(head, "Sec-Fetch-Site"), [site], "{path}");
            if path != "/" {
                assert!(header(head, "Sec-Fetch-User").is_empty(), "{path}");
            }
            if mode != "navigate" {
                assert!(
                    header(head, "Upgrade-Insecure-Requests").is_empty(),
                    "{path}"
                );
            }
            if path == "/frame" {
                assert!(header(head, "Referer").is_empty());
            }
        }
    }
}
