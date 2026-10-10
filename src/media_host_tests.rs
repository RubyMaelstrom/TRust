//! HTML media elements end to end: the prelude's HTMLMediaElement driving a
//! real player over a local HTTP server, with silent audio (HTML
//! #media-elements; local whatwg/html e5071a2).

use super::tests::{configured_engine_before_prelude, run_microtask_checkpoint, string_value};
use super::*;
use std::time::Duration;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
}

/// Serve the media fixtures by name, honouring single byte ranges.
fn serve(runtime: &tokio::runtime::Runtime) -> String {
    let listener = runtime.block_on(TcpListener::bind("127.0.0.1:0")).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    runtime.spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    if socket.read(&mut byte).await.unwrap_or(0) == 0 {
                        return;
                    }
                    head.push(byte[0]);
                }
                let head = String::from_utf8_lossy(&head).to_string();
                let path = head.split_whitespace().nth(1).unwrap_or("/").trim_start_matches('/').to_string();
                let lower = head.to_ascii_lowercase();
                let range = lower
                    .lines()
                    .find_map(|line| line.strip_prefix("range: bytes="))
                    .and_then(|value| value.split('-').next())
                    .and_then(|start| start.trim().parse::<usize>().ok());
                let Ok(body) = std::fs::read(
                    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/fixtures/media").join(&path),
                ) else {
                    let _ = socket
                        .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                        .await;
                    return;
                };
                let kind = if path.ends_with(".mp4") { "video/mp4" } else { "application/octet-stream" };
                let response = match range {
                    Some(start) if start < body.len() => {
                        let mut out = format!(
                            "HTTP/1.1 206 Partial Content\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nContent-Range: bytes {start}-{}/{}\r\nConnection: close\r\n\r\n",
                            body.len() - start,
                            body.len() - 1,
                            body.len()
                        )
                        .into_bytes();
                        out.extend_from_slice(&body[start..]);
                        out
                    }
                    _ => {
                        let mut out = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        )
                        .into_bytes();
                        out.extend_from_slice(&body);
                        out
                    }
                };
                let _ = socket.write_all(&response).await;
            });
        }
    });
    base
}

/// A graphical Window realm (inline media) at `page`.
fn page(
    runtime: &tokio::runtime::Runtime,
    page: &str,
) -> (
    lumen::Engine,
    tokio::sync::mpsc::UnboundedReceiver<LumenHostTask>,
) {
    let (task_tx, task_rx) = tokio::sync::mpsc::unbounded_channel();
    let dom = Rc::new(RefCell::new(Dom::parse_document(
        "<!doctype html><body></body>",
    )));
    dom.borrow_mut().set_inline_media(true);
    let mut state = HostState::new(dom, Rc::new(RealmClock::new()));
    state.enable_network(
        url::Url::parse(page).unwrap(),
        runtime.handle().clone(),
        Arc::new(crate::http::PageCache::default()),
        task_tx,
    );
    state.media.silent = true;
    let mut engine = configured_engine_before_prelude(state, page);
    eval(
        &mut engine,
        "globalThis.__trust_cfg.inlineMedia = true;",
        "configuration",
    )
    .unwrap();
    eval_platform_prelude(&mut engine).unwrap();
    eval(&mut engine, "globalThis.log = [];", "log").unwrap();
    (engine, task_rx)
}

