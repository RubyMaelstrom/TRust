//! Opt-in Lumen backend under construction.
//!
//! This shares TRust's real platform prelude, DOM arena, and integer-handle host boundary while
//! leaving the production Boa page actor untouched. Host operations are being moved across in
//! standards-oriented slices and are checked against Boa's canonical registry so names and
//! JavaScript-visible function lengths cannot drift silently.

use crate::dom::{AdoptError, DOCUMENT, Dom, NodeData, SelectorList};
use lumen::bytecode::Tier;
use lumen::embed::{Ctx, EvalError, NativeFn, Value};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[path = "lumen_wasm.rs"]
mod lumen_wasm;

const DEFAULT_URL: &str = "https://example.com/";
const DEFAULT_VIEWPORT: crate::layout2::Viewport = crate::layout2::Viewport {
    width: 640.0,
    height: 384.0,
};

type LumenGeomCache = (
    u64,
    std::collections::HashMap<crate::dom::NodeId, crate::layout2::PxRect>,
    std::collections::HashMap<crate::dom::NodeId, (Vec<f32>, Vec<f32>)>,
    std::collections::HashMap<crate::dom::NodeId, crate::layout2::PxRect>,
);

type LumenFetchResult = Option<(u16, String, Vec<u8>, String)>;
type LumenResourceResult = Option<(u16, String, Vec<u8>, Vec<(String, String)>)>;

#[derive(Clone, Copy)]
enum LumenResourceKind {
    ClassicScript,
    ModuleScript,
    Stylesheet,
}

/// Send-only work returned by background platform operations. Engine values never enter this
/// channel: the page thread retains Promise resolvers in [`LumenNetwork`] and settles them after
/// selecting the corresponding HTML task.
#[allow(dead_code)] // Read by the resident Lumen actor once the backend cutover reaches src/js.rs.
enum LumenHostTask {
    FetchDone {
        id: usize,
        result: LumenFetchResult,
    },
    ResourceDone {
        node_id: usize,
        name: String,
        kind: LumenResourceKind,
        result: LumenResourceResult,
        external: bool,
    },
    DynamicModule {
        request_id: u64,
        result: Option<(String, String)>,
    },
    WebSocket {
        id: usize,
        event: crate::ws::WsIn,
    },
    Worker {
        id: usize,
        event: crate::js::WorkerOut,
    },
    WorkerExited {
        id: usize,
    },
}

struct LumenNetwork {
    handle: tokio::runtime::Handle,
    cache: Arc<crate::http::PageCache>,
    fetched: Arc<std::sync::atomic::AtomicUsize>,
    next_fetch_id: usize,
    pending_fetches: HashMap<usize, Value>,
}

#[derive(Clone)]
struct LumenDynamicModuleNetwork {
    handle: tokio::runtime::Handle,
    cache: Arc<crate::http::PageCache>,
    fetched: Arc<std::sync::atomic::AtomicUsize>,
}

#[derive(Clone)]
struct LumenDynamicModuleLoader {
    page: url::Url,
    events: tokio::sync::mpsc::UnboundedSender<LumenHostTask>,
    network: Option<LumenDynamicModuleNetwork>,
}

struct LumenWebSockets {
    handle: tokio::runtime::Handle,
    page: url::Url,
    tasks: Arc<crate::http::PageTaskScope>,
    events: tokio::sync::mpsc::Sender<(usize, crate::ws::WsIn)>,
    sockets: HashMap<usize, tokio::sync::mpsc::Sender<crate::ws::WsOut>>,
    next_id: usize,
}

enum LumenWorkerCtl {
    Message(String),
    Terminate,
}

struct LumenWorkerHandle {
    ctl: std::sync::mpsc::SyncSender<LumenWorkerCtl>,
    interrupt: Arc<lumen::RuntimeInterrupt>,
}

impl Drop for LumenWorkerHandle {
    fn drop(&mut self) {
        // HTML §10.2.4 "terminate a worker": cancellation is host control
        // flow, so author catch/finally cannot observe or suppress it.
        self.interrupt.cancel();
    }
}

struct LumenPageWorkers {
    handle: tokio::runtime::Handle,
    page: url::Url,
    tasks: Arc<crate::http::PageTaskScope>,
    events: tokio::sync::mpsc::UnboundedSender<LumenHostTask>,
    workers: HashMap<usize, LumenWorkerHandle>,
    next_id: usize,
}

struct LumenWorkerSelf {
    id: usize,
    events: tokio::sync::mpsc::UnboundedSender<LumenHostTask>,
    closed: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LumenWorkerKind {
    Classic,
    Module,
}

struct LumenWorkerLaunch {
    id: usize,
    owner_page: url::Url,
    script_url: url::Url,
    kind: LumenWorkerKind,
    name: String,
    script_body: Option<Vec<u8>>,
    secure_context: bool,
}

struct HostState {
    dom: Rc<RefCell<Dom>>,
    clock: Rc<RealmClock>,
    base: url::Url,
    storage: crate::js::WebStorage,
    blobs: crate::js::BlobMap,
    viewport: Cell<crate::layout2::Viewport>,
    device_pixel_ratio: Cell<f32>,
    geom_cache: Rc<RefCell<LumenGeomCache>>,
    images: Rc<RefCell<crate::layout2::ImageSizes>>,
    task_events: Option<tokio::sync::mpsc::UnboundedSender<LumenHostTask>>,
    pending_resources: usize,
    pending_dynamic_modules: Arc<std::sync::atomic::AtomicUsize>,
    network: Option<LumenNetwork>,
    websockets: Option<LumenWebSockets>,
    workers: Option<LumenPageWorkers>,
    worker_self: Option<LumenWorkerSelf>,
    wasm: lumen_wasm::PageWasm,
}

impl HostState {
    fn new(dom: Rc<RefCell<Dom>>, clock: Rc<RealmClock>) -> Self {
        {
            let mut dom = dom.borrow_mut();
            dom.set_viewport_px(DEFAULT_VIEWPORT.width, DEFAULT_VIEWPORT.height);
            dom.set_device_pixel_ratio(1.0);
        }
        Self {
            dom,
            clock,
            base: url::Url::parse(DEFAULT_URL).expect("static default URL parses"),
            storage: Default::default(),
            blobs: Default::default(),
            viewport: Cell::new(DEFAULT_VIEWPORT),
            device_pixel_ratio: Cell::new(1.0),
            geom_cache: Rc::new(RefCell::new((
                u64::MAX,
                Default::default(),
                Default::default(),
                Default::default(),
            ))),
            images: Default::default(),
            task_events: None,
            pending_resources: 0,
            pending_dynamic_modules: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            network: None,
            websockets: None,
            workers: None,
            worker_self: None,
            wasm: lumen_wasm::PageWasm::new(),
        }
    }

    #[allow(dead_code)] // The networked test realm uses this before the resident actor is switched.
    fn enable_network(
        &mut self,
        page: url::Url,
        handle: tokio::runtime::Handle,
        cache: Arc<crate::http::PageCache>,
        events: tokio::sync::mpsc::UnboundedSender<LumenHostTask>,
    ) {
        let (ws_events, mut ws_rx) = tokio::sync::mpsc::channel(64);
        let host_events = events.clone();
        cache.spawn(&handle, async move {
            while let Some((id, event)) = ws_rx.recv().await {
                if host_events
                    .send(LumenHostTask::WebSocket { id, event })
                    .is_err()
                {
                    break;
                }
            }
        });
        self.base = page;
        self.task_events = Some(events.clone());
        self.websockets = Some(LumenWebSockets {
            handle: handle.clone(),
            page: self.base.clone(),
            tasks: cache.task_scope(),
            events: ws_events,
            sockets: HashMap::new(),
            next_id: 1,
        });
        self.workers = Some(LumenPageWorkers {
            handle: handle.clone(),
            page: self.base.clone(),
            tasks: cache.task_scope(),
            events: events.clone(),
            workers: HashMap::new(),
            next_id: 1,
        });
        self.network = Some(LumenNetwork {
            handle,
            cache,
            fetched: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            next_fetch_id: 0,
            pending_fetches: HashMap::new(),
        });
    }

    fn configure_module_loading(&self, engine: &mut lumen::Engine) {
        engine.set_import_base(self.base.as_str());
        if let Some(network) = self.network.as_ref() {
            let page = self.base.clone();
            let handle = network.handle.clone();
            let cache = network.cache.clone();
            let fetched = network.fetched.clone();
            engine.set_module_loader(move |specifier, referrer| {
                module_dependency_loader(&page, &handle, &cache, &fetched, specifier, referrer)
            });
        }

        // ECMA-262 HostLoadImportedModule/FinishLoadingImportedModule: dynamic import starts a
        // host load and returns its promise without waiting for I/O. Static graph loading retains
        // the synchronous fallback above until Lumen exposes an asynchronous graph-loader API.
        let Some(events) = self.task_events.clone() else {
            return;
        };
        let pending_dynamic_modules = self.pending_dynamic_modules.clone();
        let loader = LumenDynamicModuleLoader {
            page: self.base.clone(),
            events,
            network: self
                .network
                .as_ref()
                .map(|network| LumenDynamicModuleNetwork {
                    handle: network.handle.clone(),
                    cache: network.cache.clone(),
                    fetched: network.fetched.clone(),
                }),
        };
        engine.set_async_dynamic_module_loader(
            move |request_id, specifier, referrer, _attribute_type| {
                pending_dynamic_modules.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                queue_dynamic_module_load(&loader, request_id, specifier, referrer);
                true
            },
        );
    }
}

struct RealmClock {
    epoch_ms: Cell<f64>,
    anchored_at: Cell<Instant>,
}

impl RealmClock {
    fn new() -> Self {
        let epoch_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_secs_f64() * 1000.0)
            .unwrap_or(0.0);
        Self {
            epoch_ms: Cell::new(epoch_ms),
            anchored_at: Cell::new(Instant::now()),
        }
    }

    fn now_ms(&self) -> f64 {
        self.epoch_ms.get() + self.anchored_at.get().elapsed().as_secs_f64() * 1000.0
    }

    fn set_epoch_ms(&self, epoch_ms: f64) {
        self.epoch_ms.set(epoch_ms);
        self.anchored_at.set(Instant::now());
    }
}

#[derive(Debug, Clone)]
pub struct SpikeReport {
    pub tier: Tier,
    pub prelude_time: Duration,
    pub benchmark_time: Duration,
    pub timer_turns: usize,
    pub pre_idle_live_objects: i64,
    pub idle_gc_time: Duration,
    pub idle_gc_reclaimed: i64,
    pub post_idle_live_objects: i64,
    pub final_gc_reclaimed: i64,
    pub post_final_live_objects: i64,
    pub score: Option<String>,
    pub logs: String,
}

pub fn parse_tier(name: &str) -> Result<Tier, String> {
    match name {
        "interp" => Ok(Tier::Interp),
        "bytecode" => Ok(Tier::Bytecode),
        "jit" => Ok(Tier::Jit),
        other => Err(format!(
            "unknown tier {other:?}; expected interp, bytecode, or jit"
        )),
    }
}

pub fn run_benchmark(path: &Path, tier: Tier, threshold: u32) -> Result<SpikeReport, String> {
    let source = std::fs::read_to_string(path)
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    let mut engine = lumen::Engine::new();
    engine.set_tier(tier);
    engine.set_tier_threshold(threshold);
    let clock = Rc::new(RealmClock::new());
    let engine_clock = clock.clone();
    engine.set_wall_clock(move || engine_clock.now_ms());
    let state = HostState::new(Rc::new(RefCell::new(Dom::new())), clock);
    state.configure_module_loading(&mut engine);
    engine.ctx().op_state().put(state);
    install_host_boundary(&mut engine);

    eval(
        &mut engine,
        &format!(
            "globalThis.__trust_cfg = {{ url: {url:?}, ua: 'TRust/0.1 Lumen spike', language: 'en-US', languages: ['en-US', 'en'], width: 640, height: 384 }};",
            url = DEFAULT_URL
        ),
        "TRust configuration",
    )?;

    let prelude_started = Instant::now();
    eval(&mut engine, crate::js::PRELUDE, "TRust platform prelude")?;
    let prelude_time = prelude_started.elapsed();
    eval(
        &mut engine,
        "globalThis.__trust.oneShot = true;",
        "one-shot event-loop setup",
    )?;

    let benchmark_started = Instant::now();
    eval(&mut engine, &source, "benchmark source")?;
    engine
        .run_microtasks_interruptible()
        .map_err(|reason| format!("benchmark microtasks interrupted: {}", reason.message()))?;

    let global = engine.global_this();
    let trust = engine
        .ctx()
        .get_member(&global, "__trust")
        .map_err(|_| "read __trust after benchmark".to_string())?;
    let tick = engine
        .ctx()
        .get_member(&trust, "tick")
        .map_err(|_| "read __trust.tick after benchmark".to_string())?;
    let mut timer_turns = 0usize;
    loop {
        let ran = engine
            .call_function_interruptible(&tick, trust.clone(), &[])
            .map_err(|error| describe_eval_error(&mut engine, error, "__trust.tick"))?;
        engine.run_microtasks_interruptible().map_err(|reason| {
            format!("__trust.tick microtasks interrupted: {}", reason.message())
        })?;
        match ran {
            Value::Bool(true) => timer_turns += 1,
            Value::Bool(false) => break,
            other => {
                return Err(format!(
                    "__trust.tick returned {}, expected boolean",
                    value_string(&mut engine, &other)
                ));
            }
        }
        if timer_turns > 100_000 {
            return Err("TRust one-shot event loop exceeded 100000 turns".to_string());
        }
    }
    let benchmark_time = benchmark_started.elapsed();
    let logs_value = eval_value(&mut engine, "__trust.logs.join('\\n')", "benchmark logs")?;
    let logs = value_string(&mut engine, &logs_value);
    let score = logs
        .lines()
        .find_map(|line| line.strip_prefix("log: Score: ").map(str::to_owned));
    let pre_idle_live_objects = engine.ctx().live_object_count();
    let idle_gc_started = Instant::now();
    let idle_gc_reclaimed = engine.collect_garbage_at_idle();
    let idle_gc_time = idle_gc_started.elapsed();
    let post_idle_live_objects = engine.ctx().live_object_count();
    // Keep a second forced collection in the probe so the idle hook's completeness is visible.
    let final_gc_reclaimed = engine.ctx().collect_garbage_for_host();
    let post_final_live_objects = engine.ctx().live_object_count();

    Ok(SpikeReport {
        tier,
        prelude_time,
        benchmark_time,
        timer_turns,
        pre_idle_live_objects,
        idle_gc_time,
        idle_gc_reclaimed,
        post_idle_live_objects,
        final_gc_reclaimed,
        post_final_live_objects,
        score,
        logs,
    })
}

#[cfg(feature = "lumen-desktop")]
mod desktop {
    use super::*;
    use crate::js::{FormSubmission, Outcome, PageCmd, PageEnv, PageEvt, PageHandle, PageHover};
    use std::collections::HashSet;

    const PAGE_STACK: usize = 64 * 1024 * 1024;
    const WAKE_FLOOR: Duration = Duration::from_millis(16);
    const USER_TASK_BUDGET: Duration = Duration::from_secs(1);
    const HOST_TASK_RENDER_BURST: usize = 64;

    struct LumenPage {
        engine: lumen::Engine,
        dom: Rc<RefCell<Dom>>,
        base: url::Url,
        outcome: Outcome,
        started: Instant,
        last_render: Option<crate::http::RenderedPage>,
        #[cfg(test)]
        last_diagnostic_render: Option<String>,
        live_regions: HashSet<usize>,
        live_boundaries: HashSet<usize>,
        boundary_render: HashMap<usize, String>,
        /// Viewport, density, and decoded intrinsic-size changes can require layout even when no
        /// DOM mutation occurred during the host task.
        render_environment_dirty: bool,
    }

    enum Wake {
        Interaction(Option<PageCmd>),
        Cmd(Option<PageCmd>),
        Hover(Option<PageHover>),
        Host(Option<LumenHostTask>),
        Platform,
        Timer,
        Lifecycle,
    }

    struct InteractionTurn {
        running: Arc<std::sync::Mutex<bool>>,
    }

    impl InteractionTurn {
        fn begin(
            running: &Arc<std::sync::Mutex<bool>>,
            interrupt: &Arc<lumen::RuntimeInterrupt>,
        ) -> Self {
            *running.lock().unwrap_or_else(|error| error.into_inner()) = true;
            interrupt.begin_user_interaction();
            Self {
                running: running.clone(),
            }
        }
    }

    impl Drop for InteractionTurn {
        fn drop(&mut self) {
            *self
                .running
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = false;
        }
    }

