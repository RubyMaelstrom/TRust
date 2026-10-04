//! A pre-warmed spare page engine, after Chromium's spare renderer.
//!
//! Every navigation runs its document in a new Lumen engine (one ECMAScript
//! Agent) on its own page-actor thread, and nothing of an earlier document
//! survives into it. What a new Agent lacks is work that is identical for
//! every document: decoding the platform-prelude snapshot into the Agent's
//! shared program table and compiling bytecode and native code for it. Window
//! Realms of one Agent share that table (Lumen's
//! `eval_shared_classic_snapshot_interruptible`), which is why a child Window
//! bootstraps in a fraction of the time a new Agent needs.
//!
//! A spare is a page-actor thread that does this work during idle time. It
//! creates an engine and evaluates the prelude once in a throwaway Window Realm
//! configured as `about:blank`: no author code, nothing derived from any URL.
//! It then settles that Realm's jobs, removes its host state, collects it, and
//! waits. The next navigation claims the spare and the actor continues exactly
//! as a cold one from the point where it would have created its engine: the
//! document's `HostState`, clocks, `__trust_cfg` (URL, origin, referrer,
//! secure context, cookie/storage partitions, viewport, devicePixelRatio,
//! navigation timing, time origin) and the default Realm's own platform
//! bootstrap are all created at claim time, by the same code as a cold load.
//! Only the object-free compiled code, which Lumen validates against the live
//! Realm on every use, is shared with the warm-up.
//!
//! Invariants:
//! * at most one spare exists, and exactly one navigation claims it; the
//!   engine then belongs to that page and ends with its actor;
//! * the engine's default Realm, which becomes the page's Window, is untouched
//!   until the claim;
//! * no job, unhandled rejection, host state or Realm of the warm-up survives
//!   it; a warm-up that cannot prove this hands the claim a new engine.

use super::*;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Mutex, mpsc};

/// `SpareEngine::state`: building the engine and the process-wide prelude
/// snapshot. A claim now skips the warm-up bootstrap; nothing is wasted.
const PREPARING: u8 = 0;
/// Bootstrapping and discarding the warm-up Realm. A navigation does not wait
/// for this: it runs cold, and the spare remains for the next one.
const WARMING: u8 = 1;
/// The shared program table is warm and the warm-up Realm is gone.
const WARM: u8 = 2;
/// The warm-up failed or was interrupted; a claim gets a new engine.
const COLD: u8 = 3;
/// A navigation owns this spare.
const CLAIMED: u8 = 4;

fn state_name(state: u8) -> &'static str {
    match state {
        PREPARING => "preparing",
        WARMING => "warming",
        WARM => "warm",
        COLD => "cold",
        _ => "claimed",
    }
}

/// The waiting half of a spare actor, owned by the spare slot until a
/// navigation claims it.
pub(super) struct SpareEngine {
    claims: mpsc::Sender<desktop::PageStart>,
    interrupt: Arc<lumen::RuntimeInterrupt>,
    state: Arc<AtomicU8>,
}

impl SpareEngine {
    /// Start a spare actor thread; it warms its engine immediately.
    pub(super) fn spawn() -> Option<Self> {
        let interrupt = Arc::new(lumen::RuntimeInterrupt::default());
        let state = Arc::new(AtomicU8::new(PREPARING));
        let (claims, receiver) = mpsc::channel();
        let actor_interrupt = interrupt.clone();
        let actor_state = state.clone();
        crate::page_threads::spawn(
            desktop::page_thread_builder(),
            interrupt.clone(),
            None,
            move || {
                run(receiver, actor_interrupt, actor_state);
                crate::release_allocator_memory();
            },
        )
        .ok()?;
        Some(Self {
            claims,
            interrupt,
            state,
        })
    }

    /// Reserve this spare for one navigation unless it is mid-bootstrap,
    /// where waiting would cost more than a cold engine. Returns the state
    /// it was reserved in.
    fn reserve(&self) -> Option<u8> {
        let mut current = self.state.load(Ordering::Acquire);
        loop {
            if matches!(current, WARMING | CLAIMED) {
                return None;
            }
            match self.state.compare_exchange_weak(
                current,
                CLAIMED,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Some(current),
                Err(actual) => current = actual,
            }
        }
    }

