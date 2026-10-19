use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn static_frames_keep_duplicate_responses_clients_and_document_sheets() {
    let requests = Arc::new(AtomicUsize::new(0));
    let sheets = Arc::new(AtomicUsize::new(0));
    let bodies = requests.clone();
    let styles = sheets.clone();
    let server = crate::http::referer_server::Server::start(move |path| {
        let (mime, body) = match path {
            "/page" => (
                "text/html",
                "<body><iframe src='/same'></iframe><iframe src='/same'></iframe>".into(),
            ),
            "/same" => {
                let number = bodies.fetch_add(1, Ordering::Relaxed) + 1;
                (
                    "text/html",
                    format!("<link rel=stylesheet href='/shared.css'><p>CHILD {number}</p>"),
                )
            }
            "/shared.css" => {
                let number = styles.fetch_add(1, Ordering::Relaxed);
                (
                    "text/css",
                    if number == 0 {
                        "p{color:rgb(230,10,20)}"
                    } else {
                        "p{color:rgb(20,30,230)}"
                    }
                    .into(),
                )
            }
            _ => ("text/plain", String::new()),
        };
        (
            mime.into(),
            "Cache-Control: no-store\r\n".into(),
            body.into_bytes(),
        )
    })
    .await;
    let page = Url::parse(&format!("{}/page", server.base)).unwrap();
    let response = fetch(&Request::get(page.clone())).await.unwrap();
    let response = execute_js(response, (80, 24), (8, 16), Default::default()).await;
    assert!(
        response.live.is_none(),
        "an inert frame tree created a resident actor"
    );
    let body = String::from_utf8_lossy(&response.body);
    assert!(
        body.contains("CHILD 1") && body.contains("CHILD 2"),
        "{body}"
    );
    assert_eq!(requests.load(Ordering::Relaxed), 2);
    assert_eq!(sheets.load(Ordering::Relaxed), 2);
    let colors: Vec<_> = response
        .rendered
        .as_ref()
        .unwrap()
        .layout
        .paint
        .primitives
        .iter()
        .filter_map(|command| match command {
            crate::render::DisplayCommand::GlyphRun { shaped, color, .. }
                if shaped.text.contains("CHILD") =>
            {
                Some(*color)
            }
            _ => None,
        })
        .collect();
    assert!(
        colors.contains(&crate::render::PaintColor::Rgba(230, 10, 20, 255)),
        "{colors:?}"
    );
    assert!(
        colors.contains(&crate::render::PaintColor::Rgba(20, 30, 230, 255)),
        "{colors:?}"
    );
    let owner = response.service_client.as_ref().unwrap().clone();
    let registry = owner.client().registry().unwrap();
    let clients = registry.matching_clients(&page.origin().ascii_serialization(), 0, true, None);
    let children: Vec<_> = clients
        .iter()
        .filter(|client| client.url().path() == "/same")
        .collect();
    assert_eq!(children.len(), 2);
    assert_ne!(children[0].id, children[1].id);
    assert_eq!(
        clients.len(),
        3,
        "superseded reservations remained enumerable"
    );
    assert!(children.iter().all(|child| owner.contains(&child.id)));
    for head in server
        .heads
        .lock()
        .unwrap()
        .iter()
        .filter(|head| head.starts_with("GET /same "))
    {
        let head = head.to_ascii_lowercase();
        assert!(head.contains("sec-fetch-mode: navigate\r\n"), "{head}");
        assert!(head.contains("sec-fetch-dest: iframe\r\n"), "{head}");
    }
    // The retained deferred page must not create a root ownership cycle.
    let weak = owner.downgrade();
    drop(owner);
    drop(response);
    assert!(weak.upgrade().is_none());
    assert!(
        clients
            .iter()
            .all(|client| client.info().unwrap().is_none())
    );
}

