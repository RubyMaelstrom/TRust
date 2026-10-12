//! A page as its traversable presents it: HTML #page-visibility (system
//! visibility state, visibilitychange, VisibilityStateEntry), HTML
//! #update-the-rendering for hidden Documents, HTML #system-focus and the
//! focus update steps, HTML #document.title, Page Lifecycle
//! #dom-document-wasdiscarded, and the reports the frontend's tab strip
//! shows. Local snapshots: HTML e5071a20c856, Page Lifecycle 5ea95470.
use super::*;
use crate::js::{PageCmd, PageEnv, PageEvt, TraversableState};

const URL: &str = "https://tabs.example/page";

/// Run `html` in a resident actor presented as `state`, wait for its first
/// presentation, send `commands` (each followed by a short pause), and
/// return the serialized document of the last presentation in which
/// `until` holds, with every event seen.
async fn run_page(
    html: &str,
    state: TraversableState,
    commands: Vec<PageCmd>,
    until: impl Fn(&str) -> bool,
) -> (String, Vec<PageEvt>) {
    let mut env = PageEnv::bare(URL);
    env.terminal_presentation = false;
    env.traversable = state;
    let (handle, mut events) = crate::js::spawn_page(html.into(), env);
    let mut seen = Vec::new();
    let mut last = String::new();
    let mut record = |event: PageEvt, last: &mut String| {
        if let PageEvt::Updated { html, .. } | PageEvt::Static { html, .. } = &event
            && !html.is_empty()
        {
            *last = html.clone();
        }
        seen.push(event);
    };
    match tokio::time::timeout(Duration::from_secs(30), events.recv()).await {
        Ok(Some(event)) => record(event, &mut last),
        _ => panic!("no presentation"),
    }
    for command in commands {
        if command.is_user_interaction() {
            handle.try_send_user(command).unwrap();
        } else {
            handle.cmds.try_send(command).unwrap();
        }
        let pause = tokio::time::Instant::now() + Duration::from_millis(150);
        while let Ok(Some(event)) = tokio::time::timeout_at(pause, events.recv()).await {
            record(event, &mut last);
        }
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !until(&last) {
        match tokio::time::timeout_at(deadline, events.recv()).await {
            Ok(Some(event)) => record(event, &mut last),
            _ => panic!("the page never reached the expected state:\n{last}"),
        }
    }
    drop(handle);
    (last, seen)
}

fn output(html: &str) -> String {
    let start = html
        .find("<output id=\"o\">")
        .map(|at| at + 15)
        .unwrap_or(0);
    let end = html[start..]
        .find("</output>")
        .map_or(html.len(), |at| start + at);
    html[start..end].to_string()
}

#[tokio::test]
async fn hiding_and_showing_update_the_visibility_state_as_tasks() {
    // HTML #update-the-visibility-state: the state, a VisibilityStateEntry
    // and a bubbling visibilitychange event at the Document; the initial
    // entry is at time zero.
    let html = r#"<output id=o></output><script>
        const log = [], o = document.getElementById('o');
        const entries = () => performance.getEntriesByType('visibility-state')
            .map(e => e.name + '@' + (e.startTime === 0 ? 0 : 't') + ':' + e.duration).join('/');
        const record = what => { log.push(what); o.textContent = log.join(' '); };
        record(document.visibilityState + ':' + document.hidden + ':' + entries());
        window.addEventListener('visibilitychange', e =>
            record('change:' + document.visibilityState + ':' + document.hidden + ':' + e.bubbles
                + ':' + entries()));
        setInterval(() => {}, 1000);
    </script>"#;
    let (html, _) = run_page(
        html,
        TraversableState::SHOWN,
        vec![
            PageCmd::SystemVisibility(true),
            PageCmd::SystemVisibility(false),
        ],
        |html| output(html).matches("change:").count() == 2,
    )
    .await;
    assert_eq!(
        output(&html),
        "visible:false:visible@0:0 \
         change:hidden:true:true:visible@0:0/hidden@t:0 \
         change:visible:false:true:visible@0:0/hidden@t:0/visible@t:0"
    );
}

#[tokio::test]
async fn a_page_loaded_in_the_background_starts_hidden_without_an_event() {
    // HTML #set-the-initial-visibility-state: no visibilitychange.
    let html = r#"<output id=o></output><script>
        const o = document.getElementById('o');
        let changes = 0;
        document.addEventListener('visibilitychange', () => changes++);
        o.textContent = document.visibilityState + ':' + document.hidden + ':'
            + performance.getEntriesByType('visibility-state').map(e => e.name + '@' + e.startTime).join('/')
            + ':' + changes + ':' + document.hasFocus() + ':' + document.wasDiscarded;
    </script>"#;
    let (html, _) = run_page(
        html,
        TraversableState {
            hidden: true,
            focused: false,
            was_discarded: true,
            new_traversables: true,
            storage_shed: 0,
            audio_muted: false,
        },
        vec![],
        |html| !output(html).is_empty(),
    )
    .await;
    assert_eq!(output(&html), "hidden:true:hidden@0:0:false:true");
}

#[tokio::test]
async fn hidden_documents_get_no_animation_frames_but_keep_their_timers() {
    // HTML #update-the-rendering filters hidden Documents out; timers still
    // run (and run on time for the first ten seconds hidden).
    let html = r#"<output id=o></output><script>
        const o = document.getElementById('o');
        document.addEventListener('visibilitychange', () => {
            if (!document.hidden) return;
            requestAnimationFrame(() => {
                o.dataset.frame = document.visibilityState;
            });
            setTimeout(() => { document.title = 'timer:' + document.visibilityState; }, 50);
        });
        setInterval(() => {}, 1000);
    </script>"#;
    let mut env = PageEnv::bare(URL);
    env.terminal_presentation = false;
    let (handle, mut events) = crate::js::spawn_page(html.into(), env);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    match tokio::time::timeout_at(deadline, events.recv()).await {
        Ok(Some(_)) => {}
        _ => panic!("no presentation"),
    }
    handle
        .cmds
        .try_send(PageCmd::SystemVisibility(true))
        .unwrap();
    // The timer runs while the page is hidden; its title still reaches
    // the frontend, though nothing renders.
    let mut frame_while_hidden = false;
    loop {
        match tokio::time::timeout_at(deadline, events.recv()).await {
            Ok(Some(PageEvt::Title(title))) if title.starts_with("timer:") => {
                assert_eq!(title, "timer:hidden");
                break;
            }
            Ok(Some(PageEvt::Updated { html, .. })) => {
                frame_while_hidden |= html.contains("data-frame=");
            }
            Ok(Some(_)) => {}
            _ => panic!("the timer never ran while hidden"),
        }
    }
    assert!(!frame_while_hidden, "no animation frame while hidden");
    handle
        .cmds
        .try_send(PageCmd::SystemVisibility(false))
        .unwrap();
    loop {
        match tokio::time::timeout_at(deadline, events.recv()).await {
            Ok(Some(PageEvt::Updated { html, .. })) if html.contains("data-frame=") => {
                assert!(
                    html.contains("data-frame=\"visible\""),
                    "the frame waited until the page was shown: {html}"
                );
                break;
            }
            Ok(Some(_)) => {}
            _ => panic!("the frame never ran once shown"),
        }
    }
}

#[tokio::test]
async fn system_focus_runs_the_focus_update_steps_and_drives_focus_selectors() {
    // HTML #focus-update-steps for the traversable's chain, #has-focus-steps
    // and #selector-focus: the focused area survives, :focus does not.
    let html = r#"<input id=i autofocus><output id=o></output><script>
        const log = [], o = document.getElementById('o'), input = document.getElementById('i');
        const record = what => { log.push(what); o.textContent = log.join(' '); };
        input.focus();
        input.addEventListener('blur', () => record('input-blur'));
        input.addEventListener('focus', () => record('input-focus'));
        window.addEventListener('blur', () => record('window-blur:' + document.hasFocus()
            + ':' + (document.querySelector(':focus') === null)
            + ':' + (document.activeElement === input)));
        window.addEventListener('focus', () => record('window-focus:' + document.hasFocus()));
        setInterval(() => {}, 1000);
    </script>"#;
    let (html, _) = run_page(
        html,
        TraversableState::SHOWN,
        vec![PageCmd::SystemFocus(false), PageCmd::SystemFocus(true)],
        |html| output(html).contains("input-focus"),
    )
    .await;
    assert_eq!(
        output(&html),
        "input-blur window-blur:false:true:true window-focus:true input-focus"
    );
}

#[tokio::test]
async fn document_title_follows_the_title_element_algorithm() {
    // HTML #document.title: the first HTML title element (not an SVG one),
    // its child text only, stripped and collapsed; the setter creates the
    // element in the head, or in an SVG document element.
    let html = r#"<head></head><body><svg><title>Drawing</title></svg>
        <output id=o></output><script>
        const o = document.getElementById('o'), out = [];
        out.push(JSON.stringify(document.title));
        const title = document.createElement('title');
        title.append('  Hello\n\t world ', document.createElement('b'));
        title.lastChild.textContent = 'ignored';
        document.body.append(title);
        out.push(JSON.stringify(document.title));
        document.title = 'Set';
        out.push(JSON.stringify(document.title), document.querySelectorAll('title').length);
        const svg = new DOMParser().parseFromString(
            '<svg xmlns="http://www.w3.org/2000/svg"><g/><title> A  B </title></svg>', 'image/svg+xml');
        out.push(JSON.stringify(svg.title));
        svg.title = 'C';
        out.push(JSON.stringify(svg.title), svg.documentElement.firstChild.localName);
        const bare = document.implementation.createHTMLDocument('');
        bare.head.remove();
        bare.querySelector('title')?.remove();
        bare.title = 'nowhere';
        out.push(JSON.stringify(bare.title));
        o.textContent = out.join(' ');
    </script></body>"#;
    let (html, _) = run_page(html, TraversableState::SHOWN, vec![], |html| {
        !output(html).is_empty()
    })
    .await;
    assert_eq!(output(&html), r#""" "Hello world" "Set" 2 "A B" "C" g """#);
}

#[tokio::test]
async fn title_changes_reach_the_frontend_even_while_hidden() {
    let html = r#"<title>First</title><script>
        document.addEventListener('visibilitychange', () => {
            if (document.hidden) setTimeout(() => { document.title = 'Background news'; }, 30);
        });
        setInterval(() => {}, 1000);
    </script>"#;
    let mut env = PageEnv::bare(URL);
    env.terminal_presentation = false;
    let (handle, mut events) = crate::js::spawn_page(html.into(), env);
    let mut titles = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let mut hidden = false;
    while titles.last().map(String::as_str) != Some("Background news") {
        match tokio::time::timeout_at(deadline, events.recv()).await {
            Ok(Some(PageEvt::Title(title))) => {
                titles.push(title);
                if !hidden {
                    hidden = true;
                    handle
                        .cmds
                        .try_send(PageCmd::SystemVisibility(true))
                        .unwrap();
                }
            }
            Ok(Some(_)) => {}
            _ => panic!("titles so far: {titles:?}"),
        }
    }
    assert_eq!(titles, ["First", "Background news"]);
}

#[tokio::test]
async fn user_edits_are_reported_until_their_form_is_submitted() {
    let html = r#"<form action="https://tabs.example/sent"><input id=i name=q></form>
        <script>setInterval(() => {}, 1000);</script>"#;
    let mut env = PageEnv::bare(URL);
    env.terminal_presentation = false;
    let (handle, mut events) = crate::js::spawn_page(html.into(), env);
    let mut node = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while node.is_none() {
        match tokio::time::timeout_at(deadline, events.recv()).await {
            Ok(Some(PageEvt::Updated { outcome, .. })) => {
                node = outcome.rendered.as_ref().and_then(|rendered| {
                    rendered
                        .forms
                        .first()
                        .and_then(|form| form.fields.first())
                        .and_then(|field| field.live_node)
                });
            }
            Ok(Some(_)) => {}
            _ => panic!("no form"),
        }
    }
    let node = node.unwrap();
    handle
        .try_send_user(PageCmd::SetValue {
            node,
            value: "draft".into(),
            checked: None,
            commit: false,
        })
        .unwrap();
    let mut reports = Vec::new();
    while reports.last() != Some(&true) {
        match tokio::time::timeout_at(deadline, events.recv()).await {
            Ok(Some(PageEvt::UserEdits(edits))) => reports.push(edits),
            Ok(Some(_)) => {}
            _ => panic!("reports so far: {reports:?}"),
        }
    }
    assert_eq!(reports, [true]);
}

/// Run `html` with `state`, wait for its first presentation, send each of
/// `commands`, and return the events that followed until `done` holds for
/// one of them (or five seconds passed, when `done` is `None`).
async fn events_after(
    html: &str,
    state: TraversableState,
    commands: Vec<PageCmd>,
    done: Option<fn(&PageEvt) -> bool>,
) -> Vec<PageEvt> {
    let mut env = PageEnv::bare(URL);
    env.terminal_presentation = false;
    env.traversable = state;
    let (handle, mut events) = crate::js::spawn_page(html.into(), env);
    match tokio::time::timeout(Duration::from_secs(30), events.recv()).await {
        Ok(Some(_)) => {}
        _ => panic!("no presentation"),
    }
    for command in commands {
        if command.is_user_interaction() {
            handle.try_send_user(command).unwrap();
        } else {
            handle.cmds.try_send(command).unwrap();
        }
    }
    let mut seen = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        match tokio::time::timeout_at(deadline, events.recv()).await {
            Ok(Some(event)) => {
                let finished = done.is_some_and(|done| done(&event));
                seen.push(event);
                if finished {
                    break;
                }
            }
            Ok(None) => break,
            Err(_) if done.is_none() => break,
            Err(_) => panic!("the page never answered: {seen:?}"),
        }
    }
    drop(handle);
    seen
}

fn opened(events: &[PageEvt]) -> Vec<&crate::js::NewTraversable> {
    events
        .iter()
        .filter_map(|event| match event {
            PageEvt::OpenTraversable(request) => Some(&**request),
            _ => None,
        })
        .collect()
}

fn navigated(events: &[PageEvt]) -> Vec<&str> {
    events
        .iter()
        .filter_map(|event| match event {
            PageEvt::Navigate(url, _) => Some(url.as_str()),
            _ => None,
        })
        .collect()
}

const TABS: TraversableState = TraversableState {
    new_traversables: true,
    ..TraversableState::SHOWN
};

const LINKS: &str = r#"<a id=blank href="/blank" target=_blank>blank</a>
    <a id=named href="named" target=popup>named</a>
    <a id=opener href="/opener" target=_BLANK rel=opener>opener</a>
    <a id=plain href="/plain">plain</a>
    <script>setInterval(() => {}, 1000);</script>"#;

fn node(html: &str, id: &str) -> usize {
    crate::dom::Dom::parse_document(html).get_by_id(id).unwrap()
}

#[tokio::test]
async fn activated_hyperlinks_with_new_targets_request_new_traversables() {
    // HTML #following-hyperlinks-2 and #the-rules-for-choosing-a-navigable:
    // `_blank` and a name no navigable has request a new top-level
    // traversable; #get-an-element's-noopener makes `_blank` noopener unless
    // `rel=opener`, and a non-keyword name becomes its target name. Without
    // noopener the new traversable is an auxiliary browsing context of the
    // page's group (#creating-a-new-auxiliary-browsing-context), which the
    // hyperlink navigates, replacing its initial about:blank Document.
    let commands = ["blank", "named", "opener"]
        .into_iter()
        .map(|id| PageCmd::Click(node(LINKS, id)))
        .collect();
    let (_page, events, auxiliaries) = run_group(LINKS, commands, |_, auxiliaries| {
        auxiliaries.len() == 2
            && auxiliaries
                .iter()
                .all(|aux| !replaced(&aux.events).is_empty())
    })
    .await;
    let requests: Vec<_> = opened(&events)
        .into_iter()
        .map(|request| {
            (
                request.url.as_str(),
                request.name.as_str(),
                request.noopener,
                request.foreground,
                request.submission.is_some(),
                request
                    .initiator
                    .as_ref()
                    .map(|initiator| initiator.client.as_str()),
            )
        })
        .collect();
    assert_eq!(
        requests,
        [(
            "https://tabs.example/blank",
            "",
            true,
            true,
            false,
            Some(URL)
        )]
    );
    let auxiliaries: Vec<_> = auxiliaries
        .iter()
        .map(|aux| (aux.name.as_str(), aux.popup, replaced(&aux.events)))
        .collect();
    assert_eq!(
        auxiliaries,
        [
            ("popup", false, vec!["https://tabs.example/named"]),
            ("", false, vec!["https://tabs.example/opener"]),
        ]
    );
    assert!(navigated(&events).is_empty(), "{events:?}");
}

/// An auxiliary browsing context as its frontend sees it: its handle (kept
/// alive), name, popup state and the events its page sent.
struct Opened {
    _handle: crate::js::PageHandle,
    name: String,
    popup: bool,
    storage_shed: u64,
    events: Vec<PageEvt>,
    receiver: tokio::sync::mpsc::Receiver<PageEvt>,
}

/// Run `html` in a frontend with tabs, send `commands`, and collect the
/// page's events and those of each auxiliary browsing context it opens
/// (`PageEvt::OpenAuxiliary`) until `done` holds. Returns the page's handle
/// with them, so that the group stays alive.
async fn run_group(
    html: &str,
    commands: Vec<PageCmd>,
    done: impl Fn(&[PageEvt], &[Opened]) -> bool,
) -> (crate::js::PageHandle, Vec<PageEvt>, Vec<Opened>) {
    let mut env = PageEnv::bare(URL);
    env.terminal_presentation = false;
    env.traversable = TABS;
    let (handle, mut events) = crate::js::spawn_page(html.into(), env);
    let mut seen = Vec::new();
    match tokio::time::timeout(Duration::from_secs(30), events.recv()).await {
        Ok(Some(event)) => seen.push(event),
        _ => panic!("no presentation"),
    }
    for command in commands {
        if command.is_user_interaction() {
            handle.try_send_user(command).unwrap();
        } else {
            handle.cmds.try_send(command).unwrap();
        }
    }
    let mut opened: Vec<Opened> = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let mut progress = false;
        while let Ok(event) = events.try_recv() {
            progress = true;
            match event {
                PageEvt::OpenAuxiliary(auxiliary) => {
                    let crate::js::AuxiliaryTraversable {
                        handle,
                        events,
                        name,
                        popup,
                        storage_shed,
                    } = *auxiliary;
                    opened.push(Opened {
                        _handle: handle,
                        name,
                        popup,
                        storage_shed,
                        events: Vec::new(),
                        receiver: events,
                    });
                }
                event => seen.push(event),
            }
        }
        for auxiliary in &mut opened {
            while let Ok(event) = auxiliary.receiver.try_recv() {
                progress = true;
                auxiliary.events.push(event);
            }
        }
        if done(&seen, &opened) {
            break;
        }
        if tokio::time::Instant::now() > deadline {
            let auxiliaries: Vec<_> = opened.iter().map(|aux| &aux.events).collect();
            panic!("the group never got there: {seen:?}\n{auxiliaries:?}");
        }
        if !progress {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    (handle, seen, opened)
}

/// The navigations a page asked its frontend for with replacement.
fn replaced(events: &[PageEvt]) -> Vec<&str> {
    events
        .iter()
        .filter_map(|event| match event {
            PageEvt::Replace(url, _) => Some(url.as_str()),
            _ => None,
        })
        .collect()
}

/// The last serialization a page presented.
fn presented(events: &[PageEvt]) -> &str {
    events
        .iter()
        .rev()
        .find_map(|event| match event {
            PageEvt::Updated { html, .. } | PageEvt::Static { html, .. } if !html.is_empty() => {
                Some(html.as_str())
            }
            _ => None,
        })
        .unwrap_or_default()
}

const OPENER: &str = r#"<button id=b>open</button><output id=o></output><script>
    const o = document.getElementById('o');
    window.addEventListener('message', e => {
        o.textContent = 'message: ' + e.data + ' ' + (e.source === popup) + ' ' + e.origin;
    });
    let popup = null;
    document.getElementById('b').onclick = () => {
        popup = window.open('', 'aux');
        popup.document.write('<title>Popup</title><p id=p>written</p>');
        popup.document.close();
        const again = window.open('', 'aux');
        o.textContent = [popup === again, popup.document.getElementById('p').textContent,
            popup.opener === window, popup.name, popup.closed, popup.location.href,
            popup.document.referrer === location.href, popup.origin === origin,
            popup.self === popup, popup.top === popup, popup.parent === popup,
            popup.locationbar.visible].join(' ');
    };
    setInterval(() => {}, 1000);
</script>"#;

#[tokio::test]
async fn window_open_creates_an_auxiliary_browsing_context_its_opener_reaches() {
    // HTML #window-open-steps: `window.open('', name)` creates an auxiliary
    // browsing context (#creating-a-new-auxiliary-browsing-context) whose
    // initial about:blank Document has its creator's origin and URL as
    // referrer; the opener reaches its Document synchronously, and a second
    // call with its name finds it (#find-a-navigable-by-target-name) and
    // returns the same WindowProxy. Its opener is the creator's WindowProxy
    // (#dom-opener) and its target name the name (#dom-name).
    let (_page, events, auxiliaries) = run_group(
        OPENER,
        vec![PageCmd::Click(node(OPENER, "b"))],
        |events, auxiliaries| {
            !output(presented(events)).is_empty()
                && auxiliaries.first().is_some_and(|aux| {
                    presented(&aux.events).contains("written")
                        && aux
                            .events
                            .iter()
                            .any(|event| matches!(event, PageEvt::Title(_)))
                })
        },
    )
    .await;
    assert_eq!(
        output(presented(&events)),
        "true written true aux false about:blank true true true true true true"
    );
    let [auxiliary] = auxiliaries.as_slice() else {
        panic!("{auxiliaries:?}", auxiliaries = auxiliaries.len());
    };
    assert_eq!((auxiliary.name.as_str(), auxiliary.popup), ("aux", false));
    assert!(
        presented(&auxiliary.events).contains("<p id=\"p\">written</p>"),
        "{:?}",
        auxiliary.events
    );
    assert!(
        auxiliary
            .events
            .iter()
            .any(|event| matches!(event, PageEvt::Title(title) if title == "Popup")),
        "{:?}",
        auxiliary.events
    );
    assert_ne!(auxiliary.storage_shed, 0);
}

#[tokio::test]
async fn an_auxiliary_browsing_context_messages_and_closes_through_its_window_proxies() {
    // HTML #dom-window-postmessage between the group's Windows: the popup's
    // script posts to its opener, whose MessageEvent's source is the popup's
    // WindowProxy. HTML #dom-window-close: a traversable created by web
    // content is script-closable; `closed` is true at once ("is closing"),
    // and its frontend is asked to close it
    // (#definitely-close-a-top-level-traversable).
    let html = r#"<button id=b>open</button><button id=c>close</button><output id=o></output><script>
        const o = document.getElementById('o');
        let popup = null;
        window.addEventListener('message', e => {
            o.textContent = 'message: ' + e.data + ' ' + (e.source === popup) + ' ' + e.origin;
        });
        document.getElementById('b').onclick = () => {
            popup = window.open('', '_blank', 'popup');
            // A timer whose handler is a string runs in the popup's Realm
            // (HTML #timer-initialisation-steps).
            popup.setTimeout('opener.postMessage("hello " + opener.document.title, "*")', 0);
        };
        document.getElementById('c').onclick = () => {
            popup.close();
            o.textContent = 'closed: ' + popup.closed;
        };
        document.title = 'Opener';
        setInterval(() => {}, 1000);
    </script>"#;
    let (page, events, mut auxiliaries) = run_group(
        html,
        vec![PageCmd::Click(node(html, "b"))],
        |events, auxiliaries| {
            output(presented(events)).starts_with("message") && !auxiliaries.is_empty()
        },
    )
    .await;
    assert_eq!(
        output(presented(&events)),
        "message: hello Opener true https://tabs.example"
    );
    assert!(auxiliaries[0].popup, "the features asked for a popup");
    page.try_send_user(PageCmd::Click(node(html, "c"))).unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let auxiliary = &mut auxiliaries[0];
    while !auxiliary
        .events
        .iter()
        .any(|event| matches!(event, PageEvt::CloseTraversable))
    {
        match tokio::time::timeout_at(deadline, auxiliary.receiver.recv()).await {
            Ok(Some(event)) => auxiliary.events.push(event),
            _ => panic!("the popup was never closed: {:?}", auxiliary.events),
        }
    }
}

#[tokio::test]
async fn window_open_navigates_a_new_auxiliary_browsing_context_and_returns_null_for_noopener() {
    // HTML #window-open-steps: a URL navigates the new traversable from its
    // initial about:blank Document (replacing it), with the opener's
    // Document as the source; `noopener` (or `noreferrer`) opens one in a
    // new browsing context group instead, and returns null.
    let html = r#"<button id=b>open</button><button id=n>alone</button><output id=o></output><script>
        const o = document.getElementById('o');
        document.getElementById('b').onclick = () => {
            o.textContent += typeof window.open('/next?q=1', 'next');
        };
        // The first window.open consumed the user activation (HTML
        // #the-rules-for-choosing-a-navigable); this is another click.
        document.getElementById('n').onclick = () => {
            o.textContent += ' ' + String(window.open('/alone', '_blank', 'noopener'));
        };
        setInterval(() => {}, 1000);
    </script>"#;
    let (_page, events, auxiliaries) = run_group(
        html,
        vec![
            PageCmd::Click(node(html, "b")),
            PageCmd::Click(node(html, "n")),
        ],
        |events, auxiliaries| {
            output(presented(events)).ends_with("null")
                && auxiliaries
                    .first()
                    .is_some_and(|aux| !replaced(&aux.events).is_empty())
        },
    )
    .await;
    assert_eq!(output(presented(&events)), "object null");
    assert_eq!(
        replaced(&auxiliaries[0].events),
        ["https://tabs.example/next?q=1"]
    );
    let requests = opened(&events);
    assert_eq!(
        requests
            .iter()
            .map(|request| (request.url.as_str(), request.noopener))
            .collect::<Vec<_>>(),
        [("https://tabs.example/alone", true)]
    );
}

#[tokio::test]
async fn an_auxiliary_browsing_context_starts_with_a_copy_of_its_openers_session_storage() {
    // HTML #copy-session-storage: Storage's legacy-clone of the opener's
    // traversable storage shed; afterwards each traversable has its own.
    let html = r#"<button id=b>open</button><output id=o></output><script>
        sessionStorage.setItem('k', 'opener');
        document.getElementById('b').onclick = () => {
            const w = window.open('', 'copy');
            const copied = w.sessionStorage.getItem('k');
            w.sessionStorage.setItem('k', 'popup');
            document.getElementById('o').textContent = [copied, sessionStorage.getItem('k'),
                w.sessionStorage.getItem('k')].join(' ');
        };
        setInterval(() => {}, 1000);
    </script>"#;
    let (_page, events, _auxiliaries) =
        run_group(html, vec![PageCmd::Click(node(html, "b"))], |events, _| {
            !output(presented(events)).is_empty()
        })
        .await;
    assert_eq!(output(presented(&events)), "opener opener popup");
}

#[tokio::test]
async fn an_auxiliary_browsing_contexts_next_document_joins_its_group() {
    // HTML #navigate keeps a navigable in its browsing context group: the
    // auxiliary browsing context's next Document is created in the opener's
    // engine (`spawn_page_in`), where the opener's WindowProxy follows it
    // and its opener and target name stay. Once its frontend lets it go, it
    // is destroyed (#destroy-a-top-level-traversable) and its WindowProxy
    // reports `closed`.
    let html = r#"<button id=b>open</button><button id=c>check</button><output id=o></output><script>
        let popup = null, checks = 0;
        document.getElementById('b').onclick = () => { popup = window.open('', 'next'); };
        document.getElementById('c').onclick = () => {
            document.getElementById('o').textContent = ++checks + ': ' + (popup.closed ? 'closed' : [
                popup.document.title, popup.location.href, popup.opener === window, popup.name].join(' '));
        };
        setInterval(() => {}, 1000);
    </script>"#;
    let mut env = PageEnv::bare(URL);
    env.terminal_presentation = false;
    env.traversable = TABS;
    let (page, mut events) = crate::js::spawn_page(html.into(), env);
    let _ = tokio::time::timeout(Duration::from_secs(30), events.recv()).await;
    page.try_send_user(PageCmd::Click(node(html, "b"))).unwrap();
    let auxiliary = loop {
        match tokio::time::timeout(Duration::from_secs(30), events.recv()).await {
            Ok(Some(PageEvt::OpenAuxiliary(auxiliary))) => break *auxiliary,
            Ok(Some(_)) => {}
            other => panic!("no auxiliary browsing context: {other:?}"),
        }
    };
    let link = auxiliary.handle.link().clone();
    assert!(link.shared(), "the group has two traversables");
    let mut env = PageEnv::bare("https://tabs.example/next");
    env.terminal_presentation = false;
    env.traversable = TABS;
    env.cache = link.cache();
    let next = r#"<title>Next</title><script>
        document.title += ' ' + (opener !== null) + ' ' + window.name;
        setInterval(() => {}, 1000);
    </script>"#;
    let (handle, mut next_events) = crate::js::spawn_page_in(&link, next.into(), env);
    match tokio::time::timeout(Duration::from_secs(30), next_events.recv()).await {
        Ok(Some(PageEvt::Updated { html, .. })) => assert!(html.contains("Next"), "{html}"),
        other => panic!("the next Document never rendered: {other:?}"),
    }
    drop(auxiliary);

    // Click "check" and wait for its `n`th answer.
    let check = async |events: &mut tokio::sync::mpsc::Receiver<PageEvt>, n: usize| {
        page.try_send_user(PageCmd::Click(node(html, "c"))).unwrap();
        let prefix = format!("{n}: ");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        let mut seen = Vec::new();
        loop {
            match tokio::time::timeout_at(deadline, events.recv()).await {
                Ok(Some(PageEvt::Updated { html, .. })) if output(&html).starts_with(&prefix) => {
                    return output(&html)[prefix.len()..].to_string();
                }
                Ok(Some(event)) => seen.push(event),
                other => panic!("no answer: {other:?} after {seen:?}"),
            }
        }
    };
    assert_eq!(
        check(&mut events, 1).await,
        "Next true next https://tabs.example/next true next"
    );

    // Its frontend lets it go: the traversable is destroyed.
    drop(handle);
    drop(next_events);
    drop(link);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(check(&mut events, 2).await, "closed");
}

#[tokio::test]
async fn an_openers_next_document_stays_its_auxiliary_browsing_contexts_opener() {
    // HTML #dom-opener: the opener browsing context, not its Document. The
    // opener's next Document is created in the group's engine; the popup's
    // `opener` reaches it there. Once the opener's traversable is destroyed,
    // the popup's opener is null.
    let html = r#"<title>First</title><button id=b>open</button><script>
        document.getElementById('b').onclick = () => {
            const popup = window.open('', 'kept');
            popup.document.write('<output id=o></output>');
            popup.document.title = 'Popup';
            // The popup's own Realm reports what its opener is now.
            popup.setInterval(`document.getElementById('o').textContent =
                opener === null ? 'none' : opener.document.title`, 30);
        };
        setInterval(() => {}, 1000);
    </script>"#;
    let mut env = PageEnv::bare(URL);
    env.terminal_presentation = false;
    env.traversable = TABS;
    let (page, mut events) = crate::js::spawn_page(html.into(), env);
    let _ = tokio::time::timeout(Duration::from_secs(30), events.recv()).await;
    page.try_send_user(PageCmd::Click(node(html, "b"))).unwrap();
    let auxiliary = loop {
        match tokio::time::timeout(Duration::from_secs(30), events.recv()).await {
            Ok(Some(PageEvt::OpenAuxiliary(auxiliary))) => break *auxiliary,
            Ok(Some(_)) => {}
            other => panic!("no auxiliary browsing context: {other:?}"),
        }
    };
    let mut popup_events = auxiliary.events;
    let _popup = auxiliary.handle;
    // Wait for the popup to report `expected`.
    async fn reported(events: &mut tokio::sync::mpsc::Receiver<PageEvt>, expected: &str) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            match tokio::time::timeout_at(deadline, events.recv()).await {
                Ok(Some(PageEvt::Updated { html, .. })) if output(&html) == expected => return,
                Ok(Some(_)) => {}
                other => panic!("the popup never reported {expected}: {other:?}"),
            }
        }
    }
    reported(&mut popup_events, "First").await;

    // The opener navigates: its next Document joins the group.
    let link = page.link().clone();
    assert!(link.shared());
    let mut env = PageEnv::bare("https://tabs.example/second");
    env.terminal_presentation = false;
    env.traversable = TABS;
    env.cache = link.cache();
    let second = "<title>Second</title><script>setInterval(() => {}, 1000);</script>";
    let (second_page, mut second_events) = crate::js::spawn_page_in(&link, second.into(), env);
    match tokio::time::timeout(Duration::from_secs(30), second_events.recv()).await {
        Ok(Some(PageEvt::Updated { .. })) => {}
        other => panic!("the opener's next Document never rendered: {other:?}"),
    }
    drop(page);
    drop(events);
    reported(&mut popup_events, "Second").await;

    // The opener's traversable goes: the popup has no opener any more.
    drop(second_page);
    drop(second_events);
    drop(link);
    reported(&mut popup_events, "none").await;
}

