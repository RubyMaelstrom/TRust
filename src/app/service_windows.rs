//! Native terminal WindowClient actions (Service Workers 92aba3be
//! #client-focus, #client-navigate, #clients-openwindow). A static Document
//! owns its identity and deferred page independently of a resident actor.

use super::*;
use crate::service_worker::{Error, WindowAction, WindowOperation};
use std::sync::Arc;

const PENDING_ACTIONS: usize = 32;
const ATTACHMENT_TIMEOUT: Duration = Duration::from_secs(30);
const FOCUS_TIMEOUT: Duration = Duration::from_secs(5);

async fn inspect_focused_window(
    client: &crate::service_worker::Client,
    nested: bool,
) -> Result<Option<crate::service_worker::ClientInfo>, Error> {
    if nested {
        // A retained static Document has no task endpoint until its page
        // actor and child Realm exist. That bounded materialization phase
        // precedes SW #client-focus's focusing/inspection task; it must not
        // consume the native focus acknowledgment budget.
        match tokio::time::timeout(ATTACHMENT_TIMEOUT, client.await_attachment()).await {
            Ok(true) => {}
            Ok(false) => return Err(Error::type_error("The frame is no longer available")),
            Err(_) => return Err(Error::type_error("The frame did not become available")),
        }
    }
    tokio::time::timeout(FOCUS_TIMEOUT, client.inspect())
        .await
        .map_err(|_| Error::type_error("The terminal did not focus the document"))
        .and_then(|result| result)
        .and_then(|info| {
            if info.as_ref().is_some_and(|info| info.window.focused) {
                Ok(info)
            } else {
                Err(Error::type_error("The document could not be focused"))
            }
        })
}

pub(super) struct ServiceWindows {
    tx: mpsc::Sender<Arc<WindowAction>>,
    pub(super) rx: mpsc::Receiver<Arc<WindowAction>>,
    focusing: Vec<Arc<WindowAction>>,
    // Terminal focus reports and actual keyboard input are the available
    // native evidence. The frontend cannot activate an OS window itself.
    focused: bool,
    focus_sent: Option<bool>,
}

impl Default for ServiceWindows {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel(PENDING_ACTIONS);
        Self {
            tx,
            rx,
            focusing: Vec::new(),
            focused: false,
            focus_sent: None,
        }
    }
}

impl App {
    pub(super) fn current_service_client(&self) -> Option<Arc<crate::service_worker::Client>> {
        self.browser
            .as_ref()?
            .doc
            .service_client
            .as_ref()
            .map(|owner| owner.client())
    }
    pub(super) fn terminal_focus_changed(&mut self, focused: bool) {
        if self.service_windows.focused == focused {
            return;
        }
        self.service_windows.focused = focused;
        if let Some(document) = self
            .browser
            .as_ref()
            .and_then(|page| page.doc.service_client.as_ref())
        {
            document.presentation(false, focused);
        }
        self.sync_terminal_focus();
    }

    pub(super) fn sync_terminal_focus(&mut self) {
        let focused = self.service_windows.focused;
        if self.service_windows.focus_sent == Some(focused) {
            return;
        }
        if let Some(page) = &self.live_page
            && page
                .cmds
                .try_send(crate::js::PageCmd::SystemFocus(focused))
                .is_ok()
        {
            self.service_windows.focus_sent = Some(focused);
        }
    }

    pub(super) fn bind_service_document(&mut self) {
        let Some(document) = self
            .browser
            .as_ref()
            .and_then(|page| page.doc.service_client.as_ref())
        else {
            return;
        };
        document.presentation(false, self.service_windows.focused);
        let tx = self.service_windows.tx.clone();
        document.bind_actions(Arc::new(move |action| {
            if let Err(error) = tx.try_send(action) {
                error.into_inner().reply.complete(Err(Error::type_error(
                    "The terminal cannot accept another window operation",
                )));
            }
        }));
        document.committed();
    }

