//! Media resource fetching against a local HTTP/1.1 server.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::*;
use crate::media::demux::ByteSource;

/// Serve `body` with or without range support; count requests.
async fn serve(body: Arc<Vec<u8>>, ranges: bool, requests: Arc<AtomicUsize>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let body = body.clone();
            let requests = requests.clone();
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    if socket.read(&mut byte).await.unwrap_or(0) == 0 {
                        return;
                    }
                    head.push(byte[0]);
                }
                requests.fetch_add(1, Ordering::SeqCst);
                let head = String::from_utf8_lossy(&head).to_ascii_lowercase();
                let range = head
                    .lines()
                    .find_map(|line| line.strip_prefix("range: bytes="))
                    .and_then(|value| value.split('-').next())
                    .and_then(|start| start.trim().parse::<usize>().ok());
                let response = match (ranges, range) {
                    (true, Some(start)) if start < body.len() => {
                        let mut out = format!(
                            "HTTP/1.1 206 Partial Content\r\nContent-Type: video/mp4\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/{}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
                            body.len() - start,
                            start,
                            body.len() - 1,
                            body.len()
                        )
                        .into_bytes();
                        out.extend_from_slice(&body[start..]);
                        out
                    }
                    _ => {
                        let mut out = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: video/mp4\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
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
    port
}

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/fixtures/media")
            .join(name),
    )
    .unwrap()
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
}

fn source(runtime: &tokio::runtime::Runtime, port: u16) -> HttpSource {
    let url = url::Url::parse(&format!("http://127.0.0.1:{port}/clip.mp4")).unwrap();
    let request = crate::http::Request::get(url);
    HttpSource::new(request, runtime.handle().clone(), Arc::new(|_| {}))
}

#[test]
fn reads_out_of_order_with_range_requests() {
    let runtime = runtime();
    let body: Arc<Vec<u8>> = Arc::new((0..300_000u32).map(|i| (i % 251) as u8).collect());
    let requests = Arc::new(AtomicUsize::new(0));
    let port = runtime.block_on(serve(body.clone(), true, requests.clone()));
    let mut source = source(&runtime, port);
    assert_eq!(source.read_exact_at(0, 100).unwrap(), body[..100]);
    assert_eq!(source.len(), Some(300_000));
    assert!(source.seekable());
    // A jump far ahead starts a second request.
    assert_eq!(
        source.read_exact_at(290_000, 10_000).unwrap(),
        body[290_000..]
    );
    assert_eq!(source.read_up_to(299_990, 100).unwrap(), body[299_990..]);
    assert_eq!(source.read_up_to(300_000, 10).unwrap(), Vec::<u8>::new());
    // Bytes already cached need no request.
    let before = requests.load(Ordering::SeqCst);
    assert_eq!(source.read_exact_at(50, 50).unwrap(), body[50..100]);
    assert_eq!(requests.load(Ordering::SeqCst), before);
}

#[test]
fn servers_without_ranges_stream_from_the_start() {
    let runtime = runtime();
    let body: Arc<Vec<u8>> = Arc::new((0..100_000u32).map(|i| (i % 13) as u8).collect());
    let port = runtime.block_on(serve(body.clone(), false, Arc::new(AtomicUsize::new(0))));
    let mut source = source(&runtime, port);
    assert_eq!(source.read_exact_at(0, 10).unwrap(), body[..10]);
    assert_eq!(source.ranges_supported(), Some(false));
    // Reading forward works through the single stream.
    assert_eq!(
        source.read_exact_at(90_000, 10).unwrap(),
        body[90_000..90_010]
    );
}

#[test]
fn abort_interrupts_a_blocked_read() {
    let runtime = runtime();
    // A server that accepts and never answers.
    let listener = runtime.block_on(TcpListener::bind("127.0.0.1:0")).unwrap();
    let port = listener.local_addr().unwrap().port();
    runtime.spawn(async move {
        let mut held = Vec::new();
        while let Ok((socket, _)) = listener.accept().await {
            held.push(socket);
        }
    });
    let mut source = source(&runtime, port);
    let abort = source.abort_handle();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        abort.abort();
    });
    let error = source.read_exact_at(0, 10).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::Interrupted);
}

#[test]
fn plays_a_resource_over_http() {
    use crate::media::player::{AudioMode, Event, Input, Options, Player};
    let runtime = runtime();
    let body = Arc::new(fixture("av1_mp3.mp4"));
    let port = runtime.block_on(serve(body, true, Arc::new(AtomicUsize::new(0))));
    let (sender, receiver) = std::sync::mpsc::channel();
    let sender = Mutex::new(sender);
    let player = Player::new(
        Input::Resource {
            source: Box::new(source(&runtime, port)),
            hint: None,
        },
        Options {
            audio: AudioMode::Silent,
        },
        move |event| {
            let _ = sender.lock().unwrap().send(event);
        },
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut played = false;
    loop {
        let event = receiver
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("events");
        match event {
            Event::ReadyState(crate::media::player::ReadyState::EnoughData) if !played => {
                played = true;
                player.play();
            }
            Event::Ended => break,
            Event::Error(kind, message) => panic!("{kind:?}: {message}"),
            _ => {}
        }
    }
    assert!(player.current_time() >= 950_000);
}

#[test]
fn content_ranges_and_single_ranges() {
    assert_eq!(
        parse_content_range("bytes 0-99/1000"),
        Some((0, Some(1000)))
    );
    assert_eq!(parse_content_range("bytes 500-999/*"), Some((500, None)));
    assert_eq!(parse_content_range("bytes 9-1/10"), None);
    assert_eq!(parse_content_range("items 0-1/2"), None);
    use crate::http::parse_single_range;
    assert_eq!(parse_single_range("bytes=0-"), Some((Some(0), None)));
    assert_eq!(parse_single_range("bytes=5-10"), Some((Some(5), Some(10))));
    assert_eq!(parse_single_range("bytes=-500"), Some((None, Some(500))));
    assert_eq!(parse_single_range("bytes=10-5"), None);
    assert_eq!(parse_single_range("bytes= 0-"), None);
    assert_eq!(parse_single_range("bytes=0-1,3-4"), None);
}