#[tokio::test]
async fn an_auxiliary_browsing_contexts_child_navigables_live_in_its_arena() {
    // HTML #the-iframe-element in an auxiliary browsing context's Document:
    // the child navigable's Document and Window belong to that traversable
    // (its arena and Realms), which the opener reaches through it.
    let html = r#"<button id=b>open</button><button id=c>check</button><output id=o></output><script>
        let popup = null;
        document.getElementById('b').onclick = () => {
            popup = window.open('', 'framed');
            popup.document.write('<iframe srcdoc="<p id=inner>frame text</p>"></iframe>');
        };
        document.getElementById('c').onclick = () => {
            const frame = popup.frames[0];
            const inner = frame && frame.document.getElementById('inner');
            // The frame's parent is the popup's Window as the frame's Realm
            // holds it; this Realm's WindowProxy of the popup is another
            // object for the same Window (one per Realm, which follows the
            // popup's navigations).
            document.getElementById('o').textContent = inner
                ? [inner.textContent, frame.parent.document === popup.document,
                    frame.top.document === popup.document].join(' ') : 'none';
        };
        setInterval(() => {}, 1000);
    </script>"#;
    let mut env = PageEnv::bare(URL);
    env.terminal_presentation = false;
    env.traversable = TABS;
    let (page, mut events) = crate::js::spawn_page(html.into(), env);
    let _ = tokio::time::timeout(Duration::from_secs(30), events.recv()).await;
    page.try_send_user(PageCmd::Click(node(html, "b"))).unwrap();
    let auxiliary = loop {
        match tokio::time::timeout(Duration::from_secs(30), events.recv()).await {
            Ok(Some(PageEvt::OpenAuxiliary(auxiliary))) => break *auxiliary,
            Ok(Some(_)) => {}
            other => panic!("no auxiliary browsing context: {other:?}"),
        }
    };
    let _popup = auxiliary.handle;
    let mut popup_events = auxiliary.events;
    // The popup presents its frame's Document.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        match tokio::time::timeout_at(deadline, popup_events.recv()).await {
            Ok(Some(PageEvt::Updated { html, .. })) if html.contains("frame text") => break,
            Ok(Some(_)) => {}
            other => panic!("the popup never presented its frame: {other:?}"),
        }
    }
    page.try_send_user(PageCmd::Click(node(html, "c"))).unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let answer = loop {
        match tokio::time::timeout_at(deadline, events.recv()).await {
            Ok(Some(PageEvt::Updated { html, .. })) if !output(&html).is_empty() => {
                break output(&html);
            }
            Ok(Some(_)) => {}
            other => panic!("no answer: {other:?}"),
        }
    };
    assert_eq!(answer, "frame text true true");
}