    /// Hand one document to this reserved spare. On success the caller's page
    /// handle must own the returned interrupt: it is the engine's, and
    /// cancelling it retires the page. A spare whose thread has ended returns
    /// the document.
    pub(super) fn claim(
        self,
        start: desktop::PageStart,
        cache: &Arc<crate::http::PageCache>,
    ) -> Result<Arc<lumen::RuntimeInterrupt>, Box<desktop::PageStart>> {
        crate::page_threads::attach_cache(&self.interrupt, cache);
        match self.claims.send(start) {
            Ok(()) => Ok(self.interrupt),
            Err(mpsc::SendError(start)) => Err(Box::new(start)),
        }
    }
}

/// The spare actor thread: warm, wait for exactly one claim, then run that
/// document's actor on the warmed engine. Dropping the slot's sender (shutdown
/// or policy change) ends a waiting spare.
fn run(
    claims: mpsc::Receiver<desktop::PageStart>,
    interrupt: Arc<lumen::RuntimeInterrupt>,
    state: Arc<AtomicU8>,
) {
    // A spare whose thread ends without a claim (a panic, or shutdown) must
    // not look like one still warming: navigations would skip it forever.
    struct ColdOnExit(Arc<AtomicU8>);
    impl Drop for ColdOnExit {
        fn drop(&mut self) {
            for from in [PREPARING, WARMING] {
                let _ = self
                    .0
                    .compare_exchange(from, COLD, Ordering::AcqRel, Ordering::Acquire);
            }
        }
    }
    let _cold_on_exit = ColdOnExit(state.clone());
    let trace = std::env::var_os("TRUST_NET_TRACE").is_some();
    let started = Instant::now();
    let mut engine = lumen::Engine::new_with_interrupt(interrupt.clone());
    let engine_ms = started.elapsed().as_millis();
    // The process-wide prelude snapshot is compiled once, by whichever engine
    // asks first; any claimed page needs it, so this is never wasted.
    let snapshot = platform_prelude_snapshot();
    let snapshot_ms = started.elapsed().as_millis() - engine_ms;
    // A navigation that reserved the spare meanwhile would only wait for a
    // throwaway bootstrap: its own bootstrap compiles the same code.
    let warming = state
        .compare_exchange(PREPARING, WARMING, Ordering::AcqRel, Ordering::Acquire)
        .is_ok();
    let engine = if !warming {
        Some(engine)
    } else {
        match snapshot.and_then(|snapshot| warm(&mut engine, snapshot)) {
            Ok(()) => {
                // Return the warm-up Realm's freed pages before idling.
                crate::release_allocator_memory();
                state.store(WARM, Ordering::Release);
                Some(engine)
            }
            Err(error) => {
                state.store(COLD, Ordering::Release);
                // Shutdown cancels a warming spare; that needs no report.
                let cancelled = matches!(
                    interrupt.current_reason(),
                    Some(lumen::InterruptReason::Cancelled)
                );
                if !cancelled && (trace || std::env::var_os("TRUST_LUMEN_TRACE").is_some()) {
                    eprintln!("lumen: spare page engine discarded: {error}");
                }
                None
            }
        }
    };
    if trace {
        eprintln!(
            "js : @{:>6}ms spare page engine {} after {} ms (engine {engine_ms} ms, snapshot {snapshot_ms} ms)",
            crate::http::trace_ms(),
            if warming {
                state_name(state.load(Ordering::Acquire))
            } else {
                "claimed while preparing"
            },
            started.elapsed().as_millis(),
        );
    }
    let Ok(start) = claims.recv() else {
        return;
    };
    drop(claims);
    let engine = engine.unwrap_or_else(|| lumen::Engine::new_with_interrupt(interrupt.clone()));
    desktop::page_actor(start, interrupt, Some(engine));
}

/// Upper bound on warm-up jobs. The prelude queues a handful; a runaway
/// warm-up is discarded rather than handed to a page.
const MAX_WARM_UP_JOBS: usize = 100_000;