#[tokio::test]
async fn static_frames_srcdoc_base_and_occurrence_binding_survive_duplicate_urls() {
    let server = crate::http::referer_server::Server::start(|path| {
        (
            "text/html".into(),
            "Cache-Control: no-store\r\n".into(),
            format!("<p>{path}</p>").into_bytes(),
        )
    })
    .await;
    let page = Url::parse(&format!("{}/page", server.base)).unwrap();
    let html = "<iframe src='/same'></iframe><iframe src='/same'></iframe><iframe srcdoc=\"<base href='/nested/'><iframe src='child'></iframe>\"></iframe>";
    let storage = Default::default();
    let blobs = Default::default();
    let owner = crate::trust_js_backend::prepare_service_worker_document(
        &storage,
        &blobs,
        tokio::runtime::Handle::current(),
        &page,
        crate::js::TraversableState::SHOWN,
    )
    .unwrap();
    let records = prefetch_frame_documents(
        html,
        &page,
        &page,
        encoding_rs::UTF_8,
        Default::default(),
        Some(&owner),
    )
    .await;
    assert_eq!(records.records.len(), 4);
    assert_eq!(records.records[&vec![2, 0]].url.path(), "/nested/child");
    let mut dom = crate::dom::Dom::parse_document(html);
    let nodes = frame_nodes(&dom, dom.document());
    let mut actor = ActorFrames::new(records, &dom, false);
    let first = actor
        .navigation(&dom, nodes[0], &format!("{}/same", server.base))
        .unwrap()
        .unwrap();
    let second = actor
        .navigation(&dom, nodes[1], &format!("{}/same", server.base))
        .unwrap()
        .unwrap();
    assert_ne!(
        first.response.service_client.as_ref().unwrap().client().id,
        second.response.service_client.as_ref().unwrap().client().id
    );
    assert!(
        actor
            .navigation(&dom, nodes[0], &format!("{}/same", server.base))
            .is_none()
    );
    // A newly inserted frame with the same attributes has a different node
    // identity and cannot claim either original navigation response.
    let inserted = dom.create_element("iframe");
    dom.set_attr(inserted, "src", "/same");
    let root = dom.document();
    dom.append(root, inserted);
    assert!(
        actor
            .navigation(&dom, inserted, &format!("{}/same", server.base))
            .is_none()
    );
}

#[test]
fn static_frames_document_imports_preserve_original_sources_and_cascades() {
    let page = Url::parse("https://frames.test/").unwrap();
    let mut dom = crate::dom::Dom::parse_document("<iframe id=a></iframe><iframe id=b></iframe>");
    dom.set_doc_url(Some(page.clone()));
    let import_url = "https://frames.test/shared.css";
    for (id, color) in [("a", "rgb(230,10,20)"), ("b", "rgb(20,30,230)")] {
        let frame = dom.get_by_id(id).unwrap();
        dom.install_frame_document(
            frame,
            "<style>@import '/shared.css';</style><p>TEXT</p>",
            page.as_str(),
        );
        let document = dom.frame_document(frame).unwrap();
        let sheet = PageSheet {
            link_href: None,
            url: import_url.into(),
            base: import_url.into(),
            text: format!("p{{color:{color}}}"),
            cors_same_origin: true,
            referrer_policy: None,
        };
        dom.attach_document_sheets(document, &[sheet]);
        let paragraph = dom
            .flat_descendants(document)
            .into_iter()
            .find(|&node| dom.tag_name(node) == Some("p"))
            .unwrap();
        assert_eq!(
            dom.computed_value_resolved(paragraph, "color").as_deref(),
            Some(color)
        );
        let source = dom
            .flat_descendants(document)
            .into_iter()
            .find(|&node| dom.tag_name(node) == Some("style"))
            .unwrap();
        assert_eq!(dom.text_content(source), "@import '/shared.css';");
    }
    let a = dom.frame_document(dom.get_by_id("a").unwrap()).unwrap();
    let b = dom.frame_document(dom.get_by_id("b").unwrap()).unwrap();
    assert_ne!(
        dom.imported_sheet_source(import_url, a),
        dom.imported_sheet_source(import_url, b)
    );
}