#[tokio::test]
async fn nodes_move_between_the_documents_of_a_group() {
    // DOM #concept-node-adopt across the group's traversables: a node the
    // opener created (with its subtree, listeners and form state) is
    // inserted into its popup's Document, as libraries that render into
    // popups do (copying the opener's style elements, moving content);
    // importNode() copies into the popup's Document.
    let html = r#"<style id=s>p.moved { color: rgb(1, 2, 3) }</style><button id=b>open</button>
        <output id=o></output><script>
        document.getElementById('b').onclick = () => {
            const popup = window.open('', 'moved');
            const style = document.getElementById('s').cloneNode(true);
            popup.document.head.appendChild(style);
            const p = document.createElement('p');
            p.className = 'moved';
            p.innerHTML = 'moved <b>text</b>';
            let clicks = 0;
            p.addEventListener('click', () => clicks++);
            const input = document.createElement('input');
            input.value = 'typed';
            p.appendChild(input);
            popup.document.body.appendChild(p);
            const imported = popup.document.importNode(document.getElementById('o'), false);
            p.click();
            document.getElementById('o').textContent = [p.ownerDocument === popup.document,
                p.parentNode === popup.document.body, p.firstChild.data, p.querySelector('b').textContent,
                input.value, clicks, popup.getComputedStyle(p).color,
                imported.ownerDocument === popup.document, document.contains(p)].join(' ');
        };
        setInterval(() => {}, 1000);
    </script>"#;
    let (_page, events, auxiliaries) = run_group(
        html,
        vec![PageCmd::Click(node(html, "b"))],
        |events, auxiliaries| {
            !output(presented(events)).is_empty()
                && auxiliaries
                    .first()
                    .is_some_and(|aux| presented(&aux.events).contains("moved"))
        },
    )
    .await;
    assert_eq!(
        output(presented(&events)),
        "true true moved  text typed 1 rgb(1, 2, 3) true false"
    );
    let popup = presented(&auxiliaries[0].events);
    assert!(popup.contains("<b>text</b>"), "{popup}");
}