    /// Spawn the experimental Lumen resident realm behind the same actor
    /// contract used by both frontends. The separately named desktop binary is
    /// the only production-shaped entry point which selects this function.
    pub(crate) fn spawn_page(
        html: String,
        env: PageEnv,
    ) -> (PageHandle, tokio::sync::mpsc::Receiver<PageEvt>) {
        let cache = env.cache.clone();
        let interrupt = Arc::new(lumen::RuntimeInterrupt::default());
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(16);
        let (interaction_tx, interaction_rx) = tokio::sync::mpsc::channel(16);
        let interaction_running = Arc::new(std::sync::Mutex::new(false));
        let (hover_tx, hover_rx) = tokio::sync::watch::channel(PageHover {
            node: None,
            x: 0.0,
            y: 0.0,
        });
        let (event_tx, event_rx) = tokio::sync::mpsc::channel(16);
        let actor_interrupt = interrupt.clone();
        let actor_running = interaction_running.clone();
        let spawned = std::thread::Builder::new()
            .name(String::from("trust-page-lumen"))
            .stack_size(PAGE_STACK)
            .spawn(move || {
                page_actor(
                    html,
                    env,
                    cmd_rx,
                    interaction_rx,
                    hover_rx,
                    event_tx,
                    actor_running,
                    actor_interrupt,
                );
                crate::release_allocator_memory();
            });
        if spawned.is_err() {
            // Dropping the event sender in the failed closure tells the caller
            // to take its existing CSS-only fallback.
        }
        (
            PageHandle::from_lumen_parts(
                cmd_tx,
                interaction_tx,
                interaction_running,
                hover_tx,
                cache,
                interrupt,
            ),
            event_rx,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn page_actor(
        html: String,
        env: PageEnv,
        mut cmds: tokio::sync::mpsc::Receiver<PageCmd>,
        mut interactions: tokio::sync::mpsc::Receiver<PageCmd>,
        mut hover: tokio::sync::watch::Receiver<PageHover>,
        events: tokio::sync::mpsc::Sender<PageEvt>,
        interaction_running: Arc<std::sync::Mutex<bool>>,
        interrupt: Arc<lumen::RuntimeInterrupt>,
    ) {
        let (host_tx, mut host_rx) = tokio::sync::mpsc::unbounded_channel();
        // A no-network realm does not retain the sender in HostState. Keep the
        // lane open anyway: a closed `recv()` is immediately ready and would
        // otherwise win the actor select before lifecycle/timer/input work.
        let _host_keepalive = host_tx.clone();
        let mut page = match load_page(&html, env, host_tx, interrupt.clone()) {
            Ok(page) => page,
            Err(outcome) => {
                let _ = events.blocking_send(PageEvt::Static { html, outcome });
                return;
            }
        };

        // HTML's parser task has completed through DOMContentLoaded. Expose a
        // rendering opportunity before the separately queued load task; slow
        // dynamically prepared resources can therefore delay load without
        // hiding the interactive document shell.
        let (mut shell, mut rendered, mut has_interaction) = render_with_observers(&mut page);
        page.last_render = Some(rendered.clone());
        #[cfg(test)]
        {
            page.last_diagnostic_render = Some(crate::js::render_canonical(&shell));
        }
        let mut lifecycle_complete = false;
        let mut lifecycle_submission = None;

        // A truly inert document does not need a resident realm. Complete its load task first so
        // a load handler can still create controls, timers, workers, observers, or navigation;
        // only then classify the final state as Static. Interactive or pending-work pages retain
        // the ordinary shell-before-load rendering opportunity below.
        if !has_resident_work(&mut page, has_interaction) {
            prepare_task(&interrupt, crate::js::WALL_BUDGET);
            let _ = evaluate_task(
                &mut page,
                "__trust.readyState = 'complete'; __trust.fire(window, 'load', false);",
                "load event",
            );
            checkpoint(&mut page, "load event");
            lifecycle_complete = true;
            if let Some((url, replace)) = take_navigation(&mut page) {
                let _ = send_navigation(&events, url, replace);
                return;
            }
            lifecycle_submission = take_form_submit(&mut page);
            (shell, rendered, has_interaction) = render_with_observers(&mut page);
            page.last_render = Some(rendered.clone());
            #[cfg(test)]
            {
                page.last_diagnostic_render = Some(crate::js::render_canonical(&shell));
            }
            if !has_resident_work(&mut page, has_interaction) && lifecycle_submission.is_none() {
                rendered.direct_actor_nodes = false;
                let mut outcome = std::mem::take(&mut page.outcome);
                outcome.elapsed = page.started.elapsed();
                outcome.rendered = Some(Box::new(rendered));
                let _ = events.blocking_send(PageEvt::Static {
                    html: shell,
                    outcome,
                });
                return;
            }
        }

        let mut outcome = std::mem::take(&mut page.outcome);
        outcome.elapsed = page.started.elapsed();
        outcome.rendered = Some(Box::new(rendered));
        if events
            .blocking_send(PageEvt::Updated {
                html: shell,
                outcome,
            })
            .is_err()
        {
            return;
        }
        if let Some((form, submitter, submission)) = lifecycle_submission {
            let _ = events.blocking_send(PageEvt::SubmitForm {
                form,
                submitter,
                submission,
            });
            return;
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("trust Lumen page actor runtime");
        let wall_origin = Instant::now();
        let mut virtual_origin = trust_number(&mut page, "now").unwrap_or(0.0);
        let mut prefer_timer = false;
        let mut last_timer = None;
        let mut deferred_host_task = None;

        'event_loop: loop {
            if matches!(
                interrupt.current_reason(),
                Some(lumen::InterruptReason::Cancelled)
            ) {
                break;
            }
            let elapsed = wall_origin.elapsed().as_secs_f64() * 1000.0;
            let observed_now = trust_number(&mut page, "now").unwrap_or(virtual_origin + elapsed);
            let now = observed_now.max(virtual_origin + elapsed);
            virtual_origin = now - elapsed;
            let deadline = trust_number(&mut page, "nextDeadline");
            let timer_due = deadline.is_some_and(|deadline| deadline <= now)
                && last_timer.is_none_or(|last: Instant| last.elapsed() >= WAKE_FLOOR);
            let platform_ready = trust_bool(&mut page, "hasPlatformTask");
            let load_ready = !lifecycle_complete
                && pending_resources(&mut page) == 0
                && !trust_bool(&mut page, "hasInitialFramesPending");

            let mut immediate = None;
            if let Ok(command) = interactions.try_recv() {
                immediate = Some(Wake::Interaction(Some(command)));
            } else if hover.has_changed().unwrap_or(false) {
                immediate = Some(Wake::Hover(Some(*hover.borrow_and_update())));
            } else if let Some(task) = deferred_host_task
                .take()
                .or_else(|| host_rx.try_recv().ok())
            {
                if timer_due && prefer_timer {
                    deferred_host_task = Some(task);
                    immediate = Some(Wake::Timer);
                } else {
                    immediate = Some(Wake::Host(Some(task)));
                }
            } else if load_ready {
                immediate = Some(Wake::Lifecycle);
            } else if platform_ready && (!timer_due || !prefer_timer) {
                immediate = Some(Wake::Platform);
            } else if timer_due {
                immediate = Some(Wake::Timer);
            } else if let Ok(command) = cmds.try_recv() {
                immediate = Some(Wake::Cmd(Some(command)));
            }

            let wait = deadline.map(|deadline| {
                Duration::from_secs_f64(((deadline - now).max(0.0)) / 1000.0).max(WAKE_FLOOR)
            });
            let wake = immediate.unwrap_or_else(|| {
                interrupt.set_deadline(None);
                runtime.block_on(async {
                    tokio::select! {
                        biased;
                        command = interactions.recv() => Wake::Interaction(command),
                        changed = hover.changed() => Wake::Hover(changed.ok().map(|()| *hover.borrow_and_update())),
                        task = host_rx.recv() => Wake::Host(task),
                        command = cmds.recv() => Wake::Cmd(command),
                        () = sleep_or_pending(wait) => Wake::Timer,
                    }
                })
            });

            let _interaction = match &wake {
                Wake::Interaction(Some(_)) | Wake::Hover(Some(_)) => {
                    Some(InteractionTurn::begin(&interaction_running, &interrupt))
                }
                Wake::Cmd(Some(command)) if command.is_user_interaction() => {
                    Some(InteractionTurn::begin(&interaction_running, &interrupt))
                }
                _ => None,
            };

            match wake {
                Wake::Interaction(Some(command)) | Wake::Cmd(Some(command)) => {
                    if !dispatch_command(&mut page, command, &events, &interrupt) {
                        break;
                    }
                    prefer_timer = true;
                }
                Wake::Hover(Some(hover)) => {
                    if !dispatch_command(
                        &mut page,
                        PageCmd::Hover {
                            node: hover.node,
                            x: hover.x,
                            y: hover.y,
                        },
                        &events,
                        &interrupt,
                    ) {
                        break;
                    }
                    prefer_timer = true;
                }
                Wake::Interaction(None) | Wake::Cmd(None) | Wake::Hover(None) => break,
                Wake::Host(Some(task)) => {
                    // HTML permits one rendering opportunity after several selected tasks. Keep
                    // each host completion's mandatory microtask checkpoint, but coalesce a
                    // bounded burst of already-ready resource/network tasks into one paint. This
                    // prevents a ready module/style batch from exposing every intermediate
                    // script-removal frame while retaining timer/input fairness.
                    let mut next = Some(task);
                    for _ in 0..HOST_TASK_RENDER_BURST {
                        let Some(task) = next.take() else { break };
                        prepare_task(&interrupt, crate::js::WALL_BUDGET);
                        if let Err(error) = dispatch_host_task(&mut page.engine, task) {
                            page.outcome.errors.push(error);
                        }
                        checkpoint(&mut page, "host task");
                        next = host_rx.try_recv().ok();
                    }
                    // The bounded burst may have already removed one more task from the
                    // channel. Preserve it for the next event-loop turn instead of dropping
                    // the completion at the fairness boundary.
                    deferred_host_task = next;
                    if !finish_task_with_ack(&mut page, &events, false) {
                        break;
                    }
                    prefer_timer = true;
                }
                Wake::Host(None) => break,
                Wake::Platform => {
                    prepare_task(&interrupt, crate::js::WALL_BUDGET);
                    let _ = call_trust(&mut page, "runPlatformTask", &[], "platform task");
                    checkpoint(&mut page, "platform task");
                    if !finish_task_with_ack(&mut page, &events, false) {
                        break;
                    }
                    prefer_timer = true;
                }
                Wake::Timer => {
                    prepare_task(&interrupt, crate::js::WALL_BUDGET);
                    let real_now = virtual_origin + wall_origin.elapsed().as_secs_f64() * 1000.0;
                    let _ = call_trust(&mut page, "tickTo", &[Value::Num(real_now)], "timer task");
                    checkpoint(&mut page, "timer task");
                    if !finish_task_with_ack(&mut page, &events, false) {
                        break;
                    }
                    last_timer = Some(Instant::now());
                    prefer_timer = false;
                }
                Wake::Lifecycle => {
                    lifecycle_complete = true;
                    prepare_task(&interrupt, crate::js::WALL_BUDGET);
                    let _ = evaluate_task(
                        &mut page,
                        "__trust.readyState = 'complete'; __trust.fire(window, 'load', false);",
                        "load event",
                    );
                    checkpoint(&mut page, "load event");
                    // HTML §13.2.7 runs the readiness/load steps as their own task. It can
                    // produce a render, navigation, submission, or error, but `Settled` is
                    // TRust's acknowledgement for a frontend command. Exposing one for this
                    // internal lifecycle task can overtake the next click already queued by
                    // the frontend and make that click appear to have done nothing.
                    if !finish_task_with_ack(&mut page, &events, false) {
                        break 'event_loop;
                    }
                    prefer_timer = true;
                }
            }
            page.engine.collect_garbage_at_idle();
            // Deadlines bound author execution, not the host's idle scheduling
            // queries. Navigation/cancellation flags remain independently set.
            interrupt.set_deadline(None);
        }
    }

    async fn sleep_or_pending(wait: Option<Duration>) {
        match wait {
            Some(wait) => tokio::time::sleep(wait).await,
            None => std::future::pending::<()>().await,
        }
    }

    fn load_page(
        html: &str,
        env: PageEnv,
        host_tasks: tokio::sync::mpsc::UnboundedSender<LumenHostTask>,
        interrupt: Arc<lumen::RuntimeInterrupt>,
    ) -> Result<LumenPage, Outcome> {
        let mut outcome = Outcome::default();
        let viewport = crate::layout2::Viewport::new(
            f32::from(env.viewport.0) * f32::from(env.cell_px.0.max(1)),
            f32::from(env.viewport.1) * f32::from(env.cell_px.1.max(1)),
        );
        let dom = Rc::new(RefCell::new(Dom::parse_document(html)));
        {
            let mut dom = dom.borrow_mut();
            dom.set_viewport_px(viewport.width, viewport.height);
            dom.set_device_pixel_ratio(env.device_pixel_ratio);
            dom.set_doc_url(url::Url::parse(&env.url).ok());
            if !env.sheets.is_empty() {
                dom.attach_external_sheets(&env.sheets);
            }
        }
        let scripts: Vec<_> = {
            let dom = dom.borrow();
            dom.scripts()
                .into_iter()
                .filter(|(_, _, ty, node)| {
                    !(is_classic(ty) && dom.attr(*node, "nomodule").is_some())
                })
                .collect()
        };
        if scripts.is_empty() && !dom.borrow().hover_css_affects_rendering() {
            return Err(outcome);
        }

        let response_url = url::Url::parse(&env.url)
            .unwrap_or_else(|_| url::Url::parse(DEFAULT_URL).expect("default URL parses"));
        let base = {
            let dom = dom.borrow();
            dom.descendants(DOCUMENT)
                .into_iter()
                .find_map(|node| {
                    (dom.tag_name(node) == Some("base"))
                        .then(|| dom.attr(node, "href"))
                        .flatten()
                        .and_then(|href| response_url.join(href.trim()).ok())
                })
                .unwrap_or_else(|| response_url.clone())
        };
        dom.borrow_mut().set_doc_url(Some(base.clone()));

        interrupt.set_deadline(Some(Instant::now() + crate::js::WALL_BUDGET));
        let clock = Rc::new(RealmClock::new());
        let mut state = HostState::new(dom.clone(), clock.clone());
        state.base = base.clone();
        state.storage = env.storage.clone().unwrap_or_default();
        state.blobs = env.blobs.clone();
        state.viewport.set(viewport);
        state.device_pixel_ratio.set(env.device_pixel_ratio);
        // Inline and data-backed module scripts use the HTML task queue even when this document
        // has no network runtime. Keep that local task source independent of `enable_network`.
        state.task_events = Some(host_tasks.clone());
        if let Some(handle) = env.net.clone() {
            state.enable_network(response_url, handle, env.cache.clone(), host_tasks.clone());
            state.base = base.clone();
        }

        // Keep Lumen's release tiering policy for browser workloads. In particular, compiling
        // every function on its first call is useful for a hot synthetic loop but makes framework
        // startup pay native-code generation for large numbers of one-shot functions. Lumen's
        // LUMEN_TIER and LUMEN_TIER_THRESHOLD diagnostics remain available through Engine::new.
        let mut engine = lumen::Engine::new_with_interrupt(interrupt);
        let engine_clock = clock.clone();
        engine.set_wall_clock(move || engine_clock.now_ms());
        state.configure_module_loading(&mut engine);
        engine.ctx().op_state().put(state);
        install_host_boundary(&mut engine);

        // HTML NavigatorID: navigator.userAgent exposes the environment settings object's default
        // User-Agent value. Keep that identical to the HTTP client and Boa realm; the selected JS
        // implementation is not a distinct user agent or an observable browser capability.
        let config = format!(
            "globalThis.__trust_cfg = {{ url: {}, ua: 'TRust/0.1', language: {}, languages: [{}, {}], width: {}, height: {}, devicePixelRatio: {}, hardwareConcurrency: {}, globalPrivacyControl: {}, secureContext: {} }};",
            json_string(base.as_str()),
            json_string(crate::locale::LANGUAGE),
            json_string(crate::locale::LANGUAGES[0]),
            json_string(crate::locale::LANGUAGES[1]),
            viewport.width,
            viewport.height,
            env.device_pixel_ratio,
            std::thread::available_parallelism()
                .map(|parallelism| parallelism.get())
                .unwrap_or(8),
            crate::http::GLOBAL_PRIVACY_CONTROL,
            lumen_potentially_trustworthy(&base),
        );
        if let Err(error) = eval(&mut engine, &config, "TRust configuration") {
            outcome.errors.push(error);
            return Err(outcome);
        }
        if let Err(error) = eval(&mut engine, crate::js::PRELUDE, "TRust platform prelude") {
            outcome.errors.push(error);
            return Err(outcome);
        }

        let started = Instant::now();
        let trace = std::env::var_os("TRUST_LUMEN_TRACE").is_some();
        for (index, (src, inline, ty, node)) in scripts.into_iter().enumerate() {
            let script_started = Instant::now();
            if is_classic(&ty) {
                let source = initial_classic_source(src.as_deref(), &inline, &env);
                let Some((name, source, external)) = source else {
                    // HTML §4.12.1.1 executes a null script result by firing `error` at the
                    // element and returning. A fetch/MIME/status rejection is not an uncaught
                    // JavaScript exception and therefore does not belong in the page error tally.
                    fire_engine_script_event(&mut engine, node, "error");
                    continue;
                };
                if trace {
                    eprintln!("lumen: script[{index}] start classic {name}");
                }
                if let Err(error) = run_injected_classic_task(&mut engine, node, &name, &source) {
                    outcome.errors.push(error);
                } else if external {
                    fire_engine_script_event(&mut engine, node, "load");
                }
                if trace {
                    eprintln!(
                        "lumen: script[{index}] done +{}ms",
                        script_started.elapsed().as_millis()
                    );
                }
            } else if ty.as_deref().is_some_and(|ty| ty.trim() == "module") {
                let external = src.is_some();
                let source = initial_module_source(src.as_deref(), &inline, &env, &base);
                let Some((mut name, source)) = source else {
                    outcome.modules_skipped += 1;
                    fire_engine_script_event(&mut engine, node, "error");
                    continue;
                };
                if !external {
                    name = format!("inline-module#{}", index + 1);
                }
                let import_base = url::Url::parse(&name).unwrap_or_else(|_| base.clone());
                speculate_engine_imports(&mut engine, &import_base, source.as_bytes());
                if trace {
                    eprintln!("lumen: script[{index}] start module {name}");
                }
                if let Err(error) = run_injected_module_task(&mut engine, node, &name, &source) {
                    outcome.errors.push(error);
                }
                if trace {
                    eprintln!(
                        "lumen: script[{index}] done +{}ms",
                        script_started.elapsed().as_millis()
                    );
                }
            }
        }
        let mut page = LumenPage {
            engine,
            dom,
            base,
            outcome,
            started,
            last_render: None,
            #[cfg(test)]
            last_diagnostic_render: None,
            live_regions: HashSet::new(),
            live_boundaries: HashSet::new(),
            boundary_render: HashMap::new(),
            render_environment_dirty: false,
        };
        let _ = evaluate_task(
            &mut page,
            "__trust.readyState = 'interactive'; __trust.queueInitialFrameNavigations(); __trust.fire(document, 'DOMContentLoaded', true);",
            "DOMContentLoaded",
        );
        checkpoint(&mut page, "DOMContentLoaded");
        Ok(page)
    }

    fn is_classic(type_attr: &Option<String>) -> bool {
        match type_attr {
            None => true,
            Some(value) => matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "" | "text/javascript" | "application/javascript" | "text/ecmascript"
            ),
        }
    }

    fn initial_classic_source(
        src: Option<&str>,
        inline: &str,
        env: &PageEnv,
    ) -> Option<(String, String, bool)> {
        let Some(src) = src else {
            return Some((String::from("inline script"), inline.to_string(), false));
        };
        if src.starts_with("data:") {
            let body = crate::img::decode_data_url(src)?;
            return Some((
                src.to_string(),
                String::from_utf8_lossy(&body).into_owned(),
                true,
            ));
        }
        let body = env
            .externals
            .iter()
            .find(|(name, _)| name == src)?
            .1
            .as_ref()?;
        Some((
            src.to_string(),
            String::from_utf8_lossy(body).into_owned(),
            true,
        ))
    }

    fn initial_module_source(
        src: Option<&str>,
        inline: &str,
        env: &PageEnv,
        base: &url::Url,
    ) -> Option<(String, String)> {
        let Some(src) = src else {
            return Some((base.to_string(), inline.to_string()));
        };
        let resolved = base.join(src).ok()?;
        if resolved.scheme() == "data" {
            let content_type = data_url_content_type(resolved.as_str());
            let body = crate::img::decode_data_url(resolved.as_str())?;
            return crate::http::module_script_response_allowed(200, &content_type).then(|| {
                (
                    resolved.to_string(),
                    crate::http::decode_body(&content_type, &body),
                )
            });
        }
        let handle = env.net.as_ref()?;
        let fetch = env
            .cache
            .peek(&resolved)
            .unwrap_or_else(|| env.cache.fetch(handle, resolved.clone()));
        let response = crate::http::PageCache::block_on_fetch(Some(handle), fetch)?;
        crate::http::module_script_response_allowed(response.status, &response.content_type).then(
            || {
                (
                    resolved.to_string(),
                    crate::http::decode_body(&response.content_type, &response.body),
                )
            },
        )
    }

    fn json_string(value: &str) -> String {
        serde_json::to_string(value).unwrap_or_else(|_| String::from("\"\""))
    }

    fn pending_resources(page: &mut LumenPage) -> usize {
        page.engine
            .ctx()
            .host_mut::<HostState>()
            .map_or(0, |state| state.pending_resources)
    }

    fn has_resident_work(page: &mut LumenPage, has_interaction: bool) -> bool {
        if has_interaction
            || page.dom.borrow().hover_css_affects_rendering()
            || !page.dom.borrow().hover_hosts_is_empty()
        {
            return true;
        }
        let host_work = page
            .engine
            .ctx()
            .host_mut::<HostState>()
            .is_some_and(|state| {
                state.pending_resources > 0
                    || state
                        .pending_dynamic_modules
                        .load(std::sync::atomic::Ordering::Relaxed)
                        > 0
                    || state
                        .network
                        .as_ref()
                        .is_some_and(|network| !network.pending_fetches.is_empty())
                    || state
                        .websockets
                        .as_ref()
                        .is_some_and(|sockets| !sockets.sockets.is_empty())
                    || state
                        .workers
                        .as_ref()
                        .is_some_and(|workers| !workers.workers.is_empty())
            });
        host_work
            || trust_number(page, "nextDeadline").is_some()
            || trust_bool(page, "hasPlatformTask")
            || trust_bool(page, "hasScrollWork")
            || trust_bool(page, "hasInitialFramesPending")
    }

    fn prepare_task(interrupt: &Arc<lumen::RuntimeInterrupt>, budget: Duration) {
        interrupt.set_deadline(Some(Instant::now() + budget));
    }

    fn evaluate_task(page: &mut LumenPage, source: &str, label: &str) -> Option<Value> {
        let evaluated = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            page.engine.eval_value_interruptible(source)
        }));
        match evaluated {
            Ok(Ok(Ok(value))) => Some(value),
            Ok(Ok(Err(error))) => {
                record_eval_error(page, error, label);
                None
            }
            Ok(Err(error)) => {
                page.outcome.errors.push(format!(
                    "{label} parse error at line {}: {}",
                    error.line, error.message
                ));
                None
            }
            Err(_) => {
                page.outcome
                    .errors
                    .push(format!("{label}: Lumen engine panic — page JS halted"));
                page.outcome.panicked = true;
                None
            }
        }
    }

    fn engine_call_trust(
        engine: &mut lumen::Engine,
        name: &str,
        args: &[Value],
    ) -> Result<Value, EvalError> {
        let global = engine.global_this();
        let trust = engine
            .ctx()
            .member_get(&global, "__trust")
            .map_err(EvalError::Throw)?;
        let function = engine
            .ctx()
            .member_get(&trust, name)
            .map_err(EvalError::Throw)?;
        engine.call_function_interruptible(&function, trust, args)
    }

    fn call_trust(page: &mut LumenPage, name: &str, args: &[Value], label: &str) -> Option<Value> {
        let called = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            engine_call_trust(&mut page.engine, name, args)
        }));
        match called {
            Ok(Ok(value)) => Some(value),
            Ok(Err(error)) => {
                record_eval_error(page, error, label);
                None
            }
            Err(_) => {
                page.outcome
                    .errors
                    .push(format!("{label}: Lumen engine panic — page JS halted"));
                page.outcome.panicked = true;
                None
            }
        }
    }

    fn record_eval_error(page: &mut LumenPage, error: EvalError, label: &str) {
        match error {
            EvalError::Interrupted(lumen::InterruptReason::UserNavigation) => {}
            EvalError::Interrupted(lumen::InterruptReason::Cancelled) => {}
            EvalError::Interrupted(reason) => page
                .outcome
                .errors
                .push(format!("{label} interrupted: {}", reason.message())),
            EvalError::Throw(error) => {
                let message = describe_throw(&mut page.engine, error, label);
                page.outcome.errors.push(message);
            }
        }
    }

    fn checkpoint(page: &mut LumenPage, label: &str) {
        if let Err(reason) = page.engine.run_microtasks_interruptible()
            && !matches!(
                reason,
                lumen::InterruptReason::UserNavigation | lumen::InterruptReason::Cancelled
            )
        {
            page.outcome.errors.push(format!(
                "{label} microtasks interrupted: {}",
                reason.message()
            ));
        }
        drain_diagnostics(page);
    }

    fn drain_diagnostics(page: &mut LumenPage) {
        let error_start = page.outcome.errors.len();
        let console_start = page.outcome.console.len();
        for (source, errors) in [
            ("__trust.errors.splice(0).join('\\u0000')", true),
            ("__trust.logs.splice(0).join('\\u0000')", false),
        ] {
            let Ok(Ok(value)) = page.engine.eval_value_interruptible(source) else {
                continue;
            };
            let joined = value_string(&mut page.engine, &value);
            let destination = if errors {
                &mut page.outcome.errors
            } else {
                &mut page.outcome.console
            };
            destination.extend(
                joined
                    .split('\0')
                    .filter(|entry| !entry.is_empty())
                    .map(String::from),
            );
        }
        let rejections = page.engine.take_unhandled_rejections();
        for rejection in rejections {
            let message = value_string(&mut page.engine, &rejection);
            page.outcome
                .console
                .push(format!("unhandled rejection: {message}"));
        }
        if std::env::var_os("TRUST_LUMEN_TRACE").is_some() {
            for error in &page.outcome.errors[error_start..] {
                eprintln!("lumen: {error}");
            }
            for message in &page.outcome.console[console_start..] {
                eprintln!("lumen: console: {message}");
            }
        }
        if let Ok(source) = std::env::var("TRUST_LUMEN_PROBE") {
            match page.engine.eval_value_interruptible(&source) {
                Ok(Ok(value)) => {
                    eprintln!("lumen: probe: {}", value_string(&mut page.engine, &value))
                }
                Ok(Err(EvalError::Throw(error))) => {
                    eprintln!(
                        "lumen: probe threw: {}",
                        describe_throw(&mut page.engine, error, "probe")
                    );
                }
                Ok(Err(EvalError::Interrupted(reason))) => {
                    eprintln!("lumen: probe interrupted: {}", reason.message());
                }
                Err(error) => eprintln!(
                    "lumen: probe parse error at line {}: {}",
                    error.line, error.message
                ),
            }
        }
        page.outcome.fetches = page
            .engine
            .ctx()
            .host_mut::<HostState>()
            .and_then(|state| state.network.as_ref())
            .map_or(0, |network| {
                network.fetched.load(std::sync::atomic::Ordering::Relaxed)
            });
    }

    fn trust_number(page: &mut LumenPage, name: &str) -> Option<f64> {
        call_trust(page, name, &[], name).and_then(|value| value.as_num_opt())
    }

    fn trust_bool(page: &mut LumenPage, name: &str) -> bool {
        call_trust(page, name, &[], name).is_some_and(|value| page.engine.ctx().to_boolean(&value))
    }

    fn listener_ids(page: &mut LumenPage, name: &str) -> HashSet<usize> {
        let Some(value) = call_trust(page, name, &[], name) else {
            return HashSet::new();
        };
        let Ok(joined) = page.engine.ctx().coerce_string(&value) else {
            return HashSet::new();
        };
        joined
            .split(',')
            .filter_map(|part| part.parse().ok())
            .collect()
    }

    fn extract_live(page: &mut LumenPage) -> (String, crate::http::RenderedPage, bool) {
        prime_page_svg_sprites(page);
        let clickable_listeners = listener_ids(page, "clickables");
        let hover_listeners = listener_ids(page, "hoverables");
        let (clickable, has_interaction) = {
            let dom = page.dom.borrow();
            crate::js::clickable_set_for_dom(&dom, &clickable_listeners)
        };
        let (hover, complete_hover_hits) = {
            let dom = page.dom.borrow();
            crate::js::hover_set_for_dom(&dom, &hover_listeners)
        };
        let paint = page
            .dom
            .borrow()
            .hover_paint_subject_candidates_in(&[DOCUMENT]);
        {
            let mut dom = page.dom.borrow_mut();
            dom.set_hover_hosts(hover, complete_hover_hits);
            dom.set_paint_patch_hosts(paint.into_iter().collect());
            dom.set_render_clickables(clickable.clone(), true);
        }
        let (viewport, ratio, images) = page
            .engine
            .ctx()
            .host_mut::<HostState>()
            .map(|state| {
                (
                    state.viewport.get(),
                    state.device_pixel_ratio.get(),
                    state.images.borrow().clone(),
                )
            })
            .unwrap_or((DEFAULT_VIEWPORT, 1.0, Default::default()));
        let rendered = {
            let dom = page.dom.borrow();
            crate::http::render_arena(&dom, &page.base, viewport, ratio, None, &images)
        };
        let html = if cfg!(test) || std::env::var_os("TRUST_DUMP_RAW").is_some() {
            page.dom.borrow().serialize_live(DOCUMENT, &clickable)
        } else {
            String::new()
        };
        {
            let mut dom = page.dom.borrow_mut();
            let _ = dom.take_dirty();
            let _ = dom.take_dirty_targets();
        }
        (html, rendered, has_interaction)
    }

    /// Run the observer portions of HTML's "update the rendering" algorithm around layout.
    /// ResizeObserver callbacks precede IntersectionObserver callbacks, and either callback may
    /// alter geometry that requires another bounded style/layout/observer pass before painting.
    fn render_with_observers(page: &mut LumenPage) -> (String, crate::http::RenderedPage, bool) {
        let mut rendered = extract_live(page);
        for _ in 0..6 {
            let resized = trust_number(page, "updateResizes").unwrap_or(0.0);
            let intersected = trust_number(page, "updateIntersections").unwrap_or(0.0);
            checkpoint(page, "rendering observers");
            if resized + intersected <= 0.0 || page.outcome.panicked {
                break;
            }
            rendered = extract_live(page);
        }
        rendered
    }

    /// SVG 2 §5.6: a same-origin external `<use href="sheet.svg#symbol">` obtains the external
    /// resource document before the use-element shadow tree can be rendered. Keep the resource
    /// cache and request cap identical to the other Lumen subresource paths.
    fn prime_page_svg_sprites(page: &mut LumenPage) {
        let urls = page.dom.borrow().external_svg_use_sheets(&page.base);
        for url in urls {
            if crate::dom::sprite_sheet_cached(url.as_str()) {
                continue;
            }
            let prepared = page.engine.ctx().host_mut::<HostState>().and_then(|state| {
                let network = state.network.as_ref()?;
                if !matches!(url.scheme(), "http" | "https")
                    || !crate::http::subresource_allowed(&state.base, &url)
                {
                    return None;
                }
                let shared = if let Some(shared) = network.cache.peek(&url) {
                    shared
                } else {
                    if network
                        .fetched
                        .fetch_update(
                            std::sync::atomic::Ordering::Relaxed,
                            std::sync::atomic::Ordering::Relaxed,
                            |count| (count < crate::js::MAX_PAGE_FETCHES).then_some(count + 1),
                        )
                        .is_err()
                    {
                        return None;
                    }
                    network.cache.fetch(&network.handle, url.clone())
                };
                Some((network.handle.clone(), shared))
            });
            let Some((handle, shared)) = prepared else {
                continue;
            };
            let Some(response) = crate::http::PageCache::block_on_fetch(Some(&handle), shared)
            else {
                continue;
            };
            if (200..300).contains(&response.status) {
                let text = crate::http::decode_body(&response.content_type, &response.body);
                crate::dom::prime_sprite_sheet(url.as_str(), &text);
            }
        }
    }

    fn value_is_nullish(value: &Value) -> bool {
        matches!(value, Value::Null | Value::Undefined | Value::Empty)
    }

    fn value_to_string(page: &mut LumenPage, value: &Value) -> Option<String> {
        page.engine
            .ctx()
            .coerce_string(value)
            .ok()
            .map(|value| value.to_string())
    }

    // Command dispatch and event-tail handling are kept below the shared
    // extraction helpers so every task follows the same task → checkpoint →
    // rendering-update sequence from HTML §8.1.7.3.

    fn dispatch_command(
        page: &mut LumenPage,
        command: PageCmd,
        events: &tokio::sync::mpsc::Sender<PageEvt>,
        interrupt: &Arc<lumen::RuntimeInterrupt>,
    ) -> bool {
        match command {
            PageCmd::Click(node) => {
                prepare_interaction(page, interrupt);
                let prevented = call_trust(page, "click", &[Value::Num(node as f64)], "click")
                    .is_some_and(|value| page.engine.ctx().to_boolean(&value));
                let anchor = if prevented {
                    None
                } else {
                    call_trust(
                        page,
                        "followAnchorDefault",
                        &[Value::Num(node as f64)],
                        "hyperlink navigation",
                    )
                    .filter(|value| !value_is_nullish(value))
                    .and_then(|value| value_to_string(page, &value))
                    .filter(|value| !value.trim().is_empty())
                };
                checkpoint(page, "click");
                if let Some((url, replace)) =
                    take_navigation(page).or_else(|| anchor.map(|url| (url, false)))
                {
                    return send_navigation(events, url, replace);
                }
                let click_submit = take_click_submit(page);
                if !finish_task_with_ack(page, events, click_submit.is_none()) {
                    return false;
                }
                if let Some((form, submitter, submission)) = click_submit {
                    return events
                        .blocking_send(PageEvt::SubmitForm {
                            form,
                            submitter: Some(submitter),
                            submission,
                        })
                        .is_ok();
                }
                true
            }
            PageCmd::Key { node, input } => {
                prepare_interaction(page, interrupt);
                let (key, code) = key_and_code(&input.key);
                let prevented = call_trust(
                    page,
                    "key",
                    &[
                        Value::Num(node as f64),
                        Value::from_string(key),
                        Value::from_string(code),
                        Value::Bool(input.repeat),
                        Value::Bool(input.composing),
                        Value::Bool(input.modifiers.shift),
                        Value::Bool(input.modifiers.control),
                        Value::Bool(input.modifiers.alt),
                        Value::Bool(input.modifiers.meta),
                    ],
                    "keydown",
                )
                .is_some_and(|value| page.engine.ctx().to_boolean(&value));
                checkpoint(page, "keydown");
                if let Some((url, replace)) = take_navigation(page) {
                    return send_navigation(events, url, replace);
                }
                let click_submit = take_click_submit(page);
                if !finish_task_with_ack(page, events, click_submit.is_none()) {
                    return false;
                }
                if let Some((form, submitter, submission)) = click_submit {
                    return events
                        .blocking_send(PageEvt::SubmitForm {
                            form,
                            submitter: Some(submitter),
                            submission,
                        })
                        .is_ok();
                }
                events
                    .blocking_send(PageEvt::KeyDefault { prevented })
                    .is_ok()
            }
            PageCmd::SetValue {
                node,
                value,
                checked,
            } => {
                prepare_interaction(page, interrupt);
                let checked = checked.map_or(Value::Null, Value::Bool);
                let _ = call_trust(
                    page,
                    "formSet",
                    &[Value::Num(node as f64), Value::from_string(value), checked],
                    "form input",
                );
                checkpoint(page, "form input");
                finish_task(page, events)
            }
            PageCmd::Submit { form, submitter } => {
                prepare_interaction(page, interrupt);
                let prevented = call_trust(
                    page,
                    "formSubmit",
                    &[
                        Value::Num(form as f64),
                        submitter.map_or(Value::Null, |node| Value::Num(node as f64)),
                    ],
                    "form submit",
                )
                .is_some_and(|value| page.engine.ctx().to_boolean(&value));
                checkpoint(page, "form submit");
                if let Some((url, replace)) = take_navigation(page) {
                    return send_navigation(events, url, replace);
                }
                if !prevented {
                    return events.blocking_send(PageEvt::SubmitDefault).is_ok();
                }
                finish_task(page, events)
            }
            PageCmd::Ws { id, event } => {
                prepare_task(interrupt, crate::js::WALL_BUDGET);
                let can_render = !matches!(&event, crate::ws::WsIn::Sent(_));
                if let Err(error) =
                    dispatch_host_task(&mut page.engine, LumenHostTask::WebSocket { id, event })
                {
                    page.outcome.errors.push(error);
                }
                checkpoint(page, "WebSocket task");
                !can_render || finish_task(page, events)
            }
            PageCmd::Worker { id, event } => {
                prepare_task(interrupt, crate::js::WALL_BUDGET);
                if let Err(error) =
                    dispatch_host_task(&mut page.engine, LumenHostTask::Worker { id, event })
                {
                    page.outcome.errors.push(error);
                }
                checkpoint(page, "Worker task");
                finish_task(page, events)
            }
            PageCmd::Scroll { x, y } => {
                prepare_interaction(page, interrupt);
                let _ = call_trust(
                    page,
                    "setScroll",
                    &[Value::Num(finite_or_zero(x)), Value::Num(finite_or_zero(y))],
                    "scroll",
                );
                checkpoint(page, "scroll");
                finish_task(page, events)
            }
            PageCmd::Hover { node, x, y } => {
                prepare_interaction(page, interrupt);
                let node = node
                    .filter(|node| page.dom.borrow().is_valid(*node))
                    .map_or(Value::Null, |node| Value::Num(node as f64));
                let _ = call_trust(
                    page,
                    "hover",
                    &[
                        node,
                        Value::Num(finite_or_zero(x)),
                        Value::Num(finite_or_zero(y)),
                    ],
                    "hover",
                );
                checkpoint(page, "hover");
                finish_task(page, events)
            }
            PageCmd::RegionGeom { items } => {
                let mut dom = page.dom.borrow_mut();
                for (node, client_height, client_width) in items {
                    if dom.is_valid(node) {
                        dom.set_scroll_geom(node, client_height, client_width);
                    }
                }
                true
            }
            PageCmd::SetScroll { node, top, left } => {
                prepare_interaction(page, interrupt);
                if page.dom.borrow().is_valid(node) {
                    page.dom.borrow_mut().set_scroll_pos(
                        node,
                        finite_or_zero(top),
                        finite_or_zero(left),
                        false,
                    );
                    let _ = call_trust(
                        page,
                        "fireElementScroll",
                        &[Value::Num(node as f64)],
                        "element scroll",
                    );
                    checkpoint(page, "element scroll");
                    finish_task(page, events)
                } else {
                    events.blocking_send(PageEvt::Settled).is_ok()
                }
            }
            PageCmd::Resync => {
                let (html, rendered, _) = extract_live(page);
                page.render_environment_dirty = false;
                page.last_render = Some(rendered.clone());
                #[cfg(test)]
                {
                    page.last_diagnostic_render = Some(crate::js::render_canonical(&html));
                }
                page.boundary_render.clear();
                let mut outcome = std::mem::take(&mut page.outcome);
                outcome.elapsed = page.started.elapsed();
                outcome.rendered = Some(Box::new(rendered));
                events
                    .blocking_send(PageEvt::Updated { html, outcome })
                    .is_ok()
            }
            PageCmd::LiveRegions(nodes) => {
                page.live_regions = nodes.into_iter().collect();
                true
            }
            PageCmd::LiveBoundaries(nodes) => {
                page.live_boundaries = nodes.into_iter().collect();
                true
            }
            PageCmd::ImageSizes(sizes) => {
                let mut changed = false;
                if let Some(state) = page.engine.ctx().host_mut::<HostState>() {
                    let mut images = state.images.borrow_mut();
                    for (url, dimensions) in sizes {
                        if images.get(&url) != Some(&dimensions) {
                            images.insert(url, dimensions);
                            changed = true;
                        }
                    }
                    if changed {
                        state.geom_cache.borrow_mut().0 = u64::MAX;
                    }
                }
                if changed {
                    page.render_environment_dirty = true;
                    prepare_task(interrupt, crate::js::WALL_BUDGET);
                    let _ = call_trust(page, "updateIntersections", &[], "image geometry");
                    checkpoint(page, "image geometry");
                    finish_task(page, events)
                } else {
                    true
                }
            }
            PageCmd::Viewport(viewport) => {
                let viewport = crate::layout2::Viewport::new(viewport.width, viewport.height);
                let changed = page
                    .engine
                    .ctx()
                    .host_mut::<HostState>()
                    .is_some_and(|state| {
                        if state.viewport.get() == viewport {
                            return false;
                        }
                        state.viewport.set(viewport);
                        state.geom_cache.borrow_mut().0 = u64::MAX;
                        true
                    });
                page.dom
                    .borrow_mut()
                    .set_viewport_px(viewport.width, viewport.height);
                if changed {
                    page.render_environment_dirty = true;
                    prepare_task(interrupt, crate::js::WALL_BUDGET);
                    let _ = call_trust(
                        page,
                        "setViewport",
                        &[
                            Value::Num(f64::from(viewport.width)),
                            Value::Num(f64::from(viewport.height)),
                        ],
                        "resize",
                    );
                    checkpoint(page, "resize");
                    finish_task(page, events)
                } else {
                    true
                }
            }
            PageCmd::DevicePixelRatio(ratio) => {
                let ratio = if ratio.is_finite() && ratio > 0.0 {
                    ratio
                } else {
                    1.0
                };
                let changed = page
                    .engine
                    .ctx()
                    .host_mut::<HostState>()
                    .is_some_and(|state| {
                        if state.device_pixel_ratio.get() == ratio {
                            return false;
                        }
                        state.device_pixel_ratio.set(ratio);
                        state.geom_cache.borrow_mut().0 = u64::MAX;
                        true
                    });
                page.dom.borrow_mut().set_device_pixel_ratio(ratio);
                if changed {
                    page.render_environment_dirty = true;
                    prepare_task(interrupt, crate::js::WALL_BUDGET);
                    let _ = evaluate_task(
                        page,
                        &format!("globalThis.devicePixelRatio={ratio}"),
                        "devicePixelRatio",
                    );
                    checkpoint(page, "devicePixelRatio");
                    finish_task(page, events)
                } else {
                    true
                }
            }
        }
    }

    fn prepare_interaction(page: &mut LumenPage, interrupt: &Arc<lumen::RuntimeInterrupt>) {
        prepare_task(interrupt, USER_TASK_BUDGET);
        let _ = call_trust(page, "moResetGuard", &[], "mutation observer guard");
        let _ = page.dom.borrow_mut().take_dirty();
    }

    fn finite_or_zero(value: f64) -> f64 {
        if value.is_finite() { value } else { 0.0 }
    }

    fn key_and_code(key: &crate::core::Key) -> (String, String) {
        use crate::core::Key;
        let key_name = match key {
            Key::Character(value) | Key::Other(value) => value.clone(),
            Key::Enter => String::from("Enter"),
            Key::Escape => String::from("Escape"),
            Key::Backspace => String::from("Backspace"),
            Key::Delete => String::from("Delete"),
            Key::Tab => String::from("Tab"),
            Key::ArrowLeft => String::from("ArrowLeft"),
            Key::ArrowRight => String::from("ArrowRight"),
            Key::ArrowUp => String::from("ArrowUp"),
            Key::ArrowDown => String::from("ArrowDown"),
            Key::Home => String::from("Home"),
            Key::End => String::from("End"),
            Key::PageUp => String::from("PageUp"),
            Key::PageDown => String::from("PageDown"),
        };
        let code = match key {
            Key::Character(value) if value.len() == 1 => {
                format!("Key{}", value.to_ascii_uppercase())
            }
            Key::Character(_) => String::new(),
            _ => key_name.clone(),
        };
        (key_name, code)
    }

    #[cfg(test)]
    enum BoundaryPatchResult {
        FullRender,
        Unchanged,
        Sent(bool),
    }

    /// Preserve the actor's retained-boundary protocol while the production frontends consume
    /// complete typed layouts. This follows the same conservative rule as the Boa actor: every
    /// concrete dirty target must fit a confirmed patchable boundary, otherwise full rendering is
    /// the always-correct fallback.
    #[cfg(test)]
    fn emit_boundary_patch(
        page: &mut LumenPage,
        events: &tokio::sync::mpsc::Sender<PageEvt>,
    ) -> BoundaryPatchResult {
        let Some(mut targets) = page.dom.borrow_mut().take_dirty_targets() else {
            return BoundaryPatchResult::FullRender;
        };
        {
            let dom = page.dom.borrow();
            targets.retain(|(node, kind)| {
                dom.is_connected(*node)
                    && dom.dirty_target_can_render(*node, *kind)
                    && (*kind != crate::dom::DirtyKind::Attr || !dom.inert_positioned_attr(*node))
            });
        }
        if targets.is_empty() {
            return BoundaryPatchResult::Unchanged;
        }
        let Some(boundaries) = ({
            let dom = page.dom.borrow();
            crate::js::confined_boundaries(
                &dom,
                &page.live_regions,
                &page.live_boundaries,
                Some(&targets),
            )
        }) else {
            return BoundaryPatchResult::FullRender;
        };

        let clickable_listeners = listener_ids(page, "clickables");
        let hover_listeners = listener_ids(page, "hoverables");
        let boundary_nodes: Vec<usize> = boundaries.iter().map(|(node, _)| *node).collect();
        let clickable = {
            let dom = page.dom.borrow();
            crate::js::clickable_set_for_dom(&dom, &clickable_listeners).0
        };
        let (hover, complete_hover_hits) = {
            let dom = page.dom.borrow();
            crate::js::hover_set_for_dom(&dom, &hover_listeners)
        };
        let paint = page
            .dom
            .borrow()
            .hover_paint_subject_candidates_in(&boundary_nodes);
        {
            let mut dom = page.dom.borrow_mut();
            dom.set_hover_hosts(hover, complete_hover_hits);
            dom.extend_paint_patch_hosts(paint);
            dom.set_render_clickables(clickable.clone(), true);
        }

        let mut patches = Vec::new();
        {
            let dom = page.dom.borrow();
            for (node, tier) in boundaries {
                let html = dom.serialize_patch(node, &clickable);
                let canonical = crate::js::render_canonical(&html);
                if page.boundary_render.get(&node).map(String::as_str) == Some(canonical.as_str()) {
                    continue;
                }
                page.boundary_render.insert(node, canonical);
                patches.push(crate::js::SubtreePatch { node, html, tier });
            }
            page.boundary_render
                .retain(|node, _| dom.is_connected(*node));
        }
        if patches.is_empty() {
            return BoundaryPatchResult::Unchanged;
        }

        let (_, rendered, _) = extract_live(page);
        page.last_render = Some(rendered.clone());
        let mut outcome = std::mem::take(&mut page.outcome);
        outcome.elapsed = page.started.elapsed();
        outcome.rendered = Some(Box::new(rendered));
        BoundaryPatchResult::Sent(
            events
                .blocking_send(PageEvt::Patched { patches, outcome })
                .is_ok(),
        )
    }

    fn finish_task(page: &mut LumenPage, events: &tokio::sync::mpsc::Sender<PageEvt>) -> bool {
        finish_task_with_ack(page, events, true)
    }

    fn finish_task_with_ack(
        page: &mut LumenPage,
        events: &tokio::sync::mpsc::Sender<PageEvt>,
        acknowledge_settle: bool,
    ) -> bool {
        if page.outcome.panicked {
            let errors = std::mem::take(&mut page.outcome.errors);
            let _ = events.blocking_send(PageEvt::Trouble(errors));
            return false;
        }
        if let Some((url, replace)) = take_navigation(page) {
            return send_navigation(events, url, replace);
        }
        let fragment = take_scroll_fragment(page);
        let submission = take_form_submit(page);
        let scrolls = page.dom.borrow_mut().take_scroll_changes();
        let mut sent_primary = false;
        let dom_dirty = page.dom.borrow_mut().take_dirty();
        let environment_dirty = std::mem::take(&mut page.render_environment_dirty);
        #[cfg(test)]
        let mut render_handled = false;
        #[cfg(not(test))]
        let render_handled = false;
        #[cfg(test)]
        if dom_dirty && !environment_dirty {
            match emit_boundary_patch(page, events) {
                BoundaryPatchResult::FullRender => {}
                BoundaryPatchResult::Unchanged => render_handled = true,
                BoundaryPatchResult::Sent(ok) => {
                    if !ok {
                        return false;
                    }
                    sent_primary = true;
                    render_handled = true;
                }
            }
        }
        if (dom_dirty || environment_dirty) && !render_handled {
            let (html, rendered, _) = render_with_observers(page);
            let presentation_changed = page
                .last_render
                .as_ref()
                .is_none_or(|previous| !previous.visually_eq(&rendered));
            #[cfg(test)]
            let diagnostic_changed = page
                .last_diagnostic_render
                .as_deref()
                .is_none_or(|previous| previous != crate::js::render_canonical(&html));
            #[cfg(not(test))]
            let diagnostic_changed = false;
            let changed = presentation_changed || diagnostic_changed;
            if changed {
                page.last_render = Some(rendered.clone());
                #[cfg(test)]
                {
                    page.last_diagnostic_render = Some(crate::js::render_canonical(&html));
                }
                page.boundary_render.clear();
                let mut outcome = std::mem::take(&mut page.outcome);
                outcome.elapsed = page.started.elapsed();
                outcome.rendered = Some(Box::new(rendered));
                if events
                    .blocking_send(PageEvt::Updated { html, outcome })
                    .is_err()
                {
                    return false;
                }
                sent_primary = true;
            }
        }
        if !sent_primary && !page.outcome.errors.is_empty() {
            let errors = std::mem::take(&mut page.outcome.errors);
            if events.blocking_send(PageEvt::Trouble(errors)).is_err() {
                return false;
            }
            sent_primary = true;
        }
        for (node, top, left) in scrolls {
            if events
                .blocking_send(PageEvt::Scrolled { node, top, left })
                .is_err()
            {
                return false;
            }
            sent_primary = true;
        }
        if let Some(fragment) = fragment {
            if events
                .blocking_send(PageEvt::ScrollToFragment(fragment))
                .is_err()
            {
                return false;
            }
            sent_primary = true;
        }
        if let Some((form, submitter, submission)) = submission {
            return events
                .blocking_send(PageEvt::SubmitForm {
                    form,
                    submitter,
                    submission,
                })
                .is_ok();
        }
        sent_primary || !acknowledge_settle || events.blocking_send(PageEvt::Settled).is_ok()
    }

    fn take_navigation(page: &mut LumenPage) -> Option<(String, bool)> {
        let replace = call_trust(page, "navigationReplaces", &[], "navigation")
            .is_some_and(|value| page.engine.ctx().to_boolean(&value));
        let value = call_trust(page, "takeNavigation", &[], "navigation")?;
        if value_is_nullish(&value) {
            return None;
        }
        let url = value_to_string(page, &value)?;
        (!url.trim().is_empty()).then(|| (url.trim().to_string(), replace))
    }

    fn send_navigation(
        events: &tokio::sync::mpsc::Sender<PageEvt>,
        url: String,
        replace: bool,
    ) -> bool {
        let event = if replace {
            PageEvt::Replace(url)
        } else {
            PageEvt::Navigate(url)
        };
        events.blocking_send(event).is_ok()
    }

    fn take_scroll_fragment(page: &mut LumenPage) -> Option<String> {
        let value = call_trust(page, "takeScrollFragment", &[], "fragment navigation")?;
        (!value_is_nullish(&value))
            .then(|| value_to_string(page, &value))
            .flatten()
    }

    fn take_click_submit(page: &mut LumenPage) -> Option<(usize, usize, Option<FormSubmission>)> {
        let value = evaluate_task(
            page,
            "(function(){var s=__trust.lastClickSubmit;__trust.lastClickSubmit=null;return (s && !s.prevented) ? (s.form + ',' + s.submitter) : '';})()",
            "click submission",
        )?;
        let value = value_to_string(page, &value)?;
        let (form, submitter) = value.split_once(',')?;
        let form = form.trim().parse().ok()?;
        let submitter = submitter.trim().parse().ok()?;
        if form_method_is_dialog(page, form, submitter) {
            return None;
        }
        let submission = form_submission(page, form, Some(submitter));
        Some((form, submitter, submission))
    }

    fn take_form_submit(
        page: &mut LumenPage,
    ) -> Option<(usize, Option<usize>, Option<FormSubmission>)> {
        let value = call_trust(page, "takeFormSubmit", &[], "form submission")?;
        let value = value_to_string(page, &value)?;
        let (form, submitter) = value.split_once(',')?;
        let form = form.trim().parse().ok()?;
        let submitter = (!submitter.trim().is_empty())
            .then(|| submitter.trim().parse().ok())
            .flatten();
        let submission = form_submission(page, form, submitter);
        Some((form, submitter, submission))
    }

    fn form_submission(
        page: &mut LumenPage,
        form: usize,
        submitter: Option<usize>,
    ) -> Option<FormSubmission> {
        let value = call_trust(
            page,
            "formSubmission",
            &[
                Value::Num(form as f64),
                submitter.map_or(Value::Null, |node| Value::Num(node as f64)),
            ],
            "form entry list",
        )?;
        let json = value_to_string(page, &value)?;
        let value: serde_json::Value = serde_json::from_str(&json).ok()?;
        Some(FormSubmission {
            action: value.get("action")?.as_str()?.to_string(),
            method: value.get("method")?.as_str()?.to_string(),
            body: value.get("body")?.as_str()?.to_string(),
        })
    }

    fn form_method_is_dialog(page: &LumenPage, form: usize, submitter: usize) -> bool {
        let dom = page.dom.borrow();
        dom.attr(submitter, "formmethod")
            .or_else(|| dom.attr(form, "method"))
            .unwrap_or("get")
            .trim()
            .eq_ignore_ascii_case("dialog")
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[tokio::test]
        async fn actor_separates_lifecycle_tasks_and_checkpoints_click_microtasks() {
            let html = r#"<!doctype html><html><body>
                <span id="phase">parser</span>
                <button id="target">before</button>
                <script>
                    document.addEventListener("DOMContentLoaded", function () {
                        document.getElementById("phase").textContent = "dom";
                    });
                    window.addEventListener("load", function () {
                        document.getElementById("phase").textContent = "load";
                    });
                    document.getElementById("target").addEventListener("click", function () {
                        this.textContent = "clicked";
                        Promise.resolve().then(() => this.setAttribute("data-checkpoint", "done"));
                    });
                </script>
            </body></html>"#;
            let target = Dom::parse_document(html).get_by_id("target").unwrap();
            let (handle, mut events) = spawn_page(html.to_string(), PageEnv::bare(DEFAULT_URL));

            let first = tokio::time::timeout(Duration::from_secs(30), events.recv())
                .await
                .expect("initial Lumen render timed out")
                .expect("Lumen actor closed before initial render");
            let PageEvt::Updated { html, .. } = first else {
                panic!("expected an interactive shell, got {first:?}");
            };
            assert!(html.contains("<span id=\"phase\">dom</span>"));
            assert!(!html.contains("<span id=\"phase\">load</span>"));

            let loaded = tokio::time::timeout(Duration::from_secs(30), async {
                loop {
                    match events.recv().await {
                        Some(PageEvt::Updated { html, .. })
                            if html.contains("<span id=\"phase\">load</span>") =>
                        {
                            break;
                        }
                        Some(_) => {}
                        None => panic!("Lumen actor closed before load"),
                    }
                }
            })
            .await;
            assert!(loaded.is_ok(), "load remained blocked without resources");

            handle.try_send_user(PageCmd::Click(target)).unwrap();
            let clicked = tokio::time::timeout(Duration::from_secs(30), async {
                loop {
                    match events.recv().await {
                        Some(PageEvt::Updated { html, .. })
                            if html.contains("data-checkpoint=\"done\"") =>
                        {
                            assert!(html.contains("clicked"));
                            break;
                        }
                        Some(_) => {}
                        None => panic!("Lumen actor closed before click render"),
                    }
                }
            })
            .await;
            assert!(
                clicked.is_ok(),
                "click render preceded its mandatory microtask checkpoint"
            );
        }

        #[tokio::test]
        async fn actor_paints_parent_before_inserted_frame_navigation_and_delays_load() {
            // HTML "navigate" runs cross-document navigation in parallel; the
            // iframe and parent load events are later DOM-manipulation tasks.
            // In particular, a slow nested document must not hide the parsed,
            // DOMContentLoaded parent shell.
            let html = r#"<!doctype html><html><body>
                <span id="phase">parser</span>
                <script>
                    const phase = document.getElementById("phase");
                    const frame = document.createElement("iframe");
                    frame.srcdoc = "<p id='child'>child</p>";
                    frame.addEventListener("load", function () {
                        phase.textContent += "|frame";
                    });
                    document.body.appendChild(frame);
                    document.addEventListener("DOMContentLoaded", function () {
                        phase.textContent = "dom";
                    });
                    window.addEventListener("load", function () {
                        phase.textContent += "|load";
                    });
                </script>
            </body></html>"#;
            let (_handle, mut events) = spawn_page(html.to_string(), PageEnv::bare(DEFAULT_URL));

            let first = tokio::time::timeout(Duration::from_secs(30), events.recv())
                .await
                .expect("initial Lumen render timed out")
                .expect("Lumen actor closed before initial render");
            let PageEvt::Updated { html, .. } = first else {
                panic!("expected an interactive shell, got {first:?}");
            };
            assert!(html.contains("<span id=\"phase\">dom</span>"), "{html}");
            assert!(
                !html.contains("child"),
                "frame navigated before shell paint: {html}"
            );

            let loaded = tokio::time::timeout(Duration::from_secs(30), async {
                loop {
                    match events.recv().await {
                        Some(PageEvt::Updated { html, .. })
                            if html.contains("<span id=\"phase\">dom|frame|load</span>") =>
                        {
                            assert!(html.contains("child"));
                            break;
                        }
                        Some(_) => {}
                        None => panic!("Lumen actor closed before frame/parent load"),
                    }
                }
            })
            .await;
            assert!(
                loaded.is_ok(),
                "parent load did not follow initial iframe load task"
            );
        }
    }
}