fn run_until(
    runtime: &tokio::runtime::Runtime,
    engine: &mut lumen::Engine,
    tasks: &mut tokio::sync::mpsc::UnboundedReceiver<LumenHostTask>,
    condition: &str,
) {
    let deadline = Instant::now() + Duration::from_secs(20);
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
            string_value(engine, "log.join('|')")
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

const EVENTS: &str = "['loadstart','progress','suspend','abort','error','emptied','stalled','loadedmetadata',\
    'loadeddata','canplay','canplaythrough','playing','waiting','seeking','seeked','ended','durationchange',\
    'timeupdate','play','pause','ratechange','resize','volumechange']";

fn watch(engine: &mut lumen::Engine, element: &str) {
    eval(
        engine,
        &format!(
            "for (const type of {EVENTS}) {element}.addEventListener(type, () => {{ \
             if (type !== 'timeupdate' && type !== 'progress') log.push(type); }});"
        ),
        "listeners",
    )
    .unwrap();
}

#[test]
fn a_video_loads_plays_and_ends() {
    let runtime = runtime();
    let base = serve(&runtime);
    let (mut engine, mut tasks) = page(&runtime, &format!("{base}/page.html"));
    eval(
        &mut engine,
        "globalThis.v = document.createElement('video'); document.body.appendChild(v);",
        "video",
    )
    .unwrap();
    watch(&mut engine, "v");
    eval(
        &mut engine,
        &format!("v.muted = true; v.src = '{base}/av1_mp3.mp4';"),
        "load",
    )
    .unwrap();
    run_until(
        &runtime,
        &mut engine,
        &mut tasks,
        "log.includes('canplaythrough')",
    );
    // Inserting the element ran the resource selection algorithm, so the
    // load algorithm the src attribute runs empties it; the volumechange
    // task queued before it is discarded with the element's other pending
    // tasks (#media-element-load-algorithm). The suspend event is the
    // resource being completely fetched, whenever that happens.
    assert_eq!(
        string_value(&mut engine, "log.filter(t => t !== 'suspend').join('|')"),
        "emptied|loadstart|durationchange|resize|loadedmetadata|loadeddata|canplay|canplaythrough"
    );
    assert_eq!(string_value(&mut engine, "v.readyState"), "4");
    assert_eq!(
        string_value(&mut engine, "v.networkState === 1 || v.networkState === 2"),
        "true"
    );
    assert_eq!(string_value(&mut engine, "v.duration"), "1");
    assert_eq!(
        string_value(&mut engine, "[v.videoWidth, v.videoHeight].join()"),
        "96,64"
    );
    assert_eq!(string_value(&mut engine, "v.paused"), "true");
    assert_eq!(
        string_value(&mut engine, "v.currentSrc"),
        format!("{base}/av1_mp3.mp4")
    );
    assert_eq!(
        string_value(
            &mut engine,
            "v.audioTracks.length + ',' + v.videoTracks.length"
        ),
        "1,1"
    );
    assert_eq!(
        string_value(&mut engine, "v.seekable.length && v.seekable.end(0)"),
        "1"
    );
    eval(
        &mut engine,
        "log.length = 0; v.play().then(() => log.push('resolved'));",
        "play",
    )
    .unwrap();
    run_until(&runtime, &mut engine, &mut tasks, "log.includes('ended')");
    assert_eq!(
        string_value(
            &mut engine,
            "log.filter(t => t !== 'canplaythrough' && t !== 'canplay').join('|')"
        ),
        "play|playing|resolved|pause|ended"
    );
    assert_eq!(string_value(&mut engine, "v.paused && v.ended"), "true");
    assert_eq!(string_value(&mut engine, "v.currentTime"), "1");
    assert_eq!(
        string_value(
            &mut engine,
            "v.played.length === 1 && v.played.end(0) === 1"
        ),
        "true"
    );
    assert!(
        string_value(&mut engine, "v.getVideoPlaybackQuality().totalVideoFrames")
            .parse::<u32>()
            .unwrap()
            >= 8
    );
    // Seeking back, accurately.
    eval(&mut engine, "log.length = 0; v.currentTime = 0.25;", "seek").unwrap();
    run_until(&runtime, &mut engine, &mut tasks, "log.includes('seeked')");
    assert_eq!(string_value(&mut engine, "log.join('|')"), "seeking|seeked");
    assert_eq!(string_value(&mut engine, "v.currentTime"), "0.25");
    assert_eq!(string_value(&mut engine, "v.ended || v.seeking"), "false");
}

#[test]
fn unsupported_resources_fail_and_sources_fall_through() {
    let runtime = runtime();
    let base = serve(&runtime);
    let (mut engine, mut tasks) = page(&runtime, &format!("{base}/page.html"));
    // A src that is not media: the dedicated media source failure steps.
    eval(
        &mut engine,
        &format!(
            "globalThis.a = document.createElement('video'); document.body.appendChild(a); \
             a.addEventListener('error', () => log.push('error ' + a.error.code + ' ' + a.networkState)); \
             a.src = '{base}/missing.mp4';"
        ),
        "missing",
    )
    .unwrap();
    run_until(&runtime, &mut engine, &mut tasks, "log.length > 0");
    assert_eq!(string_value(&mut engine, "log.join('|')"), "error 4 3");
    eval(
        &mut engine,
        "a.muted = true; a.play().catch(e => log.push(e.name));",
        "play rejected",
    )
    .unwrap();
    run_until(&runtime, &mut engine, &mut tasks, "log.length > 1");
    assert_eq!(string_value(&mut engine, "log[1]"), "NotSupportedError");
    // Source children: a type that cannot play is skipped without a fetch,
    // a missing resource fails with an error at its source, the third plays.
    eval(
        &mut engine,
        &format!(
            "log.length = 0; globalThis.b = document.createElement('audio'); \
             for (const [src, type] of [['x.flv', 'video/x-flv'], ['missing.mp3', ''], ['sine.mp3', 'audio/mpeg']]) {{ \
               const s = document.createElement('source'); s.src = '{base}/' + src; \
               if (type) s.type = type; s.onerror = () => log.push('source error ' + src); b.appendChild(s); }} \
             b.onloadedmetadata = () => log.push('metadata ' + b.currentSrc.split('/').pop()); \
             document.body.appendChild(b);"
        ),
        "sources",
    )
    .unwrap();
    run_until(
        &runtime,
        &mut engine,
        &mut tasks,
        "log.some(t => t.startsWith('metadata'))",
    );
    assert_eq!(
        string_value(&mut engine, "log.join('|')"),
        "source error x.flv|source error missing.mp3|metadata sine.mp3"
    );
    assert_eq!(string_value(&mut engine, "b.error"), "null");
}

#[test]
fn can_play_type_reports_the_decoders() {
    let runtime = runtime();
    let (mut engine, _tasks) = page(&runtime, "http://127.0.0.1/page.html");
    let answer = |engine: &mut lumen::Engine, type_: &str| {
        string_value(
            engine,
            &format!("document.createElement('video').canPlayType('{type_}')"),
        )
    };
    assert_eq!(
        answer(&mut engine, "video/mp4; codecs=\"av01.0.04M.08\""),
        "probably"
    );
    assert_eq!(answer(&mut engine, "audio/ogg; codecs=vorbis"), "probably");
    assert_eq!(answer(&mut engine, "video/webm"), "maybe");
    assert_eq!(answer(&mut engine, "video/x-flv"), "");
    assert_eq!(answer(&mut engine, "application/octet-stream"), "");
}

#[test]
fn audible_autoplay_needs_activation_and_muted_autoplay_plays() {
    let runtime = runtime();
    let base = serve(&runtime);
    let (mut engine, mut tasks) = page(&runtime, &format!("{base}/page.html"));
    eval(
        &mut engine,
        &format!(
            "globalThis.loud = document.createElement('video'); loud.src = '{base}/av1_mp3.mp4'; \
             document.body.appendChild(loud); \
             loud.play().catch(e => log.push('loud ' + e.name)); \
             globalThis.quiet = document.createElement('video'); quiet.muted = true; quiet.autoplay = true; \
             quiet.onplaying = () => log.push('quiet playing'); quiet.src = '{base}/av1_mp3.mp4'; \
             document.body.appendChild(quiet);"
        ),
        "autoplay",
    )
    .unwrap();
    run_until(&runtime, &mut engine, &mut tasks, "log.length >= 2");
    assert_eq!(
        string_value(&mut engine, "log.sort().join('|')"),
        "loud NotAllowedError|quiet playing"
    );
    assert_eq!(string_value(&mut engine, "quiet.paused"), "false");
}

/// The video element is a 96×64 replaced box (its video's natural size)
/// whose paint is the player's current frame: rendering the live document
/// through the desktop's display list and CPU rasterizer shows the frame.
#[test]
fn a_video_paints_its_frames() {
    let runtime = runtime();
    let base = serve(&runtime);
    let (mut engine, mut tasks) = page(&runtime, &format!("{base}/page.html"));
    eval(
        &mut engine,
        &format!(
            "document.body.style.margin = '0'; document.body.style.background = 'white'; \
             globalThis.v = document.createElement('video'); v.muted = true; \
             v.oncanplaythrough = () => log.push('ready'); v.src = '{base}/av1.mp4'; \
             document.body.appendChild(v);"
        ),
        "video",
    )
    .unwrap();
    run_until(&runtime, &mut engine, &mut tasks, "log.includes('ready')");
    // HTML #the-video-element: the natural size is the video's.
    let rect = string_value(
        &mut engine,
        "(r => [r.width, r.height].join())(v.getBoundingClientRect())",
    );
    assert_eq!(rect, "96,64");
    let dom = engine.ctx().host_mut::<HostState>().unwrap().dom.clone();
    let base_url = url::Url::parse(&base).unwrap();
    let frame = crate::render::headless::render_dom_with_resources(
        &dom.borrow(),
        &base_url,
        crate::core::CssSize::new(160.0, 120.0),
        &[],
        &Default::default(),
        &Default::default(),
        Default::default(),
    )
    .unwrap();
    let pixel = |x: u32, y: u32| {
        let index = ((y * frame.size.width + x) * 4) as usize;
        [
            frame.pixels[index],
            frame.pixels[index + 1],
            frame.pixels[index + 2],
        ]
    };
    // Outside the video: the page background.
    assert_eq!(pixel(140, 100), [255, 255, 255]);
    // Inside: the first frame of testsrc2, which is not a flat white page.
    let inside: Vec<[u8; 3]> = (0..8).map(|i| pixel(6 + i * 11, 30)).collect();
    assert!(inside.iter().any(|p| *p != [255, 255, 255]), "{inside:?}");
    assert!(inside.windows(2).any(|w| w[0] != w[1]), "flat {inside:?}");
}

/// HTML #expose-a-user-interface-to-the-user: the controls attribute shows
/// the user agent's controls over the element; pressing them operates the
/// element through the DOM API and dispatches no page events.
#[test]
fn native_controls_draw_and_operate_the_element() {
    let runtime = runtime();
    let base = serve(&runtime);
    let (mut engine, mut tasks) = page(&runtime, &format!("{base}/page.html"));
    eval(
        &mut engine,
        &format!(
            "document.body.style.margin = '0'; document.body.style.background = 'white'; \
             globalThis.v = document.createElement('video'); v.controls = true; v.muted = true; \
             v.style.width = '320px'; v.style.height = '180px'; \
             for (const t of ['pointerdown', 'mousedown', 'click', 'play', 'volumechange']) \
               v.addEventListener(t, () => log.push(t)); \
             v.oncanplaythrough = () => log.push('ready'); v.src = '{base}/av1.mp4'; \
             document.body.appendChild(v);"
        ),
        "video",
    )
    .unwrap();
    run_until(&runtime, &mut engine, &mut tasks, "log.includes('ready')");
    let dom = engine.ctx().host_mut::<HostState>().unwrap().dom.clone();
    let video = {
        let dom = dom.borrow();
        dom.descendants(crate::dom::DOCUMENT)
            .find(|&node| dom.tag_name(node) == Some("video"))
            .unwrap()
    };
    let controls = dom
        .borrow()
        .media_presentation(video)
        .and_then(|p| p.controls)
        .unwrap();
    assert!(controls.paused && controls.visible && controls.muted);
    assert_eq!(controls.duration, 1.0);
    let frame = crate::render::headless::render_dom_with_resources(
        &dom.borrow(),
        &url::Url::parse(&base).unwrap(),
        crate::core::CssSize::new(400.0, 240.0),
        &[],
        &Default::default(),
        &Default::default(),
        Default::default(),
    )
    .unwrap();
    let pixel = |x: u32, y: u32| {
        let index = ((y * frame.size.width + x) * 4) as usize;
        [
            frame.pixels[index],
            frame.pixels[index + 1],
            frame.pixels[index + 2],
        ]
    };
    // The bar darkens the bottom 36 px of the box; the play triangle is
    // white at its centre.
    let bar = pixel(200, 165);
    assert!(bar.iter().all(|&c| c < 200), "{bar:?}");
    assert_eq!(pixel(19, 162), [255, 255, 255]);
    // Press play: the element plays, and the page sees no pointer events.
    eval(
        &mut engine,
        "log.length = 0; __trust.pointerButton(v.__id, true, 18, 162); __trust.pointerButton(v.__id, false, 18, 162);",
        "press play",
    )
    .unwrap();
    run_until(&runtime, &mut engine, &mut tasks, "log.includes('play')");
    assert_eq!(string_value(&mut engine, "v.paused"), "false");
    assert_eq!(
        string_value(&mut engine, "log.filter(t => t !== 'play').join()"),
        ""
    );
    // Mute toggles through the muted attribute's IDL state.
    eval(
        &mut engine,
        "log.length = 0; __trust.pointerButton(v.__id, true, 54, 162); __trust.pointerButton(v.__id, false, 54, 162);",
        "press mute",
    )
    .unwrap();
    run_until(
        &runtime,
        &mut engine,
        &mut tasks,
        "log.includes('volumechange')",
    );
    assert_eq!(string_value(&mut engine, "v.muted"), "false");
}

/// Media Source Extensions end to end: a MediaSource object URL attaches,
/// SourceBuffers take fragmented MP4 from fetch(), and the element plays
/// what was appended (local w3c/media-source@8e0611a).
#[test]
fn media_source_extensions_play_appended_media() {
    let runtime = runtime();
    let base = serve(&runtime);
    let (mut engine, mut tasks) = page(&runtime, &format!("{base}/page.html"));
    eval(
        &mut engine,
        &format!(
            r#"globalThis.v = document.createElement('video'); document.body.appendChild(v);
            globalThis.ms = new MediaSource();
            log.push('supported ' + MediaSource.isTypeSupported('video/mp4; codecs="av01.0.00M.08"')
                + ' ' + MediaSource.isTypeSupported('video/mp4; codecs="bogus"') + ' ' + ms.readyState);
            const append = (sb, bytes) => new Promise(resolve => {{
                sb.addEventListener('updateend', resolve, {{once: true}}); sb.appendBuffer(bytes); }});
            ms.addEventListener('sourceopen', async () => {{
                log.push('sourceopen ' + ms.readyState + ' ' + ms.duration);
                URL.revokeObjectURL(v.src);
                const video = ms.addSourceBuffer('video/mp4; codecs="av01.0.00M.08"');
                const audio = ms.addSourceBuffer('audio/mp4; codecs="flac"');
                for (const sb of [video, audio]) for (const type of ['updatestart', 'update', 'updateend'])
                    sb.addEventListener(type, () => log.push((sb === video ? 'video ' : 'audio ') + type));
                const [vb, ab] = await Promise.all(['mse_video.mp4', 'mse_audio.mp4'].map(
                    name => fetch('{base}/' + name).then(r => r.arrayBuffer())));
                await append(video, vb);
                log.push('video buffered ' + video.buffered.length + ' ' + video.buffered.start(0) + '-' + video.buffered.end(0)
                    + ' tracks ' + video.videoTracks.length + ' ' + v.videoTracks.length + ' ' + (v.videoTracks[0].sourceBuffer === video));
                await append(audio, new Uint8Array(ab));
                log.push('element buffered ' + v.buffered.length + ' ' + v.buffered.end(0) + ' ' + (v.buffered === v.buffered)
                    + ' active ' + ms.activeSourceBuffers.length);
                ms.endOfStream();
                log.push('ended ' + ms.readyState + ' ' + ms.duration + ' ' + v.duration);
            }});
            ms.addEventListener('sourceended', () => log.push('sourceended'));
            for (const type of ['loadedmetadata', 'durationchange', 'canplaythrough', 'ended', 'error'])
                v.addEventListener(type, () => log.push(type + ' ' + v.readyState));
            v.muted = true;
            v.src = URL.createObjectURL(ms);"#
        ),
        "media source",
    )
    .unwrap();
    run_until(
        &runtime,
        &mut engine,
        &mut tasks,
        "log.some(t => t.startsWith('ended '))",
    );
    run_until(
        &runtime,
        &mut engine,
        &mut tasks,
        "log.some(t => t.startsWith('canplaythrough'))",
    );
    let log = string_value(&mut engine, "log.join('|')");
    assert!(
        log.starts_with("supported true false closed|sourceopen open NaN|"),
        "{log}"
    );
    // The first initialization segment set the duration (Infinity): its
    // durationchange is queued during the append.
    assert!(
        log.contains("video updatestart|durationchange 0|video update|video updateend|"),
        "{log}"
    );
    assert!(
        log.contains("video buffered 1 0-2 tracks 1 1 true"),
        "{log}"
    );
    assert!(log.contains("element buffered 1 2 true active 2"), "{log}");
    assert!(log.contains("ended ended 2 2"), "{log}");
    // Metadata waited for both initialization segments; endOfStream() set
    // the duration from Infinity to 2.
    assert!(
        log.contains("audio updatestart|loadedmetadata 1|audio update|"),
        "{log}"
    );
    assert!(log.contains("sourceended|durationchange 1|"), "{log}");
    assert_eq!(
        string_value(&mut engine, "v.seekable.length && v.seekable.end(0)"),
        "2"
    );
    eval(&mut engine, "log.length = 0; v.play();", "play").unwrap();
    run_until(
        &runtime,
        &mut engine,
        &mut tasks,
        "log.some(t => t.startsWith('ended'))",
    );
    assert_eq!(string_value(&mut engine, "v.currentTime"), "2");
    // A new load detaches the MediaSource.
    eval(&mut engine, "log.length = 0; ms.addEventListener('sourceclose', () => log.push('sourceclose ' + ms.readyState + ' ' + ms.sourceBuffers.length)); v.removeAttribute('src'); v.load();", "detach").unwrap();
    run_until(
        &runtime,
        &mut engine,
        &mut tasks,
        "log.some(t => t.startsWith('sourceclose'))",
    );
    assert_eq!(
        string_value(&mut engine, "log.join('|')"),
        "sourceclose closed 0"
    );
    assert_eq!(string_value(&mut engine, "String(ms.duration)"), "NaN");
}

/// MSE's append error: bytes that are not the byte stream format end the
/// stream with a decode error, which fails a media element that has no
/// metadata yet.
#[test]
fn media_source_append_errors_fail_the_element() {
    let runtime = runtime();
    let base = serve(&runtime);
    let (mut engine, mut tasks) = page(&runtime, &format!("{base}/page.html"));
    eval(
        &mut engine,
        r#"globalThis.v = document.createElement('video'); document.body.appendChild(v);
        globalThis.ms = new MediaSource();
        ms.addEventListener('sourceopen', () => {
            const sb = ms.addSourceBuffer('video/webm; codecs="vp9"');
            for (const type of ['error', 'updateend']) sb.addEventListener(type, () => log.push(type + ' ' + sb.updating));
            // Not an EBML element: no valid element ID starts with 0.
            sb.appendBuffer(new Uint8Array(8));
            try { sb.appendBuffer(new Uint8Array(1)); } catch (e) { log.push(e.name); }
            try { sb.appendBuffer('text'); } catch (e) { log.push(e.name); }
            try { ms.addSourceBuffer('video/x-nope'); } catch (e) { log.push(e.name); }
        });
        ms.addEventListener('sourceended', () => log.push('sourceended ' + ms.readyState));
        v.addEventListener('error', () => log.push('element error ' + v.error.code));
        v.src = URL.createObjectURL(ms);"#,
        "append error",
    )
    .unwrap();
    run_until(
        &runtime,
        &mut engine,
        &mut tasks,
        "log.some(t => t.startsWith('element error'))",
    );
    assert_eq!(
        string_value(&mut engine, "log.join('|')"),
        "InvalidStateError|TypeError|NotSupportedError|error false|updateend false|sourceended ended|element error 4"
    );
}

/// Media Capabilities (local w3c/media-capabilities@dbc6ff7): decodingInfo()
/// answers from the decoders the host has and rejects invalid
/// configurations.
#[test]
fn media_capabilities_report_the_decoders() {
    let runtime = runtime();
    let base = serve(&runtime);
    let (mut engine, mut tasks) = page(&runtime, &format!("{base}/page.html"));
    eval(
        &mut engine,
        r#"const mc = navigator.mediaCapabilities;
        log.push(String(mc === navigator.mediaCapabilities && mc instanceof MediaCapabilities));
        const video = (contentType, width = 1280, height = 720) => ({contentType, width, height, bitrate: 2e6, framerate: 30});
        (async () => {
            for (const config of [
                {type: 'media-source', video: video('video/mp4; codecs="av01.0.05M.08"'), audio: {contentType: 'audio/mp4; codecs="flac"'}},
                {type: 'file', audio: {contentType: 'audio/ogg; codecs="vorbis"', channels: '2', samplerate: 44100}},
                {type: 'media-source', audio: {contentType: 'audio/ogg; codecs="vorbis"'}},
                {type: 'webrtc', video: video('video/webm; codecs="vp9"')},
                {type: 'file', video: video('video/mp4; codecs="av01.0.05M.08"', 7680, 4320)},
            ]) {
                const info = await mc.decodingInfo(config);
                log.push([info.supported, info.smooth, info.powerEfficient, info.keySystemAccess, info.configuration.type].join());
            }
            for (const config of [undefined, {type: 'file'}, {type: 'file', video: video('video/mp4')},
                {type: 'file', video: {...video('video/webm; codecs="vp9"'), framerate: 0}},
                {type: 'bogus', audio: {contentType: 'audio/mpeg'}},
                {type: 'file', video: {...video('video/webm; codecs="vp9"'), scalabilityMode: 'L1T1'}}]) {
                try { await mc.decodingInfo(config); log.push('resolved'); } catch (e) { log.push(e.name); }
            }
            const encoding = await mc.encodingInfo({type: 'record', audio: {contentType: 'audio/webm; codecs="opus"'}});
            log.push('encoding ' + encoding.supported + ' ' + Object.keys(encoding).join());
            log.push('done');
        })();"#,
        "media capabilities",
    )
    .unwrap();
    run_until(&runtime, &mut engine, &mut tasks, "log.includes('done')");
    assert_eq!(
        string_value(&mut engine, "log.join('|')"),
        "true|true,true,false,,media-source|true,true,true,,file|false,false,false,,media-source|\
         false,false,false,,webrtc|true,false,false,,file|TypeError|TypeError|TypeError|TypeError|TypeError|TypeError|\
         encoding false powerEfficient,smooth,supported|done"
    );
}

/// Encrypted Media Extensions with Clear Key end to end (local
/// w3c/encrypted-media@fb9130a): MSE appends CENC-encrypted media, the
/// element fires `encrypted` with the pssh, a session's license request
/// names its key ID, and the JWK license unlocks playback.
#[test]
fn clear_key_unlocks_encrypted_media() {
    let runtime = runtime();
    let base = serve(&runtime);
    let (mut engine, mut tasks) = page(&runtime, &format!("{base}/page.html"));
    eval(
        &mut engine,
        &format!(
            r#"globalThis.v = document.createElement('video'); document.body.appendChild(v);
            const config = [{{initDataTypes: ['cenc'], videoCapabilities: [{{contentType: 'video/mp4; codecs="av01.0.00M.08"'}}]}}];
            const b64url = bytes => btoa(String.fromCharCode(...new Uint8Array(bytes))).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
            (async () => {{
                try {{ await navigator.requestMediaKeySystemAccess('com.widevine.alpha', config); }} catch (e) {{ log.push('widevine ' + e.name); }}
                try {{ await navigator.requestMediaKeySystemAccess('org.w3.clearkey', [{{videoCapabilities: [{{contentType: 'video/x-nope'}}]}}]); }}
                catch (e) {{ log.push('unsupported ' + e.name); }}
                const access = await navigator.requestMediaKeySystemAccess('org.w3.clearkey', config);
                const got = access.getConfiguration();
                log.push('access ' + access.keySystem + ' ' + got.initDataTypes.join() + ' ' + got.sessionTypes.join() + ' '
                    + got.distinctiveIdentifier + ' ' + got.videoCapabilities[0].encryptionScheme);
                const keys = await access.createMediaKeys();
                log.push('certificate ' + await keys.setServerCertificate(new Uint8Array([1])));
                await v.setMediaKeys(keys);
                log.push('attached ' + (v.mediaKeys === keys));
                v.addEventListener('encrypted', async event => {{
                    log.push('encrypted ' + event.initDataType + ' ' + event.initData.byteLength);
                    const session = keys.createSession();
                    session.addEventListener('message', async message => {{
                        const request = JSON.parse(new TextDecoder().decode(message.message));
                        log.push('message ' + message.messageType + ' ' + request.kids.join());
                        session.addEventListener('keystatuseschange', () => {{
                            for (const [kid, status] of session.keyStatuses) log.push('status ' + b64url(kid) + ' ' + status);
                        }});
                        const k = b64url(new Uint8Array([0x00,0x11,0x22,0x33,0x44,0x55,0x66,0x77,0x88,0x99,0xaa,0xbb,0xcc,0xdd,0xee,0xff]));
                        await session.update(new TextEncoder().encode(JSON.stringify({{keys: [{{kty: 'oct', k, kid: request.kids[0]}}]}})));
                        log.push('updated ' + session.sessionId + ' ' + session.keyStatuses.size);
                    }});
                    await session.generateRequest(event.initDataType, event.initData);
                }}, {{once: true}});
                v.addEventListener('waitingforkey', () => log.push('waitingforkey'));
                v.addEventListener('ended', () => log.push('ended ' + v.currentTime));
                const ms = new MediaSource();
                ms.addEventListener('sourceopen', async () => {{
                    URL.revokeObjectURL(v.src);
                    const sb = ms.addSourceBuffer('video/mp4; codecs="av01.0.00M.08"');
                    const bytes = await fetch('{base}/mse_video_cenc.mp4').then(r => r.arrayBuffer());
                    sb.appendBuffer(bytes);
                    sb.addEventListener('updateend', () => {{ ms.endOfStream(); v.play(); }}, {{once: true}});
                }});
                v.muted = true;
                v.src = URL.createObjectURL(ms);
            }})().catch(e => log.push('failed ' + e.name + ' ' + e.message));"#
        ),
        "clear key",
    )
    .unwrap();
    run_until(
        &runtime,
        &mut engine,
        &mut tasks,
        "log.some(t => t.startsWith('ended') || t.startsWith('failed'))",
    );
    let log = string_value(&mut engine, "log.join('|')");
    assert!(log.starts_with("widevine NotSupportedError|unsupported NotSupportedError|access org.w3.clearkey cenc temporary not-allowed null|certificate false|attached true|encrypted cenc 52|message license-request ASNFZ4mrze8BI0VniavN7w|"), "{log}");
    assert!(
        log.contains("status ASNFZ4mrze8BI0VniavN7w usable"),
        "{log}"
    );
    assert!(log.contains("updated 1 1"), "{log}");
    assert!(log.contains("ended 2"), "{log}");
}