#[tokio::test]
async fn insertions_into_another_traversables_document_adopt_only_when_valid() {
    // DOM #concept-node-pre-insert across the group's arenas: validity is
    // checked before the node is adopted, so a refused insertion leaves it
    // in its Document; a DocumentFragment's children move while it stays;
    // adoptNode() returns a template's contents without adopting them; and
    // parsed nodes, style elements with recorded style block updates and
    // their copies move with nothing left behind in the opener's arena.
    let html = r#"<p id=old>parsed</p><button id=b>open</button><output id=o></output><script>
        document.getElementById('b').onclick = () => {
            const popup = window.open('', 'moved');
            const out = [];
            const div = document.createElement('div');
            try { popup.document.appendChild(div); out.push('appended'); }
            catch (e) { out.push(e.name); }
            out.push(div.ownerDocument === document);
            try { popup.document.body.insertBefore(div, document.body.firstChild); out.push('inserted'); }
            catch (e) { out.push(e.name); }
            out.push(div.ownerDocument === document);
            const fragment = document.createDocumentFragment();
            fragment.append(document.createElement('i'), 'text');
            popup.document.body.appendChild(fragment);
            out.push(fragment.ownerDocument === document, fragment.childNodes.length,
                popup.document.body.lastChild.data);
            const template = document.createElement('template');
            template.innerHTML = '<b>t</b>';
            const content = template.content;
            out.push(popup.document.adoptNode(content) === content, template.content === content,
                content.ownerDocument !== popup.document, template.content.firstChild.nodeName);
            const style = document.createElement('style');
            style.textContent = '#old { color: rgb(4, 5, 6) }';
            document.head.appendChild(style);
            popup.document.head.appendChild(style.cloneNode(true));
            popup.document.head.appendChild(style);
            const old = document.getElementById('old');
            popup.document.body.appendChild(old);
            out.push(old.ownerDocument === popup.document, document.getElementById('old'),
                popup.document.getElementById('old') === old, popup.getComputedStyle(old).color,
                document.styleSheets.length, popup.document.styleSheets.length);
            document.getElementById('o').textContent = out.join(' ');
        };
        setInterval(() => {}, 1000);
    </script>"#;
    let (_page, events, auxiliaries) = run_group(
        html,
        vec![PageCmd::Click(node(html, "b"))],
        |events, auxiliaries| {
            !output(presented(events)).is_empty()
                && auxiliaries
                    .first()
                    .is_some_and(|aux| presented(&aux.events).contains("parsed"))
        },
    )
    .await;
    assert_eq!(
        output(presented(&events)),
        "HierarchyRequestError true NotFoundError true true 0 text true true true B true  true rgb(4, 5, 6) 0 2"
    );
    let popup = presented(&auxiliaries[0].events);
    assert!(
        popup.contains("parsed") && popup.contains("text"),
        "{popup}"
    );
}