#[cfg(feature = "lumen-desktop")]
pub(crate) use desktop::spawn_page;

/// Lumen's implemented subset of the canonical TRust host boundary. Keep this declarative: tests
/// compare every entry with `js::HOST_FUNCTIONS`, while the table itself remains the single source
/// used to install functions into each new realm.
const LUMEN_HOST_FUNCTIONS: &[(&str, usize, NativeFn)] = &[
    ("__dom_create_element", 1, host_create_element),
    ("__dom_create_text", 1, host_create_text),
    ("__dom_create_fragment", 0, host_create_fragment),
    ("__dom_parse_document", 1, host_parse_document),
    ("__dom_create_comment", 0, host_create_comment),
    ("__dom_append", 2, host_append),
    ("__dom_insert_before", 3, host_insert_before),
    ("__dom_detach", 1, host_detach),
    ("__dom_owner_document", 1, host_owner_document),
    ("__dom_adopt", 2, host_adopt),
    ("__dom_parent", 1, host_parent),
    ("__dom_is_connected", 1, host_is_connected),
    ("__dom_contains", 2, host_contains),
    ("__dom_set_hover", 1, host_set_hover),
    ("__dom_children", 1, host_children),
    ("__dom_slot_assigned", 1, host_slot_assigned),
    ("__dom_next", 1, host_next),
    ("__dom_prev", 1, host_prev),
    ("__dom_node_type", 1, host_node_type),
    ("__dom_tag", 1, host_tag),
    ("__dom_namespace", 1, host_namespace),
    ("__dom_get_attr", 2, host_get_attr),
    ("__dom_set_attr", 3, host_set_attr),
    ("__dom_remove_attr", 2, host_remove_attr),
    ("__dom_attr_names", 1, host_attr_names),
    ("__dom_text", 1, host_text),
    ("__dom_set_text", 2, host_set_text),
    ("__dom_inner_html", 1, host_inner_html),
    ("__dom_set_inner_html", 2, host_set_inner_html),
    ("__dom_outer_html", 1, host_outer_html),
    ("__dom_insert_adjacent", 3, host_insert_adjacent),
    ("__dom_query", 3, host_query),
    ("__dom_matches", 2, host_matches),
    ("__dom_get_by_id", 1, host_get_by_id),
    ("__dom_upgrade_candidates", 2, host_upgrade_candidates),
    ("__dom_ce_candidates", 1, host_ce_candidates),
    ("__dom_clone", 2, host_clone),
    ("__dom_doc_element", 0, host_doc_element),
    ("__html_dda", 0, host_html_dda),
    ("__url_parse", 2, host_url_parse),
    ("__url_set", 3, host_url_set),
    ("__dom_attach_shadow", 1, host_attach_shadow),
    ("__dom_shadow_root", 1, host_shadow_root),
    ("__dom_adopt_styles", 2, host_adopt_styles),
    ("__css_parse", 1, host_css_parse),
    ("__css_supports_selector", 1, host_css_supports_selector),
    ("__dom_template_content", 1, host_template_content),
    ("__http_fetch", 5, host_http_fetch),
    ("__http_fetch_async", 5, host_http_fetch_async),
    ("__dom_run_injected_script", 1, host_run_injected_script),
    (
        "__dom_load_injected_stylesheet",
        1,
        host_load_injected_stylesheet,
    ),
    ("__ws_open", 2, host_ws_open),
    ("__ws_send", 3, host_ws_send),
    ("__ws_close", 3, host_ws_close),
    ("__worker_spawn", 4, host_worker_spawn),
    ("__worker_post", 2, host_worker_post),
    ("__worker_terminate", 1, host_worker_terminate),
    ("__worker_self_post", 1, host_worker_self_post),
    ("__worker_self_close", 0, host_worker_self_close),
    ("__dom_computed", 2, host_computed_style),
    ("__image_current_src", 1, host_image_current_src),
    ("__image_complete", 1, host_image_complete),
    ("__match_media", 3, host_match_media),
    ("__dom_rect", 1, host_rect),
    ("__dom_scroll_get", 2, host_scroll_get),
    ("__dom_scroll_set", 3, host_scroll_set),
    ("__dom_load_frame", 3, host_load_frame),
    ("__cookie_get", 0, host_cookie_get),
    ("__cookie_set", 1, host_cookie_set),
    ("__clock_set", 1, host_clock_set),
    ("__storage_get", 2, host_storage_get),
    ("__storage_set", 3, host_storage_set),
    ("__storage_remove", 2, host_storage_remove),
    ("__storage_clear", 1, host_storage_clear),
    ("__storage_key", 2, host_storage_key),
    ("__storage_len", 1, host_storage_len),
    ("__blob_mirror", 3, host_blob_mirror),
    ("__crypto_sha256_digest", 1, host_crypto_sha256_digest),
    ("__compression_encode", 2, host_compression_encode),
    ("__text_encode", 1, host_text_encode),
    ("__dom_popover", 2, host_dom_popover),
    ("__wasm_validate", 1, lumen_wasm::host_validate),
    ("__wasm_compile", 1, lumen_wasm::host_compile),
    ("__wasm_module_imports", 1, lumen_wasm::host_module_imports),
    ("__wasm_module_exports", 1, lumen_wasm::host_module_exports),
    (
        "__wasm_module_custom_sections",
        2,
        lumen_wasm::host_module_custom_sections,
    ),
    ("__wasm_instantiate", 3, lumen_wasm::host_instantiate),
    (
        "__wasm_instance_exports",
        2,
        lumen_wasm::host_instance_exports,
    ),
    ("__wasm_call_export", 2, lumen_wasm::host_call_export),
    ("__wasm_global_new", 3, lumen_wasm::host_global_new),
    ("__wasm_global_get", 1, lumen_wasm::host_global_get),
    ("__wasm_global_set", 2, lumen_wasm::host_global_set),
    ("__wasm_memory_new", 2, lumen_wasm::host_memory_new),
    ("__wasm_memory_size", 1, lumen_wasm::host_memory_size),
    ("__wasm_memory_grow", 2, lumen_wasm::host_memory_grow),
    ("__wasm_memory_buffer", 1, lumen_wasm::host_memory_buffer),
    ("__wasm_table_new", 4, lumen_wasm::host_table_new),
    ("__wasm_table_length", 1, lumen_wasm::host_table_length),
    ("__wasm_table_get", 2, lumen_wasm::host_table_get),
    ("__wasm_table_set", 3, lumen_wasm::host_table_set),
    ("__wasm_table_grow", 3, lumen_wasm::host_table_grow),
];

fn install_host_boundary(engine: &mut lumen::Engine) {
    debug_assert!(lumen_registry_matches_canonical_boundary());
    for &(name, len, host_fn) in LUMEN_HOST_FUNCTIONS {
        engine.define_global(name, len, host_fn);
    }
}

fn lumen_registry_matches_canonical_boundary() -> bool {
    let canonical: std::collections::HashMap<_, _> =
        crate::js::host_boundary_signatures().collect();
    let mut implemented = std::collections::HashSet::new();
    LUMEN_HOST_FUNCTIONS.iter().all(|(name, len, _)| {
        implemented.insert(*name) && canonical.get(name).copied() == Some(*len)
    })
}

fn host_dom(ctx: &mut Ctx) -> Rc<RefCell<Dom>> {
    ctx.host_mut::<HostState>()
        .expect("HostState installed before any Lumen host call")
        .dom
        .clone()
}

/// Node id from a JS argument; `None` for null, undefined, non-numbers, negatives, and stale ids.
fn host_arg_node(dom: &Dom, args: &[Value], index: usize) -> Option<usize> {
    let number = args.get(index)?.as_num_opt()?;
    let id = number as usize;
    (number >= 0.0 && dom.is_valid(id)).then_some(id)
}

fn host_arg_string(ctx: &mut Ctx, args: &[Value], index: usize) -> String {
    args.get(index)
        .and_then(|value| ctx.coerce_string(value).ok())
        .map(|value| value.to_string())
        .unwrap_or_default()
}

fn host_id_value(id: Option<usize>) -> Value {
    id.map_or(Value::Null, |id| Value::Num(id as f64))
}

fn host_ids_array(ctx: &Ctx, ids: Vec<usize>) -> Value {
    ctx.make_array(ids.into_iter().map(|id| Value::Num(id as f64)).collect())
}

/// Fetch Standard §2.2.1 method normalization plus the byte-string request-body and header
/// transport contract shared with the platform prelude.
#[allow(clippy::type_complexity)]
fn host_fetch_args(
    ctx: &mut Ctx,
    args: &[Value],
) -> (
    String,
    String,
    Option<(String, Vec<u8>)>,
    Vec<(String, String)>,
) {
    let target = host_arg_string(ctx, args, 0);
    let mut method: String = host_arg_string(ctx, args, 1)
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || "!#$%&'*+-.^_`|~".contains(*ch))
        .collect();
    if ["DELETE", "GET", "HEAD", "OPTIONS", "POST", "PUT"]
        .iter()
        .any(|known| method.eq_ignore_ascii_case(known))
    {
        method.make_ascii_uppercase();
    }
    if method.is_empty() {
        method = String::from("GET");
    }
    let body = args
        .get(2)
        .filter(|value| !matches!(value, Value::Null | Value::Undefined))
        .map(|_| host_latin1_bytes(ctx, args, 2));
    let content_type = args
        .get(3)
        .filter(|value| !matches!(value, Value::Null | Value::Undefined))
        .map(|_| host_arg_string(ctx, args, 3));
    let body = body.map(|bytes| {
        (
            content_type.unwrap_or_else(|| String::from("text/plain;charset=UTF-8")),
            bytes,
        )
    });
    let headers = args
        .get(4)
        .filter(|value| !matches!(value, Value::Null | Value::Undefined))
        .map(|_| crate::js::parse_header_blob(&host_arg_string(ctx, args, 4)))
        .unwrap_or_default();
    (target, method, body, headers)
}

fn prepare_host_request(
    state: &mut HostState,
    target: &str,
    method: String,
    body: Option<(String, Vec<u8>)>,
    headers: Vec<(String, String)>,
) -> Option<(
    tokio::runtime::Handle,
    Arc<crate::http::PageCache>,
    crate::http::Request,
)> {
    let page = state.base.clone();
    let resolved = page.join(target).ok()?;
    let network = state.network.as_mut()?;
    if !matches!(resolved.scheme(), "http" | "https")
        || !crate::http::subresource_allowed(&page, &resolved)
        || network
            .fetched
            .fetch_update(
                std::sync::atomic::Ordering::Relaxed,
                std::sync::atomic::Ordering::Relaxed,
                |count| (count < crate::js::MAX_PAGE_FETCHES).then_some(count + 1),
            )
            .is_err()
    {
        return None;
    }
    let mut request = crate::http::Request {
        method,
        url: resolved,
        body,
        headers,
        fetch_metadata: None,
    };
    crate::http::set_referrer(&mut request, &page);
    Some((network.handle.clone(), network.cache.clone(), request))
}

fn lumen_fetch_result(response: crate::http::Response) -> LumenFetchResult {
    Some((
        response.status,
        response.content_type,
        response.body,
        crate::js::headers_to_blob(&response.headers),
    ))
}

fn lumen_cached_result(response: &crate::http::CachedResp) -> LumenFetchResult {
    Some((
        response.status,
        response.content_type.clone(),
        response.body.clone(),
        crate::js::headers_to_blob(&response.headers),
    ))
}

fn host_fetch_result_value(ctx: &mut Ctx, result: LumenFetchResult) -> Value {
    let Some((status, content_type, body, headers)) = result else {
        return Value::Null;
    };
    let text = if crate::js::response_body_is_binary(&content_type) {
        Value::Undefined
    } else {
        Value::from_string(String::from_utf8_lossy(&body).into_owned())
    };
    // Fetch Body is a byte sequence. Uint8Array is accepted by the prelude's BufferSource path;
    // retain the legacy one-code-point-per-byte string only as an allocation-failure fallback.
    let bytes = ctx
        .make_uint8array(&body)
        .unwrap_or_else(|_| Value::from_string(body.iter().copied().map(char::from).collect()));
    ctx.make_array(vec![
        Value::Num(f64::from(status)),
        Value::from_string(content_type),
        text,
        bytes,
        Value::from_string(headers),
    ])
}

/// XMLHttpRequest's synchronous flag uses HTML's pause semantics. The network future runs on the
/// application runtime while only this page thread waits, avoiding a nested Tokio `block_on`.
fn host_http_fetch(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let (target, method, body, headers) = host_fetch_args(ctx, args);
    let work = ctx
        .host_mut::<HostState>()
        .and_then(|state| prepare_host_request(state, &target, method, body, headers));
    let result = match work {
        Some((handle, cache, request)) => {
            let (sender, receiver) = std::sync::mpsc::channel();
            cache.spawn(&handle, async move {
                let _ = sender.send(crate::http::fetch(&request).await.ok());
            });
            receiver.recv().ok().flatten().and_then(lumen_fetch_result)
        }
        None => None,
    };
    Ok(host_fetch_result_value(ctx, result))
}

enum AsyncFetchSource {
    Cached(crate::http::SharedFetch),
    Request(Box<crate::http::Request>),
}

/// Fetch API §5.6 creates and returns a Promise before Fetch runs in parallel. Only Send response
/// data crosses the runtime channel; the resolving function remains rooted in the page realm and
/// is invoked later when the browser selects the networking task.
fn host_http_fetch_async(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let (target, method, body, headers) = host_fetch_args(ctx, args);
    let (promise, resolve, _reject) = ctx.new_promise_with_resolvers();

    let dispatch = {
        let Some(state) = ctx.host_mut::<HostState>() else {
            let _ = ctx.invoke(resolve, Value::Undefined, &[Value::Null]);
            return Ok(promise);
        };
        let cached = state.network.as_ref().and_then(|network| {
            (method == "GET" && body.is_none())
                .then(|| state.base.join(&target).ok())
                .flatten()
                .and_then(|url| network.cache.peek(&url))
        });
        let source = match cached {
            Some(shared) => Some(AsyncFetchSource::Cached(shared)),
            None => prepare_host_request(state, &target, method, body, headers)
                .map(|(_, _, request)| AsyncFetchSource::Request(Box::new(request))),
        };
        let events = state.task_events.clone();
        match (state.network.as_mut(), source, events) {
            (Some(network), Some(source), Some(events)) => {
                let id = network.next_fetch_id;
                network.next_fetch_id += 1;
                network.pending_fetches.insert(id, resolve.clone());
                Some((
                    id,
                    network.handle.clone(),
                    network.cache.clone(),
                    events,
                    source,
                ))
            }
            _ => None,
        }
    };

    let Some((id, handle, cache, events, source)) = dispatch else {
        let _ = ctx.invoke(resolve, Value::Undefined, &[Value::Null]);
        return Ok(promise);
    };
    cache.spawn(&handle, async move {
        let result = match source {
            AsyncFetchSource::Cached(shared) => shared
                .await
                .ok()
                .and_then(|response| lumen_cached_result(&response)),
            AsyncFetchSource::Request(request) => crate::http::fetch(&request)
                .await
                .ok()
                .and_then(lumen_fetch_result),
        };
        let _ = events.send(LumenHostTask::FetchDone { id, result });
    });
    Ok(promise)
}

fn host_trust(ctx: &mut Ctx) -> Result<Value, Value> {
    let global = ctx.global_this();
    ctx.member_get(&global, "__trust")
}

fn host_call_trust(ctx: &mut Ctx, name: &str, args: &[Value]) -> Result<Value, Value> {
    let trust = host_trust(ctx)?;
    let function = ctx.member_get(&trust, name)?;
    ctx.invoke(function, trust, args)
}

fn host_resource_url(ctx: &mut Ctx, node_id: usize, fallback: Option<String>) -> Option<String> {
    host_call_trust(ctx, "resourceURL", &[Value::Num(node_id as f64)])
        .ok()
        .and_then(|value| ctx.coerce_string(&value).ok())
        .map(|value| value.to_string())
        .filter(|value| !value.trim().is_empty())
        .or_else(|| fallback.filter(|value| !value.trim().is_empty()))
}

