use super::*;
use std::future::Future;
use tokio::net::TcpListener;
use tokio_rustls::{TlsAcceptor, TlsConnector, rustls};

type ServerStream = h3::server::RequestStream<h3_quinn::BidiStream<Bytes>, Bytes>;

struct Site {
    request: Request,
    client: rustls::ClientConfig,
    endpoint: quinn::Endpoint,
    tcp: tokio::task::JoinHandle<()>,
    quic: tokio::task::JoinHandle<()>,
    connections: Arc<AtomicUsize>,
    requests: Arc<AtomicUsize>,
    tcp_requests: Arc<AtomicUsize>,
}
impl Drop for Site {
    fn drop(&mut self) {
        self.tcp.abort();
        self.quic.abort();
        self.endpoint.close(0u32.into(), b"test done");
        POOL.lock()
            .unwrap()
            .remove(&PoolKey::for_request(&self.request).unwrap());
    }
}
impl Site {
    async fn new<F, Fut>(handler: F) -> Self
    where
        F: Fn(http_wire::Request<()>, ServerStream, quinn::Connection) -> Fut
            + std::marker::Send
            + Sync
            + 'static,
        Fut: Future<Output = ()> + std::marker::Send + 'static,
    {
        crate::tls::ensure_provider();
        let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let request = Request::get(
            url::Url::parse(&format!(
                "https://localhost:{}/",
                tcp.local_addr().unwrap().port()
            ))
            .unwrap(),
        );
        // Deliberately valid ONLY for the origin, not the advertised IP host.
        let signed = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let mut roots = rustls::RootCertStore::empty();
        roots.add(signed.cert.der().clone()).unwrap();
        let key =
            rustls::pki_types::PrivateKeyDer::try_from(signed.signing_key.serialize_der()).unwrap();
        let mut server = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![signed.cert.der().clone()], key)
            .unwrap();
        server.alpn_protocols = vec![b"h3".to_vec()];
        let crypto = quinn::crypto::rustls::QuicServerConfig::try_from(server.clone()).unwrap();
        let mut config = quinn::ServerConfig::with_crypto(Arc::new(crypto));
        let mut transport = quinn::TransportConfig::default();
        transport
            .stream_receive_window((64 * 1024u32).into())
            .receive_window((256 * 1024u32).into());
        config.transport_config(Arc::new(transport));
        let endpoint = quinn::Endpoint::server(config, "127.0.0.1:0".parse().unwrap()).unwrap();
        let advertisement = format!(
            "h3=\"127.0.0.1:{}\"; ma=300",
            endpoint.local_addr().unwrap().port()
        );
        server.alpn_protocols = vec![b"h2".to_vec()];
        let acceptor = TlsAcceptor::from(Arc::new(server));
        let mut client = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        client.alpn_protocols = vec![b"h2".to_vec()];
        super::super::http2::install_test_connector(
            &request,
            TlsConnector::from(Arc::new(client.clone())),
        );
        client.alpn_protocols = vec![b"h3".to_vec()];
        let connections = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(AtomicUsize::new(0));
        let tcp_requests = Arc::new(AtomicUsize::new(0));
        let count = tcp_requests.clone();
        let tcp = tokio::spawn(async move {
            let mut tasks = tokio::task::JoinSet::new();
            loop {
                let (stream, _) = tcp.accept().await.unwrap();
                let acceptor = acceptor.clone();
                let advertisement = advertisement.clone();
                let count = count.clone();
                tasks.spawn(async move {
                    let Ok(tls) = acceptor.accept(stream).await else {
                        return;
                    };
                    let mut connection = h2::server::handshake(tls).await.unwrap();
                    while let Some(Ok((_request, mut respond))) = connection.accept().await {
                        count.fetch_add(1, Ordering::SeqCst);
                        let response = http_wire::Response::builder()
                            .status(200)
                            .header("alt-svc", &advertisement)
                            .header("content-type", "text/plain")
                            .body(())
                            .unwrap();
                        respond
                            .send_response(response, false)
                            .unwrap()
                            .send_data(Bytes::from_static(b"tcp"), true)
                            .unwrap();
                    }
                });
            }
        });
        let quic = {
            let endpoint = endpoint.clone();
            let handler = Arc::new(handler);
            let connections = connections.clone();
            let requests = requests.clone();
            tokio::spawn(async move {
                let mut tasks = tokio::task::JoinSet::new();
                while let Some(incoming) = endpoint.accept().await {
                    let handler = handler.clone();
                    let connections = connections.clone();
                    let requests = requests.clone();
                    tasks.spawn(async move {
                        let Ok(quic) = incoming.await else { return };
                        connections.fetch_add(1, Ordering::SeqCst);
                        let mut connection =
                            h3::server::Connection::new(h3_quinn::Connection::new(quic.clone()))
                                .await
                                .unwrap();
                        let mut streams = tokio::task::JoinSet::new();
                        while let Ok(Some(request)) = connection.accept().await {
                            let handler = handler.clone();
                            let requests = requests.clone();
                            let quic = quic.clone();
                            streams.spawn(async move {
                                let (request, stream) = request.resolve_request().await.unwrap();
                                requests.fetch_add(1, Ordering::SeqCst);
                                handler(request, stream, quic).await;
                            });
                        }
                    });
                }
            })
        };
        Self {
            request,
            client,
            endpoint,
            tcp,
            quic,
            connections,
            requests,
            tcp_requests,
        }
    }

    async fn warm(&self) {
        let response = super::super::fetch_once(&self.request, None).await.unwrap();
        assert_eq!(response.timing.unwrap().next_hop_protocol, "h2");
        self.trust_alternative(&self.request);
    }
    fn trust_alternative(&self, request: &Request) {
        let key = PoolKey::for_request(request).unwrap();
        let mut pool = POOL.lock().unwrap();
        let entry = pool
            .get_mut(&key)
            .expect("HTTPS response must discover Alt-Svc");
        let origin = Arc::get_mut(&mut entry.origin).expect("not connected yet");
        origin.config = client_config(self.client.clone()).unwrap();
    }
    async fn fetch(&self, request: &Request) -> Result<super::super::Response, String> {
        tokio::time::timeout(
            Duration::from_secs(5),
            super::super::fetch_once(request, None),
        )
        .await
        .expect("HTTP/3 exchange deadlocked")
    }
    async fn lease(&self) -> Lease {
        acquire(
            &PoolKey::for_request(&self.request).unwrap(),
            &mut FetchTiming::new(),
        )
        .await
        .expect("local QUIC handshake")
    }
}