#[tokio::test]
async fn without_new_traversables_a_new_target_chooses_the_current_navigable() {
    // HTML #the-rules-for-choosing-a-navigable: "If the user agent has been
    // configured such that in this instance it will choose currentNavigable".
    let events = events_after(
        LINKS,
        TraversableState::SHOWN,
        vec![PageCmd::Click(node(LINKS, "blank"))],
        Some(|event| matches!(event, PageEvt::Navigate(..))),
    )
    .await;
    assert_eq!(navigated(&events), ["https://tabs.example/blank"]);
    assert!(opened(&events).is_empty());
}

#[tokio::test]
async fn the_popup_blocker_needs_transient_activation() {
    // HTML #popup-blocker: without transient activation no navigable is
    // chosen, whatever the user agent's configuration.
    let html = r#"<a id=blank href="/blank" target=_blank>blank</a>
        <form id=f action="/sent" target=_blank><input name=q value=v></form>
        <output id=o></output><script>
        setTimeout(() => {
            document.getElementById('blank').click();
            document.getElementById('f').submit();
            document.getElementById('o').textContent = 'done';
        }, 50);
        setInterval(() => {}, 1000);
    </script>"#;
    for state in [TABS, TraversableState::SHOWN] {
        let events = events_after(
            html,
            state,
            vec![],
            Some(|event| {
                matches!(event, PageEvt::Updated { html, .. } if html.contains("<output id=\"o\">done"))
            }),
        )
        .await;
        assert!(opened(&events).is_empty(), "{events:?}");
        assert!(navigated(&events).is_empty(), "{events:?}");
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, PageEvt::SubmitForm { .. })),
            "{events:?}"
        );
    }
}