/// Fill `engine`'s shared program table by bootstrapping the platform prelude
/// in a throwaway `about:blank` Window Realm, then remove every trace of that
/// Realm. The default Realm is not touched.
fn warm(engine: &mut lumen::Engine, snapshot: &'static [u8]) -> Result<(), String> {
    let blank = url::Url::parse("about:blank").expect("about:blank parses");
    let dom = Rc::new(RefCell::new(Dom::parse_document("")));
    let mut state = HostState::new(dom, Rc::new(RealmClock::new()));
    state.base = blank.clone();
    state.window_request_urls.insert(0, blank.clone());
    // Host tasks the bootstrap queues belong to the throwaway Realm.
    let (task_tx, task_rx) = tokio::sync::mpsc::unbounded_channel();
    state.task_events = Some(LumenTaskSender::Page(task_tx));
    engine
        .ctx()
        .op_state()
        .put_retained_memory_with_external_memory(state);
    install_agent_host_hooks(engine);
    let config = main_window_config(
        &blank,
        DEFAULT_VIEWPORT,
        1.0,
        serde_json::Value::Null,
        None,
        MainWindowTraces::default(),
    );

    // As in `host_create_window_realm`, creation and bootstrap are one
    // publication step: no collection may see partially built intrinsics.
    engine.ctx().suspend_gc();
    let realm = engine.ctx().create_embed_realm();
    let Value::Obj(global) = &realm else {
        engine.ctx().resume_gc();
        return Err(String::from(
            "spare engine warm-up Realm has no global object",
        ));
    };
    let warm_up_global = Rc::downgrade(global);
    let bootstrap = engine.with_embed_realm(&realm, |engine| {
        for &(name, len, host_fn) in LUMEN_HOST_FUNCTIONS {
            engine.define_global(name, len, host_fn);
        }
        eval(engine, &config, "spare engine configuration")?;
        eval_bootstrap_snapshot(engine, snapshot, "spare engine platform prelude")
    });
    engine.ctx().resume_gc();

    // Settle the warm-up's own jobs while its host state still exists, so
    // nothing it queued can run later as part of the page. On any failure
    // (including shutdown's interruption) the caller discards this engine.
    let unavailable = |_| String::from("spare engine warm-up Realm is unavailable");
    bootstrap.map_err(unavailable)??;
    engine
        .with_embed_realm(&realm, |engine| {
            drain_jobs(engine)?;
            // The Agent keeps the last successful RegExpBuiltinExec's match,
            // and with it that Realm's %RegExp%, until a legacy static
            // accessor of that Realm materializes it. Read one here, so the
            // warm-up's own regular expressions do not retain its Realm.
            eval(engine, "void RegExp.input;", "spare engine RegExp statics")
        })
        .map_err(unavailable)??;

    // Remove every owner of the warm-up Realm, collect it, and prove it gone.
    drop(realm);
    drop(engine.ctx().op_state().take::<HostState>());
    drop(task_rx);
    let _ = engine.take_unhandled_rejections_full();
    engine.collect_garbage_at_idle();
    if warm_up_global.upgrade().is_some() {
        return Err(String::from(
            "spare engine warm-up Realm outlived its collection",
        ));
    }
    if engine.has_pending_jobs() || !engine.take_unhandled_rejections_full().is_empty() {
        return Err(String::from("spare engine warm-up left pending jobs"));
    }
    Ok(())
}

fn drain_jobs(engine: &mut lumen::Engine) -> Result<(), String> {
    for _ in 0..MAX_WARM_UP_JOBS {
        match engine.run_one_job_interruptible() {
            Ok(true) => {}
            Ok(false) => return Ok(()),
            Err(reason) => {
                return Err(format!(
                    "spare engine warm-up interrupted: {}",
                    reason.message()
                ));
            }
        }
    }
    Err(String::from("spare engine warm-up jobs did not settle"))
}

struct Slot {
    enabled: bool,
    /// Warm the next spare after each navigation renders. A single-document
    /// client warms only the one it requests.
    refill: bool,
    spare: Option<SpareEngine>,
}

static SLOT: Mutex<Slot> = Mutex::new(Slot {
    enabled: false,
    refill: false,
    spare: None,
});

fn slot() -> std::sync::MutexGuard<'static, Slot> {
    SLOT.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// `TRUST_NO_SPARE_ENGINE` (presence flag) keeps every navigation cold.
fn disabled_by_environment() -> bool {
    static DISABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *DISABLED.get_or_init(|| std::env::var_os("TRUST_NO_SPARE_ENGINE").is_some())
}

/// Allow (or forbid) a spare engine in this process. Frontends that browse
/// enable it; library users and tests never get one implicitly. Disabling
/// discards a waiting spare.
pub(crate) fn set_enabled(enabled: bool, refill: bool) -> bool {
    let enabled = enabled && !disabled_by_environment();
    let discarded = {
        let mut slot = slot();
        slot.enabled = enabled;
        slot.refill = enabled && refill;
        if enabled { None } else { slot.spare.take() }
    };
    drop(discarded);
    enabled
}

