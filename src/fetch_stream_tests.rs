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