#[tokio::test]
async fn form_submissions_to_new_targets_carry_their_entry_list() {
    // HTML #form-submission-algorithm: the rules for choosing a navigable
    // run for the submitter's formtarget, else the form's target; a new
    // top-level traversable navigates to the submission.
    let html = r#"<form action="/sent" method=post target=results>
        <input name=q value="tabs"><button id=go>go</button>
        <button id=self formtarget=_self formaction="/here">here</button></form>
        <script>setInterval(() => {}, 1000);</script>"#;
    // A name no navigable has: an auxiliary browsing context of the group,
    // whose frontend runs the submission.
    let (_page, events, auxiliaries) = run_group(
        html,
        vec![PageCmd::Click(node(html, "go"))],
        |_, auxiliaries| {
            auxiliaries.first().is_some_and(|aux| {
                aux.events
                    .iter()
                    .any(|event| matches!(event, PageEvt::SubmitForm { .. }))
            })
        },
    )
    .await;
    assert!(opened(&events).is_empty(), "{events:?}");
    let auxiliary = &auxiliaries[0];
    assert_eq!(auxiliary.name, "results");
    let submission = auxiliary
        .events
        .iter()
        .find_map(|event| match event {
            PageEvt::SubmitForm { submission, .. } => submission.as_ref(),
            _ => None,
        })
        .expect("the form's submission");
    assert_eq!(submission.method.to_ascii_lowercase(), "post");
    assert_eq!(submission.body, b"q=tabs");

    let events = events_after(
        html,
        TABS,
        vec![PageCmd::Click(node(html, "self"))],
        Some(|event| matches!(event, PageEvt::SubmitForm { .. })),
    )
    .await;
    assert!(opened(&events).is_empty(), "{events:?}");
}