fn host_push_injected_error(ctx: &mut Ctx, message: impl Into<String>) {
    let Ok(trust) = host_trust(ctx) else {
        return;
    };
    let Ok(errors) = ctx.member_get(&trust, "errors") else {
        return;
    };
    let Ok(push) = ctx.member_get(&errors, "push") else {
        return;
    };
    let _ = ctx.invoke(push, errors, &[Value::from_string(message.into())]);
}

fn host_fire_script_event(ctx: &mut Ctx, node_id: usize, event_type: &str) {
    let _ = host_call_trust(ctx, "bindFrameForNode", &[Value::Num(node_id as f64)]);
    let _ = host_call_trust(
        ctx,
        "scriptEvent",
        &[
            Value::Num(node_id as f64),
            Value::from_string(event_type.to_string()),
        ],
    );
    let _ = host_call_trust(ctx, "restoreFrame", &[]);
}

/// The media type metadata of a `data:` URL. Fetch's data-URL processor defaults an omitted type
/// to `text/plain;charset=US-ASCII`; module-script fetching subsequently rejects that default
/// because it is not a JavaScript MIME type.
fn data_url_content_type(url: &str) -> String {
    let mut metadata = url
        .strip_prefix("data:")
        .and_then(|rest| rest.split_once(',').map(|(metadata, _)| metadata))
        .unwrap_or_default()
        .trim();
    if metadata
        .get(metadata.len().saturating_sub(";base64".len())..)
        .is_some_and(|suffix| suffix.eq_ignore_ascii_case(";base64"))
    {
        metadata = &metadata[..metadata.len() - ";base64".len()];
    }
    if metadata.is_empty() {
        String::from("text/plain;charset=US-ASCII")
    } else if metadata.starts_with(';') {
        format!("text/plain{metadata}")
    } else {
        metadata.to_string()
    }
}

fn data_resource_result(url: &str) -> LumenResourceResult {
    crate::img::decode_data_url(url.trim())
        .map(|body| (200, data_url_content_type(url), body, Vec::new()))
}

fn send_resource_completion(
    ctx: &mut Ctx,
    node_id: usize,
    name: String,
    kind: LumenResourceKind,
    result: LumenResourceResult,
    external: bool,
) -> bool {
    let Some((events, pending)) = ctx.host_mut::<HostState>().and_then(|state| {
        let events = state.task_events.clone()?;
        state.pending_resources += 1;
        Some((events, state.pending_resources))
    }) else {
        return false;
    };
    if events
        .send(LumenHostTask::ResourceDone {
            node_id,
            name,
            kind,
            result,
            external,
        })
        .is_err()
    {
        if let Some(state) = ctx.host_mut::<HostState>() {
            state.pending_resources = pending.saturating_sub(1);
        }
        return false;
    }
    true
}

fn spawn_resource_fetch(
    ctx: &mut Ctx,
    node_id: usize,
    kind: LumenResourceKind,
    request: crate::http::Request,
) -> bool {
    let name = request.url.to_string();
    let Some((handle, cache, events)) = ctx.host_mut::<HostState>().and_then(|state| {
        let events = state.task_events.clone()?;
        let network = state.network.as_ref()?;
        state.pending_resources += 1;
        Some((network.handle.clone(), network.cache.clone(), events))
    }) else {
        return false;
    };
    let shared = cache.peek(&request.url);
    cache.spawn(&handle, async move {
        let result = match shared {
            Some(shared) => shared.await.ok().map(|response| {
                (
                    response.status,
                    response.content_type.clone(),
                    response.body.clone(),
                    response.headers.clone(),
                )
            }),
            None => crate::http::fetch(&request).await.ok().map(|response| {
                (
                    response.status,
                    response.content_type,
                    response.body,
                    response.headers,
                )
            }),
        };
        let _ = events.send(LumenHostTask::ResourceDone {
            node_id,
            name,
            kind,
            result,
            external: true,
        });
    });
    true
}

fn queue_resource_error(ctx: &mut Ctx, node_id: usize, kind: LumenResourceKind, name: String) {
    if !send_resource_completion(ctx, node_id, name, kind, None, true) {
        host_fire_script_event(ctx, node_id, "error");
    }
}

fn host_eval_inline_classic(ctx: &mut Ctx, node_id: usize, source: String) {
    let _ = host_call_trust(ctx, "bindFrameForNode", &[Value::Num(node_id as f64)]);
    let trust = host_trust(ctx).ok();
    let old_current = trust
        .as_ref()
        .and_then(|trust| ctx.member_get(trust, "currentScript").ok());
    if let Some(trust) = trust.as_ref() {
        let _ = ctx.member_set(trust, "currentScript", Value::Num(node_id as f64));
    }
    let result = (|| {
        let global = ctx.global_this();
        let indirect_eval = ctx.member_get(&global, "eval")?;
        ctx.invoke(
            indirect_eval,
            Value::Undefined,
            &[Value::from_string(source)],
        )
    })();
    if let Err(error) = result {
        let message = ctx
            .coerce_string(&error)
            .map(|message| message.to_string())
            .unwrap_or_else(|_| String::from("injected inline script failed"));
        host_push_injected_error(ctx, format!("injected-inline: {message}"));
    }
    if let (Some(trust), Some(old_current)) = (trust.as_ref(), old_current) {
        let _ = ctx.member_set(trust, "currentScript", old_current);
    }
    let _ = host_call_trust(ctx, "restoreFrame", &[]);
}

/// HTML §4.12.1.1 post-connection and prepare-the-script-element steps for scripts inserted
/// through the live DOM. The prelude owns the already-started/type/connected gates; this host owns
/// source acquisition and execution.
fn host_run_injected_script(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let (node_id, src, text, module) = {
        let dom = host_dom(ctx);
        let dom = dom.borrow();
        let Some(node_id) = host_arg_node(&dom, args, 0) else {
            return Ok(Value::Undefined);
        };
        (
            node_id,
            dom.attr(node_id, "src").map(str::to_string),
            dom.text_content(node_id),
            dom.attr(node_id, "type")
                .is_some_and(|value| value.trim().eq_ignore_ascii_case("module")),
        )
    };
    let src = host_resource_url(ctx, node_id, src);

    if let Some(src) = src {
        if src.trim_start().starts_with("data:") {
            let result = data_resource_result(src.trim());
            if !send_resource_completion(
                ctx,
                node_id,
                src,
                if module {
                    LumenResourceKind::ModuleScript
                } else {
                    LumenResourceKind::ClassicScript
                },
                result,
                true,
            ) {
                host_fire_script_event(ctx, node_id, "error");
            }
            return Ok(Value::Undefined);
        }
        let request = ctx.host_mut::<HostState>().and_then(|state| {
            prepare_host_request(state, &src, String::from("GET"), None, Vec::new())
                .map(|(_, _, request)| request)
        });
        let kind = if module {
            LumenResourceKind::ModuleScript
        } else {
            LumenResourceKind::ClassicScript
        };
        match request {
            Some(request) => {
                if !spawn_resource_fetch(ctx, node_id, kind, request) {
                    queue_resource_error(ctx, node_id, kind, src);
                }
            }
            None => queue_resource_error(ctx, node_id, kind, src),
        }
    } else if module {
        let base = state_base(ctx);
        if !send_resource_completion(
            ctx,
            node_id,
            base,
            LumenResourceKind::ModuleScript,
            Some((
                200,
                String::from("text/javascript"),
                text.into_bytes(),
                Vec::new(),
            )),
            false,
        ) {
            host_push_injected_error(ctx, "inline module could not enter the host task queue");
        }
    } else if !text.is_empty() {
        // A non-parser-inserted inline classic script executes immediately in the element's
        // post-connection steps. Its exception is reported, not rethrown from appendChild().
        host_eval_inline_classic(ctx, node_id, text);
    }
    Ok(Value::Undefined)
}

fn state_base(ctx: &mut Ctx) -> String {
    ctx.host_mut::<HostState>()
        .map(|state| state.base.to_string())
        .unwrap_or_else(|| String::from("about:blank"))
}

/// HTML stylesheet-link processing: fetching is parallel, while attaching the CSSStyleSheet and
/// firing `load`/`error` occur in the later resource task.
fn host_load_injected_stylesheet(
    ctx: &mut Ctx,
    _this: Value,
    args: &[Value],
) -> Result<Value, Value> {
    let (node_id, href) = {
        let dom = host_dom(ctx);
        let dom = dom.borrow();
        let Some(node_id) = host_arg_node(&dom, args, 0) else {
            return Ok(Value::Undefined);
        };
        (node_id, dom.attr(node_id, "href").map(str::to_string))
    };
    let Some(href) = host_resource_url(ctx, node_id, href) else {
        return Ok(Value::Undefined);
    };
    if href.trim_start().starts_with("data:") {
        if !send_resource_completion(
            ctx,
            node_id,
            href.clone(),
            LumenResourceKind::Stylesheet,
            data_resource_result(href.trim()),
            true,
        ) {
            host_fire_script_event(ctx, node_id, "error");
        }
        return Ok(Value::Undefined);
    }
    let request = ctx.host_mut::<HostState>().and_then(|state| {
        prepare_host_request(state, &href, String::from("GET"), None, Vec::new())
            .map(|(_, _, request)| request)
    });
    match request {
        Some(request) => {
            if !spawn_resource_fetch(ctx, node_id, LumenResourceKind::Stylesheet, request) {
                queue_resource_error(ctx, node_id, LumenResourceKind::Stylesheet, href);
            }
        }
        None => queue_resource_error(ctx, node_id, LumenResourceKind::Stylesheet, href),
    }
    Ok(Value::Undefined)
}

/// WebSockets §3.1 constructor boundary: the prelude performs Web IDL/URL/subprotocol
/// validation synchronously; this host applies the page's private-network policy and starts the
/// RFC 6455 connection in parallel. Protocol feedback returns as WebSocket-task-source work.
fn host_ws_open(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let target = host_arg_string(ctx, args, 0);
    let protocols = host_arg_string(ctx, args, 1);
    let Some(protocols) = crate::ws::parse_protocols(&protocols) else {
        return Ok(Value::Num(-1.0));
    };
    let connection = ctx.host_mut::<HostState>().and_then(|state| {
        let sockets = state.websockets.as_mut()?;
        let resolved = sockets.page.join(&target).ok()?;
        if !matches!(resolved.scheme(), "ws" | "wss") || resolved.fragment().is_some() {
            return None;
        }
        let mut http_equivalent = resolved.clone();
        http_equivalent
            .set_scheme(if resolved.scheme() == "wss" {
                "https"
            } else {
                "http"
            })
            .ok()?;
        if !crate::http::subresource_allowed(&sockets.page, &http_equivalent) {
            return None;
        }
        let id = sockets.next_id;
        sockets.next_id += 1;
        let origin = sockets.page.origin().ascii_serialization();
        let cookie = crate::http::cookies_for_request(&http_equivalent);
        let (sender, task) = crate::ws::connect(
            resolved,
            protocols,
            origin,
            (!cookie.is_empty()).then_some(cookie),
            &sockets.handle,
            id,
            sockets.events.clone(),
        );
        sockets.tasks.track(task);
        sockets.sockets.insert(id, sender);
        Some(id)
    });
    Ok(connection.map_or(Value::Num(-1.0), |id| Value::Num(id as f64)))
}

/// WebSockets §3.1 `send()`: queue one complete text or binary message without blocking the
/// page thread. `bufferedAmount` is maintained in the prelude and decremented only by the later
/// [`crate::ws::WsIn::Sent`] task after the transport accepts these application bytes.
fn host_ws_send(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let id = args
        .first()
        .and_then(Value::as_num_opt)
        .and_then(|id| (id.is_finite() && id >= 0.0 && id.fract() == 0.0).then_some(id as usize));
    let data = host_arg_string(ctx, args, 1);
    let binary = matches!(args.get(2), Some(Value::Bool(true)));
    let sent = ctx
        .host_mut::<HostState>()
        .and_then(|state| state.websockets.as_mut())
        .and_then(|sockets| id.and_then(|id| sockets.sockets.get(&id)))
        .is_some_and(|sender| {
            sender
                .try_send(if binary {
                    crate::ws::WsOut::Binary(data.chars().map(|ch| ch as u32 as u8).collect())
                } else {
                    crate::ws::WsOut::Text(data)
                })
                .is_ok()
        });
    Ok(Value::Bool(sent))
}

/// WebSockets §3.1 `close()`: code zero is the boundary sentinel for an omitted status code,
/// whose RFC 6455 Close frame has an empty body. Validation and the synchronous CLOSING state
/// transition happen in the shared prelude before this non-blocking transport command.
fn host_ws_close(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let id = args
        .first()
        .and_then(Value::as_num_opt)
        .and_then(|id| (id.is_finite() && id >= 0.0 && id.fract() == 0.0).then_some(id as usize));
    let code = args
        .get(1)
        .and_then(Value::as_num_opt)
        .filter(|code| code.is_finite() && *code >= 0.0 && *code <= f64::from(u16::MAX))
        .unwrap_or_default() as u16;
    let reason = host_arg_string(ctx, args, 2);
    if let Some(sender) = ctx
        .host_mut::<HostState>()
        .and_then(|state| state.websockets.as_mut())
        .and_then(|sockets| id.and_then(|id| sockets.sockets.get(&id)))
    {
        let _ = sender.try_send(crate::ws::WsOut::Close(code, reason));
    }
    Ok(Value::Undefined)
}

fn lumen_potentially_trustworthy(url: &url::Url) -> bool {
    match url.scheme() {
        "https" | "wss" | "file" => true,
        "http" => url.host_str().is_some_and(|host| {
            host.eq_ignore_ascii_case("localhost")
                || host
                    .strip_suffix(".localhost")
                    .is_some_and(|prefix| !prefix.is_empty())
                || host
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|address| address.is_loopback())
        }),
        _ => false,
    }
}

/// HTML §10.2.6 `Worker()` construction: URL parsing is synchronous in the shared prelude; the
/// worker realm, script fetch, and evaluation start in parallel on a dedicated agent thread.
fn host_worker_spawn(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    const MAX_LUMEN_WORKERS: usize = 16;
    const LUMEN_WORKER_STACK: usize = 64 * 1024 * 1024;

    let target = host_arg_string(ctx, args, 0);
    let kind = if host_arg_string(ctx, args, 1) == "module" {
        LumenWorkerKind::Module
    } else {
        LumenWorkerKind::Classic
    };
    let name = host_arg_string(ctx, args, 2);
    let script_body = args
        .get(3)
        .filter(|value| !matches!(value, Value::Null | Value::Undefined))
        .map(|_| host_latin1_bytes(ctx, args, 3));

    let Some((id, launch, handle, tasks, events)) = ctx
        .host_mut::<HostState>()
        .and_then(|state| state.workers.as_mut())
        .and_then(|workers| {
            if workers.workers.len() >= MAX_LUMEN_WORKERS {
                return None;
            }
            let script_url = workers.page.join(&target).ok()?;
            let secure_context = lumen_potentially_trustworthy(&workers.page)
                && (script_url.scheme() == "blob" || lumen_potentially_trustworthy(&script_url));
            let id = workers.next_id;
            workers.next_id += 1;
            Some((
                id,
                LumenWorkerLaunch {
                    id,
                    owner_page: workers.page.clone(),
                    script_url,
                    kind,
                    name,
                    script_body,
                    secure_context,
                },
                workers.handle.clone(),
                workers.tasks.clone(),
                workers.events.clone(),
            ))
        })
    else {
        return Ok(Value::Num(-1.0));
    };

    let (ctl, ctl_rx) = std::sync::mpsc::sync_channel(64);
    let interrupt = Arc::new(lumen::RuntimeInterrupt::default());
    let worker_interrupt = interrupt.clone();
    let panic_events = events.clone();
    let spawned = std::thread::Builder::new()
        .name(format!("trust-lumen-worker-{id}"))
        .stack_size(LUMEN_WORKER_STACK)
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run_lumen_worker(launch, handle, tasks, events, ctl_rx, worker_interrupt);
            }));
            if result.is_err() {
                let _ = panic_events.send(LumenHostTask::Worker {
                    id,
                    event: crate::js::WorkerOut::Error(String::from("Lumen worker engine panic")),
                });
            }
            let _ = panic_events.send(LumenHostTask::WorkerExited { id });
        });
    if spawned.is_err() {
        return Ok(Value::Num(-1.0));
    }
    if let Some(workers) = ctx
        .host_mut::<HostState>()
        .and_then(|state| state.workers.as_mut())
    {
        workers
            .workers
            .insert(id, LumenWorkerHandle { ctl, interrupt });
    }
    Ok(Value::Num(id as f64))
}

/// MessagePort post-message steps serialize in the sender's realm before this call; the wire
/// snapshot is queued FIFO and deserialized only when the worker selects its message task.
fn host_worker_post(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let id = args
        .first()
        .and_then(Value::as_num_opt)
        .and_then(|id| (id.is_finite() && id >= 0.0 && id.fract() == 0.0).then_some(id as usize));
    let message = host_arg_string(ctx, args, 1);
    let sent = ctx
        .host_mut::<HostState>()
        .and_then(|state| state.workers.as_mut())
        .and_then(|workers| id.and_then(|id| workers.workers.get(&id)))
        .is_some_and(|worker| {
            worker
                .ctl
                .try_send(LumenWorkerCtl::Message(message))
                .is_ok()
        });
    Ok(Value::Bool(sent))
}

/// HTML §10.2.4 terminate-a-worker: discard queued messages and interrupt author code even when
/// the worker is currently executing instead of parked on its inbox.
fn host_worker_terminate(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let id = args
        .first()
        .and_then(Value::as_num_opt)
        .and_then(|id| (id.is_finite() && id >= 0.0 && id.fract() == 0.0).then_some(id as usize));
    if let Some(worker) = ctx
        .host_mut::<HostState>()
        .and_then(|state| state.workers.as_mut())
        .and_then(|workers| id.and_then(|id| workers.workers.remove(&id)))
    {
        worker.interrupt.cancel();
        let _ = worker.ctl.try_send(LumenWorkerCtl::Terminate);
    }
    Ok(Value::Undefined)
}

fn host_worker_self_post(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let message = host_arg_string(ctx, args, 0);
    if let Some(worker) = ctx
        .host_mut::<HostState>()
        .and_then(|state| state.worker_self.as_ref())
    {
        let _ = worker.events.send(LumenHostTask::Worker {
            id: worker.id,
            event: crate::js::WorkerOut::Message(message),
        });
    }
    Ok(Value::Undefined)
}

fn host_worker_self_close(ctx: &mut Ctx, _this: Value, _args: &[Value]) -> Result<Value, Value> {
    if let Some(worker) = ctx
        .host_mut::<HostState>()
        .and_then(|state| state.worker_self.as_mut())
    {
        worker.closed = true;
    }
    Ok(Value::Undefined)
}

fn install_lumen_worker_boundary(engine: &mut lumen::Engine) {
    // DedicatedWorkerGlobalScope is DOM-less. Install only the operations the
    // shared worker prelude can reach, including its independent per-agent
    // WebAssembly store.
    for &(name, len, function) in &[
        ("__url_parse", 2, host_url_parse as NativeFn),
        ("__url_set", 3, host_url_set as NativeFn),
        ("__http_fetch", 5, host_http_fetch as NativeFn),
        ("__worker_self_post", 1, host_worker_self_post as NativeFn),
        ("__worker_self_close", 0, host_worker_self_close as NativeFn),
        ("__blob_mirror", 3, host_blob_mirror as NativeFn),
        (
            "__crypto_sha256_digest",
            1,
            host_crypto_sha256_digest as NativeFn,
        ),
        (
            "__compression_encode",
            2,
            host_compression_encode as NativeFn,
        ),
        ("__text_encode", 1, host_text_encode as NativeFn),
        ("__wasm_validate", 1, lumen_wasm::host_validate as NativeFn),
        ("__wasm_compile", 1, lumen_wasm::host_compile as NativeFn),
        (
            "__wasm_module_imports",
            1,
            lumen_wasm::host_module_imports as NativeFn,
        ),
        (
            "__wasm_module_exports",
            1,
            lumen_wasm::host_module_exports as NativeFn,
        ),
        (
            "__wasm_module_custom_sections",
            2,
            lumen_wasm::host_module_custom_sections as NativeFn,
        ),
        (
            "__wasm_instantiate",
            3,
            lumen_wasm::host_instantiate as NativeFn,
        ),
        (
            "__wasm_instance_exports",
            2,
            lumen_wasm::host_instance_exports as NativeFn,
        ),
        (
            "__wasm_call_export",
            2,
            lumen_wasm::host_call_export as NativeFn,
        ),
        (
            "__wasm_global_new",
            3,
            lumen_wasm::host_global_new as NativeFn,
        ),
        (
            "__wasm_global_get",
            1,
            lumen_wasm::host_global_get as NativeFn,
        ),
        (
            "__wasm_global_set",
            2,
            lumen_wasm::host_global_set as NativeFn,
        ),
        (
            "__wasm_memory_new",
            2,
            lumen_wasm::host_memory_new as NativeFn,
        ),
        (
            "__wasm_memory_size",
            1,
            lumen_wasm::host_memory_size as NativeFn,
        ),
        (
            "__wasm_memory_grow",
            2,
            lumen_wasm::host_memory_grow as NativeFn,
        ),
        (
            "__wasm_memory_buffer",
            1,
            lumen_wasm::host_memory_buffer as NativeFn,
        ),
        (
            "__wasm_table_new",
            4,
            lumen_wasm::host_table_new as NativeFn,
        ),
        (
            "__wasm_table_length",
            1,
            lumen_wasm::host_table_length as NativeFn,
        ),
        (
            "__wasm_table_get",
            2,
            lumen_wasm::host_table_get as NativeFn,
        ),
        (
            "__wasm_table_set",
            3,
            lumen_wasm::host_table_set as NativeFn,
        ),
        (
            "__wasm_table_grow",
            3,
            lumen_wasm::host_table_grow as NativeFn,
        ),
    ] {
        engine.define_global(name, len, function);
    }
}

fn lumen_same_origin(left: &url::Url, right: &url::Url) -> bool {
    left.scheme() == right.scheme()
        && left.host_str() == right.host_str()
        && left.port_or_known_default() == right.port_or_known_default()
}

struct LumenWorkerScript {
    url: url::Url,
    source: String,
}

struct LumenWorkerModuleFetch {
    page: url::Url,
    handle: tokio::runtime::Handle,
    cache: Arc<crate::http::PageCache>,
    fetched: Arc<std::sync::atomic::AtomicUsize>,
}

/// HTML §8.1.4.2 classic/module worker fetch. Top-level HTTP(S) worker requests are same-origin;
/// classic HTTP(S) responses and all module responses require a JavaScript MIME type. `data:` and
/// active same-partition `blob:` entries are fetched without applying the HTTP MIME gate.
fn fetch_lumen_worker_script(
    launch: &LumenWorkerLaunch,
    handle: &tokio::runtime::Handle,
    cache: &Arc<crate::http::PageCache>,
) -> Option<LumenWorkerScript> {
    if let Some(body) = launch.script_body.as_ref() {
        return (launch.script_url.scheme() == "blob").then(|| LumenWorkerScript {
            url: launch.script_url.clone(),
            source: String::from_utf8_lossy(body).into_owned(),
        });
    }
    if launch.script_url.scheme() == "data" {
        let content_type = data_url_content_type(launch.script_url.as_str());
        if launch.kind == LumenWorkerKind::Module
            && !crate::http::module_script_response_allowed(200, &content_type)
        {
            return None;
        }
        let body = crate::img::decode_data_url(launch.script_url.as_str())?;
        return Some(LumenWorkerScript {
            url: launch.script_url.clone(),
            source: String::from_utf8_lossy(&body).into_owned(),
        });
    }
    if !matches!(launch.script_url.scheme(), "http" | "https")
        || !lumen_same_origin(&launch.owner_page, &launch.script_url)
        || !crate::http::subresource_allowed(&launch.owner_page, &launch.script_url)
    {
        return None;
    }

    let request = crate::http::Request::get(launch.script_url.clone());
    let (sender, receiver) = std::sync::mpsc::channel();
    cache.spawn(handle, async move {
        let result = crate::http::fetch(&request).await.ok().map(|response| {
            (
                response.url,
                response.status,
                response.content_type,
                response.body,
            )
        });
        let _ = sender.send(result);
    });
    let (response_url, status, content_type, body) = receiver.recv().ok().flatten()?;
    if !lumen_same_origin(&launch.owner_page, &response_url)
        || !crate::http::module_script_response_allowed(status, &content_type)
    {
        return None;
    }
    Some(LumenWorkerScript {
        url: response_url,
        source: String::from_utf8_lossy(&body).into_owned(),
    })
}

fn send_lumen_worker_error(
    events: &tokio::sync::mpsc::UnboundedSender<LumenHostTask>,
    id: usize,
    message: impl Into<String>,
) {
    let _ = events.send(LumenHostTask::Worker {
        id,
        event: crate::js::WorkerOut::Error(message.into()),
    });
}

/// Trusted bootstrap runs before author code but still observes a cancellation which raced with
/// realm construction. `Ok(false)` is the silent HTML termination path; parse/throw failures are
/// genuine platform bootstrap defects and are reported to the owner.
fn eval_lumen_worker_setup(
    engine: &mut lumen::Engine,
    source: &str,
    label: &str,
) -> Result<bool, String> {
    match engine.eval_value_interruptible(source) {
        Err(error) => Err(format!(
            "{label} parse error at line {}: {}",
            error.line, error.message
        )),
        Ok(Err(EvalError::Throw(error))) => Err(describe_throw(engine, error, label)),
        Ok(Err(EvalError::Interrupted(_))) => Ok(false),
        Ok(Ok(_)) => Ok(true),
    }
}

fn eval_lumen_worker_classic(
    engine: &mut lumen::Engine,
    source: &str,
    label: &str,
    events: &tokio::sync::mpsc::UnboundedSender<LumenHostTask>,
    id: usize,
) -> bool {
    match engine.eval_value_interruptible(source) {
        Err(error) => {
            send_lumen_worker_error(
                events,
                id,
                format!(
                    "{label} parse error at line {}: {}",
                    error.line, error.message
                ),
            );
        }
        Ok(Err(EvalError::Throw(error))) => {
            send_lumen_worker_error(events, id, describe_throw(engine, error, label));
        }
        Ok(Err(EvalError::Interrupted(_))) => return false,
        Ok(Ok(_)) => {}
    }
    if engine.run_microtasks_interruptible().is_err() {
        return false;
    }
    true
}

fn eval_lumen_worker_module(
    engine: &mut lumen::Engine,
    script: &LumenWorkerScript,
    fetch: LumenWorkerModuleFetch,
    events: &tokio::sync::mpsc::UnboundedSender<LumenHostTask>,
    id: usize,
) -> bool {
    let loader_page = fetch.page;
    let loader_handle = fetch.handle;
    let loader_cache = fetch.cache;
    let loader_fetched = fetch.fetched;
    match engine.eval_module_attrs_interruptible(
        &script.source,
        script.url.as_str(),
        move |specifier, referrer, _attributes| {
            module_dependency_loader(
                &loader_page,
                &loader_handle,
                &loader_cache,
                &loader_fetched,
                specifier,
                referrer,
            )
        },
    ) {
        Err(error) => send_lumen_worker_error(
            events,
            id,
            format!(
                "{} parse error at line {}: {}",
                script.url, error.line, error.message
            ),
        ),
        Ok(lumen::ExecutionOutcome::Throw { name, message }) => send_lumen_worker_error(
            events,
            id,
            format!("{} threw {name}: {message}", script.url),
        ),
        Ok(lumen::ExecutionOutcome::Interrupted { .. }) => return false,
        Ok(lumen::ExecutionOutcome::Value(_)) => {}
    }
    true
}

fn lumen_worker_internal_call(
    engine: &mut lumen::Engine,
    name: &str,
    args: &[Value],
) -> Result<Value, EvalError> {
    let global = engine.global_this();
    let worker = engine
        .ctx()
        .member_get(&global, "__wkr")
        .map_err(EvalError::Throw)?;
    let function = engine
        .ctx()
        .member_get(&worker, name)
        .map_err(EvalError::Throw)?;
    engine.call_function_interruptible(&function, worker, args)
}

fn lumen_worker_report_buffered_errors(
    engine: &mut lumen::Engine,
    events: &tokio::sync::mpsc::UnboundedSender<LumenHostTask>,
    id: usize,
) -> bool {
    match lumen_worker_internal_call(engine, "takeErrors", &[]) {
        Ok(value) => {
            let errors = value_string(engine, &value);
            for error in errors.split('\u{1e}').filter(|error| !error.is_empty()) {
                send_lumen_worker_error(events, id, error.to_string());
            }
            true
        }
        Err(EvalError::Interrupted(_)) => false,
        Err(EvalError::Throw(error)) => {
            send_lumen_worker_error(
                events,
                id,
                describe_throw(engine, error, "worker error reporting"),
            );
            true
        }
    }
}

fn lumen_worker_closed(engine: &mut lumen::Engine) -> bool {
    engine
        .ctx()
        .host_mut::<HostState>()
        .and_then(|state| state.worker_self.as_ref())
        .is_some_and(|worker| worker.closed)
}

fn lumen_worker_deadline(engine: &mut lumen::Engine) -> Option<f64> {
    match lumen_worker_internal_call(engine, "nextDeadline", &[]) {
        Ok(Value::Num(deadline)) if deadline.is_finite() => Some(deadline),
        _ => None,
    }
}

fn lumen_worker_now(engine: &mut lumen::Engine) -> f64 {
    match lumen_worker_internal_call(engine, "now", &[]) {
        Ok(Value::Num(now)) if now.is_finite() => now,
        _ => 0.0,
    }
}