/// Start warming a spare when enabled and none exists. Cheap and idempotent:
/// the work happens on the spare's own thread.
pub(crate) fn prewarm() {
    let mut slot = slot();
    if slot.enabled && slot.spare.is_none() {
        slot.spare = SpareEngine::spawn();
    }
}

/// A navigation has rendered: warm the next navigation's engine.
pub(crate) fn refill() {
    let mut slot = slot();
    if slot.enabled && slot.refill && slot.spare.is_none() {
        slot.spare = SpareEngine::spawn();
    }
}

/// Take the waiting spare for one navigation, unless it is still
/// bootstrapping: then the navigation runs cold and the spare stays for the
/// next one.
pub(super) fn take() -> Option<SpareEngine> {
    let mut slot = slot();
    let reserved = slot.spare.as_ref()?.reserve();
    if std::env::var_os("TRUST_NET_TRACE").is_some() {
        eprintln!(
            "js : @{:>6}ms {}",
            crate::http::trace_ms(),
            match reserved {
                Some(state) => format!("claimed spare page engine ({})", state_name(state)),
                None => String::from("spare page engine still warming; this navigation runs cold"),
            }
        );
    }
    reserved.and_then(|_| slot.spare.take())
}

/// Process shutdown: no new spare, and a waiting one ends before its thread
/// is joined with the other page actors.
pub(crate) fn shutdown() {
    set_enabled(false, false);
}

#[cfg(test)]
impl SpareEngine {
    /// Wait until the spare has finished warming; true when it is warm.
    pub(super) fn wait_until_warm(&self, limit: Duration) -> bool {
        let deadline = Instant::now() + limit;
        while matches!(self.state.load(Ordering::Acquire), PREPARING | WARMING)
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(5));
        }
        self.state.load(Ordering::Acquire) == WARM
    }

    /// Reserve as `take` does, for tests that hold their own spare.
    pub(super) fn reserved(self) -> Option<Self> {
        self.reserve().map(|_| self)
    }

    pub(super) fn interrupt(&self) -> Arc<lumen::RuntimeInterrupt> {
        self.interrupt.clone()
    }
}