/// A native button press or release at (`x`, `y`), which the actor hit
/// tests itself.
fn pointer(
    (node, x, y): (usize, f64, f64),
    pressed: bool,
    button: i16,
    modifiers: crate::core::Modifiers,
) -> PageCmd {
    PageCmd::PointerButton {
        node: Some(node),
        pressed,
        x,
        y,
        button,
        metadata: crate::js::PointerMetadata {
            modifiers,
            ..Default::default()
        },
    }
}

#[tokio::test]
async fn the_users_new_tab_preference_opens_any_hyperlink_without_an_opener() {
    // The user's preference to follow a hyperlink in a new top-level
    // traversable: Ctrl+click in the background, Shift in front, and the
    // middle button's auxclick default; the page's own target is not used.
    let html = r#"<style>a { position: fixed; left: 0; width: 60px; height: 10px; }</style>
        <a id=plain href="/plain" style="top: 0">plain</a>
        <a id=named href="/named" target=popup style="top: 12px">named</a>
        <script>setInterval(() => {}, 1000);</script>"#;
    let plain = (node(html, "plain"), 4.0, 4.0);
    let named = (node(html, "named"), 4.0, 16.0);
    let ctrl = crate::core::Modifiers {
        control: true,
        ..Default::default()
    };
    let shift = crate::core::Modifiers {
        shift: true,
        ..Default::default()
    };
    let none = crate::core::Modifiers::default();
    let events = events_after(
        html,
        TABS,
        vec![
            pointer(plain, true, 0, ctrl),
            pointer(plain, false, 0, ctrl),
            pointer(named, true, 0, shift),
            pointer(named, false, 0, shift),
            pointer(named, true, 1, none),
            pointer(named, false, 1, none),
        ],
        Some(|event| {
            matches!(event, PageEvt::OpenTraversable(request) if request.url.ends_with("/named")
                && !request.foreground)
        }),
    )
    .await;
    let requests: Vec<_> = opened(&events)
        .into_iter()
        .map(|request| {
            (
                request.url.as_str(),
                request.name.as_str(),
                request.noopener,
                request.foreground,
            )
        })
        .collect();
    assert_eq!(
        requests,
        [
            ("https://tabs.example/plain", "", true, false),
            ("https://tabs.example/named", "", true, true),
            ("https://tabs.example/named", "", true, false),
        ],
        "{events:?}"
    );
    assert!(navigated(&events).is_empty(), "{events:?}");

    // Without new traversables the preference has nothing to open: the
    // hyperlink follows its own target.
    let events = events_after(
        html,
        TraversableState::SHOWN,
        vec![
            pointer(plain, true, 0, ctrl),
            pointer(plain, false, 0, ctrl),
        ],
        Some(|event| matches!(event, PageEvt::Navigate(..))),
    )
    .await;
    assert_eq!(navigated(&events), ["https://tabs.example/plain"]);
}

#[tokio::test]
async fn the_page_shows_after_its_load_event() {
    // HTML #the-end: load, then page showing and pageshow (persisted false)
    // at the Window with the Document as its target.
    let html = r#"<output id=o></output><script>
        const o = document.getElementById('o'), log = [];
        addEventListener('load', () => log.push('load'));
        addEventListener('pageshow', e => {
            log.push('pageshow:' + e.persisted + ':' + (e.target === document) + ':' + e.bubbles
                + ':' + (e instanceof PageTransitionEvent) + ':' + document.readyState);
            o.textContent = log.join(' ');
        });
        setInterval(() => {}, 1000);
    </script>"#;
    let (html, _) = run_page(html, TraversableState::SHOWN, vec![], |html| {
        !output(html).is_empty()
    })
    .await;
    assert_eq!(output(&html), "load pageshow:false:true:true:true:complete");
}

/// Run `html` and send each command, answering every unload prompt with
/// the next of `answers`; return the events until `done`.
async fn unload_checks(
    html: &str,
    commands: Vec<PageCmd>,
    mut answers: Vec<bool>,
    done: impl Fn(&[PageEvt]) -> bool,
) -> Vec<PageEvt> {
    let mut env = PageEnv::bare(URL);
    env.terminal_presentation = false;
    let (handle, mut events) = crate::js::spawn_page(html.into(), env);
    let mut seen = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    match tokio::time::timeout_at(deadline, events.recv()).await {
        Ok(Some(event)) => seen.push(event),
        _ => panic!("no presentation"),
    }
    answers.reverse();
    for command in commands {
        let user = command.is_user_interaction();
        let check = matches!(command, PageCmd::CheckUnload { .. });
        if user {
            handle.try_send_user(command).unwrap();
        } else {
            handle.cmds.try_send(command).unwrap();
        }
        // Wait for each check's answer (answering its prompt) before the next
        // command; give other commands a moment.
        let pause = tokio::time::Instant::now() + Duration::from_millis(150);
        loop {
            let event = if check {
                tokio::time::timeout_at(deadline, events.recv()).await
            } else {
                match tokio::time::timeout_at(pause, events.recv()).await {
                    Err(_) => break,
                    ok => ok,
                }
            };
            match event {
                Ok(Some(PageEvt::UnloadPrompt(prompt))) => {
                    seen.push(PageEvt::Settled);
                    prompt.answer(answers.pop().expect("an answer for each prompt"));
                }
                Ok(Some(event)) => {
                    let answered = matches!(event, PageEvt::UnloadChecked { .. });
                    seen.push(event);
                    if check && answered {
                        break;
                    }
                }
                _ => panic!("the page stopped answering: {seen:?}"),
            }
        }
    }
    while !done(&seen) {
        match tokio::time::timeout_at(deadline, events.recv()).await {
            Ok(Some(PageEvt::UnloadPrompt(prompt))) => {
                prompt.answer(answers.pop().expect("an answer for each prompt"))
            }
            Ok(Some(event)) => seen.push(event),
            _ => panic!("the page never reached the expected state: {seen:?}"),
        }
    }
    drop(handle);
    seen
}