/// One Lumen realm per dedicated worker agent. No engine value crosses the thread boundary;
/// messages are structured-clone wire strings, and every selected message/timer task is followed
/// by its own microtask checkpoint before the loop parks or selects another task.
fn run_lumen_worker(
    launch: LumenWorkerLaunch,
    handle: tokio::runtime::Handle,
    tasks: Arc<crate::http::PageTaskScope>,
    events: tokio::sync::mpsc::UnboundedSender<LumenHostTask>,
    ctl_rx: std::sync::mpsc::Receiver<LumenWorkerCtl>,
    interrupt: Arc<lumen::RuntimeInterrupt>,
) {
    let cache = Arc::new(crate::http::PageCache::with_task_scope(tasks));
    let clock = Rc::new(RealmClock::new());
    let mut state = HostState::new(Rc::new(RefCell::new(Dom::new())), clock.clone());
    state.base = launch.script_url.clone();
    state.network = Some(LumenNetwork {
        handle: handle.clone(),
        cache: cache.clone(),
        fetched: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        next_fetch_id: 0,
        pending_fetches: HashMap::new(),
    });
    state.worker_self = Some(LumenWorkerSelf {
        id: launch.id,
        events: events.clone(),
        closed: false,
    });

    let mut engine = lumen::Engine::new_with_interrupt(interrupt);
    let engine_clock = clock.clone();
    engine.set_wall_clock(move || engine_clock.now_ms());
    state.configure_module_loading(&mut engine);
    engine.ctx().op_state().put(state);
    install_lumen_worker_boundary(&mut engine);

    let worker_type = if launch.kind == LumenWorkerKind::Module {
        "module"
    } else {
        "classic"
    };
    let config = format!(
        "globalThis.__worker_cfg = {{ id: {}, name: {}, type: {}, url: {}, language: {}, languages: [{}, {}], hwc: {}, globalPrivacyControl: {}, secureContext: {} }};",
        launch.id,
        serde_json::to_string(&launch.name).unwrap_or_else(|_| String::from("\"\"")),
        serde_json::to_string(worker_type).expect("static worker type serializes"),
        serde_json::to_string(launch.script_url.as_str()).expect("URL serializes"),
        serde_json::to_string(crate::locale::LANGUAGE).expect("locale serializes"),
        serde_json::to_string(crate::locale::LANGUAGES[0]).expect("locale serializes"),
        serde_json::to_string(crate::locale::LANGUAGES[1]).expect("locale serializes"),
        std::thread::available_parallelism()
            .map(|parallelism| parallelism.get())
            .unwrap_or(8),
        crate::http::GLOBAL_PRIVACY_CONTROL,
        launch.secure_context,
    );
    for (source, label) in [
        (config.as_str(), "worker configuration"),
        (crate::js::worker_prelude(), "worker platform prelude"),
    ] {
        match eval_lumen_worker_setup(&mut engine, source, label) {
            Ok(true) => {}
            Ok(false) => return,
            Err(error) => {
                send_lumen_worker_error(&events, launch.id, error);
                return;
            }
        }
    }

    let Some(script) = fetch_lumen_worker_script(&launch, &handle, &cache) else {
        send_lumen_worker_error(
            &events,
            launch.id,
            format!("worker script failed to load: {}", launch.script_url),
        );
        return;
    };
    engine.set_import_base(script.url.as_str());
    let continued = match launch.kind {
        LumenWorkerKind::Classic => eval_lumen_worker_classic(
            &mut engine,
            &script.source,
            script.url.as_str(),
            &events,
            launch.id,
        ),
        LumenWorkerKind::Module => {
            let fetched = engine
                .ctx()
                .host_mut::<HostState>()
                .and_then(|state| state.network.as_ref())
                .map(|network| network.fetched.clone())
                .unwrap_or_default();
            eval_lumen_worker_module(
                &mut engine,
                &script,
                LumenWorkerModuleFetch {
                    page: launch.owner_page.clone(),
                    handle: handle.clone(),
                    cache: cache.clone(),
                    fetched,
                },
                &events,
                launch.id,
            )
        }
    };
    if !continued
        || !lumen_worker_report_buffered_errors(&mut engine, &events, launch.id)
        || lumen_worker_closed(&mut engine)
    {
        return;
    }
    loop {
        let base_ms = lumen_worker_now(&mut engine);
        let wall = Instant::now();
        let deadline = lumen_worker_deadline(&mut engine);
        let command = match deadline {
            Some(deadline) => {
                let wait = Duration::from_secs_f64(((deadline - base_ms).max(0.0)) / 1000.0);
                match ctl_rx.recv_timeout(wait) {
                    Ok(command) => Some(command),
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => None,
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
            None => match ctl_rx.recv() {
                Ok(command) => Some(command),
                Err(_) => break,
            },
        };

        let task_result = match command {
            Some(LumenWorkerCtl::Terminate) => break,
            Some(LumenWorkerCtl::Message(message)) => {
                lumen_worker_internal_call(&mut engine, "message", &[Value::from_string(message)])
            }
            None => lumen_worker_internal_call(
                &mut engine,
                "tick",
                &[Value::Num(base_ms + wall.elapsed().as_secs_f64() * 1000.0)],
            ),
        };
        match task_result {
            Ok(_) => {}
            Err(EvalError::Interrupted(_)) => break,
            Err(EvalError::Throw(error)) => send_lumen_worker_error(
                &events,
                launch.id,
                describe_throw(&mut engine, error, "worker task"),
            ),
        }
        if engine.run_microtasks_interruptible().is_err()
            || !lumen_worker_report_buffered_errors(&mut engine, &events, launch.id)
            || lumen_worker_closed(&mut engine)
        {
            break;
        }
        engine.collect_garbage_at_idle();
    }
}

fn module_dependency_loader(
    page: &url::Url,
    handle: &tokio::runtime::Handle,
    cache: &Arc<crate::http::PageCache>,
    fetched: &std::sync::atomic::AtomicUsize,
    specifier: &str,
    referrer: &str,
) -> Option<(String, String)> {
    let resolved = resolve_module_specifier(page, specifier, referrer)?;
    if resolved.scheme() == "data" {
        let content_type = data_url_content_type(resolved.as_str());
        if !crate::http::module_script_response_allowed(200, &content_type) {
            return None;
        }
        let body = crate::img::decode_data_url(resolved.as_str())?;
        return Some((
            resolved.to_string(),
            crate::http::decode_body(&content_type, &body),
        ));
    }
    if !matches!(resolved.scheme(), "http" | "https")
        || !crate::http::subresource_allowed(page, &resolved)
    {
        return None;
    }
    let response = if let Some(shared) = cache.peek(&resolved) {
        crate::http::PageCache::block_on_fetch(Some(handle), shared)?
    } else {
        if fetched
            .fetch_update(
                std::sync::atomic::Ordering::Relaxed,
                std::sync::atomic::Ordering::Relaxed,
                |count| (count < crate::js::MAX_PAGE_FETCHES).then_some(count + 1),
            )
            .is_err()
        {
            return None;
        }
        let shared = cache.fetch(handle, resolved.clone());
        crate::http::PageCache::block_on_fetch(Some(handle), shared)?
    };
    crate::http::module_script_response_allowed(response.status, &response.content_type).then(
        || {
            speculate_module_imports(page, handle, cache, fetched, &resolved, &response.body);
            (
                resolved.to_string(),
                crate::http::decode_body(&response.content_type, &response.body),
            )
        },
    )
}

/// Resolve a module specifier without an import map. HTML's resolve-a-module-specifier algorithm
/// accepts URL-like specifiers here; a bare specifier is a failure rather than a path relative to
/// the referrer. Import-map support can replace this boundary without changing either loader.
fn resolve_module_specifier(page: &url::Url, specifier: &str, referrer: &str) -> Option<url::Url> {
    if !(specifier.starts_with('/')
        || specifier.starts_with("./")
        || specifier.starts_with("../")
        || url::Url::parse(specifier).is_ok())
    {
        return None;
    }
    let base = url::Url::parse(referrer).unwrap_or_else(|_| page.clone());
    base.join(specifier).ok()
}

fn data_dynamic_module_result(resolved: &url::Url) -> Option<(String, String)> {
    let content_type = data_url_content_type(resolved.as_str());
    crate::http::module_script_response_allowed(200, &content_type)
        .then(|| crate::img::decode_data_url(resolved.as_str()))
        .flatten()
        .map(|body| {
            (
                resolved.to_string(),
                crate::http::decode_body(&content_type, &body),
            )
        })
}

/// Start one dynamic module fetch and report only Send data back to the owning page thread. Fetch
/// and HTML module-script MIME checks happen off-thread; parsing, linking, evaluation, promise
/// settlement, and the following microtask checkpoint remain serialized in the JS realm.
fn queue_dynamic_module_load(
    loader: &LumenDynamicModuleLoader,
    request_id: u64,
    specifier: &str,
    referrer: &str,
) {
    let Some(resolved) = resolve_module_specifier(&loader.page, specifier, referrer) else {
        let _ = loader.events.send(LumenHostTask::DynamicModule {
            request_id,
            result: None,
        });
        return;
    };
    if resolved.scheme() == "data" {
        let result = data_dynamic_module_result(&resolved);
        let _ = loader
            .events
            .send(LumenHostTask::DynamicModule { request_id, result });
        return;
    }
    let Some(network) = loader.network.as_ref() else {
        let _ = loader.events.send(LumenHostTask::DynamicModule {
            request_id,
            result: None,
        });
        return;
    };
    if !matches!(resolved.scheme(), "http" | "https")
        || !crate::http::subresource_allowed(&loader.page, &resolved)
    {
        let _ = loader.events.send(LumenHostTask::DynamicModule {
            request_id,
            result: None,
        });
        return;
    }

    let shared = if let Some(shared) = network.cache.peek(&resolved) {
        shared
    } else {
        if network
            .fetched
            .fetch_update(
                std::sync::atomic::Ordering::Relaxed,
                std::sync::atomic::Ordering::Relaxed,
                |count| (count < crate::js::MAX_PAGE_FETCHES).then_some(count + 1),
            )
            .is_err()
        {
            let _ = loader.events.send(LumenHostTask::DynamicModule {
                request_id,
                result: None,
            });
            return;
        }
        network.cache.fetch(&network.handle, resolved.clone())
    };
    let events = loader.events.clone();
    network.cache.spawn(&network.handle, async move {
        let result = shared.await.ok().and_then(|response| {
            crate::http::module_script_response_allowed(response.status, &response.content_type)
                .then(|| {
                    (
                        resolved.to_string(),
                        crate::http::decode_body(&response.content_type, &response.body),
                    )
                })
        });
        let _ = events.send(LumenHostTask::DynamicModule { request_id, result });
    });
}

fn speculate_engine_imports(engine: &mut lumen::Engine, base: &url::Url, body: &[u8]) {
    let Some((page, handle, cache, fetched)) =
        engine.ctx().host_mut::<HostState>().and_then(|state| {
            let network = state.network.as_ref()?;
            Some((
                state.base.clone(),
                network.handle.clone(),
                network.cache.clone(),
                network.fetched.clone(),
            ))
        })
    else {
        return;
    };
    speculate_module_imports(&page, &handle, &cache, &fetched, base, body);
}

/// HTML §4.12.1 fetches a module script and its dependencies in parallel. Lumen's graph loader
/// intentionally stays synchronous and atomic, so warm every statically named dependency in the
/// shared page cache before that loader asks for them in source order.
fn speculate_module_imports(
    page: &url::Url,
    handle: &tokio::runtime::Handle,
    cache: &Arc<crate::http::PageCache>,
    fetched: &std::sync::atomic::AtomicUsize,
    base: &url::Url,
    body: &[u8],
) {
    for specifier in crate::js::scan_module_imports(body)
        .into_iter()
        .take(crate::js::MAX_SPECULATIVE_IMPORTS)
    {
        let Some(resolved) = base
            .join(&specifier)
            .ok()
            .filter(|url| matches!(url.scheme(), "http" | "https"))
        else {
            continue;
        };
        if cache.peek(&resolved).is_some() {
            continue;
        }
        if !crate::http::subresource_allowed(page, &resolved)
            || fetched
                .fetch_update(
                    std::sync::atomic::Ordering::Relaxed,
                    std::sync::atomic::Ordering::Relaxed,
                    |count| (count < crate::js::MAX_PAGE_FETCHES).then_some(count + 1),
                )
                .is_err()
        {
            continue;
        }
        if std::env::var_os("TRUST_NET_TRACE").is_some() {
            eprintln!("lumen: module prefetch {resolved}");
        }
        cache.prefetch(handle, resolved);
    }
}

fn push_engine_error(engine: &mut lumen::Engine, message: String) {
    host_push_injected_error(engine.ctx(), message);
}

fn fire_engine_script_event(engine: &mut lumen::Engine, node_id: usize, event_type: &str) {
    host_fire_script_event(engine.ctx(), node_id, event_type);
}

/// HTML's run-a-module-script completion steps wait for the module's evaluation promise, including
/// top-level await. Retaining this as a pending resource also keeps the document load event behind
/// parser-inserted module evaluation. Each reaction runs during the owning realm's microtask
/// checkpoint and performs the element's success/failure steps exactly once.
fn track_module_evaluation(engine: &mut lumen::Engine, node_id: usize, name: &str) -> bool {
    let Some(promise) = engine.module_evaluation_promise(name) else {
        return false;
    };
    if let Some(state) = engine.ctx().host_mut::<HostState>() {
        state.pending_resources += 1;
    } else {
        return false;
    }

    let fulfilled = engine.ctx().new_native_fn(
        "",
        0,
        Rc::new(move |ctx, _this, _args| {
            if let Some(state) = ctx.host_mut::<HostState>() {
                state.pending_resources = state.pending_resources.saturating_sub(1);
            }
            host_fire_script_event(ctx, node_id, "load");
            Ok(Value::Undefined)
        }),
    );
    let failed_name = name.to_string();
    let rejected = engine.ctx().new_native_fn(
        "",
        1,
        Rc::new(move |ctx, _this, args| {
            if let Some(state) = ctx.host_mut::<HostState>() {
                state.pending_resources = state.pending_resources.saturating_sub(1);
            }
            let reason = args
                .first()
                .and_then(|value| ctx.coerce_string(value).ok())
                .map(|value| value.to_string())
                .unwrap_or_else(|| String::from("module evaluation failed"));
            host_push_injected_error(ctx, format!("module {failed_name}: {reason}"));
            host_fire_script_event(ctx, node_id, "error");
            Ok(Value::Undefined)
        }),
    );
    let attached = engine
        .ctx()
        .member_get(&promise, "then")
        .and_then(|then| engine.ctx().invoke(then, promise, &[fulfilled, rejected]))
        .is_ok();
    if !attached && let Some(state) = engine.ctx().host_mut::<HostState>() {
        state.pending_resources = state.pending_resources.saturating_sub(1);
    }
    attached
}

fn run_injected_classic_task(
    engine: &mut lumen::Engine,
    node_id: usize,
    name: &str,
    source: &str,
) -> Result<(), String> {
    let trust = host_trust(engine.ctx()).map_err(|_| "read __trust".to_string())?;
    let document_base = state_base(engine.ctx());
    engine.set_import_base(name);
    let old_current = engine
        .ctx()
        .member_get(&trust, "currentScript")
        .unwrap_or(Value::Null);
    let _ = host_call_trust(
        engine.ctx(),
        "bindFrameForNode",
        &[Value::Num(node_id as f64)],
    );
    let _ = engine
        .ctx()
        .member_set(&trust, "currentScript", Value::Num(node_id as f64));
    let evaluated = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        engine.eval_value_interruptible(source)
    }));
    match evaluated {
        Ok(Ok(Ok(_))) => {}
        Ok(Ok(Err(error))) => {
            let message = describe_eval_error(engine, error, name);
            push_engine_error(engine, message);
        }
        Ok(Err(error)) => push_engine_error(
            engine,
            format!(
                "{name} parse error at line {}: {}",
                error.line, error.message
            ),
        ),
        Err(_) => push_engine_error(engine, format!("{name}: Lumen engine panic")),
    }
    // HTML clean-up after running script performs a checkpoint once the script stack is empty.
    let checkpoint = engine
        .run_microtasks_interruptible()
        .map_err(|reason| format!("{name} microtasks interrupted: {}", reason.message()));
    let _ = engine
        .ctx()
        .member_set(&trust, "currentScript", old_current);
    let _ = host_call_trust(engine.ctx(), "restoreFrame", &[]);
    engine.set_import_base(&document_base);
    checkpoint
}

fn run_injected_module_task(
    engine: &mut lumen::Engine,
    node_id: usize,
    name: &str,
    source: &str,
) -> Result<(), String> {
    let snapshot = engine.ctx().host_mut::<HostState>().and_then(|state| {
        let network = state.network.as_ref()?;
        Some((
            state.base.clone(),
            network.handle.clone(),
            network.cache.clone(),
            network.fetched.clone(),
        ))
    });
    let evaluated = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if let Some((page, handle, cache, fetched)) = snapshot {
            let loader_page = page.clone();
            engine.eval_module_attrs_interruptible(
                source,
                name,
                move |specifier, referrer, _attributes| {
                    module_dependency_loader(
                        &loader_page,
                        &handle,
                        &cache,
                        &fetched,
                        specifier,
                        referrer,
                    )
                },
            )
        } else {
            engine.eval_module_attrs_interruptible(
                source,
                name,
                |_specifier, _referrer, _attributes| None,
            )
        }
    }));
    match evaluated {
        Ok(Ok(lumen::ExecutionOutcome::Value(_))) => {
            if !track_module_evaluation(engine, node_id, name) {
                fire_engine_script_event(engine, node_id, "load");
            }
            engine.run_microtasks_interruptible().map_err(|reason| {
                format!("module {name} microtasks interrupted: {}", reason.message())
            })?;
        }
        Ok(Ok(lumen::ExecutionOutcome::Throw { name: ty, message })) => {
            push_engine_error(engine, format!("module {name}: {ty}: {message}"));
            fire_engine_script_event(engine, node_id, "error");
        }
        Ok(Ok(lumen::ExecutionOutcome::Interrupted { reason })) => {
            return Err(format!("module {name} interrupted: {}", reason.message()));
        }
        Ok(Err(error)) => {
            push_engine_error(
                engine,
                format!(
                    "module {name} parse error at line {}: {}",
                    error.line, error.message
                ),
            );
            fire_engine_script_event(engine, node_id, "error");
        }
        Err(_) => {
            push_engine_error(engine, format!("module {name}: Lumen engine panic"));
            fire_engine_script_event(engine, node_id, "error");
        }
    }
    Ok(())
}

fn run_resource_task(
    engine: &mut lumen::Engine,
    node_id: usize,
    name: String,
    kind: LumenResourceKind,
    result: LumenResourceResult,
    external: bool,
) -> Result<(), String> {
    match kind {
        LumenResourceKind::ClassicScript => match result {
            Some((status, content_type, body, headers))
                if crate::http::classic_script_response_allowed(
                    status,
                    &content_type,
                    &headers,
                ) =>
            {
                let source = crate::http::decode_body(&content_type, &body);
                run_injected_classic_task(engine, node_id, &name, &source)?;
                if external {
                    fire_engine_script_event(engine, node_id, "load");
                }
            }
            _ => fire_engine_script_event(engine, node_id, "error"),
        },
        LumenResourceKind::ModuleScript => match result {
            Some((status, content_type, body, _headers))
                if crate::http::module_script_response_allowed(status, &content_type) =>
            {
                let source = crate::http::decode_body(&content_type, &body);
                run_injected_module_task(engine, node_id, &name, &source)?;
            }
            _ => fire_engine_script_event(engine, node_id, "error"),
        },
        LumenResourceKind::Stylesheet => match result {
            Some((status, content_type, body, headers))
                if crate::http::stylesheet_response_allowed(status, &content_type, &headers) =>
            {
                let css = crate::http::decode_body(&content_type, &body);
                let dom = engine
                    .ctx()
                    .host_mut::<HostState>()
                    .expect("HostState installed before resource dispatch")
                    .dom
                    .clone();
                dom.borrow_mut().attach_sheet_to_link(node_id, css);
                fire_engine_script_event(engine, node_id, "load");
            }
            _ => fire_engine_script_event(engine, node_id, "error"),
        },
    }
    Ok(())
}

fn dispatch_websocket_task(
    engine: &mut lumen::Engine,
    id: usize,
    event: crate::ws::WsIn,
) -> Result<(), String> {
    let mut args = vec![
        Value::Num(id as f64),
        Value::Undefined,
        Value::from_string(String::new()),
        Value::Bool(false),
        Value::Num(0.0),
        Value::from_string(String::new()),
        Value::Bool(false),
        Value::Bool(false),
        Value::from_string(String::new()),
    ];
    match event {
        crate::ws::WsIn::Open { protocol } => {
            args[1] = Value::from_string(String::from("open"));
            args[8] = Value::from_string(protocol);
        }
        crate::ws::WsIn::Text(message) => {
            args[1] = Value::from_string(String::from("message"));
            args[2] = Value::from_string(message);
        }
        crate::ws::WsIn::Binary(bytes) => {
            args[1] = Value::from_string(String::from("message"));
            args[2] = Value::from_string(bytes.into_iter().map(char::from).collect());
            args[3] = Value::Bool(true);
        }
        crate::ws::WsIn::Sent(bytes) => {
            args[1] = Value::from_string(String::from("drain"));
            args[4] = Value::Num(bytes as f64);
        }
        crate::ws::WsIn::Closed {
            code,
            reason,
            was_clean,
            failed,
        } => {
            if let Some(sockets) = engine
                .ctx()
                .host_mut::<HostState>()
                .and_then(|state| state.websockets.as_mut())
            {
                sockets.sockets.remove(&id);
            }
            args[1] = Value::from_string(String::from("close"));
            args[4] = Value::Num(f64::from(code));
            args[5] = Value::from_string(reason);
            args[6] = Value::Bool(was_clean);
            args[7] = Value::Bool(failed);
        }
    }
    host_call_trust(engine.ctx(), "wsEvent", &args)
        .map(|_| ())
        .map_err(|error| {
            engine
                .ctx()
                .coerce_string(&error)
                .map(|message| format!("WebSocket task: {message}"))
                .unwrap_or_else(|_| String::from("WebSocket task failed"))
        })
}

/// Run the engine-owned portion of one selected host task. The caller performs the HTML event
/// loop's microtask checkpoint after this returns, before selecting another task.
#[allow(dead_code)] // The networked test realm uses this before the resident actor is switched.
fn dispatch_host_task(engine: &mut lumen::Engine, task: LumenHostTask) -> Result<(), String> {
    match task {
        LumenHostTask::FetchDone { id, result } => {
            let resolve = engine
                .ctx()
                .host_mut::<HostState>()
                .and_then(|state| state.network.as_mut())
                .and_then(|network| network.pending_fetches.remove(&id));
            let Some(resolve) = resolve else {
                return Ok(());
            };
            let value = host_fetch_result_value(engine.ctx(), result);
            engine
                .call_function_interruptible(&resolve, Value::Undefined, &[value])
                .map_err(|error| describe_eval_error(engine, error, "fetch networking task"))?;
        }
        LumenHostTask::ResourceDone {
            node_id,
            name,
            kind,
            result,
            external,
        } => {
            if let Some(state) = engine.ctx().host_mut::<HostState>() {
                state.pending_resources = state.pending_resources.saturating_sub(1);
            }
            run_resource_task(engine, node_id, name, kind, result, external)?;
        }
        LumenHostTask::DynamicModule { request_id, result } => {
            if let Some(state) = engine.ctx().host_mut::<HostState>() {
                let _ = state.pending_dynamic_modules.fetch_update(
                    std::sync::atomic::Ordering::Relaxed,
                    std::sync::atomic::Ordering::Relaxed,
                    |count| Some(count.saturating_sub(1)),
                );
            }
            if let Some((name, source)) = result.as_ref()
                && let Ok(base) = url::Url::parse(name)
            {
                speculate_engine_imports(engine, &base, source.as_bytes());
            }
            let _ = engine.finish_dynamic_module_load(request_id, result);
        }
        LumenHostTask::WebSocket { id, event } => {
            dispatch_websocket_task(engine, id, event)?;
        }
        LumenHostTask::Worker { id, event } => {
            let (name, payload) = match event {
                crate::js::WorkerOut::Message(message) => ("workerMessage", message),
                crate::js::WorkerOut::Error(message) => ("workerError", message),
            };
            host_call_trust(
                engine.ctx(),
                name,
                &[Value::Num(id as f64), Value::from_string(payload)],
            )
            .map_err(|error| {
                engine
                    .ctx()
                    .coerce_string(&error)
                    .map(|message| format!("Worker task: {message}"))
                    .unwrap_or_else(|_| String::from("Worker task failed"))
            })?;
        }
        LumenHostTask::WorkerExited { id } => {
            if let Some(workers) = engine
                .ctx()
                .host_mut::<HostState>()
                .and_then(|state| state.workers.as_mut())
            {
                workers.workers.remove(&id);
            }
        }
    }
    Ok(())
}

fn host_create_element(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let tag = host_arg_string(ctx, args, 0);
    let dom = host_dom(ctx);
    let id = dom.borrow_mut().create_element(&tag);
    Ok(host_id_value(Some(id)))
}

fn host_create_text(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let text = host_arg_string(ctx, args, 0);
    let dom = host_dom(ctx);
    let id = dom.borrow_mut().create_text(&text);
    Ok(host_id_value(Some(id)))
}

fn host_create_fragment(ctx: &mut Ctx, _this: Value, _args: &[Value]) -> Result<Value, Value> {
    let dom = host_dom(ctx);
    let id = dom.borrow_mut().create_fragment();
    Ok(host_id_value(Some(id)))
}

/// HTML fragment parsing and `DOMParser` use distinct algorithms. This operation deliberately uses
/// HTML's full document parsing algorithm and transplants its `html`/`head`/`body` tree into the
/// page arena; fragment parsing is exposed by the later inner-HTML boundary slice.
fn host_parse_document(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let html = host_arg_string(ctx, args, 0);
    let dom = host_dom(ctx);
    let id = dom.borrow_mut().parse_document_into(&html);
    Ok(host_id_value(Some(id)))
}

fn host_create_comment(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let text = host_arg_string(ctx, args, 0);
    let dom = host_dom(ctx);
    let id = dom.borrow_mut().create_comment(&text);
    Ok(host_id_value(Some(id)))
}

/// DOM Standard §4.2.3's host-including inclusive-ancestor validity check. The prelude translates
/// `false` to `HierarchyRequestError`, preserving the existing one-call mutation boundary.
fn host_append(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let dom = host_dom(ctx);
    let mut dom = dom.borrow_mut();
    if let (Some(parent), Some(child)) =
        (host_arg_node(&dom, args, 0), host_arg_node(&dom, args, 1))
    {
        if dom.is_host_including_inclusive_ancestor(child, parent) {
            return Ok(Value::Bool(false));
        }
        dom.append(parent, child);
    }
    Ok(Value::Bool(true))
}

fn host_insert_before(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let dom = host_dom(ctx);
    let mut dom = dom.borrow_mut();
    if let (Some(parent), Some(child)) =
        (host_arg_node(&dom, args, 0), host_arg_node(&dom, args, 1))
    {
        if dom.is_host_including_inclusive_ancestor(child, parent) {
            return Ok(Value::Bool(false));
        }
        let reference = host_arg_node(&dom, args, 2);
        if reference.is_some_and(|reference| dom.node(reference).parent != Some(parent)) {
            return Ok(Value::Num(-1.0));
        }
        dom.insert_before(parent, child, reference);
    }
    Ok(Value::Bool(true))
}

fn host_detach(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let dom = host_dom(ctx);
    let mut dom = dom.borrow_mut();
    if let Some(id) = host_arg_node(&dom, args, 0) {
        dom.detach(id);
    }
    Ok(Value::Undefined)
}

fn host_owner_document(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let dom = host_dom(ctx);
    let dom = dom.borrow();
    Ok(host_id_value(
        host_arg_node(&dom, args, 0).and_then(|id| dom.owner_document(id)),
    ))
}

/// DOM Standard §4.5's adopt algorithm. Negative values are an internal result enum interpreted by
/// the prelude, which exposes the required Web IDL exceptions and custom-element reactions.
fn host_adopt(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let dom = host_dom(ctx);
    let mut dom = dom.borrow_mut();
    let Some(document) = host_arg_node(&dom, args, 0) else {
        return Ok(Value::Num(-1.0));
    };
    let Some(node) = host_arg_node(&dom, args, 1) else {
        return Ok(Value::Num(-2.0));
    };
    let result = match dom.adopt_node(document, node) {
        Ok(old_document) => old_document as f64,
        Err(AdoptError::TargetNotDocument) => -3.0,
        Err(AdoptError::InvalidNode) => -2.0,
        Err(AdoptError::Document) => -4.0,
        Err(AdoptError::ShadowRoot) => -5.0,
    };
    Ok(Value::Num(result))
}

fn host_parent(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let dom = host_dom(ctx);
    let dom = dom.borrow();
    Ok(host_id_value(
        host_arg_node(&dom, args, 0).and_then(|id| dom.node(id).parent),
    ))
}

fn host_is_connected(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let dom = host_dom(ctx);
    let dom = dom.borrow();
    Ok(Value::Bool(
        host_arg_node(&dom, args, 0).is_some_and(|id| dom.is_connected(id)),
    ))
}

fn host_contains(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let dom = host_dom(ctx);
    let dom = dom.borrow();
    let contains = match (host_arg_node(&dom, args, 0), host_arg_node(&dom, args, 1)) {
        (Some(ancestor), Some(node)) => {
            let mut current = dom.node(node).parent;
            loop {
                match current {
                    Some(parent) if parent == ancestor => break true,
                    Some(parent) => current = dom.node(parent).parent,
                    None => break false,
                }
            }
        }
        _ => false,
    };
    Ok(Value::Bool(contains))
}

fn host_set_hover(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let target = args
        .first()
        .and_then(Value::as_num_opt)
        .filter(|number| *number >= 0.0)
        .map(|number| number as usize);
    let dom = host_dom(ctx);
    let affected = dom.borrow_mut().set_hover_chain(target);
    Ok(Value::Bool(affected))
}

fn host_children(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let dom = host_dom(ctx);
    let ids = {
        let dom = dom.borrow();
        host_arg_node(&dom, args, 0)
            .map(|id| dom.children(id))
            .unwrap_or_default()
    };
    Ok(host_ids_array(ctx, ids))
}

/// DOM Standard §4.2.2.4 assigned-nodes lookup for `HTMLSlotElement.assignedNodes()`.
fn host_slot_assigned(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let dom = host_dom(ctx);
    let ids = {
        let dom = dom.borrow();
        host_arg_node(&dom, args, 0)
            .map(|id| dom.slot_assigned_nodes(id))
            .unwrap_or_default()
    };
    Ok(host_ids_array(ctx, ids))
}

fn host_next(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let dom = host_dom(ctx);
    let dom = dom.borrow();
    Ok(host_id_value(
        host_arg_node(&dom, args, 0).and_then(|id| dom.node(id).next_sibling),
    ))
}