#[cfg(test)]
pub(super) fn has_spare() -> bool {
    slot().spare.is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::js::{PageEnv, PageEvt};

    const URL: &str = "https://spare.example/doc?q=1";

    /// Everything about the page's global environment that does not depend on
    /// the clock: global and interface descriptors in enumeration order,
    /// prototype chains, intrinsic identity, error behaviour, the per-document
    /// configuration, and a child Window Realm.
    const SURFACE: &str = r#"<!doctype html><title>surface</title><pre id="fp"></pre><script>
    (() => {
        const lines = [];
        const name = key => typeof key === "symbol" ? "@" + String(key.description) : key;
        const shape = d => ("value" in d
            ? "v:" + typeof d.value
                + (typeof d.value === "function" ? "/" + d.value.length + "/" + d.value.name : "")
                + (d.writable ? "w" : "-")
            : "a:" + typeof d.get + "/" + typeof d.set)
            + (d.enumerable ? "e" : "-") + (d.configurable ? "c" : "-");
        const own = (label, object) => {
            for (const key of Reflect.ownKeys(object))
                lines.push(label + "." + name(key) + " " + shape(Reflect.getOwnPropertyDescriptor(object, key)));
        };
        const ctor = p => Object.prototype.hasOwnProperty.call(p, "constructor")
            && typeof p.constructor === "function" ? p.constructor.name : "?";
        const chain = object => {
            const names = [];
            for (let p = Object.getPrototypeOf(object); p; p = Object.getPrototypeOf(p)) names.push(ctor(p));
            return names.join(">");
        };
        own("global", globalThis);
        for (const key of Reflect.ownKeys(globalThis)) {
            const value = Reflect.getOwnPropertyDescriptor(globalThis, key).value;
            if (typeof value === "function" && value.prototype && typeof value.prototype === "object") {
                own(name(key), value);
                own(name(key) + ".prototype", value.prototype);
                lines.push(name(key) + " chain " + chain(value.prototype));
            }
        }
        lines.push("window " + chain(window), "document " + chain(document),
            "div " + chain(document.createElement("div")));
        const thrown = f => {
            try { f(); return "none"; } catch (e) {
                return e.constructor.name + ":" + (Object.getPrototypeOf(e) === e.constructor.prototype)
                    + ":" + e.message;
            }
        };
        const frame = document.createElement("iframe");
        document.body.appendChild(frame);
        const child = frame.contentWindow;
        lines.push(JSON.stringify({
            array: Object.getPrototypeOf([]) === Array.prototype,
            fn: Object.getPrototypeOf(function () {}) === Function.prototype,
            asyncFn: Object.getPrototypeOf(async function () {}).constructor.name,
            promise: (async () => {})() instanceof Promise,
            indirectEval: (0, eval)("this") === globalThis,
            functionCtor: Function("return this")() === globalThis,
            windowCtor: window.constructor === Window,
            documentProto: chain(document) === chain(new Document()),
            event: new Event("x") instanceof Event,
            div: document.createElement("div") instanceof HTMLDivElement,
            symbolFor: Symbol.for("trust.spare") === Symbol.for("trust.spare"),
            nullProperty: thrown(() => null.x),
            unbound: thrown(() => notDefinedAnywhere),
            dom: thrown(() => document.createElement("1")),
            json: thrown(() => JSON.parse("{")),
            href: location.href, origin: location.origin, secure: isSecureContext,
            referrer: document.referrer,
            ua: navigator.userAgent, size: innerWidth + "x" + innerHeight, dpr: devicePixelRatio,
            cookie: typeof document.cookie, storage: typeof localStorage,
            childArray: child.Array !== Array && child.Array.name,
            childProto: Object.getPrototypeOf(child.document.createElement("p"))
                === child.HTMLParagraphElement.prototype,
            childOrigin: child.location.origin,
        }));
        document.getElementById("fp").textContent = lines.join("\n");
    })();
    </script>"#;

    async fn rendered(events: &mut tokio::sync::mpsc::Receiver<PageEvt>, marker: &str) -> String {
        tokio::time::timeout(Duration::from_secs(180), async {
            loop {
                match events.recv().await {
                    Some(
                        PageEvt::Updated { html, outcome } | PageEvt::Static { html, outcome },
                    ) => {
                        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
                        if html.contains(marker) {
                            return html;
                        }
                    }
                    Some(PageEvt::Trouble(errors)) => panic!("{errors:?}"),
                    Some(_) => {}
                    None => panic!("page actor ended before rendering {marker:?}"),
                }
            }
        })
        .await
        .expect("page actor timed out")
    }

    fn warm_spare() -> SpareEngine {
        let spare = SpareEngine::spawn().expect("spawn spare page engine");
        assert!(
            spare.wait_until_warm(Duration::from_secs(180)),
            "spare page engine did not warm"
        );
        spare.reserved().expect("a warm spare is claimable")
    }

    fn surface_of(html: &str) -> String {
        let start = html.find("<pre id=\"fp\">").expect("fingerprint") + "<pre id=\"fp\">".len();
        let end = start + html[start..].find("</pre>").expect("fingerprint end");
        html[start..end].to_string()
    }

    #[tokio::test]
    async fn spare_engine_page_has_the_cold_engine_global_surface() {
        let (_cold_handle, mut cold) =
            desktop::spawn_page_with(None, SURFACE.into(), PageEnv::bare(URL));
        let cold = surface_of(&rendered(&mut cold, "childOrigin").await);

        let spare = warm_spare();
        let (_warm_handle, mut warm) =
            desktop::spawn_page_with(Some(spare), SURFACE.into(), PageEnv::bare(URL));
        let warm = surface_of(&rendered(&mut warm, "childOrigin").await);
        assert_same_surface(&cold, &warm, "warm spare");

        // Reserved while still preparing: the warm-up bootstrap is skipped.
        if let Some(spare) = SpareEngine::spawn()
            .expect("spawn spare page engine")
            .reserved()
        {
            let (_handle, mut events) =
                desktop::spawn_page_with(Some(spare), SURFACE.into(), PageEnv::bare(URL));
            let preparing = surface_of(&rendered(&mut events, "childOrigin").await);
            assert_same_surface(&cold, &preparing, "spare claimed while preparing");
        }
    }

    fn assert_same_surface(cold: &str, spare: &str, label: &str) {
        assert!(cold.lines().count() > 1000, "surface too small: {cold}");
        assert!(
            cold.contains(r#""href":"https://spare.example/doc?q=1""#),
            "{cold}"
        );
        assert!(cold.contains(r#""childArray":"Array""#), "{cold}");
        if cold != spare {
            let (index, (expected, actual)) = cold
                .lines()
                .zip(spare.lines())
                .enumerate()
                .find(|(_, (a, b))| a != b)
                .unwrap_or((0, ("<length>", "<length>")));
            panic!("{label} surface differs at line {index}: cold {expected:?}, spare {actual:?}");
        }
    }

    #[tokio::test]
    async fn spare_engine_time_origin_is_the_documents_not_the_spares() {
        const PAGE: &str = r#"<p id="t"></p><script>
            document.getElementById("t").textContent =
                "now=" + Math.round(performance.now()) + ";drift=" +
                Math.round(Date.now() - performance.timeOrigin - performance.now()) + ";";
        </script>"#;
        let spare = warm_spare();
        // Idle long enough that a leaked spare clock would be obvious.
        std::thread::sleep(Duration::from_millis(1500));
        let claimed = Instant::now();
        let (_handle, mut events) =
            desktop::spawn_page_with(Some(spare), PAGE.into(), PageEnv::bare(URL));
        let html = rendered(&mut events, ";drift=").await;
        let elapsed = claimed.elapsed().as_millis() as i64;
        let field = |name: &str| -> i64 {
            let start = html.find(name).expect("field") + name.len();
            let end = start + html[start..].find(';').expect("field end");
            html[start..end].parse().expect("integer field")
        };
        assert!(
            field("now=") <= elapsed + 1,
            "performance.now() predates the claim: {html} after {elapsed} ms"
        );
        assert!(field("drift=").abs() <= 2, "{html}");
    }

    #[test]
    fn warm_up_leaves_no_realm_jobs_or_host_state_and_an_untouched_default_realm() {
        desktop::page_thread_builder()
            .spawn(|| {
                const SURFACE: &str = "Reflect.ownKeys(globalThis).map(String).join() + \
                    '|' + Reflect.ownKeys(Object.prototype).map(String).join()";
                let mut fresh = lumen::Engine::new();
                let expected = eval_value(&mut fresh, SURFACE, "fresh surface")
                    .map(|value| value_string(&mut fresh, &value))
                    .unwrap();

                // `warm` itself proves that its Realm was collected.
                let mut engine = lumen::Engine::new();
                warm(&mut engine, platform_prelude_snapshot().unwrap()).unwrap();
                assert!(engine.ctx().host_mut::<HostState>().is_none());
                assert!(!engine.has_pending_jobs());
                assert!(engine.take_unhandled_rejections().is_empty());
                let surface = eval_value(&mut engine, SURFACE, "warm surface")
                    .map(|value| value_string(&mut engine, &value))
                    .unwrap();
                assert_eq!(surface, expected);
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn a_dropped_spare_ends_its_thread() {
        let spare = warm_spare();
        let interrupt = spare.interrupt();
        drop(spare);
        // The thread's own handle goes; ours and the thread registry's may remain.
        let deadline = Instant::now() + Duration::from_secs(30);
        while Arc::strong_count(&interrupt) > 2 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            Arc::strong_count(&interrupt) <= 2,
            "spare thread still waiting"
        );
        assert!(interrupt.current_reason().is_none());
    }

    #[test]
    fn a_bootstrapping_spare_is_left_for_the_next_navigation() {
        let spare_in = |state| SpareEngine {
            claims: mpsc::channel().0,
            interrupt: Default::default(),
            state: Arc::new(AtomicU8::new(state)),
        };
        for (state, claimable) in [
            (PREPARING, true),
            (WARMING, false),
            (WARM, true),
            (COLD, true),
            (CLAIMED, false),
        ] {
            let spare = spare_in(state);
            assert_eq!(
                spare.reserve(),
                claimable.then_some(state),
                "{}",
                state_name(state)
            );
            assert_eq!(
                spare.state.load(Ordering::Acquire),
                if claimable { CLAIMED } else { state }
            );
        }
    }

    #[test]
    fn only_web_start_addresses_warm_a_spare_at_startup() {
        for address in [
            "example.org",
            "https://example.org/",
            "HTTP://example.org/",
            "file:///tmp/page.html",
            "page.html",
        ] {
            assert!(
                crate::js::start_address_may_use_page_engine(address),
                "{address}"
            );
        }
        for address in [
            "gopher://example.org/",
            "gemini://example.org/",
            "telnet://bbs",
        ] {
            assert!(
                !crate::js::start_address_may_use_page_engine(address),
                "{address}"
            );
        }
    }

    #[test]
    fn library_users_never_get_an_implicit_spare() {
        // Only a frontend's `enable_spare_page_engine` arms the slot.
        refill();
        prewarm();
        assert!(!has_spare());
    }
}