async fn ok(
    _request: http_wire::Request<()>,
    mut stream: ServerStream,
    _connection: quinn::Connection,
) {
    stream
        .send_response(
            http_wire::Response::builder()
                .header("content-length", "4")
                .body(())
                .unwrap(),
        )
        .await
        .unwrap();
    stream.send_data(Bytes::from_static(b"quic")).await.unwrap();
    stream.finish().await.unwrap();
}

#[tokio::test]
async fn http3_discovers_from_http2_authenticates_origin_and_reuses_session() {
    let site = Site::new(|request, stream, connection| async move {
        assert_eq!(request.uri().host(), Some("localhost"));
        assert!(
            request.headers()["alt-used"]
                .to_str()
                .unwrap()
                .starts_with("127.0.0.1:")
        );
        assert!(!request.headers().contains_key("connection"));
        ok(request, stream, connection).await;
    })
    .await;
    site.warm().await;
    let first = site.fetch(&site.request).await.unwrap();
    assert_eq!(first.body, b"quic");
    let timing = first.timing.unwrap();
    assert_eq!(timing.next_hop_protocol, "h3");
    assert!(!timing.connection_reused);
    assert_eq!(timing.connect_start, timing.secure_connection_start);
    assert!(timing.request_start <= timing.final_response_start);
    assert_eq!(timing.encoded_body_size, 4);
    let second = site.fetch(&site.request).await.unwrap();
    assert!(second.timing.unwrap().connection_reused);
    assert_eq!(site.connections.load(Ordering::SeqCst), 1);
    assert_eq!(site.tcp_requests.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn http3_multiplexes_without_serializing_slow_responses() {
    let gate = Arc::new(tokio::sync::Barrier::new(12));
    let site = Site::new(move |request, stream, conn| {
        let gate = gate.clone();
        async move {
            gate.wait().await;
            ok(request, stream, conn).await;
        }
    })
    .await;
    site.warm().await;
    let _lease = site.lease().await;
    let responses = futures::future::join_all((0..12).map(|_| site.fetch(&site.request))).await;
    for response in responses {
        assert_eq!(response.unwrap().body, b"quic");
    }
    assert_eq!(site.connections.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn http3_upload_and_response_progress_independently_across_flow_control() {
    let site = Site::new(|request, mut stream, _| async move {
        assert_eq!(request.method(), "POST");
        let mut received = 0;
        while let Some(data) = stream.recv_data().await.unwrap() {
            received += data.remaining();
        }
        assert_eq!(received, 3 * 1024 * 1024);
        stream
            .send_response(
                http_wire::Response::builder()
                    .header("content-length", received)
                    .body(())
                    .unwrap(),
            )
            .await
            .unwrap();
        for _ in 0..48 {
            stream
                .send_data(Bytes::from(vec![b'z'; UPLOAD_CHUNK]))
                .await
                .unwrap();
        }
        stream.finish().await.unwrap();
    })
    .await;
    site.warm().await;
    let mut request = site.request.clone();
    request.method = "POST".into();
    request.body = Some((
        "application/octet-stream".into(),
        vec![b'a'; 3 * 1024 * 1024],
    ));
    assert_eq!(
        site.fetch(&request).await.unwrap().body,
        vec![b'z'; 3 * 1024 * 1024]
    );
}

#[tokio::test]
async fn http3_early_final_response_survives_stopped_upload() {
    for code in [Code::H3_NO_ERROR, Code::H3_REQUEST_CANCELLED] {
        let site = Site::new(move |_, mut stream, _| async move {
            stream.stop_sending(code);
            tokio::time::sleep(Duration::from_millis(20)).await;
            stream
                .send_response(http_wire::Response::builder().status(413).body(()).unwrap())
                .await
                .unwrap();
            stream
                .send_data(Bytes::from_static(b"too big"))
                .await
                .unwrap();
            stream.finish().await.unwrap();
        })
        .await;
        site.warm().await;
        let mut request = site.request.clone();
        request.method = "POST".into();
        request.body = Some(("text/plain".into(), vec![b'x'; 8 * 1024 * 1024]));
        let response = site.fetch(&request).await.unwrap();
        assert_eq!(response.status, 413);
        assert_eq!(response.body, b"too big");
        assert_eq!(site.requests.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn http3_no_replay_of_ambiguous_post_but_explicit_rejection_can_fall_back() {
    for reject in [false, true] {
        let site = Site::new(move |_, mut stream, conn| async move {
            if reject {
                stream.stop_stream(Code::H3_REQUEST_REJECTED);
            } else {
                conn.close(
                    Code::H3_NO_ERROR.value().try_into().unwrap(),
                    b"lost response",
                );
            }
        })
        .await;
        site.warm().await;
        let mut request = site.request.clone();
        request.method = "POST".into();
        request.body = Some(("text/plain".into(), b"one submission".to_vec()));
        let response = site.fetch(&request).await;
        if reject {
            assert_eq!(response.unwrap().body, b"tcp");
        } else {
            assert!(response.is_err());
        }
        assert_eq!(
            site.tcp_requests.load(Ordering::SeqCst),
            if reject { 2 } else { 1 }
        );
    }
}

#[tokio::test]
async fn http3_misdirected_post_retries_origin_and_ignores_421_advertisement() {
    let site = Site::new(|_, mut stream, _| async move {
        stream
            .send_response(
                http_wire::Response::builder()
                    .status(421)
                    .header("alt-svc", "h3=\"untrusted.invalid:443\"")
                    .body(())
                    .unwrap(),
            )
            .await
            .unwrap();
        stream.finish().await.unwrap();
    })
    .await;
    site.warm().await;
    let mut request = site.request.clone();
    request.method = "POST".into();
    let response = site.fetch(&request).await.unwrap();
    assert_eq!(response.body, b"tcp");
    let pool = POOL.lock().unwrap();
    assert_eq!(
        pool[&PoolKey::for_request(&request).unwrap()]
            .origin
            .alternative
            .host,
        "127.0.0.1"
    );
}

#[tokio::test]
async fn http3_invalid_certificate_falls_back_without_sending_request() {
    let site = Site::new(ok).await;
    // Discover but do NOT install the test trust root for QUIC.
    site.fetch(&site.request).await.unwrap();
    let response = site.fetch(&site.request).await.unwrap();
    assert_eq!(response.timing.unwrap().next_hop_protocol, "h2");
    assert_eq!(site.requests.load(Ordering::SeqCst), 0);
    let key = PoolKey::for_request(&site.request).unwrap();
    assert!(matches!(
        *POOL.lock().unwrap()[&key].origin.state.lock().unwrap(),
        State::Failed(_)
    ));
}

#[tokio::test]
async fn http3_header_length_and_bodyless_validation() {
    for mode in ["length", "bodyless", "connection", "trailers"] {
        let site = Site::new(move |_, mut stream, _| async move {
            let mut response = http_wire::Response::builder();
            match mode {
                "length" => response = response.header("content-length", 10),
                "bodyless" => response = response.status(204),
                "connection" => response = response.header("connection", "close"),
                _ => {}
            }
            if stream
                .send_response(response.body(()).unwrap())
                .await
                .is_err()
            {
                return;
            }
            if stream.send_data(Bytes::from_static(b"bad")).await.is_err() {
                return;
            }
            if mode == "trailers" {
                let mut trailers = http_wire::HeaderMap::new();
                trailers.insert("connection", "close".parse().unwrap());
                let _ = stream.send_trailers(trailers).await;
            } else {
                let _ = stream.finish().await;
            }
        })
        .await;
        site.warm().await;
        assert!(site.fetch(&site.request).await.is_err(), "{mode}");
        assert_eq!(
            site.tcp_requests.load(Ordering::SeqCst),
            1,
            "malformed responses must not silently retry"
        );
    }
}

#[tokio::test]
async fn http3_informational_responses_trailers_and_head() {
    let site = Site::new(|request, mut stream, _| async move {
        stream
            .send_response(
                http_wire::Response::builder()
                    .status(103)
                    .header("link", "</test>; rel=preload")
                    .body(())
                    .unwrap(),
            )
            .await
            .unwrap();
        stream
            .send_response(
                http_wire::Response::builder()
                    .status(200)
                    .header("content-length", 4)
                    .body(())
                    .unwrap(),
            )
            .await
            .unwrap();
        if request.method() != "HEAD" {
            stream.send_data(Bytes::from_static(b"quic")).await.unwrap();
        }
        let mut trailers = http_wire::HeaderMap::new();
        trailers.insert("x-test-trailer", "kept separate".parse().unwrap());
        stream.send_trailers(trailers).await.unwrap();
    })
    .await;
    site.warm().await;
    let response = site.fetch(&site.request).await.unwrap();
    assert_eq!(response.body, b"quic");
    assert!(
        !response
            .headers
            .iter()
            .any(|(name, _)| name == "x-test-trailer")
    );
    let timing = response.timing.unwrap();
    assert!(timing.first_interim_response_start > 0.0);
    assert!(timing.first_interim_response_start <= timing.final_response_start);
    let mut request = site.request.clone();
    request.method = "HEAD".into();
    assert!(site.fetch(&request).await.unwrap().body.is_empty());
}

#[tokio::test]
async fn http3_cancel_pending_read_stops_both_halves() {
    let started = Arc::new(Notify::new());
    let canceled = Arc::new(Notify::new());
    let (report, mut reports) = tokio::sync::mpsc::unbounded_channel();
    let site = Site::new({
        let started = started.clone();
        let canceled = canceled.clone();
        move |request, mut stream, conn| {
            let started = started.clone();
            let canceled = canceled.clone();
            let report = report.clone();
            async move {
                if request.uri().path() != "/cancel" {
                    ok(request, stream, conn).await;
                    return;
                }
                started.notify_one();
                // Leave the upload flow-controlled and the client's response
                // read Pending until its fetch future has been dropped.
                canceled.notified().await;
                let reset = loop {
                    match stream.recv_data().await {
                        Ok(Some(_)) => {}
                        Err(StreamError::RemoteTerminate { code, .. }) => {
                            break code == Code::H3_REQUEST_CANCELLED;
                        }
                        _ => break false,
                    }
                };
                let stopped = match stream.send_response(http_wire::Response::new(())).await {
                    Err(StreamError::RemoteTerminate { code, .. }) => {
                        code == Code::H3_REQUEST_CANCELLED
                    }
                    _ => false,
                };
                report.send((reset, stopped)).unwrap();
            }
        }
    })
    .await;
    site.warm().await;
    let mut request = site.request.clone();
    request.url.set_path("/cancel");
    request.method = "POST".into();
    request.body = Some(("text/plain".into(), vec![0; 16 * 1024 * 1024]));
    let task = tokio::spawn(async move { super::super::fetch_once(&request, None).await });
    tokio::time::timeout(Duration::from_secs(3), started.notified())
        .await
        .unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    canceled.notify_one();
    let result = tokio::time::timeout(Duration::from_secs(3), reports.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result, (true, true));
    assert_eq!(site.fetch(&site.request).await.unwrap().body, b"quic");
    assert_eq!(site.connections.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn http3_download_streams_to_disk_and_enforces_its_own_limit() {
    let site = Site::new(|request, mut stream, _| async move {
        assert_eq!(request.headers()["accept-encoding"], "identity");
        stream
            .send_response(
                http_wire::Response::builder()
                    .header("content-length", 196608)
                    .body(())
                    .unwrap(),
            )
            .await
            .unwrap();
        for _ in 0..3 {
            if stream
                .send_data(Bytes::from(vec![b'd'; 65536]))
                .await
                .is_err()
            {
                return;
            }
        }
        let _ = stream.finish().await;
    })
    .await;
    site.warm().await;
    let lease = site.lease().await;
    let mut response = download(&lease, &site.request, 200000, &mut FetchTiming::new())
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    let path = std::env::temp_dir().join(format!(
        "trust-h3-download-{}-{}",
        std::process::id(),
        site.request.url.port().unwrap()
    ));
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .await
        .unwrap();
    assert_eq!(response.write_to(&mut file).await.unwrap(), 196608);
    drop(file);
    assert_eq!(tokio::fs::read(&path).await.unwrap(), vec![b'd'; 196608]);
    tokio::fs::remove_file(path).await.unwrap();
    assert!(
        download(&lease, &site.request, 100, &mut FetchTiming::new())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn http3_udp_blackhole_uses_tcp_promptly_and_single_flights_handshake() {
    let site = Site::new(ok).await;
    site.warm().await;
    // A bound UDP socket accepts packets but never answers: no ICMP shortcut.
    let blackhole = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let key = PoolKey::for_request(&site.request).unwrap();
    let origin = {
        let mut pool = POOL.lock().unwrap();
        let entry = pool.get_mut(&key).unwrap();
        Arc::get_mut(&mut entry.origin).unwrap().alternative.port =
            blackhole.local_addr().unwrap().port();
        entry.origin.clone()
    };
    let first = acquire(&key, &mut FetchTiming::new()).await;
    assert!(first.is_none());
    assert!(matches!(*origin.state.lock().unwrap(), State::Connecting));
    // Followers do not spend another 250 ms awaiting the in-flight probe.
    tokio::time::timeout(Duration::from_millis(100), async {
        assert!(acquire(&key, &mut FetchTiming::new()).await.is_none());
    })
    .await
    .unwrap();
    let response = site.fetch(&site.request).await.unwrap();
    assert_eq!(response.body, b"tcp");
    assert_eq!(site.requests.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn http3_cache_honors_expiry_clear_partition_credentials_and_backoff() {
    use super::super::{CookieContext, CredentialsMode, FetchPolicy, RequestMode};
    let request = Request::get(url::Url::parse("https://cache.h3-test.invalid/").unwrap());
    let key = PoolKey::for_request(&request).unwrap();
    let timing = FetchTiming::new();
    let headers = Headers::from([("alt-svc".into(), "h3=\":443\"; ma=300".into())]);
    let mut timing = timing;
    timing.request_start = crate::performance::now_ms();
    remember(&request, 403, &headers, &timing);
    let origin = POOL.lock().unwrap()[&key].origin.clone();
    *origin.state.lock().unwrap() = State::Failed(Instant::now() + RETRY_DELAY);
    remember(&request, 200, &headers, &timing);
    assert!(Arc::ptr_eq(&origin, &POOL.lock().unwrap()[&key].origin));
    assert!(acquire(&key, &mut FetchTiming::new()).await.is_none());
    let mut third_party = request.clone();
    third_party.cookie_context = Some(CookieContext::subresource(
        &url::Url::parse("https://elsewhere.invalid/").unwrap(),
    ));
    assert!(
        !POOL
            .lock()
            .unwrap()
            .contains_key(&PoolKey::for_request(&third_party).unwrap())
    );
    let mut anonymous = request.clone();
    anonymous.fetch_policy = Some(FetchPolicy {
        origin: request.url.clone(),
        mode: RequestMode::Cors,
        credentials: CredentialsMode::Omit,
    });
    assert!(
        !POOL
            .lock()
            .unwrap()
            .contains_key(&PoolKey::for_request(&anonymous).unwrap())
    );
    remember(
        &request,
        421,
        &Headers::from([("alt-svc".into(), "clear".into())]),
        &timing,
    );
    assert!(POOL.lock().unwrap().contains_key(&key));
    remember(
        &request,
        200,
        &Headers::from([("alt-svc".into(), "h3=\":443\", clear".into())]),
        &timing,
    );
    assert!(!POOL.lock().unwrap().contains_key(&key));
    remember(&request, 200, &headers, &timing);
    POOL.lock().unwrap().get_mut(&key).unwrap().expires = Instant::now();
    assert!(acquire(&key, &mut FetchTiming::new()).await.is_none());
    remember(&request, 200, &headers, &timing);
    forget_site("h3-test.invalid");
    assert!(!POOL.lock().unwrap().contains_key(&key));
}

#[tokio::test]
async fn http3_preserves_cookie_policy_and_decodes_compressed_content() {
    let site = Site::new(|request, mut stream, _| async move {
        let name = format!("h3_test_{}", request.uri().port_u16().unwrap());
        let cookie = request
            .headers()
            .get("cookie")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let body = if cookie.contains(&format!("{name}=saved")) {
            b"restored"
        } else {
            b"new-user"
        };
        use std::io::Write;
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(body).unwrap();
        let body = encoder.finish().unwrap();
        stream
            .send_response(
                http_wire::Response::builder()
                    .header(
                        "set-cookie",
                        format!("{name}=saved; Secure; HttpOnly; SameSite=Lax; Path=/"),
                    )
                    .header("content-encoding", "gzip")
                    .header("content-length", body.len())
                    .body(())
                    .unwrap(),
            )
            .await
            .unwrap();
        stream.send_data(Bytes::from(body)).await.unwrap();
        stream.finish().await.unwrap();
    })
    .await;
    site.warm().await;
    let mut request = site.request.clone();
    request.cookie_context = Some(super::super::CookieContext::subresource(&request.url));
    assert_eq!(site.fetch(&request).await.unwrap().body, b"new-user");
    let response = site.fetch(&request).await.unwrap();
    assert_eq!(response.body, b"restored");
    assert!(
        !response
            .headers
            .iter()
            .any(|(name, _)| name == "set-cookie")
    );
    let timing = response.timing.unwrap();
    assert!(timing.encoded_body_size > timing.decoded_body_size);
}

#[tokio::test]
#[ignore = "live HTTP/3 diagnostic; set TRUST_NET_DIAG to an HTTPS URL"]
async fn http3_live_transport_probe() {
    let url = std::env::var("TRUST_NET_DIAG").expect("set TRUST_NET_DIAG");
    let mut request = Request::get(url::Url::parse(&url).unwrap());
    super::super::set_navigation_metadata(&mut request, None);
    let mut saw_h3 = false;
    for _ in 0..3 {
        let response = super::super::fetch_once(&request, None).await.unwrap();
        let timing = response.timing.unwrap();
        eprintln!(
            "transport: status={} protocol={} reused={} bytes={} challenge={} elapsed_ms={:.1}",
            response.status,
            timing.next_hop_protocol,
            timing.connection_reused,
            response.body.len(),
            response.challenge.is_some(),
            timing.response_end - timing.start_time
        );
        saw_h3 |= timing.next_hop_protocol == "h3";
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    assert!(
        saw_h3,
        "no HTTP/3 response (check Alt-Svc and UDP reachability)"
    );
}

#[tokio::test]
async fn http3_rejects_oversized_encoded_header_before_buffering_payload() {
    let mut site = Site::new(ok).await;
    site.warm().await;
    site.quic.abort();
    let _ = (&mut site.quic).await;
    let endpoint = site.endpoint.clone();
    let raw = tokio::spawn(async move {
        let conn = endpoint.accept().await.unwrap().await.unwrap();
        let mut control = conn.open_uni().await.unwrap();
        control.write_all(&[0, 4, 0]).await.unwrap(); // control + empty SETTINGS
        let (mut send, _recv) = conn.accept_bi().await.unwrap();
        // A HEADERS frame advertising 512 KiB+1, without ANY payload.
        send.write_all(&[1, 0x80, 8, 0, 1]).await.unwrap();
        let stopped = send.stopped().await.unwrap().unwrap();
        assert_eq!(stopped.into_inner(), Code::H3_EXCESSIVE_LOAD.value());
    });
    assert!(
        site.fetch(&site.request)
            .await
            .unwrap_err()
            .contains("metadata exceeds")
    );
    tokio::time::timeout(Duration::from_secs(3), raw)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(site.tcp_requests.load(Ordering::SeqCst), 1);
}