fn host_prev(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let dom = host_dom(ctx);
    let dom = dom.borrow();
    Ok(host_id_value(
        host_arg_node(&dom, args, 0).and_then(|id| dom.node(id).prev_sibling),
    ))
}

fn host_clock_set(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let epoch_ms = match args.first() {
        Some(value) => ctx.coerce_number(value)?,
        None => f64::NAN,
    };
    if let Some(state) = ctx.host_mut::<HostState>() {
        state.clock.set_epoch_ms(epoch_ms);
    }
    Ok(Value::Undefined)
}

fn host_node_type(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let dom = host_dom(ctx);
    let dom = dom.borrow();
    let node_type = match host_arg_node(&dom, args, 0).map(|id| &dom.node(id).data) {
        Some(NodeData::Element { .. }) => 1,
        Some(NodeData::Text(_)) => 3,
        Some(NodeData::Comment(_)) => 8,
        Some(NodeData::Document) => 9,
        Some(NodeData::Doctype) => 10,
        Some(NodeData::Fragment) => 11,
        None => 0,
    };
    Ok(Value::Num(node_type as f64))
}

fn host_tag(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let dom = host_dom(ctx);
    let dom = dom.borrow();
    Ok(
        match host_arg_node(&dom, args, 0).and_then(|id| dom.tag_name(id)) {
            Some(tag) => Value::from_string(tag.to_owned()),
            None => Value::Null,
        },
    )
}

fn host_namespace(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let dom = host_dom(ctx);
    let dom = dom.borrow();
    Ok(
        match host_arg_node(&dom, args, 0).and_then(|id| dom.namespace_uri(id)) {
            Some(namespace) => Value::from_string(namespace.to_owned()),
            None => Value::Null,
        },
    )
}

fn host_get_attr(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let name = host_arg_string(ctx, args, 1);
    let dom = host_dom(ctx);
    let dom = dom.borrow();
    Ok(
        match host_arg_node(&dom, args, 0).and_then(|id| dom.attr(id, &name)) {
            Some(value) => Value::from_string(value.to_owned()),
            None => Value::Null,
        },
    )
}

fn host_set_attr(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let name = host_arg_string(ctx, args, 1);
    let value = host_arg_string(ctx, args, 2);
    let dom = host_dom(ctx);
    let mut dom = dom.borrow_mut();
    if let Some(id) = host_arg_node(&dom, args, 0) {
        dom.set_attr(id, &name, &value);
    }
    Ok(Value::Undefined)
}

fn host_remove_attr(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let name = host_arg_string(ctx, args, 1);
    let dom = host_dom(ctx);
    let mut dom = dom.borrow_mut();
    if let Some(id) = host_arg_node(&dom, args, 0) {
        dom.remove_attr(id, &name);
    }
    Ok(Value::Undefined)
}

fn host_attr_names(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let dom = host_dom(ctx);
    let names = {
        let dom = dom.borrow();
        host_arg_node(&dom, args, 0)
            .map(|id| dom.attr_names(id))
            .unwrap_or_default()
    };
    Ok(ctx.make_array(names.into_iter().map(Value::from_string).collect()))
}

fn host_text(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let dom = host_dom(ctx);
    let dom = dom.borrow();
    let text = host_arg_node(&dom, args, 0)
        .map(|id| {
            dom.comment_text(id)
                .map(str::to_owned)
                .unwrap_or_else(|| dom.text_content(id))
        })
        .unwrap_or_default();
    Ok(Value::from_string(text))
}

fn host_set_text(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let text = host_arg_string(ctx, args, 1);
    let dom = host_dom(ctx);
    let mut dom = dom.borrow_mut();
    if let Some(id) = host_arg_node(&dom, args, 0) {
        if dom.comment_text(id).is_some() {
            dom.set_comment_text(id, &text);
        } else {
            dom.set_text(id, &text);
        }
    }
    Ok(Value::Undefined)
}

fn host_inner_html(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let dom = host_dom(ctx);
    let dom = dom.borrow();
    let html = host_arg_node(&dom, args, 0)
        .map(|id| dom.inner_html(id))
        .unwrap_or_default();
    Ok(Value::from_string(html))
}

/// HTML §13.5 fragment parsing with the target element as the context. Template markup is directed
/// into its template-contents fragment, matching HTML's template insertion mode.
fn host_set_inner_html(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let html = host_arg_string(ctx, args, 1);
    let dom = host_dom(ctx);
    let mut dom = dom.borrow_mut();
    if let Some(id) = host_arg_node(&dom, args, 0) {
        let context_tag = dom.tag_name(id).unwrap_or("div").to_owned();
        let target = dom.content_target(id);
        for child in dom.children(target) {
            dom.detach(child);
        }
        for node in dom.parse_fragment_into(&context_tag, &html) {
            dom.append(target, node);
        }
    }
    Ok(Value::Undefined)
}

fn host_outer_html(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let dom = host_dom(ctx);
    let dom = dom.borrow();
    let html = host_arg_node(&dom, args, 0)
        .map(|id| dom.serialize_js(id))
        .unwrap_or_default();
    Ok(Value::from_string(html))
}

fn host_insert_adjacent(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let position = host_arg_string(ctx, args, 1);
    let html = host_arg_string(ctx, args, 2);
    let dom = host_dom(ctx);
    let mut dom = dom.borrow_mut();
    let Some(id) = host_arg_node(&dom, args, 0) else {
        return Ok(Value::Undefined);
    };
    let context_tag = match position.as_str() {
        "beforebegin" | "afterend" => dom
            .node(id)
            .parent
            .and_then(|parent| dom.tag_name(parent))
            .unwrap_or("div")
            .to_owned(),
        _ => dom.tag_name(id).unwrap_or("div").to_owned(),
    };
    let nodes = dom.parse_fragment_into(&context_tag, &html);
    match position.as_str() {
        "afterbegin" => {
            let first = dom.node(id).first_child;
            for node in nodes {
                dom.insert_before(id, node, first);
            }
        }
        "beforebegin" => {
            if let Some(parent) = dom.node(id).parent {
                for node in nodes {
                    dom.insert_before(parent, node, Some(id));
                }
            }
        }
        "afterend" => {
            if let Some(parent) = dom.node(id).parent {
                let after = dom.node(id).next_sibling;
                for node in nodes {
                    dom.insert_before(parent, node, after);
                }
            }
        }
        _ => {
            for node in nodes {
                dom.append(id, node);
            }
        }
    }
    Ok(Value::Undefined)
}

fn host_query(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let selector = host_arg_string(ctx, args, 1);
    let first_only = matches!(args.get(2), Some(Value::Bool(true)));
    let dom = host_dom(ctx);
    let ids = {
        let dom = dom.borrow();
        match (
            host_arg_node(&dom, args, 0),
            SelectorList::parse_cached(&selector),
        ) {
            (Some(root), Some(selector)) => dom.query(root, &selector, first_only),
            _ => Vec::new(),
        }
    };
    Ok(host_ids_array(ctx, ids))
}

fn host_matches(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let selector = host_arg_string(ctx, args, 1);
    let dom = host_dom(ctx);
    let dom = dom.borrow();
    let matches = match (
        host_arg_node(&dom, args, 0),
        SelectorList::parse_cached(&selector),
    ) {
        (Some(id), Some(selector)) => dom.matches(id, &selector),
        _ => false,
    };
    Ok(Value::Bool(matches))
}

fn host_get_by_id(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let target = host_arg_string(ctx, args, 0);
    let dom = host_dom(ctx);
    let dom = dom.borrow();
    Ok(host_id_value(dom.get_by_id(&target)))
}

fn host_upgrade_candidates(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let name = host_arg_string(ctx, args, 1).to_ascii_lowercase();
    let dom = host_dom(ctx);
    let ids = {
        let dom = dom.borrow();
        host_arg_node(&dom, args, 0)
            .map(|root| dom.elements_by_tag_composed(root, &name))
            .unwrap_or_default()
    };
    Ok(host_ids_array(ctx, ids))
}

fn host_ce_candidates(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let dom = host_dom(ctx);
    let ids = {
        let dom = dom.borrow();
        host_arg_node(&dom, args, 0)
            .map(|root| dom.custom_elements_composed(root))
            .unwrap_or_default()
    };
    Ok(host_ids_array(ctx, ids))
}

/// DOM §4.4 cloneNode, including HTML template contents via the shared arena clone algorithm.
fn host_clone(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let deep = matches!(args.get(1), Some(Value::Bool(true)));
    let dom = host_dom(ctx);
    let mut dom = dom.borrow_mut();
    let id = host_arg_node(&dom, args, 0).map(|id| dom.clone_subtree(id, deep));
    Ok(host_id_value(id))
}

fn host_doc_element(ctx: &mut Ctx, _this: Value, _args: &[Value]) -> Result<Value, Value> {
    let dom = host_dom(ctx);
    let dom = dom.borrow();
    Ok(host_id_value(
        dom.children(DOCUMENT)
            .into_iter()
            .find(|&child| dom.tag_name(child) == Some("html")),
    ))
}

/// ECMA-262 Annex B.3.6 requires the host-defined `document.all` exotic to participate in
/// language-level `typeof`, truthiness, and loose-equality exceptions. Lumen owns those semantics;
/// the browser adapter only requests the realm-local exotic.
fn host_html_dda(ctx: &mut Ctx, _this: Value, _args: &[Value]) -> Result<Value, Value> {
    Ok(ctx.make_html_dda())
}

fn host_attach_shadow(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let dom = host_dom(ctx);
    let mut dom = dom.borrow_mut();
    let root = host_arg_node(&dom, args, 0).map(|host| dom.attach_shadow(host));
    Ok(host_id_value(root))
}

fn host_shadow_root(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let dom = host_dom(ctx);
    let dom = dom.borrow();
    Ok(host_id_value(
        host_arg_node(&dom, args, 0).and_then(|host| dom.shadow_root(host)),
    ))
}

fn host_adopt_styles(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let css = host_arg_string(ctx, args, 1);
    let dom = host_dom(ctx);
    let mut dom = dom.borrow_mut();
    if let Some(scope) = host_arg_node(&dom, args, 0) {
        dom.set_adopted_styles(scope, &css);
    }
    Ok(Value::Undefined)
}

fn host_css_parse(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let css = host_arg_string(ctx, args, 0);
    Ok(Value::from_string(crate::dom::parse_cssom_json(&css)))
}

fn host_css_supports_selector(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let selector = host_arg_string(ctx, args, 0);
    Ok(Value::Bool(crate::dom::selector_parses(&selector)))
}

fn host_template_content(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let dom = host_dom(ctx);
    let dom = dom.borrow();
    Ok(host_id_value(
        host_arg_node(&dom, args, 0).map(|id| dom.content_target(id)),
    ))
}

/// WHATWG URL §4.4 basic URL parser, delegated to the standards-oriented `url` crate. The tuple is
/// the compact boundary representation consumed by the shared JavaScript `URL` wrapper.
fn host_url_parse(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let href = match args.first() {
        Some(value) => ctx.coerce_string(value)?.to_string(),
        None => "undefined".to_string(),
    };
    let base = match args.get(1) {
        None | Some(Value::Null | Value::Undefined) => None,
        Some(value) => Some(ctx.coerce_string(value)?.to_string()),
    };
    let parsed = match base {
        Some(base) => url::Url::parse(&base).and_then(|base| base.join(&href)),
        None => url::Url::parse(&href),
    };
    let Ok(url) = parsed else {
        return Ok(Value::Null);
    };
    Ok(host_url_parts(ctx, &url))
}

fn host_url_parts(ctx: &Ctx, url: &url::Url) -> Value {
    let host = match (url.host_str(), url.port()) {
        (Some(host), Some(port)) => format!("{host}:{port}"),
        (Some(host), None) => host.to_owned(),
        _ => String::new(),
    };
    let parts = [
        url.as_str().to_string(),
        format!("{}:", url.scheme()),
        host,
        url.host_str().unwrap_or("").to_string(),
        url.port().map(|port| port.to_string()).unwrap_or_default(),
        url.path().to_string(),
        url.query()
            .map(|query| format!("?{query}"))
            .unwrap_or_default(),
        url.fragment()
            .map(|fragment| format!("#{fragment}"))
            .unwrap_or_default(),
        url.origin().ascii_serialization(),
        url.username().to_string(),
        url.password().unwrap_or("").to_string(),
    ];
    ctx.make_array(parts.into_iter().map(Value::from_string).collect())
}

/// WHATWG URL component-setter algorithms. Setter validation failures are silent no-ops; only an
/// invalid starting URL or an unknown internal component name returns null.
fn host_url_set(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let href = host_arg_string(ctx, args, 0);
    let component = host_arg_string(ctx, args, 1);
    let value = host_arg_string(ctx, args, 2);
    let Ok(mut url) = url::Url::parse(&href) else {
        return Ok(Value::Null);
    };
    match component.as_str() {
        "protocol" => {
            let _ = url.set_scheme(value.strip_suffix(':').unwrap_or(&value));
        }
        "username" => {
            let _ = url.set_username(&value);
        }
        "password" => {
            let _ = url.set_password((!value.is_empty()).then_some(value.as_str()));
        }
        "host" => {
            if value.is_empty() {
                let _ = url.set_host(None);
            } else {
                let bare = lumen_host_without_port(&value);
                let _ = url.set_host(Some(bare));
                if bare.len() < value.len()
                    && let Ok(port) = value[bare.len() + 1..].parse::<u16>()
                {
                    let _ = url.set_port(Some(port));
                }
            }
        }
        "hostname" => {
            let bare = lumen_host_without_port(&value);
            let _ = url.set_host((!bare.is_empty()).then_some(bare));
        }
        "port" => {
            if value.is_empty() {
                let _ = url.set_port(None);
            } else if let Ok(port) = value.parse::<u16>() {
                let _ = url.set_port(Some(port));
            }
        }
        "pathname" => url.set_path(&value),
        "search" => {
            let query = value.strip_prefix('?').unwrap_or(&value);
            url.set_query((!query.is_empty()).then_some(query));
        }
        "hash" => {
            let fragment = value.strip_prefix('#').unwrap_or(&value);
            url.set_fragment((!fragment.is_empty()).then_some(fragment));
        }
        _ => return Ok(Value::Null),
    }
    Ok(host_url_parts(ctx, &url))
}

fn lumen_host_without_port(host: &str) -> &str {
    if let Some(rest) = host.strip_prefix('[') {
        return match rest.find(']') {
            Some(index) => &host[..index + 2],
            None => host,
        };
    }
    match host.rfind(':') {
        Some(index) => &host[..index],
        None => host,
    }
}

fn host_layout_environment(ctx: &mut Ctx) -> (url::Url, crate::layout2::Viewport, f32) {
    let state = ctx
        .host_mut::<HostState>()
        .expect("HostState installed before any Lumen host call");
    (
        state.base.clone(),
        state.viewport.get(),
        state.device_pixel_ratio.get(),
    )
}

/// Keep the geometry used by CSSOM View reads on the same epoch-keyed layout pass as TRust's Boa
/// adapter. The resulting rectangles remain floating-point CSS pixels; terminal quantization is
/// still confined to `layout2::paint`.
fn ensure_host_geom_cache(ctx: &mut Ctx) -> Rc<RefCell<LumenGeomCache>> {
    let (dom, base, viewport, cache, images) = {
        let state = ctx
            .host_mut::<HostState>()
            .expect("HostState installed before any Lumen host call");
        (
            state.dom.clone(),
            state.base.clone(),
            state.viewport.get(),
            state.geom_cache.clone(),
            state.images.clone(),
        )
    };
    let dom = dom.borrow();
    let epoch = dom.epoch();
    let mut cached = cache.borrow_mut();
    if cached.0 != epoch {
        let (forms, controls) = crate::http::extract_forms_arena(&dom, &base, None);
        let (boxes, tracks, scrolling_areas) = crate::layout2::measure_boxes_css(
            &dom,
            &base,
            viewport,
            &forms,
            &controls,
            &images.borrow(),
        );
        cached.1 = boxes;
        cached.2 = tracks;
        cached.3 = scrolling_areas;
        cached.0 = epoch;
    }
    drop(cached);
    cache
}

fn host_resolved_grid_tracks(ctx: &mut Ctx, args: &[Value], columns: bool) -> Option<String> {
    let cache = ensure_host_geom_cache(ctx);
    let id = {
        let dom = host_dom(ctx);
        let dom = dom.borrow();
        host_arg_node(&dom, args, 0)?
    };
    let cached = cache.borrow();
    let (column_tracks, row_tracks) = cached.2.get(&id)?;
    let tracks = if columns { column_tracks } else { row_tracks };
    if tracks.is_empty() {
        return None;
    }
    Some(
        tracks
            .iter()
            .map(|width| format!("{}px", width.round().max(0.0) as i64))
            .collect::<Vec<_>>()
            .join(" "),
    )
}

/// CSSOM §7.2/§9 resolved-value backing. Grid track lists are used values captured by the same
/// layout pass; all other properties come from the canonical DOM cascade.
fn host_computed_style(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let name = host_arg_string(ctx, args, 1);
    if (name == "grid-template-columns" || name == "grid-template-rows")
        && let Some(value) = host_resolved_grid_tracks(ctx, args, name == "grid-template-columns")
    {
        return Ok(Value::from_string(value));
    }
    let dom = host_dom(ctx);
    let dom = dom.borrow();
    Ok(
        match host_arg_node(&dom, args, 0).and_then(|id| dom.cssom_resolved_value(id, &name)) {
            Some(value) => Value::from_string(value),
            None => Value::Null,
        },
    )
}

/// CSSOM View §4.1: parse and evaluate the media query list against the document environment.
fn host_match_media(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let query = host_arg_string(ctx, args, 0);
    let viewport = args
        .get(1)
        .and_then(Value::as_num_opt)
        .zip(args.get(2).and_then(Value::as_num_opt))
        .filter(|(width, height)| {
            width.is_finite() && *width >= 0.0 && height.is_finite() && *height >= 0.0
        });
    let dom = host_dom(ctx);
    let dom = dom.borrow();
    let matches = match viewport {
        Some((width, height)) => dom.media_matches_at(&query, width as f32, height as f32),
        None => dom.media_matches(&query),
    };
    Ok(Value::Bool(matches))
}

/// HTML §4.8.4 exposes the absolute URL of the selected current image request.
fn host_image_current_src(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let (base, viewport, density) = host_layout_environment(ctx);
    let dom = host_dom(ctx);
    let dom = dom.borrow();
    let Some(id) = host_arg_node(&dom, args, 0) else {
        return Ok(Value::from_string(String::new()));
    };
    Ok(
        crate::responsive_image::select(&dom, id, &base, viewport, density).map_or_else(
            || Value::from_string(String::new()),
            |selected| Value::from_string(selected.source),
        ),
    )
}

/// HTML §4.8.4 `complete`: omitted/empty sources are complete. Until the frontend's resource
/// availability state is injected into this spike, synchronously available data URLs are the
/// selected requests that can be proven completely available.
fn host_image_complete(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let (base, viewport, density) = host_layout_environment(ctx);
    let dom = host_dom(ctx);
    let dom = dom.borrow();
    let Some(id) = host_arg_node(&dom, args, 0) else {
        return Ok(Value::Bool(false));
    };
    let src = dom.attr(id, "src").unwrap_or("").trim();
    let srcset = dom.attr(id, "srcset").unwrap_or("").trim();
    if src.is_empty() && srcset.is_empty() {
        return Ok(Value::Bool(true));
    }
    let complete = crate::responsive_image::select(&dom, id, &base, viewport, density)
        .is_some_and(|selected| selected.source.starts_with("data:"));
    Ok(Value::Bool(complete))
}

/// CSSOM View §6 bounding-box backing, sourced directly from canonical layout fragments.
fn host_rect(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let cache = ensure_host_geom_cache(ctx);
    let id = {
        let dom = host_dom(ctx);
        let dom = dom.borrow();
        host_arg_node(&dom, args, 0)
    };
    let rect = id.and_then(|id| cache.borrow().1.get(&id).copied());
    Ok(match rect {
        Some(rect) => ctx.make_array(vec![
            Value::Num(rect.left),
            Value::Num(rect.top),
            Value::Num(rect.width),
            Value::Num(rect.height),
        ]),
        None => Value::Null,
    })
}

/// CSSOM View §6 scroll metrics. Scrolling-area dimensions come from the layout fragment pass;
/// mutable offsets and client dimensions remain canonical DOM state.
fn host_scroll_get(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let which = args.get(1).and_then(Value::as_num_opt).unwrap_or(0.0) as u8;
    let scrolling_area = if matches!(which, 2 | 3) {
        let cache = ensure_host_geom_cache(ctx);
        let id = {
            let dom = host_dom(ctx);
            let dom = dom.borrow();
            host_arg_node(&dom, args, 0)
        };
        id.and_then(|id| cache.borrow().3.get(&id).copied())
    } else {
        None
    };
    let dom = host_dom(ctx);
    let dom = dom.borrow();
    let Some(id) = host_arg_node(&dom, args, 0) else {
        return Ok(Value::Null);
    };
    Ok(match dom.scroll_metric(id, which) {
        Some(value) => Value::Num(value),
        None if which == 2 => scrolling_area.map_or(Value::Null, |rect| Value::Num(rect.height)),
        None if which == 3 => scrolling_area.map_or(Value::Null, |rect| Value::Num(rect.width)),
        None => Value::Null,
    })
}

fn host_scroll_set(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let top = args.get(1).and_then(Value::as_num_opt).unwrap_or(0.0);
    let left = args.get(2).and_then(Value::as_num_opt).unwrap_or(0.0);
    let dom = host_dom(ctx);
    let mut dom = dom.borrow_mut();
    let Some(id) = host_arg_node(&dom, args, 0) else {
        return Ok(Value::Bool(false));
    };
    Ok(Value::Bool(dom.set_scroll_pos(id, top, left, true)))
}

/// HTML's iframe processing installs a parsed nested document and resolves its URLs at the frame
/// boundary. `Dom::install_frame_document` is shared by both engine adapters.
fn host_load_frame(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let html = host_arg_string(ctx, args, 1);
    let base = host_arg_string(ctx, args, 2);
    let dom = host_dom(ctx);
    let mut dom = dom.borrow_mut();
    if let Some(frame) = host_arg_node(&dom, args, 0) {
        dom.install_frame_document(frame, &html, &base);
    }
    Ok(Value::Undefined)
}

fn host_cookie_get(ctx: &mut Ctx, _this: Value, _args: &[Value]) -> Result<Value, Value> {
    let page = ctx
        .host_mut::<HostState>()
        .expect("HostState installed before any Lumen host call")
        .base
        .clone();
    Ok(Value::from_string(crate::http::cookies_for_js(&page)))
}

fn host_cookie_set(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let line = host_arg_string(ctx, args, 0);
    let page = ctx
        .host_mut::<HostState>()
        .expect("HostState installed before any Lumen host call")
        .base
        .clone();
    crate::http::set_cookie_from_js(&page, &line);
    Ok(Value::Undefined)
}

fn host_storage_bucket(ctx: &mut Ctx, args: &[Value]) -> (crate::js::WebStorage, String) {
    let kind = host_arg_string(ctx, args, 0);
    let state = ctx
        .host_mut::<HostState>()
        .expect("HostState installed before any Lumen host call");
    (
        state.storage.clone(),
        format!("{kind}:{}", state.base.origin().ascii_serialization()),
    )
}

fn host_storage_get(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let key = host_arg_string(ctx, args, 1);
    let (storage, bucket) = host_storage_bucket(ctx, args);
    let storage = storage.lock().unwrap();
    Ok(
        match storage.get(&bucket).and_then(|bucket| bucket.get(&key)) {
            Some(value) => Value::from_string(value.clone()),
            None => Value::Null,
        },
    )
}

fn host_storage_set(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let key = host_arg_string(ctx, args, 1);
    let value = host_arg_string(ctx, args, 2);
    let (storage, bucket) = host_storage_bucket(ctx, args);
    storage
        .lock()
        .unwrap()
        .entry(bucket)
        .or_default()
        .insert(key, value);
    Ok(Value::Undefined)
}

fn host_storage_remove(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let key = host_arg_string(ctx, args, 1);
    let (storage, bucket) = host_storage_bucket(ctx, args);
    if let Some(bucket) = storage.lock().unwrap().get_mut(&bucket) {
        bucket.remove(&key);
    }
    Ok(Value::Undefined)
}

fn host_storage_clear(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let (storage, bucket) = host_storage_bucket(ctx, args);
    storage.lock().unwrap().remove(&bucket);
    Ok(Value::Undefined)
}

fn host_storage_key(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let index = args.get(1).and_then(Value::as_num_opt).unwrap_or(-1.0);
    let (storage, bucket) = host_storage_bucket(ctx, args);
    let storage = storage.lock().unwrap();
    let key = (index >= 0.0)
        .then(|| {
            storage
                .get(&bucket)
                .and_then(|bucket| bucket.keys().nth(index as usize).cloned())
        })
        .flatten();
    Ok(key.map_or(Value::Null, Value::from_string))
}

fn host_storage_len(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let (storage, bucket) = host_storage_bucket(ctx, args);
    let len = storage
        .lock()
        .unwrap()
        .get(&bucket)
        .map_or(0, std::collections::HashMap::len);
    Ok(Value::Num(len as f64))
}

fn host_latin1_bytes(ctx: &mut Ctx, args: &[Value], index: usize) -> Vec<u8> {
    args.get(index)
        .and_then(|value| ctx.coerce_string(value).ok())
        .map(|string| {
            string
                .chars()
                .map(|character| character as u32 as u8)
                .collect()
        })
        .unwrap_or_default()
}

fn host_blob_mirror(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let url = host_arg_string(ctx, args, 0);
    let bytes = host_latin1_bytes(ctx, args, 1);
    let mime = host_arg_string(ctx, args, 2);
    if !url.is_empty() {
        let blobs = ctx
            .host_mut::<HostState>()
            .expect("HostState installed before any Lumen host call")
            .blobs
            .clone();
        blobs.lock().unwrap().insert(url, (bytes, mime));
    }
    Ok(Value::Undefined)
}

fn host_resolved_promise(ctx: &mut Ctx, value: Value) -> Result<Value, Value> {
    let global = ctx.global_this();
    let promise = ctx.member_get(&global, "Promise")?;
    let resolve = ctx.member_get(&promise, "resolve")?;
    ctx.invoke(resolve, promise.clone(), &[value])
}

/// Web Crypto §14.3.5 copies the `BufferSource` bytes before digesting and resolves the returned
/// promise with a realm-local `ArrayBuffer` containing the digest.
fn host_crypto_sha256_digest(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    use sha2::Digest as _;

    let input = args
        .first()
        .and_then(|value| ctx.buffer_source_bytes(value, false))
        .unwrap_or_default();
    let digest = sha2::Sha256::digest(input);
    let view = ctx.make_uint8array(&digest)?;
    let buffer = ctx.member_get(&view, "buffer")?;
    host_resolved_promise(ctx, buffer)
}

/// Compression Streams §4's compression operation. The JavaScript TransformStream owns chunking
/// and invokes this once with the copied, bounded aggregate at flush time.
fn host_compression_encode(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    use std::io::Write as _;

    const MAX_STREAM_CODEC_BYTES: usize = 16 * 1024 * 1024;
    let format = host_arg_string(ctx, args, 0);
    let input = args
        .get(1)
        .and_then(|value| ctx.buffer_source_bytes(value, false))
        .ok_or_else(|| {
            ctx.make_error(
                "TypeError",
                "CompressionStream input must be a BufferSource",
            )
        })?;
    if input.len() > MAX_STREAM_CODEC_BYTES {
        return Err(ctx.make_error(
            "RangeError",
            "CompressionStream input exceeds the 16 MiB page limit",
        ));
    }
    let output = match format.as_str() {
        "deflate" => {
            let mut encoder =
                flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
            encoder.write_all(&input).and_then(|()| encoder.finish())
        }
        "deflate-raw" => {
            let mut encoder =
                flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
            encoder.write_all(&input).and_then(|()| encoder.finish())
        }
        "gzip" => {
            let mut encoder =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            encoder.write_all(&input).and_then(|()| encoder.finish())
        }
        _ => return Err(ctx.make_error("TypeError", "Unsupported compression format")),
    }
    .map_err(|error| ctx.make_error("TypeError", format!("CompressionStream failed: {error}")))?;
    if output.len() > MAX_STREAM_CODEC_BYTES {
        return Err(ctx.make_error(
            "RangeError",
            "CompressionStream output exceeds the 16 MiB page limit",
        ));
    }
    ctx.make_uint8array(&output)
}

/// Encoding §7.4 UTF-8 encode: the Web IDL `USVString` conversion has already replaced lone
/// surrogates before the host call, and the result is a fresh realm-local `Uint8Array`.
fn host_text_encode(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let text = host_arg_string(ctx, args, 0);
    ctx.make_uint8array(text.as_bytes())
}

fn host_dom_popover(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let open = matches!(args.get(1), Some(Value::Bool(true)));
    let dom = host_dom(ctx);
    let mut dom = dom.borrow_mut();
    if let Some(id) = host_arg_node(&dom, args, 0) {
        dom.set_popover_open(id, open);
    }
    Ok(Value::Undefined)
}

fn eval(engine: &mut lumen::Engine, source: &str, label: &str) -> Result<(), String> {
    eval_value(engine, source, label).map(|_| ())
}

fn eval_value(engine: &mut lumen::Engine, source: &str, label: &str) -> Result<Value, String> {
    match engine.eval_value_interruptible(source) {
        Err(error) => Err(format!(
            "{label} parse error at line {}: {}",
            error.line, error.message
        )),
        Ok(Err(error)) => Err(describe_eval_error(engine, error, label)),
        Ok(Ok(value)) => Ok(value),
    }
}

fn describe_eval_error(engine: &mut lumen::Engine, error: EvalError, label: &str) -> String {
    match error {
        EvalError::Throw(thrown) => describe_throw(engine, thrown, label),
        EvalError::Interrupted(reason) => format!("{label} interrupted: {}", reason.message()),
    }
}

fn describe_throw(engine: &mut lumen::Engine, thrown: Value, label: &str) -> String {
    let rendered = value_string(engine, &thrown);
    let name = engine
        .ctx()
        .get_member(&thrown, "name")
        .ok()
        .map(|value| value_string(engine, &value))
        .filter(|name| !name.is_empty() && name != "undefined");
    let message = engine
        .ctx()
        .get_member(&thrown, "message")
        .ok()
        .map(|value| value_string(engine, &value))
        .filter(|message| !message.is_empty() && message != "undefined");
    match (name, message) {
        (Some(name), Some(message)) => format!("{label} threw {name}: {message}"),
        _ => format!("{label} threw {rendered}"),
    }
}

