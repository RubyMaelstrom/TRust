use super::*;
use tokio::{net::TcpListener, task::JoinHandle};

pub(crate) fn acceptor() -> tokio_rustls::TlsAcceptor {
    crate::tls::tests::unverified_acceptor(tokio_rustls::rustls::ALL_VERSIONS)
}

/// Each request gets a replacement certificate. Also require the client to
/// send close_notify, including when a Gopher terminator ends the read early.
pub(crate) async fn serve(
    replies: impl FnOnce(u16) -> Vec<(Vec<u8>, Vec<u8>)>,
) -> (GopherUrl, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.3:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let url = GopherUrl::parse(&format!("gophers://127.0.0.3:{port}/1/")).unwrap();
    let replies = replies(port);
    let server = tokio::spawn(async move {
        for (expected, response) in replies {
            let (socket, _) = timeout(Duration::from_secs(5), listener.accept())
                .await
                .unwrap()
                .unwrap();
            let mut stream = acceptor().accept(socket).await.unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n") {
                request.push(stream.read_u8().await.unwrap());
                assert!(request.len() <= MAX_REQUEST + 2);
            }
            assert_eq!(request, expected);
            stream.write_all(&response).await.unwrap();
            stream.shutdown().await.unwrap();
            let n = timeout(Duration::from_secs(3), stream.read(&mut [0; 1]))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                n, 0,
                "client must close cleanly without sending another request"
            );
        }
    });
    (url, server)
}

#[test]
fn gophers_urls_preserve_transport_and_opaque_selectors_across_routers() {
    for address in [
        "GOPHERS://EXAMPLE.TEST",
        "gophers://[::1]:7070/0/a%20b%FF",
        "gophers://e/0/a/../b%3Fquestion",
        "gophers://e/7/s%09query%09+text/plain",
    ] {
        let url = GopherUrl::parse(address).unwrap();
        assert!(url.tls);
        assert_eq!(GopherUrl::parse(&url.to_string()), Some(url.clone()));
        let target = Link::Gopher(url.clone());
        assert_eq!(absolute_link(address), Some(target.clone()));
        assert_eq!(crate::gemini::absolute_link(address), Some(target.clone()));
        assert_eq!(
            crate::core::parse_navigation_target(address).unwrap(),
            (target.clone(), false)
        );
        assert_eq!(
            crate::http::resolve(&url::Url::parse("https://web.test/").unwrap(), address),
            target
        );
    }
    let secure = GopherUrl::parse("gophers://e/1/").unwrap();
    let plain = GopherUrl::parse("gopher://e/1/").unwrap();
    assert_eq!(secure.port, 70);
    assert_ne!(secure, plain);
    assert_ne!(
        crate::bookmarks::canonical(&Link::Gopher(secure)).unwrap(),
        crate::bookmarks::canonical(&Link::Gopher(plain)).unwrap()
    );
    assert_eq!(
        GopherUrl::parse("gophers://e/0/a/../b%FF")
            .unwrap()
            .selector,
        b"/a/../b\xff"
    );
    for bad in [
        "gophers://",
        "gophers://e/0/a%0Ainject",
        "gophers://user:pass@e/1/",
        "gophers://e:bad/",
    ] {
        assert!(crate::core::parse_navigation_target(bad).is_err());
    }
}

#[test]
fn gophers_menu_links_inherit_tls_only_for_the_same_endpoint() {
    let base = GopherUrl::parse("gophers://e/1/menu").unwrap();
    let raw = b"0Local\t/text\te\t70\r\n\
                7Search\t/search\te\t70\t+\r\n\
                0Other port\t/text\te\t71\r\n\
                0Other host\t/text\tf\t70\r\n\
                hExplicit plaintext\tURL:gopher://e/0/text\te\t70\r\n\
                hExplicit TLS\tURL:gophers://f/0/text\te\t70\r\n\
                8Telnet\t\te\t70\r\n.\r\n";
    let doc = parse(&base, raw.to_vec(), false, 120);
    for (row, tls) in [
        (0, true),
        (1, true),
        (2, false),
        (3, false),
        (4, false),
        (5, true),
    ] {
        assert!(matches!(&doc.lines[row].link, Some(Link::Gopher(url)) if url.tls == tls));
    }
    assert!(matches!(
        doc.lines[6].link,
        Some(Link::Telnet { tls: false, .. })
    ));
    let Some(Link::Gopher(search)) = &doc.lines[1].link else {
        panic!()
    };
    assert!(search.with_query("hello").unwrap().tls);
    let info =
        information_target(doc.lines[1].link.as_ref(), Some(&Link::Gopher(base)), false).unwrap();
    assert!(matches!(info, Link::Gopher(url) if url.tls && url.is_metadata()));
}

#[test]
fn gophers_gemtext_and_html_relative_links_keep_the_base_scheme() {
    let base = GopherUrl::parse("gophers://e/0/dir/page.gmi").unwrap();
    for (href, expected) in [
        ("next.gmi", "gophers://e/0/dir/next.gmi"),
        ("//other:7070/0/file", "gophers://other:7070/0/file"),
        ("gopher://e/0/plain", "gopher://e/0/plain"),
    ] {
        assert_eq!(resolve_gmi(&base, href).to_string(), expected);
        let html_base = url::Url::parse(&base.to_string()).unwrap();
        assert_eq!(crate::http::resolve(&html_base, href).to_string(), expected);
    }
}