    pub(super) fn attach_service_live_page(&mut self, live: http::LivePage) {
        self.live_page = Some(live.handle);
        self.page_rx = Some(live.events);
        // Synchronize the current terminal viewport, scroll and history on
        // its next turn, including when a static page resumes in place.
        self.last_scroll_sent = None;
        self.image_sizes_sent = None;
        self.viewport_sent = None;
        self.session_history_sent = None;
        self.page_history_updates = 0;
        self.hover_sent = None;
        self.hover_want = None;
        self.service_windows.focus_sent = None;
        self.sync_terminal_focus();
    }

    fn service_page_available(&mut self, document: &crate::js::DocumentClient) -> bool {
        if self
            .live_page
            .as_ref()
            .is_some_and(|page| !page.cmds.is_closed())
        {
            return true;
        }
        let Some(live) = http::frames::resume_static_page(
            document,
            crate::js::TraversableState {
                focused: self.service_windows.focused,
                ..crate::js::TraversableState::SHOWN
            },
        ) else {
            return false;
        };
        self.attach_service_live_page(live);
        true
    }

    pub(super) fn service_window_action(&mut self, action: Arc<WindowAction>) {
        if !action.reply.pending() {
            return;
        }
        let document = self
            .browser
            .as_ref()
            .and_then(|page| page.doc.service_client.clone());
        let Some(document) = document.filter(|document| document.contains(&action.client.id))
        else {
            action.reply.complete(Err(Error::type_error(
                "The document is no longer fully active",
            )));
            return;
        };
        if !matches!(action.client.info(), Ok(Some(_))) {
            action.reply.complete(Err(Error::type_error(
                "The document is no longer fully active",
            )));
            return;
        }
        let nested = action.client.id != document.client().id;
        match &action.operation {
            WindowOperation::Focus => {
                // HTML #has-focus-steps first checks system focus. Selecting
                // this browser view cannot activate a background terminal.
                if !self.service_windows.focused {
                    action.reply.complete(Err(Error::type_error(
                        "The terminal does not have system focus",
                    )));
                    return;
                }
                self.service_windows
                    .focusing
                    .retain(|action| action.reply.pending());
                if self.service_windows.focusing.len() >= PENDING_ACTIONS {
                    action
                        .reply
                        .complete(Err(Error::type_error("Too many pending focus requests")));
                    return;
                }
                if nested && !self.service_page_available(&document) {
                    action
                        .reply
                        .complete(Err(Error::type_error("The frame is no longer available")));
                    return;
                }
                self.mode = Mode::Session;
                self.viewer = None;
                action.client.focus_window();
                document.presentation(false, self.service_windows.focused);
                self.sync_terminal_focus();
                let pending = action.clone();
                tokio::spawn(async move {
                    let result = inspect_focused_window(&pending.client, nested).await;
                    pending.reply.complete(result);
                });
                self.service_windows.focusing.push(action);
            }
            WindowOperation::Navigate(_) if nested => {
                if !self.service_page_available(&document) {
                    action
                        .reply
                        .complete(Err(Error::type_error("The frame is no longer available")));
                    return;
                }
                // Attachment queues this action until restored child realms
                // exist; it never navigates the terminal's top-level page.
                action.client.send_window_action(action.clone());
            }
            WindowOperation::Navigate(url) => {
                if self
                    .live_page
                    .as_ref()
                    .is_some_and(|page| !page.cmds.is_closed())
                {
                    action.client.send_window_action(action.clone());
                    return;
                }
                if !self.allow_page_file_navigation(&Link::Http(url.clone())) {
                    action
                        .reply
                        .complete(Err(Error::type_error("The navigation is not allowed")));
                    return;
                }
                let policy = self.browser.as_ref().unwrap().doc.referrer_policy;
                let mut initiator =
                    http::NavigationInitiator::document(&action.client.url(), policy);
                initiator.service_worker_context =
                    crate::trust_js_backend::service_worker_fetch::for_document(&document);
                initiator.service_worker_reply = Some(action.reply.clone());
                self.replace_nav = false;
                self.start_fetch_opts(Link::Http(url.clone()), false, Some(initiator));
            }
            WindowOperation::Open(_) => action.reply.complete(Err(Error::type_error(
                "The terminal cannot open another window",
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service_worker::{Client, ClientInfo, ClientKind};

    fn native_document(app: &mut App, url: &Url) -> Arc<Client> {
        let mut doc = http::parse(
            url,
            "text/html",
            b"<body>native document",
            80,
            24,
            &Default::default(),
        );
        let owner = crate::trust_js_backend::prepare_service_worker_document(
            &app.web_storage,
            &Default::default(),
            tokio::runtime::Handle::current(),
            url,
            crate::js::TraversableState::SHOWN,
        )
        .unwrap();
        let client = owner.client();
        doc.service_client = Some(owner);
        app.navigate_to(doc);
        client
    }

    async fn finish_fetch(app: &mut App) {
        let message = tokio::time::timeout(
            Duration::from_secs(30),
            app.fetch_rx.as_mut().expect("fetch was started").recv(),
        )
        .await
        .expect("fetch completed")
        .expect("fetch reply");
        assert!(
            message.result.is_ok(),
            "{:?}",
            message.result.as_ref().err()
        );
        app.on_fetch(message);
    }

    fn drain(app: &mut App) {
        loop {
            let action = app.service_windows.rx.try_recv().ok();
            let Some(action) = action else { break };
            app.service_window_action(action);
        }
        loop {
            let message = app.fetch_rx.as_mut().and_then(|rx| rx.try_recv().ok());
            let Some(message) = message else { break };
            app.on_fetch(message);
        }
        for _ in 0..64 {
            let event = app.page_rx.as_mut().and_then(|rx| rx.try_recv().ok());
            let Some(event) = event else { break };
            app.on_page_evt(event);
        }
    }

    async fn action(
        app: &mut App,
        client: &Arc<Client>,
        operation: WindowOperation,
    ) -> Result<Option<ClientInfo>, Error> {
        let client = client.clone();
        let key = client.storage_key();
        let pending = tokio::spawn(async move { client.window_action(operation, key).await });
        let deadline = tokio::time::Instant::now()
            + ATTACHMENT_TIMEOUT
            + FOCUS_TIMEOUT
            + Duration::from_secs(5);
        while !pending.is_finished() {
            drain(app);
            assert!(tokio::time::Instant::now() < deadline, "{}", app.status);
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        pending.await.unwrap()
    }

    #[tokio::test]
    async fn terminal_service_focus_uses_native_focus_and_rejects_unsupported_open() {
        let mut app = App::new(None, 23);
        app.web_storage = Default::default();
        let client = native_document(
            &mut app,
            &Url::parse("https://terminal.test/start").unwrap(),
        );
        let failed = action(&mut app, &client, WindowOperation::Focus).await;
        assert_eq!(failed.unwrap_err().name, "TypeError");
        assert!(!client.info().unwrap().unwrap().window.focused);
        app.on_terminal_event(TermEvent::FocusGained).await;
        app.mode = Mode::Command;
        let focused = action(&mut app, &client, WindowOperation::Focus)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(focused.id, client.id);
        assert!(focused.window.focused);
        assert_eq!(app.mode, Mode::Session);
        assert!(
            app.live_page.is_none(),
            "native top-level focus needs no actor"
        );
        app.on_terminal_event(TermEvent::FocusLost).await;
        assert!(!client.info().unwrap().unwrap().window.focused);
        let failed = action(&mut app, &client, WindowOperation::Focus).await;
        assert_eq!(failed.unwrap_err().name, "TypeError");
        let failed = action(
            &mut app,
            &client,
            WindowOperation::Open(Url::parse("https://terminal.test/other").unwrap()),
        )
        .await;
        assert_eq!(failed.unwrap_err().name, "TypeError");
        assert!(app.fetch_rx.is_none());
        assert_eq!(
            app.browser.as_ref().unwrap().doc.url.to_string(),
            client.url().as_str()
        );
    }

    #[tokio::test]
    async fn terminal_service_focus_reports_retry_after_busy_actor() {
        let mut app = App::new(None, 23);
        let (sender, mut receiver) = mpsc::channel(1);
        app.live_page = Some(crate::js::PageHandle::from_test_sender(sender));
        app.terminal_focus_changed(true);
        assert!(matches!(
            receiver.recv().await,
            Some(crate::js::PageCmd::SystemFocus(true))
        ));
        app.live_page
            .as_ref()
            .unwrap()
            .cmds
            .try_send(crate::js::PageCmd::SystemFocus(true))
            .unwrap();
        app.terminal_focus_changed(false);
        assert!(matches!(
            receiver.recv().await,
            Some(crate::js::PageCmd::SystemFocus(true))
        ));
        app.sync_terminal_focus();
        assert!(matches!(
            receiver.recv().await,
            Some(crate::js::PageCmd::SystemFocus(false))
        ));
        app.sync_terminal_focus();
        assert!(
            receiver.try_recv().is_err(),
            "settled focus emits no idle work"
        );
        app.drop_live_page();
    }

    #[tokio::test(start_paused = true)]
    async fn terminal_service_focus_deadlines_separate_attachment_and_acknowledgment() {
        use crate::service_worker::{Notice, Registry, WindowState};
        let registry = Registry::new(Arc::new(|_| {}), None);
        let url = Url::parse("https://terminal.test/frame").unwrap();
        let client = registry
            .reserve_client_with_origin(
                url.origin().ascii_serialization(),
                url.clone(),
                url.origin(),
                false,
            )
            .unwrap();
        client.set_secure_context(true);
        client.set_execution_ready();
        let focusing = client.clone();
        let pending = tokio::spawn(async move { inspect_focused_window(&focusing, true).await });
        tokio::task::yield_now().await;
        tokio::time::advance(FOCUS_TIMEOUT + Duration::from_secs(1)).await;
        assert!(
            !pending.is_finished(),
            "cold materialization is not a focus acknowledgment"
        );
        let (inspections, mut receive) = mpsc::unbounded_channel();
        client.attach(Arc::new(move |notice| {
            if let Notice::Inspect { inspection } = notice {
                inspections.send(inspection).unwrap();
            }
        }));
        let inspection = receive.recv().await.unwrap();
        tokio::time::advance(FOCUS_TIMEOUT - Duration::from_secs(1)).await;
        assert!(!pending.is_finished());
        inspection.complete(Some(WindowState {
            focused: true,
            focus_within: true,
            frame_type: "nested".into(),
            ..Default::default()
        }));
        assert_eq!(pending.await.unwrap().unwrap().unwrap().id, client.id);

        let never_attaches = registry
            .reserve_client_with_origin(
                url.origin().ascii_serialization(),
                url.clone(),
                url.origin(),
                false,
            )
            .unwrap();
        let pending =
            tokio::spawn(async move { inspect_focused_window(&never_attaches, true).await });
        tokio::task::yield_now().await;
        tokio::time::advance(ATTACHMENT_TIMEOUT + Duration::from_secs(1)).await;
        assert_eq!(
            pending.await.unwrap().unwrap_err().message,
            "The frame did not become available"
        );

        let unresponsive = client.clone();
        let pending =
            tokio::spawn(async move { inspect_focused_window(&unresponsive, true).await });
        let _inspection = receive.recv().await.unwrap();
        tokio::time::advance(FOCUS_TIMEOUT + Duration::from_secs(1)).await;
        assert_eq!(
            pending.await.unwrap().unwrap_err().message,
            "The terminal did not focus the document"
        );
    }

    #[tokio::test]
    async fn terminal_service_stale_and_saturated_actions_finish_without_navigation() {
        let mut app = App::new(None, 23);
        app.web_storage = Default::default();
        let old = native_document(&mut app, &Url::parse("https://terminal.test/old").unwrap());
        let request_client = old.clone();
        let pending = tokio::spawn(async move {
            request_client
                .window_action(WindowOperation::Focus, request_client.storage_key())
                .await
        });
        let stale = tokio::time::timeout(Duration::from_secs(5), app.service_windows.rx.recv())
            .await
            .unwrap()
            .unwrap();
        let current = native_document(
            &mut app,
            &Url::parse("https://terminal.test/current").unwrap(),
        );
        app.service_window_action(stale);
        assert_eq!(pending.await.unwrap().unwrap_err().name, "TypeError");
        assert!(old.info().unwrap().is_none());
        let mut pending = Vec::new();
        for _ in 0..=PENDING_ACTIONS {
            let client = current.clone();
            pending.push(tokio::spawn(async move {
                client
                    .window_action(WindowOperation::Open(client.url()), client.storage_key())
                    .await
            }));
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !pending.iter().any(|request| request.is_finished()) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "queue overflow did not reject"
            );
            tokio::task::yield_now().await;
        }
        assert_eq!(app.service_windows.rx.len(), PENDING_ACTIONS);
        drain(&mut app);
        for request in pending {
            assert_eq!(request.await.unwrap().unwrap_err().name, "TypeError");
        }
        assert!(app.fetch_rx.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn terminal_service_navigation_commits_native_client_and_history() {
        let server = http::referer_server::Server::start(|_| {
            (
                "text/html".into(),
                String::new(),
                b"<body>terminal navigation".to_vec(),
            )
        })
        .await;
        let mut app = App::new(None, 23);
        app.web_storage = Default::default();
        app.js_enabled = true;
        let start = Url::parse(&format!("{}/start", server.base)).unwrap();
        let client = native_document(&mut app, &start);
        let next = start.join("/next").unwrap();
        let navigated = action(&mut app, &client, WindowOperation::Navigate(next.clone()))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(navigated.url, next.as_str());
        assert_ne!(navigated.id, client.id);
        assert!(client.info().unwrap().is_none());
        let browser = app.browser.as_ref().unwrap();
        assert_eq!(browser.doc.url.to_string(), next.as_str());
        assert_eq!(browser.history.len(), 1);
        assert_eq!(browser.history[0].url.to_string(), start.as_str());
        assert!(browser.forward.is_empty());
        assert_eq!(server.referer("/next"), Some(Some(start.to_string())));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn terminal_service_nested_actions_resume_their_own_static_document() {
        let server = http::referer_server::Server::start(|path| {
            let body = if path == "/page" {
                "<body>top<iframe src='/frame'></iframe><iframe src='/sibling'></iframe>"
            } else if path == "/next" {
                "<body>next frame"
            } else {
                "<body>static frame"
            };
            ("text/html".into(), String::new(), body.as_bytes().to_vec())
        })
        .await;
        let mut app = App::new(None, 23);
        app.web_storage = Default::default();
        app.js_enabled = true;
        app.on_terminal_event(TermEvent::FocusGained).await;
        let top = Url::parse(&format!("{}/page", server.base)).unwrap();
        app.start_fetch(Link::Http(top.clone()));
        finish_fetch(&mut app).await;
        assert!(
            app.live_page.is_none(),
            "plain frame documents remain inert"
        );
        let owner = app
            .browser
            .as_ref()
            .unwrap()
            .doc
            .service_client
            .as_ref()
            .unwrap()
            .clone();
        let root = owner.client();
        let clients = root.registry().unwrap().matching_clients(
            &root.storage_key(),
            0,
            true,
            Some(ClientKind::Window),
        );
        let frame = clients
            .iter()
            .find(|client| client.url().path() == "/frame")
            .unwrap()
            .clone();
        let sibling = clients
            .iter()
            .find(|client| client.url().path() == "/sibling")
            .unwrap()
            .clone();
        let focused = action(&mut app, &frame, WindowOperation::Focus)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            focused.id, frame.id,
            "activation retains the committed child identity"
        );
        assert!(focused.window.focused);
        assert!(app.live_page.is_some());
        let next = top.join("/next").unwrap();
        let navigated = action(&mut app, &frame, WindowOperation::Navigate(next.clone()))
            .await
            .unwrap()
            .unwrap();
        assert_ne!(navigated.id, frame.id);
        assert_eq!(navigated.url, next.as_str());
        assert_eq!(navigated.window.frame_type, "nested");
        let browser = app.browser.as_ref().unwrap();
        assert_eq!(browser.doc.url.to_string(), top.as_str());
        assert_eq!(
            browser.doc.service_client.as_ref().unwrap().client().id,
            root.id
        );
        assert!(browser.history.is_empty() && browser.forward.is_empty());
        assert_eq!(
            sibling.info().unwrap().unwrap().url,
            top.join("/sibling").unwrap().as_str()
        );
        assert!(owner.contains(&sibling.id));
        assert!(owner.contains(&navigated.id));
        app.drop_live_page();
    }

    #[tokio::test]
    async fn terminal_service_no_document_responses_keep_actor_history_and_settle_reply() {
        // HTML e5071a20 #populating-a-session-history-entry and
        // #navigation-as-a-download: status/disposition prevent a new
        // Document even when the response carries renderable HTML bytes.
        let server = http::referer_server::Server::start(|_| {
            (
                "text/html".into(),
                String::new(),
                b"<body>unused navigation body".to_vec(),
            )
        })
        .await;
        let mut app = App::new(None, 23);
        app.web_storage = Default::default();
        let previous = Url::parse(&format!("{}/previous", server.base)).unwrap();
        native_document(&mut app, &previous);
        let start = previous.join("/start").unwrap();
        let client = native_document(&mut app, &start);
        let (sender, mut actor_commands) = mpsc::channel(16);
        app.live_page = Some(crate::js::PageHandle::from_test_sender(sender));
        let generation = app.document_generation;
        let original_body = app.browser.as_ref().unwrap().doc.raw.clone();
        for status in [204, 205, 200] {
            let target = start.join(&format!("/response-{status}")).unwrap();
            let asking = client.clone();
            let operation = WindowOperation::Navigate(target.clone());
            let reply =
                tokio::spawn(
                    async move { asking.window_action(operation, asking.storage_key()).await },
                );
            let queued =
                tokio::time::timeout(Duration::from_secs(5), app.service_windows.rx.recv())
                    .await
                    .unwrap()
                    .unwrap();
            // Supply the fetched destination reservation at the frontend
            // boundary. The internal native task is deliberately direct;
            // author-visible navigate() separately requires a controller.
            let reserved = crate::trust_js_backend::prepare_service_worker_document(
                &app.web_storage,
                &Default::default(),
                tokio::runtime::Handle::current(),
                &target,
                crate::js::TraversableState::SHOWN,
            )
            .unwrap();
            let reserved_client = reserved.client();
            reserved.navigation_reply(Some(queued.reply.clone()));
            let mut response = http::fetch(&http::Request::get(target.clone()))
                .await
                .unwrap();
            response.status = status;
            response.service_client = Some(reserved);
            if status == 200 {
                response.headers.push((
                    "content-disposition".into(),
                    "attachment; filename=sample.html".into(),
                ));
            }
            let payload =
                prepare_http_payload(response, (80, 24), (8, 16), app.web_storage.clone(), true)
                    .await;
            assert_eq!(matches!(payload, Payload::Http(_)), status != 200);
            app.replace_nav = true;
            app.nav_from_post = true;
            app.pending_travel = Some(-1);
            app.on_fetch(FetchMsg {
                target: Link::Http(target),
                result: Ok(payload),
            });
            let surviving = tokio::time::timeout(Duration::from_secs(5), reply)
                .await
                .unwrap()
                .unwrap()
                .unwrap()
                .unwrap();
            assert_eq!(surviving.id, client.id);
            assert_eq!(surviving.url, start.as_str());
            assert!(reserved_client.info().unwrap().is_none());
            assert!(
                app.live_page.is_some(),
                "a response without a Document retains its actor"
            );
            assert_eq!(app.document_generation, generation);
            let browser = app.browser.as_ref().unwrap();
            assert_eq!(browser.doc.raw, original_body);
            assert_eq!(
                browser.doc.service_client.as_ref().unwrap().client().id,
                client.id
            );
            assert_eq!(browser.doc.url.to_string(), start.as_str());
            assert_eq!(browser.history.len(), 1);
            assert_eq!(browser.history[0].url.to_string(), previous.as_str());
            assert!(browser.forward.is_empty());
            assert!(!app.replace_nav && !app.nav_from_post && app.pending_travel.is_none());
            assert_eq!(app.file_dialog.is_some(), status == 200);
            app.live_page
                .as_ref()
                .unwrap()
                .cmds
                .try_send(crate::js::PageCmd::SystemFocus(true))
                .unwrap();
            assert!(matches!(
                actor_commands.recv().await,
                Some(crate::js::PageCmd::SystemFocus(true))
            ));
        }
        app.drop_live_page();
    }
}