fn value_string(engine: &mut lumen::Engine, value: &Value) -> String {
    engine
        .ctx()
        .coerce_string(value)
        .map(|string| string.to_string())
        .unwrap_or_else(|_| format!("<{}>", value.type_of()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn platform_engine() -> lumen::Engine {
        let clock = Rc::new(RealmClock::new());
        configured_engine(
            HostState::new(Rc::new(RefCell::new(Dom::new())), clock),
            DEFAULT_URL,
        )
    }

    fn configured_engine(state: HostState, url: &str) -> lumen::Engine {
        let mut engine = lumen::Engine::new();
        engine.set_tier(Tier::Interp);
        let clock = state.clock.clone();
        let engine_clock = clock.clone();
        engine.set_wall_clock(move || engine_clock.now_ms());
        state.configure_module_loading(&mut engine);
        engine.ctx().op_state().put(state);
        install_host_boundary(&mut engine);
        eval(
            &mut engine,
            &format!("globalThis.__trust_cfg = {{ url: {url:?}, width: 640, height: 384 }};"),
            "configuration",
        )
        .unwrap();
        eval(&mut engine, crate::js::PRELUDE, "prelude").unwrap();
        engine
    }

    fn run_microtask_checkpoint(engine: &mut lumen::Engine) {
        engine
            .run_microtasks_interruptible()
            .unwrap_or_else(|reason| {
                panic!("microtask checkpoint interrupted: {}", reason.message())
            });
    }

    fn call_trust_method(engine: &mut lumen::Engine, name: &str, args: &[Value]) -> Value {
        let global = engine.global_this();
        let trust = engine
            .ctx()
            .get_member(&global, "__trust")
            .unwrap_or_else(|_| panic!("read __trust"));
        let method = engine
            .ctx()
            .get_member(&trust, name)
            .unwrap_or_else(|_| panic!("read __trust.{name}"));
        match engine.call_function_interruptible(&method, trust, args) {
            Ok(value) => value,
            Err(error) => panic!("{}", describe_eval_error(engine, error, name)),
        }
    }

    fn string_value(engine: &mut lumen::Engine, expression: &str) -> String {
        let value = eval_value(engine, expression, expression).unwrap();
        value_string(engine, &value)
    }

    async fn read_test_client_frame(stream: &mut tokio::net::TcpStream) -> (u8, Vec<u8>) {
        use tokio::io::AsyncReadExt as _;

        let mut head = [0u8; 2];
        stream.read_exact(&mut head).await.unwrap();
        assert_ne!(head[1] & 0x80, 0, "RFC 6455 client frames are masked");
        let mut length = u64::from(head[1] & 0x7f);
        if length == 126 {
            let mut extended = [0u8; 2];
            stream.read_exact(&mut extended).await.unwrap();
            length = u64::from(u16::from_be_bytes(extended));
        } else if length == 127 {
            let mut extended = [0u8; 8];
            stream.read_exact(&mut extended).await.unwrap();
            length = u64::from_be_bytes(extended);
        }
        let mut mask = [0u8; 4];
        stream.read_exact(&mut mask).await.unwrap();
        let mut payload = vec![0; usize::try_from(length).unwrap()];
        stream.read_exact(&mut payload).await.unwrap();
        for (index, byte) in payload.iter_mut().enumerate() {
            *byte ^= mask[index & 3];
        }
        (head[0] & 0x0f, payload)
    }

    async fn write_test_server_frame(
        stream: &mut tokio::net::TcpStream,
        opcode: u8,
        payload: &[u8],
    ) {
        use tokio::io::AsyncWriteExt as _;

        let mut frame = vec![0x80 | opcode];
        match payload.len() {
            length @ 0..=125 => frame.push(length as u8),
            length @ 126..=65535 => {
                frame.push(126);
                frame.extend_from_slice(&(length as u16).to_be_bytes());
            }
            length => {
                frame.push(127);
                frame.extend_from_slice(&(length as u64).to_be_bytes());
            }
        }
        frame.extend_from_slice(payload);
        stream.write_all(&frame).await.unwrap();
        stream.flush().await.unwrap();
    }

    #[test]
    fn tier_names_are_explicit() {
        assert_eq!(parse_tier("interp").unwrap(), Tier::Interp);
        assert_eq!(parse_tier("bytecode").unwrap(), Tier::Bytecode);
        assert_eq!(parse_tier("jit").unwrap(), Tier::Jit);
        assert!(parse_tier("fast").is_err());
    }

    #[test]
    fn data_url_media_types_follow_the_fetch_processor() {
        assert_eq!(
            data_url_content_type("data:,plain"),
            "text/plain;charset=US-ASCII"
        );
        assert_eq!(
            data_url_content_type("data:;charset=utf-8,plain"),
            "text/plain;charset=utf-8"
        );
        assert_eq!(
            data_url_content_type("data:text/javascript;charset=utf-8;BaSe64,ZXhwb3J0IHt9"),
            "text/javascript;charset=utf-8"
        );
    }

    #[test]
    fn lumen_registry_is_a_unique_arity_checked_subset_of_the_host_boundary() {
        let canonical: Vec<_> = crate::js::host_boundary_signatures().collect();
        assert_eq!(canonical.len(), 101, "canonical host boundary changed");
        assert_eq!(
            canonical
                .iter()
                .map(|(name, _)| *name)
                .collect::<std::collections::HashSet<_>>()
                .len(),
            canonical.len(),
            "canonical host boundary contains a duplicate name"
        );
        assert!(lumen_registry_matches_canonical_boundary());
        assert_eq!(LUMEN_HOST_FUNCTIONS.len(), 101);

        let mut engine = platform_engine();
        for &(name, length, _) in LUMEN_HOST_FUNCTIONS {
            let actual = eval_value(&mut engine, &format!("{name}.length"), name).unwrap();
            assert_eq!(actual.as_num_opt(), Some(length as f64), "{name}.length");
        }
    }

    #[test]
    fn webassembly_boundary_preserves_store_identity_memory_and_reentry() {
        // WebAssembly JS Interface §§4.1–4.2 and 5.1–5.6: one store per agent, one wrapper per
        // address, imported host calls see the current memory Data Block, memory growth detaches
        // the previous fixed buffer, and i64 crosses the JS boundary as BigInt.
        let mut engine = platform_engine();
        let mut module = wat::parse_str(
            r#"
            (module
              (import "env" "observe" (func $observe (result i32)))
              (import "env" "boom" (func $boom))
              (import "env" "g" (global $g (mut i32)))
              (import "env" "memory" (memory $memory 1 3))
              (import "env" "table" (table $table 1 3 externref))
              (export "memory" (memory $memory))
              (export "table" (table $table))
              (export "g" (global $g))
              (func (export "addGlobal") (param i32) (result i32)
                local.get 0 global.get $g i32.add)
              (func (export "read2") (result i32)
                i32.const 2 i32.load8_u)
              (func (export "write0") (param i32)
                i32.const 0 local.get 0 i32.store8)
              (func (export "bridge") (result i32)
                i32.const 0 i32.const 37 i32.store8
                call $observe
                i32.const 1 i32.load8_u
                i32.add)
              (func (export "growInside") (result i32)
                i32.const 1 memory.grow)
              (func (export "callBoom") call $boom)
              (global (export "big") (mut i64) (i64.const -2)))
            "#,
        )
        .unwrap();
        // A custom section named "note" with payload [1, 2, 3].
        module.extend_from_slice(&[0, 8, 4, b'n', b'o', b't', b'e', 1, 2, 3]);
        let forwarding = wat::parse_str(
            r#"
            (module
              (import "x" "f" (func $f (param i32) (result i32)))
              (export "f" (func $f)))
            "#,
        )
        .unwrap();
        let reentrant = wat::parse_str(
            r#"
            (module
              (import "env" "instantiate" (func $instantiate (result i32)))
              (func (export "run") (result i32) call $instantiate))
            "#,
        )
        .unwrap();
        let nested = wat::parse_str(
            r#"
            (module
              (import "env" "started" (func $started))
              (func $start call $started)
              (start $start)
              (func (export "answer") (result i32) i32.const 23))
            "#,
        )
        .unwrap();
        let module_value = engine
            .ctx()
            .make_uint8array(&module)
            .unwrap_or_else(|_| panic!("make wasm fixture"));
        let forwarding_value = engine
            .ctx()
            .make_uint8array(&forwarding)
            .unwrap_or_else(|_| panic!("make forwarding fixture"));
        let reentrant_value = engine
            .ctx()
            .make_uint8array(&reentrant)
            .unwrap_or_else(|_| panic!("make reentrant fixture"));
        let nested_value = engine
            .ctx()
            .make_uint8array(&nested)
            .unwrap_or_else(|_| panic!("make nested fixture"));
        let global = engine.global_this();
        engine
            .ctx()
            .member_set(&global, "wasmFixture", module_value)
            .unwrap_or_else(|_| panic!("install wasm fixture"));
        engine
            .ctx()
            .member_set(&global, "wasmForwardingFixture", forwarding_value)
            .unwrap_or_else(|_| panic!("install forwarding fixture"));
        engine
            .ctx()
            .member_set(&global, "wasmReentrantFixture", reentrant_value)
            .unwrap_or_else(|_| panic!("install reentrant fixture"));
        engine
            .ctx()
            .member_set(&global, "wasmNestedFixture", nested_value)
            .unwrap_or_else(|_| panic!("install nested fixture"));

        eval(
            &mut engine,
            r#"
            const wasmModule = new WebAssembly.Module(wasmFixture);
            const importGlobal = new WebAssembly.Global({ value: "i32", mutable: true }, 4);
            const importMemory = new WebAssembly.Memory({ initial: 1, maximum: 3 });
            const importTable = new WebAssembly.Table({ element: "externref", initial: 1, maximum: 3 });
            let observed = -1;
            let wasmInstance;
            const sentinel = { sentinel: true };
            const imports = { env: {
                g: importGlobal,
                memory: importMemory,
                table: importTable,
                observe() {
                    const bytes = new Uint8Array(importMemory.buffer);
                    observed = bytes[0];
                    bytes[1] = 5;
                    return wasmInstance.exports.addGlobal(observed);
                },
                boom() { throw sentinel; }
            }};
            wasmInstance = new WebAssembly.Instance(wasmModule, imports);
            const firstBuffer = importMemory.buffer;
            const sameBuffer = firstBuffer === importMemory.buffer;
            new Uint8Array(firstBuffer)[2] = 8;
            const readFromJs = wasmInstance.exports.read2();
            wasmInstance.exports.write0(11);
            const writtenFromWasm = new Uint8Array(firstBuffer)[0];
            const bridge = wasmInstance.exports.bridge();
            const internalOldPages = wasmInstance.exports.growInside();
            const detachedAfterInternalGrow = firstBuffer.byteLength;
            const secondBuffer = importMemory.buffer;
            const explicitOldPages = importMemory.grow(1);
            const detachedAfterExplicitGrow = secondBuffer.byteLength;

            const marker = { marker: 1 };
            importTable.set(0, marker);
            const tableIdentity = importTable.get(0) === marker;
            const oldTableLength = importTable.grow(1);
            const tableDefault = importTable.get(1) === undefined;
            const anyfunc = new WebAssembly.Table({ element: "anyfunc", initial: 1 });
            const anyfuncDefault = anyfunc.get(0) === null;
            let explicitUndefinedRejected = false;
            try { anyfunc.set(0, undefined); } catch (error) {
                explicitUndefinedRejected = error instanceof TypeError;
            }

            const forwardingModule = new WebAssembly.Module(wasmForwardingFixture);
            const forwardingInstance = new WebAssembly.Instance(forwardingModule, {
                x: { f: wasmInstance.exports.addGlobal }
            });
            const functionIdentity = forwardingInstance.exports.f === wasmInstance.exports.addGlobal;
            let nestedStarts = 0;
            let nestedBoundary = false;
            const reentrantInstance = new WebAssembly.Instance(
                new WebAssembly.Module(wasmReentrantFixture),
                { env: { instantiate() {
                    const nestedModule = new WebAssembly.Module(wasmNestedFixture);
                    const nestedGlobal = new WebAssembly.Global({ value: "i32" }, 7);
                    const nestedMemory = new WebAssembly.Memory({ initial: 1 });
                    const nestedTable = new WebAssembly.Table({ element: "externref", initial: 1 });
                    nestedBoundary = WebAssembly.validate(wasmNestedFixture) &&
                        WebAssembly.Module.imports(nestedModule)[0].name === "started" &&
                        WebAssembly.Module.exports(nestedModule)[0].name === "answer" &&
                        WebAssembly.Module.customSections(nestedModule, "missing").length === 0 &&
                        nestedGlobal.value === 7 && nestedMemory.buffer.byteLength === 65536 &&
                        nestedTable.get(0) === undefined;
                    const inner = new WebAssembly.Instance(nestedModule, {
                        env: { started() { nestedStarts++; } }
                    });
                    return inner.exports.answer();
                } } }
            );
            const nestedInstantiation = reentrantInstance.exports.run() === 23 &&
                nestedStarts === 1 && nestedBoundary;
            let throwIdentity = false;
            try { wasmInstance.exports.callBoom(); }
            catch (error) { throwIdentity = error === sentinel; }
            const custom = WebAssembly.Module.customSections(wasmModule, "note")[0];
            const descriptors = [
                WebAssembly.Module.imports(wasmModule).map(v => v.kind).join(','),
                WebAssembly.Module.exports(wasmModule).map(v => v.kind).join(','),
                Array.from(new Uint8Array(custom)).join(',')
            ].join(';');
            const identity = wasmInstance.exports.memory === importMemory &&
                wasmInstance.exports.table === importTable && wasmInstance.exports.g === importGlobal;
            const big = wasmInstance.exports.big.value;
            wasmInstance.exports.big.value = 9n;
            const globalDefaults = new WebAssembly.Global({ value: "f64" }).value === 0 &&
                new WebAssembly.Global({ value: "externref" }).value === undefined;
            let badMemory = false, badTable = false, keyedTransfer = false;
            try { new WebAssembly.Memory({ initial: 4294967296 }); }
            catch (error) { badMemory = error instanceof TypeError; }
            try { new WebAssembly.Table({ element: "externref" }); }
            catch (error) { badTable = error instanceof TypeError; }
            try { importMemory.buffer.transfer(); }
            catch (error) { keyedTransfer = error instanceof TypeError; }

            globalThis.wasmResult = [
                WebAssembly.validate(wasmFixture), sameBuffer, readFromJs, writtenFromWasm,
                observed, bridge, internalOldPages, detachedAfterInternalGrow,
                explicitOldPages, detachedAfterExplicitGrow, tableIdentity, oldTableLength,
                tableDefault, anyfuncDefault, explicitUndefinedRejected, functionIdentity,
                nestedInstantiation, throwIdentity, identity, String(big),
                String(wasmInstance.exports.big.value), globalDefaults,
                badMemory, badTable, keyedTransfer, descriptors
            ].join('|');
            "#,
            "WebAssembly Lumen boundary",
        )
        .unwrap();

        assert_eq!(
            string_value(&mut engine, "wasmResult"),
            "true|true|8|11|37|46|1|0|2|0|true|1|true|true|true|true|true|true|true|-2|9|true|true|true|true|function,function,global,memory,table;memory,table,global,function,function,function,function,function,function,global;1,2,3"
        );
    }

    #[test]
    fn fetch_completion_is_a_networking_task_with_byte_exact_body() {
        // Fetch §5.6 creates the promise before fetching in parallel; Fetch §2 queues response
        // processing on the networking task source; HTML §8.1.7.3 then performs one microtask
        // checkpoint. The fourth host-array item remains a BufferSource so binary bytes survive.
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let page = url::Url::parse(DEFAULT_URL).unwrap();
        let response_url = page.join("api").unwrap();
        let cache = Arc::new(crate::http::PageCache::default());
        cache.seed_with_headers(
            response_url.to_string(),
            206,
            String::from("application/octet-stream"),
            vec![
                (
                    String::from("content-type"),
                    String::from("application/octet-stream"),
                ),
                (String::from("x-result"), String::from("exact")),
            ],
            vec![0, 0x80, 0xff, 65],
        );
        let (task_tx, mut task_rx) = tokio::sync::mpsc::unbounded_channel();
        let clock = Rc::new(RealmClock::new());
        let mut state = HostState::new(Rc::new(RefCell::new(Dom::new())), clock);
        state.enable_network(page, runtime.handle().clone(), cache, task_tx);
        let mut engine = configured_engine(state, DEFAULT_URL);

        eval(
            &mut engine,
            r#"
                globalThis.fetchOrder = [];
                fetch('/api').then(function (response) {
                    fetchOrder.push('response:' + response.status + ':' + response.headers.get('x-result'));
                    return response.arrayBuffer();
                }).then(function (buffer) {
                    fetchOrder.push('bytes:' + Array.from(new Uint8Array(buffer)).join(','));
                }, function (error) {
                    fetchOrder.push('error:' + error);
                });
                fetchOrder.push('script');
            "#,
            "start fetch",
        )
        .unwrap();
        run_microtask_checkpoint(&mut engine);
        assert_eq!(string_value(&mut engine, "fetchOrder.join('|')"), "script");
        assert_eq!(
            engine
                .ctx()
                .host_mut::<HostState>()
                .and_then(|state| state.network.as_ref())
                .map(|network| network.pending_fetches.len()),
            Some(1)
        );

        let task = runtime
            .block_on(async { tokio::time::timeout(Duration::from_secs(2), task_rx.recv()).await })
            .expect("network task completes")
            .expect("network task channel remains open");
        dispatch_host_task(&mut engine, task).unwrap();
        assert_eq!(
            string_value(&mut engine, "fetchOrder.join('|')"),
            "script",
            "settling the promise does not inline its reactions into the networking task"
        );
        run_microtask_checkpoint(&mut engine);
        assert_eq!(
            string_value(&mut engine, "fetchOrder.join('|')"),
            "script|response:206:exact|bytes:0,128,255,65"
        );
        assert_eq!(
            engine
                .ctx()
                .host_mut::<HostState>()
                .and_then(|state| state.network.as_ref())
                .map(|network| network.pending_fetches.len()),
            Some(0)
        );
    }

    #[test]
    fn fetch_without_a_network_grant_rejects_at_the_microtask_checkpoint() {
        let mut engine = platform_engine();
        eval(
            &mut engine,
            "globalThis.fetchOrder = ['script']; fetch('/blocked').catch(() => fetchOrder.push('rejected'));",
            "blocked fetch",
        )
        .unwrap();
        assert_eq!(string_value(&mut engine, "fetchOrder.join('|')"), "script");
        run_microtask_checkpoint(&mut engine);
        assert_eq!(
            string_value(&mut engine, "fetchOrder.join('|')"),
            "script|rejected"
        );
    }

    #[test]
    fn synchronous_xhr_boundary_blocks_only_the_page_thread_and_preserves_bytes() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        // XHR §3.5.6 permits the synchronous flag to pause its Window task. The actual I/O must
        // remain on TRust's runtime, both to keep the browser responsive and to avoid nested
        // runtime entry when a synchronous request originates in a JS callback.
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let listener =
            runtime.block_on(async { tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap() });
        let address = listener.local_addr().unwrap();
        let (request_tx, request_rx) = std::sync::mpsc::channel();
        runtime.spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0u8; 2048];
            let header_end = loop {
                let read = socket.read(&mut buffer).await.unwrap();
                assert_ne!(read, 0, "request ended before its headers");
                request.extend_from_slice(&buffer[..read]);
                if let Some(index) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                    break index + 4;
                }
            };
            let headers = String::from_utf8_lossy(&request[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    line.strip_prefix("Content-Length: ")
                        .or_else(|| line.strip_prefix("content-length: "))
                })
                .and_then(|length| length.trim().parse::<usize>().ok())
                .unwrap_or(0);
            while request.len() < header_end + content_length {
                let read = socket.read(&mut buffer).await.unwrap();
                assert_ne!(read, 0, "request ended before its body");
                request.extend_from_slice(&buffer[..read]);
            }
            request_tx.send(request).unwrap();
            socket
                .write_all(
                    b"HTTP/1.1 201 Created\r\nContent-Type: application/octet-stream\r\nX-Answer: exact\r\nContent-Length: 4\r\nConnection: close\r\n\r\n\0\x80\xffA",
                )
                .await
                .unwrap();
        });

        let page_url = format!("http://{address}/page");
        let page = url::Url::parse(&page_url).unwrap();
        let cache = Arc::new(crate::http::PageCache::default());
        let (task_tx, _task_rx) = tokio::sync::mpsc::unbounded_channel();
        let clock = Rc::new(RealmClock::new());
        let mut state = HostState::new(Rc::new(RefCell::new(Dom::new())), clock);
        state.enable_network(page, runtime.handle().clone(), cache, task_tx);
        let mut engine = configured_engine(state, &page_url);
        eval(
            &mut engine,
            r#"
                const syncResponse = __http_fetch(
                    '/sync', 'POST', String.fromCharCode(0, 128, 255),
                    'application/octet-stream', 'x-custom\nyes'
                );
                globalThis.syncFetchResult = [
                    syncResponse[0], syncResponse[1],
                    syncResponse[4].indexOf('x-answer\nexact') >= 0,
                    Array.from(syncResponse[3]).join(',')
                ].join('|');
            "#,
            "synchronous fetch",
        )
        .unwrap();
        assert_eq!(
            string_value(&mut engine, "syncFetchResult"),
            "201|application/octet-stream|true|0,128,255,65"
        );

        let request = request_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("server observed request");
        let header_end = request
            .windows(4)
            .position(|bytes| bytes == b"\r\n\r\n")
            .map(|index| index + 4)
            .unwrap();
        let headers = String::from_utf8_lossy(&request[..header_end]).to_ascii_lowercase();
        assert!(headers.starts_with("post /sync http/1.1\r\n"), "{headers}");
        assert!(headers.contains("x-custom: yes\r\n"), "{headers}");
        assert!(
            headers.contains(&format!("referer: {page_url}\r\n")),
            "{headers}"
        );
        assert_eq!(&request[header_end..], &[0, 0x80, 0xff]);
    }

    #[test]
    fn workers_use_lumen_realms_and_preserve_task_microtask_order() {
        // HTML §10.2.4/§10.2.6 and §8.1.7: each Worker gets a distinct dedicated agent;
        // incoming port messages and timers are tasks with a microtask checkpoint between them.
        // MessagePort post-message steps clone immediately and dispatch trusted MessageEvents with
        // an empty origin. Module workers run as modules and importScripts() rejects there.
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let page = url::Url::parse(DEFAULT_URL).unwrap();
        let cache = Arc::new(crate::http::PageCache::default());
        let (task_tx, mut task_rx) = tokio::sync::mpsc::unbounded_channel();
        let clock = Rc::new(RealmClock::new());
        let mut state = HostState::new(Rc::new(RefCell::new(Dom::new())), clock);
        state.enable_network(page.clone(), runtime.handle().clone(), cache, task_tx);
        let mut engine = configured_engine(state, page.as_str());

        let classic_source = r#"
            var workerOrder = [];
            var workerWasm = new WebAssembly.Instance(new WebAssembly.Module(new Uint8Array([
                0,97,115,109,1,0,0,0,1,5,1,96,0,1,127,3,2,1,0,7,10,1,6,
                97,110,115,119,101,114,0,0,10,6,1,4,0,65,42,11
            ]))).exports.answer();
            addEventListener('message', function (event) { workerOrder.push('listener'); });
            onmessage = function (event) {
                workerOrder.push('handler');
                setTimeout(function () {
                    postMessage({ kind: 'timer1', cycle: event.data === event.data.self,
                        workerOrder: workerOrder.join(','), trusted: event.isTrusted,
                        origin: event.origin, workerName: self.name, wasm: workerWasm });
                    Promise.resolve().then(function () { postMessage({ kind: 'micro' }); });
                }, 0);
                setTimeout(function () { postMessage({ kind: 'timer2' }); }, 0);
            };
        "#;
        let module_source = r#"
            export const answer = 42;
            var importError = '';
            try { importScripts('data:text/javascript,'); }
            catch (error) { importError = error.name; }
            postMessage({ kind: 'module', answer: answer, importError: importError });
        "#;
        let data_url = |source: &str| {
            // A data URL is not form-urlencoded: `+` is a literal plus, not a
            // space. Percent-encode every source byte so the Fetch data-URL
            // processor reconstructs the script exactly.
            let encoded = source
                .as_bytes()
                .iter()
                .map(|byte| format!("%{byte:02X}"))
                .collect::<Vec<_>>()
                .join("");
            format!("data:text/javascript,{}", encoded)
        };
        let classic_url = serde_json::to_string(&data_url(classic_source)).unwrap();
        let module_url = serde_json::to_string(&data_url(module_source)).unwrap();
        let spinner_url = serde_json::to_string(&data_url("while (true) {}")).unwrap();
        eval(
            &mut engine,
            &format!(
                r#"
                globalThis.workerLog = [];
                globalThis.workerTrusted = true;
                globalThis.workerOrigins = [];
                globalThis.workerErrors = 0;
                try {{ new Worker('http://['); }} catch (error) {{ workerLog.push('bad-url:' + error.name); }}
                try {{ new Worker('data:text/javascript,', {{ type: 'invalid' }}); }} catch (error) {{ workerLog.push('bad-type:' + error.name); }}

                globalThis.classicWorker = new Worker({classic_url}, {{ name: 'echo' }});
                classicWorker.addEventListener('message', function (event) {{
                    workerLog.push('listener:' + event.data.kind);
                    workerTrusted = workerTrusted && event.isTrusted;
                    workerOrigins.push(event.origin);
                }});
                classicWorker.onmessage = function (event) {{
                    workerLog.push('handler:' + event.data.kind);
                    if (event.data.kind === 'timer1') globalThis.timer1 = event.data;
                }};
                classicWorker.onerror = function () {{ workerErrors++; }};
                var cyclic = {{ value: 41 }}; cyclic.self = cyclic;
                classicWorker.postMessage(cyclic);

                globalThis.moduleWorker = new Worker({module_url}, {{ type: 'module' }});
                moduleWorker.onmessage = function (event) {{
                    workerLog.push('module:' + event.data.answer + ':' + event.data.importError);
                    workerTrusted = workerTrusted && event.isTrusted;
                    workerOrigins.push(event.origin);
                }};
                moduleWorker.onerror = function () {{ workerErrors++; }};

                globalThis.spinnerWorker = new Worker({spinner_url});
                spinnerWorker.onerror = function () {{ workerErrors++; }};
                spinnerWorker.terminate();
                "#
            ),
            "Worker setup",
        )
        .unwrap();

        for _ in 0..12 {
            let log = string_value(&mut engine, "workerLog.join('|')");
            if log.contains("handler:timer2") && log.contains("module:42:TypeError") {
                break;
            }
            let task = runtime
                .block_on(async {
                    tokio::time::timeout(Duration::from_secs(15), task_rx.recv()).await
                })
                .expect("Worker task completes")
                .expect("Worker task channel remains open");
            dispatch_host_task(&mut engine, task).unwrap();
            run_microtask_checkpoint(&mut engine);
        }

        let log = string_value(&mut engine, "workerLog.join('|')");
        assert!(
            log.starts_with("bad-url:SyntaxError|bad-type:TypeError"),
            "{log}"
        );
        assert!(log.contains("module:42:TypeError"), "{log}");
        let timer1 = log.find("listener:timer1|handler:timer1").unwrap();
        let microtask = log.find("listener:micro|handler:micro").unwrap();
        let timer2 = log.find("listener:timer2|handler:timer2").unwrap();
        assert!(
            timer1 < microtask && microtask < timer2,
            "one timer task and its microtasks must precede the next timer task: {log}"
        );
        assert_eq!(string_value(&mut engine, "String(timer1.cycle)"), "true");
        assert_eq!(
            string_value(&mut engine, "timer1.workerOrder"),
            "listener,handler"
        );
        assert_eq!(string_value(&mut engine, "String(timer1.trusted)"), "true");
        assert_eq!(string_value(&mut engine, "timer1.origin"), "");
        assert_eq!(string_value(&mut engine, "timer1.workerName"), "echo");
        assert_eq!(string_value(&mut engine, "String(timer1.wasm)"), "42");
        assert_eq!(string_value(&mut engine, "String(workerTrusted)"), "true");
        assert_eq!(string_value(&mut engine, "workerOrigins.join(',')"), ",,,");
        assert_eq!(string_value(&mut engine, "String(workerErrors)"), "0");

        eval(
            &mut engine,
            "classicWorker.terminate(); moduleWorker.terminate();",
            "Worker cleanup",
        )
        .unwrap();
    }

    #[test]
    fn websocket_boundary_negotiates_and_delivers_ordered_protocol_tasks() {
        // WebSockets §2.2/§4 and RFC 6455 §4.1: the opening response proves receipt of
        // the nonce, selects one offered subprotocol, and every open/message/send-complete/close
        // notification returns to the page as an ordered WebSocket-task-source task.
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let listener = runtime
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = runtime.spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut chunk = [0u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = stream.read(&mut chunk).await.unwrap();
                assert_ne!(read, 0, "client closed during opening handshake");
                request.extend_from_slice(&chunk[..read]);
            }
            let request = String::from_utf8(request).unwrap();
            assert!(
                request.contains("Sec-WebSocket-Protocol: chat, superchat\r\n"),
                "{request}"
            );
            let key = request
                .lines()
                .find_map(|line| line.strip_prefix("Sec-WebSocket-Key:").map(str::trim))
                .unwrap();
            let accept = crate::ws::websocket_accept(key);
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 101 Switching Protocols\r\n\
                         Upgrade: websocket\r\n\
                         Connection: keep-alive, Upgrade\r\n\
                         Sec-WebSocket-Accept: {accept}\r\n\
                         Sec-WebSocket-Protocol: chat\r\n\r\n"
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();

            let text = read_test_client_frame(&mut stream).await;
            let binary = read_test_client_frame(&mut stream).await;
            assert_eq!(text, (0x1, "hé".as_bytes().to_vec()));
            assert_eq!(binary, (0x2, vec![0, 0x80, 0xff]));
            write_test_server_frame(&mut stream, 0x1, b"reply").await;
            write_test_server_frame(&mut stream, 0x2, &[0, 0x80, 0xff]).await;

            let close = read_test_client_frame(&mut stream).await;
            assert_eq!(close.0, 0x8);
            assert_eq!(&close.1[..2], &1000u16.to_be_bytes());
            assert_eq!(&close.1[2..], b"bye");
            write_test_server_frame(&mut stream, 0x8, &close.1).await;
        });

        let page = url::Url::parse(&format!("http://127.0.0.1:{port}/page")).unwrap();
        let cache = Arc::new(crate::http::PageCache::default());
        let (task_tx, mut task_rx) = tokio::sync::mpsc::unbounded_channel();
        let clock = Rc::new(RealmClock::new());
        let mut state = HostState::new(Rc::new(RefCell::new(Dom::new())), clock);
        state.enable_network(page.clone(), runtime.handle().clone(), cache, task_tx);
        let mut engine = configured_engine(state, page.as_str());
        eval(
            &mut engine,
            &format!(
                r#"
                globalThis.wsLog = [];
                globalThis.wsErrors = 0;
                globalThis.wsEventsTrusted = true;
                try {{ new WebSocket('ftp://example.test/'); }} catch (error) {{ wsLog.push('bad-url:' + error.name); }}
                try {{ new WebSocket('/duplicate', ['chat', 'chat']); }} catch (error) {{ wsLog.push('bad-protocol:' + error.name); }}
                globalThis.socket = new WebSocket('http://127.0.0.1:{port}/echo', ['chat', 'superchat']);
                socket.binaryType = 'arraybuffer';
                try {{ socket.binaryType = 'invalid'; }} catch (error) {{ wsLog.push('bad-binary:' + error.name); }}
                try {{ socket.send('too-soon'); }} catch (error) {{ wsLog.push('connecting-send:' + error.name); }}
                try {{ socket.close(2000); }} catch (error) {{ wsLog.push('bad-close:' + error.name); }}
                try {{ socket.close(1000, 'é'.repeat(62)); }} catch (error) {{ wsLog.push('long-reason:' + error.name); }}
                globalThis.openListenerCount = 0;
                socket.addEventListener('open', function (event) {{ openListenerCount++; wsLog.push('open-listener'); wsEventsTrusted = wsEventsTrusted && event.isTrusted; }});
                socket.onopen = function (event) {{
                    wsEventsTrusted = wsEventsTrusted && event.isTrusted;
                    wsLog.push('open:' + socket.protocol);
                    socket.send('hé');
                    socket.send(new Uint8Array([0, 128, 255]));
                    globalThis.bufferedDuringOpen = socket.bufferedAmount;
                }};
                globalThis.messageCount = 0;
                socket.onmessage = function (event) {{
                    wsEventsTrusted = wsEventsTrusted && event.isTrusted;
                    messageCount++;
                    if (typeof event.data === 'string') wsLog.push('text:' + event.data + ':' + event.origin);
                    else wsLog.push('binary:' + Array.from(new Uint8Array(event.data)).join(','));
                    if (messageCount === 2) socket.close(1000, 'bye');
                }};
                socket.onerror = function () {{ wsErrors++; }};
                socket.onclose = function (event) {{
                    wsEventsTrusted = wsEventsTrusted && event.isTrusted;
                    wsLog.push('close:' + event.code + ':' + event.reason + ':' + event.wasClean);
                    socket.send('z');
                    globalThis.bufferedAfterClose = socket.bufferedAmount;
                    globalThis.wsClosed = true;
                }};
                "#
            ),
            "WebSocket setup",
        )
        .unwrap();

        for _ in 0..12 {
            if string_value(&mut engine, "String(globalThis.wsClosed === true)") == "true" {
                break;
            }
            let task = runtime
                .block_on(async {
                    tokio::time::timeout(Duration::from_secs(2), task_rx.recv()).await
                })
                .expect("WebSocket task completes")
                .expect("WebSocket task channel remains open");
            dispatch_host_task(&mut engine, task).unwrap();
            run_microtask_checkpoint(&mut engine);
        }
        runtime.block_on(server).unwrap();

        let log = string_value(&mut engine, "wsLog.join('|')");
        assert!(log.contains("bad-url:SyntaxError"), "{log}");
        assert!(log.contains("bad-protocol:SyntaxError"), "{log}");
        assert!(log.contains("bad-binary:TypeError"), "{log}");
        assert!(log.contains("connecting-send:InvalidStateError"), "{log}");
        assert!(log.contains("bad-close:InvalidAccessError"), "{log}");
        assert!(log.contains("long-reason:SyntaxError"), "{log}");
        assert!(log.contains("open-listener|open:chat"), "{log}");
        assert_eq!(log.matches("open:chat").count(), 1, "{log}");
        assert!(
            log.contains(&format!("text:reply:ws://127.0.0.1:{port}")),
            "{log}"
        );
        assert!(log.contains("binary:0,128,255"), "{log}");
        assert!(log.contains("close:1000:bye:true"), "{log}");
        assert_eq!(string_value(&mut engine, "String(openListenerCount)"), "1");
        assert_eq!(string_value(&mut engine, "String(wsErrors)"), "0");
        assert_eq!(string_value(&mut engine, "String(wsEventsTrusted)"), "true");
        assert_eq!(string_value(&mut engine, "String(bufferedDuringOpen)"), "6");
        assert_eq!(string_value(&mut engine, "String(bufferedAfterClose)"), "1");
        assert_eq!(
            string_value(&mut engine, "socket.url"),
            format!("ws://127.0.0.1:{port}/echo")
        );
    }

    #[test]
    fn injected_scripts_modules_and_stylesheets_complete_as_resource_tasks() {
        // HTML §4.12.1.1: a connected inline classic executes during post-connection, while
        // external classic/module scripts execute when their fetched result is ready. HTML
        // §4.2.4.3 attaches a successfully obtained CSS sheet before firing the link's load event.
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let page = url::Url::parse(DEFAULT_URL).unwrap();
        let cache = Arc::new(crate::http::PageCache::default());
        let seed = |path: &str, content_type: &str, body: &[u8]| {
            let url = page.join(path).unwrap();
            cache.seed_with_headers(
                url.to_string(),
                200,
                content_type.to_string(),
                vec![(String::from("content-type"), content_type.to_string())],
                body.to_vec(),
            );
        };
        seed(
            "chunk.js",
            "text/javascript",
            br#"
                resourceOrder.push('classic-exec');
                globalThis.classicCurrent = document.currentScript === document.getElementById('classic');
                import('./classic-dep.js').then(function (dependency) {
                    globalThis.classicImport = dependency.answer;
                    resourceOrder.push('classic-import:' + dependency.answer);
                });
                Promise.resolve().then(function () {
                    globalThis.classicMicroCurrent = document.currentScript === document.getElementById('classic');
                    resourceOrder.push('classic-micro');
                });
            "#,
        );
        seed(
            "classic-dep.js",
            "text/javascript",
            b"export const answer = 17;",
        );
        seed(
            "module.js",
            "text/javascript",
            br#"
                import { answer } from 'data:text/javascript,export%20const%20answer%20%3D%2042%3B';
                globalThis.moduleAnswer = answer;
                resourceOrder.push('module-exec');
            "#,
        );
        seed(
            "chunk.css",
            "text/css",
            b"#resource-target { display: grid; grid-template-columns: 90px 110px; }",
        );
        let (task_tx, mut task_rx) = tokio::sync::mpsc::unbounded_channel();
        let clock = Rc::new(RealmClock::new());
        let mut state = HostState::new(Rc::new(RefCell::new(Dom::new())), clock);
        state.enable_network(page, runtime.handle().clone(), cache, task_tx);
        let mut engine = configured_engine(state, DEFAULT_URL);

        eval(
            &mut engine,
            r#"
                globalThis.resourceOrder = [];
                const html = document.createElement('html');
                const head = document.createElement('head');
                const body = document.createElement('body');
                document.appendChild(html); html.appendChild(head); html.appendChild(body);
                const target = document.createElement('div');
                target.id = 'resource-target';
                document.body.appendChild(target);

                const inline = document.createElement('script');
                globalThis.inlineElement = inline;
                inline.textContent = "globalThis.inlineCurrent = document.currentScript === globalThis.inlineElement; resourceOrder.push('inline-exec'); Promise.resolve().then(function () { resourceOrder.push('inline-micro'); })";
                document.body.appendChild(inline);
                resourceOrder.push('after-inline');

                const emptyModule = document.createElement('script');
                emptyModule.type = 'module';
                emptyModule.onload = function () { resourceOrder.push('inline-module-load'); };
                emptyModule.onerror = function () { resourceOrder.push('inline-module-error'); };
                document.body.appendChild(emptyModule);

                const link = document.createElement('link');
                link.rel = 'stylesheet'; link.href = '/chunk.css';
                link.onload = function () { resourceOrder.push('style-load:' + getComputedStyle(target).display); };
                link.onerror = function () { resourceOrder.push('style-error'); };
                document.head.appendChild(link);

                const classic = document.createElement('script');
                classic.id = 'classic'; classic.src = '/chunk.js';
                classic.onload = function () { resourceOrder.push('classic-load'); };
                classic.onerror = function () { resourceOrder.push('classic-error'); };
                document.body.appendChild(classic);

                const module = document.createElement('script');
                module.type = 'module'; module.src = '/module.js';
                module.onload = function () { resourceOrder.push('module-load:' + moduleAnswer); };
                module.onerror = function () { resourceOrder.push('module-error'); };
                document.body.appendChild(module);
                resourceOrder.push('after-external-insert');
            "#,
            "insert dynamic resources",
        )
        .unwrap();
        run_microtask_checkpoint(&mut engine);
        assert_eq!(
            string_value(&mut engine, "resourceOrder.slice(0, 4).join('|')"),
            "inline-exec|after-inline|after-external-insert|inline-micro"
        );
        assert_eq!(string_value(&mut engine, "String(inlineCurrent)"), "true");
        assert_eq!(
            engine
                .ctx()
                .host_mut::<HostState>()
                .map(|state| state.pending_resources),
            Some(4)
        );

        // Four inserted resources plus the classic script's asynchronous dynamic-import fetch.
        // The latter does not delay the classic script element's load event, but its promise must
        // still settle in a later networking task.
        for _ in 0..8 {
            let resources_done = engine
                .ctx()
                .host_mut::<HostState>()
                .is_some_and(|state| state.pending_resources == 0);
            if resources_done
                && string_value(&mut engine, "String(globalThis.classicImport)") == "17"
            {
                break;
            }
            let task = runtime
                .block_on(async {
                    tokio::time::timeout(Duration::from_secs(2), task_rx.recv()).await
                })
                .expect("resource task completes")
                .expect("resource task channel remains open");
            dispatch_host_task(&mut engine, task).unwrap();
            run_microtask_checkpoint(&mut engine);
        }
        let order = string_value(&mut engine, "resourceOrder.join('|')");
        assert!(order.contains("style-load:grid"), "{order}");
        assert!(order.contains("classic-exec"), "{order}");
        assert!(order.contains("classic-micro"), "{order}");
        assert!(order.contains("classic-import:17"), "{order}");
        assert!(order.contains("classic-load"), "{order}");
        assert!(order.contains("inline-module-load"), "{order}");
        assert!(order.contains("module-exec"), "{order}");
        assert!(order.contains("module-load:42"), "{order}");
        assert!(!order.contains("-error"), "{order}");
        assert_eq!(order.matches("style-load:grid").count(), 1, "{order}");
        assert_eq!(order.matches("classic-load").count(), 1, "{order}");
        assert_eq!(order.matches("inline-module-load").count(), 1, "{order}");
        assert_eq!(order.matches("module-load:42").count(), 1, "{order}");
        assert!(
            order.find("classic-exec") < order.find("classic-micro")
                && order.find("classic-micro") < order.find("classic-load"),
            "classic script cleanup/load ordering: {order}"
        );
        assert!(
            order.find("classic-load") < order.find("classic-import:17"),
            "dynamic import must not delay classic-script load completion: {order}"
        );
        assert_eq!(string_value(&mut engine, "String(classicCurrent)"), "true");
        assert_eq!(string_value(&mut engine, "String(classicImport)"), "17");
        assert_eq!(
            string_value(&mut engine, "String(classicMicroCurrent)"),
            "true"
        );
        assert_eq!(
            engine
                .ctx()
                .host_mut::<HostState>()
                .map(|state| state.pending_resources),
            Some(0)
        );
    }

    #[test]
    fn top_level_await_delays_module_load_completion() {
        // HTML §4.12.1: running a module script waits for its evaluation promise. In particular,
        // top-level await must delay both the script element's load event and the document's load
        // event until the awaited dynamic-import graph finishes evaluating.
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let page = url::Url::parse(DEFAULT_URL).unwrap();
        let cache = Arc::new(crate::http::PageCache::default());
        cache.seed(
            page.join("dep.js").unwrap().to_string(),
            200,
            String::from("text/javascript"),
            b"globalThis.awaitedModuleBody = 'done'; export default 1;".to_vec(),
        );
        let (task_tx, mut task_rx) = tokio::sync::mpsc::unbounded_channel();
        let clock = Rc::new(RealmClock::new());
        let mut state = HostState::new(Rc::new(RefCell::new(Dom::new())), clock);
        state.enable_network(page, runtime.handle().clone(), cache, task_tx);
        let mut engine = configured_engine(state, DEFAULT_URL);
        eval(
            &mut engine,
            r#"
                const html = document.createElement('html');
                const body = document.createElement('body');
                document.appendChild(html); html.appendChild(body);
                const script = document.createElement('script');
                script.id = 'entry';
                globalThis.awaitedModuleEvent = 'pending';
                script.onload = () => awaitedModuleEvent = 'load';
                script.onerror = () => awaitedModuleEvent = 'error';
                body.appendChild(script);
            "#,
            "module event target",
        )
        .unwrap();
        let node_id = engine
            .ctx()
            .host_mut::<HostState>()
            .and_then(|state| state.dom.borrow().get_by_id("entry"))
            .expect("module event target exists");

        run_injected_module_task(
            &mut engine,
            node_id,
            DEFAULT_URL,
            "await import('./dep.js'); globalThis.awaitedEntryBody = 'done';",
        )
        .unwrap();
        assert_eq!(string_value(&mut engine, "awaitedModuleEvent"), "pending");
        assert_eq!(
            engine
                .ctx()
                .host_mut::<HostState>()
                .map(|state| state.pending_resources),
            Some(1)
        );

        let task = runtime
            .block_on(async { tokio::time::timeout(Duration::from_secs(2), task_rx.recv()).await })
            .expect("dynamic module task completes")
            .expect("dynamic module channel remains open");
        dispatch_host_task(&mut engine, task).unwrap();
        run_microtask_checkpoint(&mut engine);

        assert_eq!(string_value(&mut engine, "awaitedModuleBody"), "done");
        assert_eq!(string_value(&mut engine, "awaitedEntryBody"), "done");
        assert_eq!(string_value(&mut engine, "awaitedModuleEvent"), "load");
        assert_eq!(
            engine
                .ctx()
                .host_mut::<HostState>()
                .map(|state| state.pending_resources),
            Some(0)
        );
    }

    #[test]
    fn core_dom_boundary_runs_through_the_shared_prelude() {
        // WHATWG DOM §4.2.3 insertion, §4.4 Node, and §4.5 Document/adoptNode; WHATWG HTML's
        // DOMParser HTML-document parsing steps. This intentionally enters through the exposed JS
        // objects rather than testing Rust adapters in isolation.
        let mut engine = platform_engine();
        eval(
            &mut engine,
            r##"
            const root = document.createElement("SECTION");
            root.setAttribute("DATA-X", "one");
            const text = document.createTextNode("hello");
            const comment = document.createComment("marker");
            root.appendChild(text);
            root.insertBefore(comment, text);
            const span = document.createElement("span");
            span.textContent = "child";
            root.appendChild(span);
            const fragment = document.createDocumentFragment();
            const bold = document.createElement("b");
            bold.textContent = "frag";
            fragment.appendChild(bold);
            root.appendChild(fragment);

            document.appendChild(root);
            const connectedBeforeAdoption = root.isConnected;
            let cycleError = "none";
            try { span.appendChild(root); } catch (error) { cycleError = error.name; }

            const parsed = new DOMParser().parseFromString(
                "<!doctype html><html><head><title>x</title></head><body><p>y</p></body></html>",
                "text/html"
            );
            const parsedHtml = parsed.childNodes.find(node => node.nodeType === 1);
            const parsedSections = parsedHtml.children.map(node => node.localName).join(",");
            const adopted = parsed.adoptNode(root);

            const foreignParent = document.createElement("aside");
            const foreignChild = document.createElement("em");
            foreignParent.appendChild(foreignChild);
            const rejected = document.createElement("i");
            let insertError = "none", removeError = "none";
            try { root.insertBefore(rejected, foreignChild); }
            catch (error) { insertError = error.name; }
            try { root.removeChild(foreignChild); }
            catch (error) { removeError = error.name; }

            globalThis.coreDomResult = [
                root.localName,
                root.getAttribute("data-x"),
                root.getAttributeNames().join(","),
                comment.nextSibling === text,
                text.previousSibling === comment,
                root.textContent,
                cycleError,
                connectedBeforeAdoption,
                adopted === root,
                root.ownerDocument === parsed,
                root.parentNode === null,
                root.isConnected,
                parsedHtml.localName,
                parsedSections,
                insertError,
                rejected.parentNode === null,
                removeError,
                foreignChild.parentNode === foreignParent
            ].join("|");
            "##,
            "core DOM boundary",
        )
        .unwrap();

        assert_eq!(
            string_value(&mut engine, "coreDomResult"),
            "section|one|data-x|true|true|hellochildfrag|HierarchyRequestError|true|true|true|true|false|html|head,body|NotFoundError|true|NotFoundError|true"
        );
    }

    #[test]
    fn document_all_uses_lumens_real_html_dda_exotic() {
        let mut engine = platform_engine();
        eval(
            &mut engine,
            r#"
            globalThis.htmlDdaResult = [
                typeof document.all,
                Boolean(document.all),
                document.all == null,
                document.all == undefined,
                document.all === null,
                document.all === undefined,
                document.all === document.all,
                String(document.all())
            ].join("|");
            "#,
            "document.all Annex B semantics",
        )
        .unwrap();
        assert_eq!(
            string_value(&mut engine, "htmlDdaResult"),
            "undefined|false|true|true|false|false|true|null"
        );
    }

    #[test]
    fn selectors_serialization_shadow_templates_css_and_url_share_the_live_arena() {
        // DOM scope-match/clone algorithms, HTML fragment parsing and serialization, Shadow DOM
        // host/root relationships, CSS selector parsing, and WHATWG URL component setters.
        let mut engine = platform_engine();
        eval(
            &mut engine,
            r##"
            const html = document.createElement("html");
            const body = document.createElement("body");
            document.appendChild(html);
            html.appendChild(body);
            body.innerHTML = '<section id="a" class="x"><template><b>inside</b></template>'
                + '<div id="host"></div></section>';
            const section = body.querySelector("section.x");
            section.insertAdjacentHTML("beforeend", '<p data-k="v">tail</p>');
            const paragraph = section.querySelector("p[data-k=v]");
            const clone = section.cloneNode(true);
            const template = section.querySelector("template");
            const host = section.querySelector("#host");
            const shadow = host.attachShadow({ mode: "open" });
            shadow.innerHTML = '<slot></slot><i class="shadow-item">shade</i>';

            const style = document.createElement("style");
            style.textContent = "p { color: red } @media (min-width: 1px) { b { display: block } }";
            body.appendChild(style);
            const url = new URL("/a", "https://example.com/base");
            url.pathname = "c%20d";
            url.search = "?q=1";
            url.hash = "#h";

            globalThis.extendedDomResult = [
                document.documentElement === html,
                section.matches("section.x"),
                body.querySelectorAll("section > p").length,
                paragraph.getAttribute("data-k"),
                clone !== section && clone.querySelector("p").textContent,
                template.content.firstElementChild.localName,
                host.shadowRoot === shadow,
                shadow.querySelector("i.shadow-item").textContent,
                CSS.supports("selector(section.x > p)"),
                style.sheet.cssRules.length,
                body.innerHTML.includes('data-k="v"'),
                url.href
            ].join("|");
            "##,
            "extended synchronous DOM boundary",
        )
        .unwrap();

        assert_eq!(
            string_value(&mut engine, "extendedDomResult"),
            "true|true|1|v|tail|b|true|shade|true|2|true|https://example.com/c%20d?q=1#h"
        );
    }

    #[test]
    fn geometry_media_images_and_frames_use_canonical_platform_state() {
        // CSSOM §7.2/§9, CSSOM View §§4/6, and HTML §§4.8.4/4.8.5. The assertions enter through
        // the shared prelude so wrapper behavior and the Lumen host calls are covered together.
        let mut engine = platform_engine();
        eval(
            &mut engine,
            r##"
            const html = document.createElement("html");
            const body = document.createElement("body");
            document.appendChild(html);
            html.appendChild(body);
            body.innerHTML = `
                <div id="grid" style="display:grid;width:240px;height:80px;grid-template-columns:100px 140px">
                    <span>a</span><span>b</span>
                </div>
                <div id="scroller" style="width:120px;height:40px;overflow:auto">
                    <div style="width:300px;height:100px">large</div>
                </div>
                <img id="responsive" src="fallback.png"
                     srcset="small.png 320w, large.png 640w" sizes="100vw">
                <img id="blank"><img id="inline" src="data:image/gif;base64,R0lGODlhAQABAIAAAAAAAP///ywAAAAAAQABAAACAUwAOw==">
                <iframe id="frame" srcdoc="<a id='inside' href='child'>child</a>"></iframe>`;

            const grid = document.getElementById("grid");
            const gridStyle = getComputedStyle(grid);
            const rect = grid.getBoundingClientRect();
            const scroller = document.getElementById("scroller");
            const overflow = scroller.scrollWidth > scroller.clientWidth
                && scroller.scrollHeight > scroller.clientHeight;
            scroller.scrollLeft = 30;
            scroller.scrollTop = 25;

            const responsive = document.getElementById("responsive");
            const frame = document.getElementById("frame");
            const inside = frame.contentDocument.querySelector("#inside");
            globalThis.geometryResult = [
                matchMedia("screen and (min-width: 600px)").matches,
                matchMedia("(max-width: 639px)").matches,
                gridStyle.display,
                gridStyle.gridTemplateColumns,
                rect.width,
                rect.height,
                overflow,
                scroller.scrollLeft,
                scroller.scrollTop,
                responsive.currentSrc,
                responsive.complete,
                document.getElementById("blank").complete,
                document.getElementById("inline").complete,
                inside.textContent,
                inside.getAttribute("href")
            ].join("|");
            "##,
            "geometry and environment boundary",
        )
        .unwrap();

        assert_eq!(
            string_value(&mut engine, "geometryResult"),
            "true|false|grid|100px 140px|240|80|true|30|25|https://example.com/large.png|false|true|true|child|https://example.com/child"
        );
    }

    #[test]
    fn binary_storage_cookie_blob_and_popover_hosts_follow_the_platform_prelude() {
        // Encoding §7.4, Web Crypto §14.3.5, Compression Streams §4, Web Storage, cookies, File
        // API blob URLs, and HTML popover state all enter through the shared browser surface.
        let mut engine = platform_engine();
        eval(
            &mut engine,
            r##"
            const hex = value => Array.from(new Uint8Array(value))
                .map(byte => byte.toString(16).padStart(2, "0")).join("");
            const encoded = new TextEncoder().encode("Aé🙂");
            globalThis.binaryResult = [Array.from(encoded).join(",")];

            const source = new Uint8Array([0, 97, 98, 99, 0]);
            crypto.subtle.digest("SHA-256", source.subarray(1, 4))
                .then(value => binaryResult.push(hex(value)));
            crypto.subtle.digest("SHA-256", new DataView(source.buffer, 1, 3))
                .then(value => binaryResult.push(hex(value)));

            const compression = new CompressionStream("gzip");
            const writer = compression.writable.getWriter();
            const reader = compression.readable.getReader();
            writer.write(new TextEncoder().encode("hello"));
            writer.close();
            reader.read().then(result => binaryResult.push(
                result.value[0] + "," + result.value[1] + "," + (result.value.byteLength > 10)
            ));

            localStorage.clear();
            localStorage.setItem("alpha", "one");
            localStorage.setItem("beta", "two");
            const stored = [localStorage.length, localStorage.getItem("alpha")].join(":");
            localStorage.removeItem("beta");
            document.cookie = "lumen_port_cookie=ready; Path=/";

            const blobUrl = URL.createObjectURL(new Blob([
                new Uint8Array([0, 128, 255])
            ], { type: "application/x-lumen-port" }));
            globalThis.blobPortUrl = blobUrl;

            const popover = document.createElement("div");
            popover.setAttribute("popover", "auto");
            document.appendChild(popover);
            popover.showPopover();
            const open = popover.matches(":popover-open");
            popover.hidePopover();
            globalThis.hostStateResult = [
                stored,
                localStorage.length,
                document.cookie.includes("lumen_port_cookie=ready"),
                open,
                popover.matches(":popover-open")
            ].join("|");
            "##,
            "binary and stateful host boundary",
        )
        .unwrap();
        run_microtask_checkpoint(&mut engine);

        assert_eq!(
            string_value(&mut engine, "binaryResult.join('|')"),
            "65,195,169,240,159,153,130|ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad|ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad|31,139,true"
        );
        assert_eq!(
            string_value(&mut engine, "hostStateResult"),
            "2:one|1|true|true|false"
        );

        let blob_url = string_value(&mut engine, "blobPortUrl");
        let blobs = engine
            .ctx()
            .host_mut::<HostState>()
            .expect("host state")
            .blobs
            .clone();
        assert_eq!(
            blobs.lock().unwrap().get(&blob_url).cloned(),
            Some((vec![0, 128, 255], "application/x-lumen-port".to_owned()))
        );
    }

    #[test]
    fn minimal_boundary_boots_the_real_prelude() {
        let mut engine = platform_engine();
        let node_type = eval_value(&mut engine, "document.nodeType", "node type").unwrap();
        assert_eq!(node_type.as_num_opt(), Some(9.0));
        assert_eq!(crate::dom::DOCUMENT, 0);
    }

    #[test]
    fn html_task_microtask_and_timer_order_is_preserved() {
        // HTML §8.1.7.3: the host performs a microtask checkpoint after the script task and after
        // each timer task. A timer queued by the first callback follows already-queued timers.
        let mut engine = platform_engine();
        eval(
            &mut engine,
            r#"
            __trust.oneShot = true;
            globalThis.order = ["script"];
            Promise.resolve().then(() => order.push("script-microtask"));
            setTimeout(() => {
                order.push("timer-1");
                Promise.resolve().then(() => order.push("timer-1-microtask"));
                setTimeout(() => order.push("nested-timer"), 0);
            }, 0);
            setTimeout(() => order.push("timer-2"), 0);
            "#,
            "ordering setup",
        )
        .unwrap();

        assert_eq!(string_value(&mut engine, "order.join(',')"), "script");
        run_microtask_checkpoint(&mut engine);
        assert_eq!(
            string_value(&mut engine, "order.join(',')"),
            "script,script-microtask"
        );

        for expected in [
            "script,script-microtask,timer-1,timer-1-microtask",
            "script,script-microtask,timer-1,timer-1-microtask,timer-2",
            "script,script-microtask,timer-1,timer-1-microtask,timer-2,nested-timer",
        ] {
            assert!(matches!(
                call_trust_method(&mut engine, "tick", &[]),
                Value::Bool(true)
            ));
            run_microtask_checkpoint(&mut engine);
            assert_eq!(string_value(&mut engine, "order.join(',')"), expected);
        }
        assert!(matches!(
            call_trust_method(&mut engine, "tick", &[]),
            Value::Bool(false)
        ));
    }

    #[test]
    fn resident_realm_and_interval_state_survive_host_reentry() {
        let mut engine = platform_engine();
        eval(
            &mut engine,
            r#"
            globalThis.counter = 40;
            globalThis.intervalOrder = [];
            const interval = setInterval(function (prefix) {
                counter++;
                intervalOrder.push(prefix + counter);
                Promise.resolve().then(() => intervalOrder.push("micro-" + counter));
                if (counter === 42) clearInterval(interval);
            }, 5, "v");
            "#,
            "resident realm setup",
        )
        .unwrap();
        eval(&mut engine, "counter += 1", "second host entry").unwrap();

        let now = eval_value(&mut engine, "__trust.now() + 100", "timer deadline").unwrap();
        assert_eq!(
            call_trust_method(&mut engine, "tickTo", &[now]).as_num_opt(),
            Some(1.0)
        );
        run_microtask_checkpoint(&mut engine);
        assert_eq!(
            string_value(&mut engine, "intervalOrder.join(',')"),
            "v42,micro-42"
        );
        let now = eval_value(&mut engine, "__trust.now() + 100", "timer deadline").unwrap();
        assert_eq!(
            call_trust_method(&mut engine, "tickTo", &[now]).as_num_opt(),
            Some(0.0)
        );
        assert_eq!(string_value(&mut engine, "String(counter)"), "42");
    }

    #[test]
    fn navigation_interrupts_the_old_realm_without_poisoning_the_next_realm() {
        let mut old_realm = platform_engine();
        eval(&mut old_realm, "globalThis.marker = 1", "old realm marker").unwrap();
        let interrupt = old_realm.interrupt_handle();
        interrupt.request_user_navigation();
        match old_realm
            .eval_value_interruptible("marker = 99")
            .expect("navigation probe parses")
        {
            Err(EvalError::Interrupted(lumen::InterruptReason::UserNavigation)) => {}
            _ => panic!("old realm did not yield to navigation"),
        }

        // A navigation yield is reusable while the current page actor is still being unwound.
        interrupt.begin_user_interaction();
        eval(&mut old_realm, "marker += 1", "rearmed old realm").unwrap();
        assert_eq!(string_value(&mut old_realm, "String(marker)"), "2");

        // Page teardown is permanent. A replacement page receives a distinct, unpoisoned handle
        // and realm; no author global from the old page crosses the navigation boundary.
        interrupt.cancel();
        match old_realm
            .eval_value_interruptible("marker = 100")
            .expect("teardown probe parses")
        {
            Err(EvalError::Interrupted(lumen::InterruptReason::Cancelled)) => {}
            _ => panic!("torn-down realm accepted another author task"),
        }
        drop(old_realm);

        let mut new_realm = platform_engine();
        assert_eq!(string_value(&mut new_realm, "typeof marker"), "undefined");
        eval(&mut new_realm, "globalThis.marker = 7", "new realm marker").unwrap();
        assert_eq!(string_value(&mut new_realm, "String(marker)"), "7");
    }
}