#[tokio::test]
async fn gophers_menu_search_information_and_views_survive_certificate_rotation() {
    let (base, server) = serve(|port| {
        let item = format!("0Text\t/text\t127.0.0.3\t{port}\t+");
        let info = format!("+INFO: {item}\r\n+VIEWS:\r\n text/plain: <6>\r\n");
        vec![
            (
                b"/\r\n".to_vec(),
                format!("7Search\t/search\t127.0.0.3\t{port}\t+\r\n{item}\r\n.\r\n").into_bytes(),
            ),
            (
                b"/search\twords\t+\r\n".to_vec(),
                format!("+-1\r\n{item}\r\n.\r\n").into_bytes(),
            ),
            (
                b"/text\t!\r\n".to_vec(),
                format!("+{}\r\n{info}", info.len()).into_bytes(),
            ),
            (
                b"/text\t+text/plain\r\n".to_vec(),
                b"+6\r\nHello\n".to_vec(),
            ),
        ]
    })
    .await;
    let root = fetch(&base).await.unwrap();
    assert!(root.reply.notice.is_none());
    let doc = render(&base, root, 120);
    let Some(Link::Gopher(search)) = &doc.lines[0].link else {
        panic!()
    };
    let search = search.with_query("words").unwrap();
    let results = fetch(&search).await.unwrap();
    assert!(results.reply.notice.is_none());
    let results = render(&search, results, 120);
    let Link::Gopher(info) =
        information_target(results.lines[0].link.as_ref(), None, false).unwrap()
    else {
        panic!()
    };
    assert!(info.tls);
    let metadata = render(&info, fetch(&info).await.unwrap(), 120);
    let view = metadata
        .lines
        .iter()
        .find_map(|line| match &line.link {
            Some(Link::Gopher(url)) if url.view_mime().as_deref() == Some("text/plain") => {
                Some(url.clone())
            }
            _ => None,
        })
        .unwrap();
    assert!(view.tls);
    let body = fetch(&view).await.unwrap();
    assert_eq!(body.reply.body, b"Hello\n");
    assert!(body.reply.notice.is_none());
    server.await.unwrap();
}

#[tokio::test]
async fn gophers_streaming_cancellation_closes_tls_and_retains_complete_lines() {
    let listener = TcpListener::bind("127.0.0.3:0").await.unwrap();
    let url = GopherUrl::parse(&format!(
        "gophers://127.0.0.3:{}/0/cancel",
        listener.local_addr().unwrap().port()
    ))
    .unwrap();
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut stream = acceptor().accept(socket).await.unwrap();
        while stream.read_u8().await.unwrap() != b'\n' {}
        stream.write_all(b"readable\r\npartial").await.unwrap();
        stream.flush().await.unwrap();
        assert_eq!(
            timeout(Duration::from_secs(3), stream.read(&mut [0; 1]))
                .await
                .unwrap()
                .unwrap(),
            0
        );
    });
    let mut updates = Vec::new();
    let result = timeout(
        Duration::from_secs(3),
        fetch_updates(&url, |reply| {
            updates.push(reply.body);
            async { false }
        }),
    )
    .await
    .unwrap();
    assert!(result.unwrap_err().contains("cancelled"));
    assert_eq!(updates, [b"readable\r\n".to_vec()]);
    server.await.unwrap();
}

#[tokio::test]
async fn gophers_binary_images_and_framed_downloads_use_tls() {
    let image = file_tests::webp();
    let archive = b"PK\x03\x04binary\0\xff\r\n.\r\n";
    let (mut url, server) = serve(|_| {
        vec![
            (b"/image.webp\r\n".to_vec(), image.clone()),
            (
                b"/archive\t+application/octet-stream\r\n".to_vec(),
                [format!("+{}\r\n", archive.len()).as_bytes(), archive].concat(),
            ),
        ]
    })
    .await;
    url.item_type = '9';
    url.selector = b"/image.webp".to_vec();
    let FileResponse::Document(image_response) = fetch_file(&url).await.unwrap() else {
        panic!()
    };
    assert_eq!(image_response.body, image);
    assert_eq!(image_response.content_type, "image/webp");
    assert_eq!(image_response.url.scheme(), "gophers");

    url.selector = b"/archive".to_vec();
    let offer =
        crate::download::DownloadOffer::from_gopher(url.with_plus(b"+application/octet-stream"))
            .unwrap();
    let path = std::env::temp_dir().join(format!(
        "trust-gophers-download-{}-{}.bin",
        std::process::id(),
        url.port
    ));
    assert_eq!(
        crate::download::save(&offer, &path).await.unwrap(),
        archive.len() as u64
    );
    assert_eq!(std::fs::read(&path).unwrap(), archive);
    std::fs::remove_file(path).unwrap();
    server.await.unwrap();
}