fn checked(events: &[PageEvt]) -> Vec<(u64, bool)> {
    events
        .iter()
        .filter_map(|event| match event {
            PageEvt::UnloadChecked { request, proceed } => Some((*request, *proceed)),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn beforeunload_asks_the_user_only_after_sticky_activation() {
    // HTML #steps-to-fire-beforeunload: a canceled event (or a non-empty
    // returnValue, or a handler's return value) asks the user to confirm,
    // but only when the Window has sticky activation; the answer decides.
    let html = r#"<button id=b>activate</button><output id=o></output><script>
        let fired = 0;
        addEventListener('beforeunload', e => {
            fired++;
            document.getElementById('o').textContent = fired + ':' + (e instanceof BeforeUnloadEvent)
                + ':' + e.cancelable + ':' + (e.target === window);
            e.preventDefault();
        });
        setInterval(() => {}, 1000);
    </script>"#;
    let button = node(html, "b");
    let events = unload_checks(
        html,
        vec![
            PageCmd::CheckUnload { request: 1 },
            PageCmd::Click(button),
            PageCmd::CheckUnload { request: 2 },
            PageCmd::CheckUnload { request: 3 },
        ],
        vec![false, true],
        |events| checked(events).len() == 3,
    )
    .await;
    assert_eq!(checked(&events), [(1, true), (2, false), (3, true)]);
    let prompts_before = |request| {
        let at = events
            .iter()
            .position(
                |event| matches!(event, PageEvt::UnloadChecked { request: r, .. } if *r == request),
            )
            .unwrap();
        matches!(events[at - 1], PageEvt::Settled)
    };
    assert!(
        !prompts_before(1),
        "no sticky activation, no prompt: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, PageEvt::BeforeUnloadListeners(true)))
    );

    // A handler returning a string and a listener setting returnValue ask
    // the same way; without either nothing is asked.
    for script in [
        "onbeforeunload = () => 'Unsaved';",
        "addEventListener('beforeunload', e => { e.returnValue = 'x'; });",
        "addEventListener('beforeunload', () => {});",
    ] {
        let html = format!(
            "<button id=b>go</button><script>{script} setInterval(() => {{}}, 1000);</script>"
        );
        let button = node(&html, "b");
        let events = unload_checks(
            &html,
            vec![PageCmd::Click(button), PageCmd::CheckUnload { request: 7 }],
            vec![false],
            |events| checked(events).len() == 1,
        )
        .await;
        let asks = !script.contains("() => {}");
        assert_eq!(checked(&events), [(7, !asks)], "{script}");
    }
}

#[tokio::test]
async fn a_canceled_unload_keeps_the_page_and_its_navigation_never_leaves() {
    // HTML #navigate: a page's own navigation checks if unloading is
    // canceled; when the user stays, nothing navigates and the page goes on.
    let html = r#"<a id=a href="/elsewhere">away</a><output id=o></output><script>
        addEventListener('beforeunload', e => e.preventDefault());
        setInterval(() => {}, 1000);
    </script>"#;
    let anchor = node(html, "a");
    let events = unload_checks(html, vec![PageCmd::Click(anchor)], vec![false], |events| {
        events.iter().any(|event| matches!(event, PageEvt::Settled))
    })
    .await;
    assert!(navigated(&events).is_empty(), "{events:?}");
    let events = unload_checks(html, vec![PageCmd::Click(anchor)], vec![true], |events| {
        !navigated(events).is_empty()
    })
    .await;
    assert_eq!(navigated(&events), ["https://tabs.example/elsewhere"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_ending_page_unloads_and_its_beacons_outlive_it() {
    // HTML #unload-a-document: pagehide (with the Document still visible),
    // then visibilitychange to hidden, then unload, both page transition
    // events targeted at the Document; a keepalive beacon from the unload
    // handler reaches the server after the page ended. Page Lifecycle
    // #discarding instead freezes the page and never unloads it.
    let server = crate::http::referer_server::Server::start(|_| {
        (String::from("text/plain"), String::new(), Vec::new())
    })
    .await;
    let html = r#"<script>
        const log = [];
        addEventListener('pageshow', () => { document.title = 'shown'; });
        addEventListener('pagehide', e => log.push('pagehide_' + e.persisted + '_'
            + document.visibilityState + '_' + (e.target === document)));
        document.addEventListener('visibilitychange', () => log.push('vis_' + document.visibilityState));
        document.addEventListener('freeze', () => navigator.sendBeacon('/frozen'));
        addEventListener('unload', e => {
            log.push('unload_' + (e.target === document));
            navigator.sendBeacon('/log?' + log.join('.'));
            location.href = '/never';
        });
    </script>"#;
    for discard in [false, true] {
        let mut env = PageEnv::bare(&format!("{}/page", server.base));
        env.terminal_presentation = false;
        env.net = Some(tokio::runtime::Handle::current());
        let (handle, mut events) = crate::js::spawn_page(html.into(), env);
        match tokio::time::timeout(Duration::from_secs(30), events.recv()).await {
            Ok(Some(PageEvt::Updated { .. })) => {}
            other => panic!("the page must stay resident for its listeners: {other:?}"),
        }
        // A page shows once loaded (its pagehide pairs with that pageshow).
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            match tokio::time::timeout_at(deadline, events.recv()).await {
                Ok(Some(PageEvt::Title(title))) if title == "shown" => break,
                Ok(Some(_)) => {}
                other => panic!("the page never showed: {other:?}"),
            }
        }
        if discard {
            handle.discard(&tokio::runtime::Handle::current());
        } else {
            handle.unload(&tokio::runtime::Handle::current());
        }
        // The page ends: its event channel closes, without a navigation.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while let Ok(Some(event)) = tokio::time::timeout_at(deadline, events.recv()).await {
            assert!(!matches!(event, PageEvt::Navigate(..)), "{event:?}");
        }
    }
    server
        .wait_for(&[
            "/log?pagehide_false_visible_true.vis_hidden.unload_true",
            "/frozen",
        ])
        .await;
    assert_eq!(server.log.lock().unwrap().len(), 2, "{:?}", server.log);
}
