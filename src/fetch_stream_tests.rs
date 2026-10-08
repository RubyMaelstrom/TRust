//! Response bodies read as they arrive, against local servers that write
//! their bodies in delayed pieces: Fetch #concept-http-network-fetch for
//! fetch(), XMLHttpRequest's processBodyChunk, and HTML server-sent events
//! (#server-sent-events) with reconnection.

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
    lumen::Engine,
    tokio::sync::mpsc::UnboundedReceiver<LumenHostTask>,
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

fn log(engine: &mut lumen::Engine) -> String {
    string_value(engine, "log.join('|')")
}

/// Run the event loop until `condition` holds: host networking tasks,
/// platform tasks and microtasks, with the timer clock advancing in steps.
fn run_until(
    runtime: &tokio::runtime::Runtime,
    engine: &mut lumen::Engine,
    tasks: &mut tokio::sync::mpsc::UnboundedReceiver<LumenHostTask>,
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
    engine: &mut lumen::Engine,
    tasks: &mut tokio::sync::mpsc::UnboundedReceiver<LumenHostTask>,
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

fn streams_in_flight(engine: &mut lumen::Engine) -> usize {
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
